//! Lower the Clotho AST into the Atropos MIR (SSA form) for the Phase-1 subset.
//!
//! In addition to the usual synchronous subset, this module lowers `async fn`
//! into a genuine suspending state machine: a future struct holding the
//! coroutine `state` plus every live slot, a `poll` coroutine driven by an
//! entry `switch` on `state` (one resume block per `.await` point), and a sync
//! `run` executor wrapper that drives the future to completion. See
//! [`Lower::lower_async_function`] for the transformation.

use crate::mir;
use tpt_moira_clotho::ast::{
    self, BinOp as ABinOp, Expr, ExprKind, ItemKind, Pat, Program, Stmt, StmtKind, TypeKind,
    UnOp as AUnOp,
};
use std::collections::HashMap;
use std::fmt;

/// An error encountered while lowering to MIR.
#[derive(Debug, Clone)]
pub struct LowerError(pub String);

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lowering error: {}", self.0)
    }
}

type Result<T> = std::result::Result<T, LowerError>;

/// Lower a whole program. `use` declarations are ignored; unsupported items
/// (enum, const, static, nested items) produce an error.
pub fn lower_program(prog: &Program) -> Result<mir::Program> {
    let mut out = mir::Program::default();
    // Collect async fn names up front so synchronous call sites can target the
    // generated run-wrapper (transparent synchronous call) rather than the
    // (absent) bare async body.
    let async_names: std::collections::HashSet<String> = prog
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Function(f) if f.is_async => Some(f.name.node.clone()),
            _ => None,
        })
        .collect();
    // Names of every top-level function (incl. async), used to distinguish a bare
    // call path (`foo(..)`) from an indirect call through a function-pointer local
    // (a closure value) during lowering.
    let all_fn_names: std::collections::HashSet<String> = prog
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Function(f) => Some(f.name.node.clone()),
            _ => None,
        })
        .collect();
    // Literal top-level constants (`const X: i32 = 3;`) are folded into their
    // uses: the map feeds `lower_expr`'s path resolution so a const reference
    // becomes a constant operand instead of an undeclared C variable.
    let const_values: std::collections::HashMap<String, i128> = prog
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Const(c) => const_literal(&c.value).map(|v| (c.name.node.clone(), v)),
            ItemKind::Static(s) => const_literal(&s.value).map(|v| (s.name.node.clone(), v)),
            _ => None,
        })
        .collect();
    // Enum layout registry. The prelude `std` enums are modeled even when the
    // source declares no `enum` items; user `enum` definitions register their
    // own variants below.
    let mut enum_defs: Vec<(String, Vec<(String, usize)>)> = vec![
        (
            "Option".to_string(),
            vec![("Some".to_string(), 1), ("None".to_string(), 0)],
        ),
        (
            "Result".to_string(),
            vec![("Ok".to_string(), 1), ("Err".to_string(), 1)],
        ),
    ];
    for item in &prog.items {
        if let ItemKind::Enum(e) = &item.kind {
            enum_defs.push((
                e.name.node.clone(),
                e.variants
                    .iter()
                    .map(|v| {
                        (
                            v.name.node.clone(),
                            v.fields.as_ref().map(|f| f.len()).unwrap_or(0),
                        )
                    })
                    .collect(),
            ));
        }
    }
    // Each enum becomes a tagged struct `{ long long tag; long long f0; }`;
    // each variant maps its constructor name to the enum + tag + arity.
    let mut enum_structs: Vec<mir::StructDef> = Vec::new();
    let mut variants: HashMap<String, (String, i128, usize)> = HashMap::new();
    for (ename, vars) in &enum_defs {
        let needs_payload = vars.iter().any(|(_, a)| *a > 0);
        let mut fields = vec![("tag".to_string(), "long long".to_string())];
        if needs_payload {
            fields.push(("f0".to_string(), "long long".to_string()));
        }
        enum_structs.push(mir::StructDef {
            name: ename.clone(),
            fields,
        });
        for (tag, (vname, arity)) in vars.iter().enumerate() {
            variants.insert(vname.clone(), (ename.clone(), tag as i128, *arity));
        }
    }


    for st in &enum_structs {
        if !out.structs.iter().any(|s| s.name == st.name) {
            out.structs.push(st.clone());
        }
    }
    for item in &prog.items {
        // `#[cfg(test)]` items (e.g. `mod tests`) are not part of the
        // compiled artifact; test-only code uses macros and library surface
        // outside the codegen subset.
        if item.is_cfg_test() {
            continue;
        }
        match &item.kind {
            ItemKind::Struct(s) => {
                let fields = s
                    .fields
                    .iter()
                    .map(|f| (f.name.node.clone(), type_to_c(&f.ty)))
                    .collect();
                out.structs.push(mir::StructDef {
                    name: s.name.node.clone(),
                    fields,
                });
            }
            // Enums are lowered as tagged structs, registered above.
            ItemKind::Enum(_) => {}
            ItemKind::Use(_) => { /* ignored in Phase 1 */ }
            ItemKind::Impl(_) => { /* trait impls are consumed by the type checker */ }
            ItemKind::Module(_) => { /* resolved/inlined by the frontend */ }
            // Literal constants were folded into `const_values` above; their
            // uses become constant operands, so the item itself needs no code.
            ItemKind::Const(_) | ItemKind::Static(_) => {}
            ItemKind::Function(f) => {
                if f.is_async {
                    let mut l = Lower::new(&out.structs);
                    l.async_fns = async_names.clone();
                    l.all_fn_names = all_fn_names.clone();
                    l.const_values = const_values.clone();
                    let ad = l.lower_async_function(f)?;
                    out.structs.push(mir::StructDef {
                        name: ad.def.fut_name.clone(),
                        fields: ad.def.fut_fields.clone(),
                    });
                    out.functions.push(ad.poll_fn.clone());
                    out.functions.push(ad.run_fn.clone());
                    out.functions.push(ad.ctor_fn.clone());
                    out.functions.extend(ad.closures.iter().cloned());
                    out.async_fns.push(ad.def);
                } else {
                    let mut l = Lower::new(&out.structs);
                    l.async_fns = async_names.clone();
                    l.all_fn_names = all_fn_names.clone();
                    l.const_values = const_values.clone();
                    l.variants = variants.clone();
                    let func = l.lower_function(f)?;
                    let tuple_structs: Vec<mir::StructDef> =
                        l.tuple_structs.values().cloned().collect();
                    out.functions.push(func);
                    out.functions.extend(l.closures.drain(..));
                    out.structs.extend(tuple_structs);
                }
            }
            other => {
                return Err(LowerError(format!(
                    "unsupported top-level item in codegen subset: {:?}",
                    matches!(other, ItemKind::Enum(_))
                )));
            }
        }
    }
    Ok(out)
}

/// The constant value of a `const`/`static` initializer, when it is a literal
/// (optionally negated integer/bool/char). Non-literal initializers return
/// `None` and their uses fall back to variable reads.
fn const_literal(e: &ast::Expr) -> Option<i128> {
    match &e.kind {
        ast::ExprKind::Lit(ast::Lit::Int(v, _)) => Some(*v as i128),
        ast::ExprKind::Lit(ast::Lit::Bool(b)) => Some(*b as i128),
        ast::ExprKind::Lit(ast::Lit::Char(c)) => Some(*c as i128),
        ast::ExprKind::Unary(ast::UnOp::Neg, inner) => const_literal(inner).map(|v| -v),
        _ => None,
    }
}

/// Convert a Clotho type to a C type string, or `"long long"` when unknown.
fn type_to_c(ty: &ast::Type) -> String {
    match &ty.kind {
        ast::TypeKind::Path(p, _) => p
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_else(|| "long long".to_string()),
        ast::TypeKind::Ref(inner) => format!("{}*", type_to_c(inner)),
        ast::TypeKind::RefMut(inner) => format!("{}*", type_to_c(inner)),
        ast::TypeKind::Future(inner) => type_to_c(inner),
        _ => "long long".to_string(),
    }
}

struct Lower {
    structs: Vec<mir::StructDef>,
    func: mir::Function,
    temp_counter: usize,
    current: usize,
    /// Inferred types for SSA temporaries. `long long` when unknown.
    types: HashMap<String, String>,
    /// Stack of `(break_target, continue_target)` for loops.
    loops: Vec<(usize, usize)>,
    /// Names of `async fn`s, so calls to them can target the generated
    /// run-wrapper / future-constructor instead of the (absent) bare body.
    async_fns: std::collections::HashSet<String>,
    /// Lengths of `let`-bound array literals (e.g. `let xs = [1, 2, 3];`), so a
    /// later `for x in xs { .. }` can iterate the known-length array.
    array_lens: HashMap<String, usize>,
    /// Literal top-level constants (`const X: i32 = 3;`) folded into uses:
    /// `lower_expr` substitutes the constant operand for the path.
    const_values: HashMap<String, i128>,
    /// Enum-variant registry: variant constructor name -> (enum struct name,
    /// tag index, payload arity). Enums are represented as tagged structs
    /// `{ long long tag; long long f0; }`; construction lowers to a struct
    /// literal and variant patterns lower to a tag test + field projections.
    variants: HashMap<String, (String, i128, usize)>,
    /// When set, the enclosing `async fn` state-machine bookkeeping: the future
    /// struct name, the `self` parameter local name, the output C type, and the
    /// list of `(slot_name, c_type)` that must be round-tripped across a
    /// suspension (all parameters and locals).
    async_ctx: Option<AsyncCtx>,
    /// True while lowering the inner expression of `.await`, so calls to async
    /// fns yield their future struct (`__ctor`) rather than running to
    /// completion (`__run`).
    expecting_future: bool,
    /// Generated anonymous struct types backing tuple values, keyed by a
    /// deterministic name derived from the component C types. Emitted into the
    /// program's `structs` at the end of lowering.
    tuple_structs: HashMap<String, mir::StructDef>,
    /// Counter for lambda-lifted closure function names (`__moira_closure_N`).
    closure_counter: usize,
    /// Top-level functions (closures) lambda-lifted out of the current function
    /// body, appended to the program after the enclosing function is lowered.
    closures: Vec<mir::Function>,
    /// Names of every top-level (and `async`) function in the program, so a bare
    /// call path can be told apart from an indirect call through a function-pointer
    /// local (a closure value).
    all_fn_names: std::collections::HashSet<String>,
}

/// Build a deterministic struct name for a tuple with the given component C
/// types, e.g. `__tuple_i32_i64`. Empty component list (the unit value) is not
/// modeled as a struct.
fn tuple_struct_name(elem_c_tys: &[String]) -> String {
    let mut name = String::from("__tuple");
    for t in elem_c_tys {
        name.push('_');
        name.push_str(t.replace(' ', "_").replace('*', "p").as_str());
    }
    name
}

/// Build a deterministic C function-pointer type for a non-capturing closure
/// with `param_count` parameters. Every parameter (and the result) is `long long`
/// so the type lines up with the lambda-lifted MIR function's signature.
fn func_ptr_c_type(param_count: usize) -> String {
    if param_count == 0 {
        "long long (*)(void)".to_string()
    } else {
        format!("long long (*)({})", vec!["long long"; param_count].join(", "))
    }
}

/// Collect every single-segment path name referenced by `e` into `out` (deduped).
/// Used by [`closure_captures`] to detect captured variables structurally.
fn collect_path_names(
    e: &Expr,
    bound: &std::collections::HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    match &e.kind {
        ExprKind::Path(p) => {
            if p.segments.len() == 1 {
                let n = p.segments[0].node.clone();
                if !bound.contains(&n) && seen.insert(n.clone()) {
                    out.push(n);
                }
            }
        }
        ExprKind::Lit(_) => {}
        ExprKind::Binary(l, _, r) => {
            collect_path_names(l, bound, out, seen);
            collect_path_names(r, bound, out, seen);
        }
        ExprKind::Unary(_, e) => collect_path_names(e, bound, out, seen),
        ExprKind::Call(f, args) => {
            collect_path_names(f, bound, out, seen);
            for a in args {
                collect_path_names(a, bound, out, seen);
            }
        }
        ExprKind::MethodCall(r, _, args) => {
            collect_path_names(r, bound, out, seen);
            for a in args {
                collect_path_names(a, bound, out, seen);
            }
        }
        ExprKind::Field(e, _) => collect_path_names(e, bound, out, seen),
        ExprKind::Index(b, i) => {
            collect_path_names(b, bound, out, seen);
            collect_path_names(i, bound, out, seen);
        }
        ExprKind::Ref(e, _) => collect_path_names(e, bound, out, seen),
        ExprKind::Deref(e) => collect_path_names(e, bound, out, seen),
        ExprKind::Array(es) => {
            for e in es {
                collect_path_names(e, bound, out, seen);
            }
        }
        ExprKind::Struct(_, fields, base) => {
            for (_, e) in fields {
                collect_path_names(e, bound, out, seen);
            }
            if let Some(b) = base {
                collect_path_names(b, bound, out, seen);
            }
        }
        ExprKind::Tuple(es) => {
            for e in es {
                collect_path_names(e, bound, out, seen);
            }
        }
        ExprKind::Block(b) => collect_path_names_block(b, bound, out, seen),
        ExprKind::If(c, tb, eb) => {
            collect_path_names(c, bound, out, seen);
            collect_path_names_block(tb, bound, out, seen);
            if let Some(e) = eb {
                collect_path_names(e, bound, out, seen);
            }
        }
        ExprKind::While(c, b) => {
            collect_path_names(c, bound, out, seen);
            collect_path_names_block(b, bound, out, seen);
        }
        ExprKind::Loop(b) => collect_path_names_block(b, bound, out, seen),
        ExprKind::For(p, e, b) => {
            collect_path_names(e, bound, out, seen);
            let mut b2 = bound.clone();
            bind_pat_names_in(p, &mut b2);
            collect_path_names_block(b, &b2, out, seen);
        }
        ExprKind::Return(e) => {
            if let Some(e) = e {
                collect_path_names(e, bound, out, seen);
            }
        }
        ExprKind::Break | ExprKind::Continue => {}
        ExprKind::Match(s, arms) => {
            collect_path_names(s, bound, out, seen);
            for (p, e) in arms {
                let mut b2 = bound.clone();
                bind_pat_names_in(p, &mut b2);
                collect_path_names(e, &b2, out, seen);
            }
        }
        ExprKind::Await(e) => collect_path_names(e, bound, out, seen),
        ExprKind::Unsafe(b) => collect_path_names_block(b, bound, out, seen),
        ExprKind::Closure { params, body, .. } => {
            let mut b2 = bound.clone();
            for p in params {
                if let Ok(n) = pat_name(&p.pat) {
                    b2.insert(n);
                }
            }
            collect_path_names(body, &b2, out, seen);
        }
        ExprKind::Cast(e, _) => collect_path_names(e, bound, out, seen),
        ExprKind::Range(a, b, _) => {
            if let Some(a) = a {
                collect_path_names(a, bound, out, seen);
            }
            if let Some(b) = b {
                collect_path_names(b, bound, out, seen);
            }
        }
        _ => {}
    }
}

fn collect_path_names_block(
    b: &ast::Block,
    bound: &std::collections::HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    let mut bound_set = bound.clone();
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Local(local) => {
                if let Some(init) = &local.init {
                    collect_path_names(init, &bound_set, out, seen);
                }
                if let Ok(n) = pat_name(&local.pat) {
                    bound_set.insert(n);
                }
            }
            StmtKind::Semi(e) | StmtKind::Expr(e) => collect_path_names(e, &bound_set, out, seen),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &b.tail {
        collect_path_names(tail, &bound_set, out, seen);
    }
}

fn bind_pat_names_in(pat: &Pat, out: &mut std::collections::HashSet<String>) {
    match pat {
        Pat::Ident(n) => {
            out.insert(n.node.clone());
        }
        Pat::Tuple(ps) => {
            for p in ps {
                bind_pat_names_in(p, out);
            }
        }
        Pat::Wild => {}
        _ => {}
    }
}

/// Return the names a closure would need to capture: single-segment path names in
/// `body` that are not bound within the closure (`bound`, its own params and any
/// internal `let`s) and not top-level function names (`fn_names`).
fn closure_captures(
    body: &Expr,
    bound: &std::collections::HashSet<String>,
    fn_names: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect_path_names(body, bound, &mut names, &mut seen);
    let mut caps: Vec<String> = names
        .into_iter()
        .filter(|n| !bound.contains(n) && !fn_names.contains(n))
        .collect();
    caps.sort();
    caps.dedup();
    caps
}

/// Bookkeeping for lowering an `async fn` into a suspending state machine.
struct AsyncCtx {
    self_local: String,
    /// `(slot_name, c_type)` for every parameter and local that must persist
    /// across a suspension point.
    slots: Vec<(String, String)>,
    /// Number of `.await` points seen so far.
    await_count: usize,
    /// Resume-block index for each `.await` point (resume label `i+1`).
    resume_blocks: Vec<usize>,
}

impl Lower {
    fn new(structs: &[mir::StructDef]) -> Self {
        Lower {
            structs: structs.to_vec(),
            func: mir::Function {
                name: String::new(),
                params: Vec::new(),
                ret: None,
                locals: Vec::new(),
                temp_types: HashMap::new(),
                blocks: Vec::new(),
            },
            temp_counter: 0,
            current: 0,
            types: HashMap::new(),
            loops: Vec::new(),
            array_lens: HashMap::new(),
            const_values: HashMap::new(),
            variants: HashMap::new(),
            async_fns: std::collections::HashSet::new(),
            async_ctx: None,
            expecting_future: false,
            tuple_structs: HashMap::new(),
            closure_counter: 0,
            closures: Vec::new(),
            all_fn_names: std::collections::HashSet::new(),
        }
    }

    fn lower_function(&mut self, f: &ast::Function) -> Result<mir::Function> {
        self.func.name = f.name.node.clone();
        self.func.ret = f.ret.as_ref().map(type_to_c);
        for p in &f.params {
            let name = pat_name(&p.pat)?;
            let ty = type_to_c(&p.ty);
            self.func.params.push(name.clone());
            self.func.locals.push((name, Some(ty)));
        }
        self.new_block(); // entry = block 0, current = 0
        let value = self.lower_block_value(&f.body)?;
        // Terminate the function with a return of the tail value.
        if let mir::Terminator::Unreachable = self.func.blocks[self.current].term {
            let ret = if self.func.ret.is_some() {
                Some(value)
            } else {
                None
            };
            self.set_term(mir::Terminator::Return(ret));
        }
        // Publish the inferred SSA temporary types so the MIR carries them
        // (`temp_types` is what the SSA builder and the C backend read for each
        // `tN`). Without this the map stays empty and temporaries are neither
        // typed nor declared in the C output.
        self.func.temp_types = self.types.clone();
        Ok(self.func.clone())
    }

    /// Lower an `async fn` into its state-machine MIR: a future struct, a `poll`
    /// coroutine (with one resume block per `.await` point, selected by a
    /// `switch` on the saved `state`), and a sync `run` executor wrapper that
    /// drives the future to completion.
    ///
    /// The body must be *linear* — `.await` may appear only at the top level of
    /// the function body (not nested inside `if`/`while`/`loop`/`for`); this is
    /// the supported async subset and keeps the resume-point transformation
    /// straightforward while still producing a genuine suspending state machine.
    fn lower_async_function(mut self, f: &ast::Function) -> Result<AsyncLower> {
        let fut_struct = format!("{}__fut", f.name.node);
        let poll_name = format!("{}__poll", f.name.node);
        let run_name = format!("{}__run", f.name.node);
        let output_ty = f
            .ret
            .as_ref()
            .map(type_to_c)
            .unwrap_or_else(|| "long long".to_string());

        // Collect every parameter + `let` local as a slot that must persist
        // across a suspension.
        let mut slots: Vec<(String, String)> = Vec::new();
        for p in &f.params {
            let name = pat_name(&p.pat)?;
            let ty = type_to_c(&p.ty);
            slots.push((name, ty));
        }
        collect_locals(&f.body, &mut slots);

        // Future struct layout: `state`, params, locals, output, one nested
        // future slot per `.await` point (filled in as we discover them).
        let mut poll_locals: Vec<(String, Option<String>)> =
            vec![("self__fut".to_string(), Some(format!("{}*", fut_struct)))];
        // Each slot is a local of the poll coroutine (the body reads params /
        // locals as ordinary locals; the entry block copies them out of
        // `self__fut` and resume blocks copy them back in).
        for (sname, sty) in &slots {
            poll_locals.push((sname.clone(), Some(sty.clone())));
        }
        self.func = mir::Function {
            name: poll_name.clone(),
            params: vec!["self__fut".to_string()],
            ret: Some("long long".to_string()),
            locals: poll_locals,
            temp_types: HashMap::new(),
            blocks: Vec::new(),
        };
        self.async_ctx = Some(AsyncCtx {
            self_local: "self__fut".to_string(),
            slots: slots.clone(),
            await_count: 0,
            resume_blocks: Vec::new(),
        });

        // Validate linearity, then count the `.await` points (each needs a
        // nested future slot + a resume block).
        let mut await_points = Vec::new();
        let mut err = None;
        scan_awaits(&f.body, &mut await_points, &mut err);
        if let Some(e) = err {
            return Err(e);
        }

        // --- future struct fields ---
        let mut fut_fields: Vec<(String, String)> =
            vec![("__state".to_string(), "long long".to_string())];
        for (sname, sty) in &slots {
            fut_fields.push((sname.clone(), sty.clone()));
        }
        fut_fields.push(("_out".to_string(), output_ty.clone()));
        for (i, ap) in await_points.iter().enumerate() {
            // The nested slot holds the awaited future's own generated struct.
            let callee = match &ap.kind {
                ExprKind::Call(c, _) => match &c.kind {
                    ExprKind::Path(p) => p.segments.last().map(|s| s.node.clone()),
                    _ => None,
                },
                _ => None,
            }
            .unwrap_or_else(|| "void".to_string());
            fut_fields.push((format!("__awt{}", i), format!("{}__fut", callee)));
        }

        // --- poll coroutine blocks ---
        let b_entry = self.new_block();
        let b_start = self.new_block();
        let b_done = self.new_block();

        // b_done: already finished — report READY.
        self.current = b_done;
        self.set_term(mir::Terminator::Return(Some(mir::Operand::Const(0))));

        // b_start: copy parameters into locals, then run the body.
        self.current = b_start;
        for (sname, _sty) in &slots {
            if f.params
                .iter()
                .any(|p| pat_name(&p.pat).ok().as_deref() == Some(sname))
            {
                // parameter: load from self->field into the local
                let t = self.fresh_temp("long long");
                self.emit(
                    t.clone(),
                    mir::Rvalue::Field(mir::Operand::var("self__fut"), sname.clone(), true),
                );
                self.emit(sname.clone(), mir::Rvalue::Use(mir::Operand::var(t)));
            } else {
                // non-parameter local: initialize to 0 so the slot-save at the
                // first suspension point reads a defined value (the body assigns
                // it before it is actually used across the await).
                self.emit(sname.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
            }
        }
        let ret_op = self.lower_block_value(&f.body)?;

        // After the body (in whatever block it ends — the entry `b_start` for a
        // body with no `.await`, or the final resume block otherwise), store the
        // output, mark DONE, and return READY. This epilogue must live in the
        // *body* block, never in the entry block: the entry block is solely the
        // `state` dispatch built just below. (Previously the epilogue was emitted
        // while `self.current` still pointed at the entry block, which clobbered
        // the `Switch` terminator and left the body unreachable.)
        let out_t = self.fresh_temp(output_ty.clone());
        self.emit(out_t.clone(), mir::Rvalue::Use(ret_op));
        self.emit(
            "__scratch".to_string(),
            mir::Rvalue::StoreField(
                mir::Operand::var("self__fut"),
                "_out".to_string(),
                mir::Operand::var(out_t),
                true,
            ),
        );
        let _state_done = self.fresh_temp("long long");
        self.emit(
            "__scratch".to_string(),
            mir::Rvalue::StoreField(
                mir::Operand::var("self__fut"),
                "__state".to_string(),
                mir::Operand::Const(-1),
                true,
            ),
        );
        self.set_term(mir::Terminator::Return(Some(mir::Operand::Const(0))));

        // Entry `switch` on `state` (built last so every resume block is known):
        // `0` runs the body from the top; each `.await`'s resume label `k+1`
        // jumps to that await's resume block; anything else means the future has
        // already finished, so report READY.
        let resume_blocks = self
            .async_ctx
            .as_ref()
            .map(|c| c.resume_blocks.clone())
            .unwrap_or_default();
        let state_t = self.fresh_temp("long long");
        self.current = b_entry;
        self.emit(
            state_t.clone(),
            mir::Rvalue::Field(mir::Operand::var("self__fut"), "__state".to_string(), true),
        );
        let mut cases: Vec<(i64, usize)> = vec![(0, b_start)];
        for (i, &rb) in resume_blocks.iter().enumerate() {
            cases.push(((i + 1) as i64, rb));
        }
        self.set_term(mir::Terminator::Switch(
            mir::Operand::var(state_t),
            cases,
            b_done,
        ));

        let poll_fn = mir::Function {
            name: self.func.name.clone(),
            params: self.func.params.clone(),
            ret: self.func.ret.clone(),
            locals: self.func.locals.clone(),
            temp_types: self.types.clone(),
            blocks: self.func.blocks.clone(),
        };

        // --- run wrapper: `(args) -> T`, builds the future, polls to done ---
        let run_fn = build_run_wrapper(f, &fut_struct, &poll_name, &output_ty, &slots);
        let ctor_fn = build_ctor_wrapper(f, &fut_struct);

        Ok(AsyncLower {
            def: mir::AsyncDef {
                fn_name: f.name.node.clone(),
                fut_name: fut_struct,
                fut_fields,
                poll_name,
                run_name,
                output_ty,
            },
            poll_fn,
            run_fn,
            ctor_fn,
            closures: self.closures,
        })
    }

    fn new_block(&mut self) -> usize {
        self.func.blocks.push(mir::Block::default());
        self.func.blocks.len() - 1
    }

    fn set_term(&mut self, term: mir::Terminator) {
        self.func.blocks[self.current].term = term;
    }

    fn emit(&mut self, place: String, rvalue: mir::Rvalue) {
        self.func.blocks[self.current]
            .stmts
            .push(mir::Stmt { place, rvalue });
    }

    fn fresh_temp(&mut self, ty: impl Into<String>) -> String {
        let t = format!("t{}", self.temp_counter);
        self.temp_counter += 1;
        self.types.insert(t.clone(), ty.into());
        t
    }

    fn temp_type_of(&self, name: &str) -> String {
        self.types
            .get(name)
            .cloned()
            .unwrap_or_else(|| "long long".to_string())
    }

    /// Return (creating if necessary) the generated struct type name for a tuple
    /// whose components have the given C types. The struct definition (field
    /// `f0`, `f1`, … of each component type) is recorded for emission into the
    /// program's struct list.
    fn tuple_type(&mut self, elem_c_tys: Vec<String>) -> String {
        let name = tuple_struct_name(&elem_c_tys);
        self.tuple_structs.entry(name.clone()).or_insert_with(|| {
            let fields = elem_c_tys
                .iter()
                .enumerate()
                .map(|(i, t)| (format!("f{}", i), t.clone()))
                .collect();
            mir::StructDef {
                name: name.clone(),
                fields,
            }
        });
        name
    }

    /// Lower a block, returning the operand holding its value (tail expression
    /// or `()` when there is none).
    fn lower_block_value(&mut self, b: &ast::Block) -> Result<mir::Operand> {
        for s in &b.stmts {
            self.lower_stmt(s)?;
        }
        match &b.tail {
            Some(tail) => self.lower_expr(tail),
            None => Ok(mir::Operand::Unit),
        }
    }

    /// Lambda-lift a non-capturing closure `|params| body` into a fresh top-level
    /// `mir::Function` and return a `Func` operand referencing it (the closure
    /// value). The lifted function always returns `long long` (a `()` result is
    /// returned as `0`, which callers discard) so its C signature lines up with the
    /// uniform `long long (*)(long long, …)` function-pointer type used for closure
    /// values.
    ///
    /// Capturing closures are rejected with the same precise message the type
    /// checker uses, because the codegen subset does not yet model an environment
    /// struct for captured variables.
    fn lower_closure(&mut self, params: &[ast::Param], body: &Expr) -> Result<mir::Operand> {
        let mut param_names = std::collections::HashSet::new();
        for p in params {
            param_names.insert(pat_name(&p.pat)?);
        }
        // Structural capture analysis: any single-segment path name the body
        // references that is neither a closure parameter nor a top-level function is
        // a captured variable.
        let captures = closure_captures(body, &param_names, &self.all_fn_names);
        if !captures.is_empty() {
            return Err(LowerError(format!(
                "closures that capture variables are not yet supported in the codegen subset (captured: {})",
                captures.join(", ")
            )));
        }

        let name = format!("__moira_closure_{}", self.closure_counter);
        self.closure_counter += 1;

        // Build the lifted function in its own context, seeded with the enclosing
        // environment so nested closures and async names resolve.
        let mut sub = Lower::new(&self.structs);
        sub.async_fns = self.async_fns.clone();
        sub.all_fn_names = self.all_fn_names.clone();
        sub.closure_counter = self.closure_counter;
        sub.func.name = name.clone();
        sub.func.ret = Some("long long".to_string());
        for p in params {
            let n = pat_name(&p.pat)?;
            // Structural lowering: closure parameters are `long long` so the lifted
            // signature matches the uniform function-pointer type.
            sub.func.params.push(n.clone());
            sub.func.locals.push((n, Some("long long".to_string())));
        }
        sub.new_block();
        // A closure body is an expression; a block-bodied closure (`|x| { .. }`)
        // lowers through `lower_block_value`, while a bare expression (`|x| x + 1`)
        // lowers through `lower_expr`. Both leave the entry block's terminator
        // `Unreachable`, so we attach the `return` of the tail value.
        let value = match &body.kind {
            ExprKind::Block(b) => sub.lower_block_value(b)?,
            _ => sub.lower_expr(body)?,
        };
        if let mir::Terminator::Unreachable = sub.func.blocks[sub.current].term {
            let ret = if sub.func.ret.is_some() {
                Some(value)
            } else {
                None
            };
            sub.set_term(mir::Terminator::Return(ret));
        }
        sub.func.temp_types = sub.types.clone();
        self.closure_counter = sub.closure_counter;
        self.closures.push(sub.func.clone());
        Ok(mir::Operand::Func(name))
    }

    fn lower_stmt(&mut self, s: &Stmt) -> Result<()> {
        match &s.kind {
            StmtKind::Local(local) => {
                // Tuple destructuring `let (a, b, ..) = init;` binds each
                // identifier in the pattern to the matching tuple component.
                if matches!(local.pat, Pat::Tuple(_)) {
                    return self.lower_tuple_destructure(local);
                }
                let name = pat_name(&local.pat)?;
                // An array-literal initializer binds the local as an array
                // declaration directly. C cannot assign arrays by value, so we
                // must not declare `long long name;` and then `name = temp` —
                // instead emit `long long name[len] = {..};` inline with
                // `place == name`. The length is recorded so a later
                // `for x in name` can iterate the array.
                if let Some(ExprKind::Array(elems)) = local.init.as_ref().map(|i| &i.kind) {
                    let mut operands = Vec::with_capacity(elems.len());
                    for e in elems {
                        operands.push(self.lower_expr(e)?);
                    }
                    self.array_lens.insert(name.clone(), elems.len());
                    self.emit(
                        name.clone(),
                        mir::Rvalue::Array("long long".to_string(), operands),
                    );
                    return Ok(());
                }
                // Infer the declared local type: prefer an explicit annotation,
                // otherwise fall back to a struct-literal RHS so `let p = Point{..}`
                // is typed `struct Point` (otherwise it defaults to `long long`
                // and the struct assignment/field access fails to compile).
                let ty = match (&local.ty, &local.init) {
                    (Some(t), _) => Some(type_to_c(t)),
                    (None, Some(init)) => match &init.kind {
                        ExprKind::Struct(path, _, _) => {
                            path.segments.last().map(|s| s.node.clone())
                        }
                        _ if self.variant_c_type(init).is_some() => {
                            // A variant construction (`let o = Some(5);`) types
                            // the local as the enum's tagged struct.
                            self.variant_c_type(init)
                        }
                        ExprKind::Closure { params, .. } => {
                            // A closure value is a function pointer; type the local
                            // with the uniform `long long (*)(long long, …)` so the C
                            // declaration matches the lambda-lifted function.
                            Some(func_ptr_c_type(params.len()))
                        }
                        ExprKind::Tuple(elems) => {
                            // Infer the generated struct type for the tuple shape
                            // so field access (`p.0`) resolves to the right type.
                            let c_tys: Vec<String> = elems
                                .iter()
                                .map(|e| {
                                    // Best-effort element C type from the
                                    // initializer; constants default to i64.
                                    match &e.kind {
                                        ExprKind::Lit(ast::Lit::Int(..)) => "long long".to_string(),
                                        ExprKind::Lit(ast::Lit::Bool(_)) => "int".to_string(),
                                        _ => "long long".to_string(),
                                    }
                                })
                                .collect();
                            Some(self.tuple_type(c_tys))
                        }
                        _ => None,
                    },
                    (None, None) => None,
                };
                // For an `async fn` the poll coroutine pre-declares every slot
                // (parameter + `let` local) as a local; do not re-declare it here
                // or the MIR carries duplicate locals and the C backend must
                // paper over the redeclaration.
                if !self.func.locals.iter().any(|(n, _)| n == &name) {
                    self.func.locals.push((name.clone(), ty.clone()));
                }
                // Record the local's type so `temp_type_of`/`p.0` resolution works
                // for locals as well as temporaries.
                if let Some(t) = &ty {
                    self.types.insert(name.clone(), t.clone());
                }
                if let Some(init) = &local.init {
                    let v = self.lower_expr(init)?;
                    self.emit(name.clone(), mir::Rvalue::Use(v));
                }
                Ok(())
            }
            StmtKind::Semi(e) | StmtKind::Expr(e) => {
                self.lower_expr(e)?;
                Ok(())
            }
            StmtKind::Item(_) => Err(LowerError(
                "nested items are not supported in the codegen subset".into(),
            )),
        }
    }

    /// Lower `let (a, b, ..) = init;` — bind each identifier in the tuple pattern
    /// to the matching component of the initializer. Tuple-literal RHSs are
    /// decomposed element-wise (each component lowered directly into its binding);
    /// any other RHS is lowered once into a tuple value and each binding reads its
    /// generated `.fN` field. An optional explicit tuple-type annotation supplies
    /// the per-element C types.
    fn lower_tuple_destructure(&mut self, local: &ast::Local) -> Result<()> {
        let elems = match &local.pat {
            Pat::Tuple(e) => e,
            _ => unreachable!("lower_tuple_destructure called on a non-tuple pattern"),
        };
        let ann_elems: Option<Vec<String>> = match &local.ty {
            Some(t) if matches!(t.kind, TypeKind::Tuple(_)) => {
                if let TypeKind::Tuple(ts) = &t.kind {
                    Some(ts.iter().map(type_to_c).collect())
                } else {
                    None
                }
            }
            _ => None,
        };
        // Tuple-literal RHS: lower each component directly into its binding.
        if let Some(ExprKind::Tuple(inits)) = local.init.as_ref().map(|e| &e.kind) {
            if inits.len() != elems.len() {
                return Err(LowerError(
                    "tuple pattern / initializer arity mismatch".into(),
                ));
            }
            for (i, pat) in elems.iter().enumerate() {
                let name = match pat {
                    Pat::Ident(n) => n.node.clone(),
                    Pat::Wild => continue,
                    _ => {
                        return Err(LowerError(
                            "only identifier/wildcard elements are supported in `let` tuple patterns"
                                .into(),
                        ))
                    }
                };
                let v = self.lower_expr(&inits[i])?;
                let ct = ann_elems
                    .as_ref()
                    .and_then(|a| a.get(i).cloned())
                    .unwrap_or_else(|| self.temp_type_of(&name_of(&v)));
                if !self.func.locals.iter().any(|(n, _)| n == &name) {
                    self.func.locals.push((name.clone(), Some(ct.clone())));
                }
                self.types.insert(name.clone(), ct);
                self.emit(name.clone(), mir::Rvalue::Use(v));
            }
            return Ok(());
        }
        // Otherwise lower the whole RHS once into a tuple value `o`.
        let o = match &local.init {
            Some(e) => self.lower_expr(e)?,
            None => {
                return Err(LowerError(
                    "`let` tuple pattern requires an initializer".into(),
                ))
            }
        };
        let base_ty = self.temp_type_of(&name_of(&o));
        for (i, pat) in elems.iter().enumerate() {
            let name = match pat {
                Pat::Ident(n) => n.node.clone(),
                Pat::Wild => continue,
                _ => {
                    return Err(LowerError(
                        "only identifier/wildcard elements are supported in `let` tuple patterns"
                            .into(),
                    ))
                }
            };
            let ct = ann_elems
                .as_ref()
                .and_then(|a| a.get(i).cloned())
                .or_else(|| {
                    self.tuple_structs
                        .get(&base_ty)
                        .and_then(|s| s.fields.get(i))
                        .map(|(_, t)| t.clone())
                })
                .unwrap_or_else(|| "long long".to_string());
            if !self.func.locals.iter().any(|(n, _)| n == &name) {
                self.func.locals.push((name.clone(), Some(ct.clone())));
            }
            self.types.insert(name.clone(), ct);
            self.emit(
                name.clone(),
                mir::Rvalue::Field(o.clone(), format!("f{i}"), false),
            );
        }
        Ok(())
    }

    /// Bind a `for`/loop pattern to a per-iteration element value. A single
    /// identifier binds directly; a tuple pattern `for (a, b) in arr` reads each
    /// element's `.fN` field from the tuple-typed `elem`.
    fn lower_bind_pattern(&mut self, pat: &ast::Pat, elem: mir::Operand, elem_ct: &str) -> Result<()> {
        if let ast::Pat::Tuple(elems) = pat {
            for (i, e) in elems.iter().enumerate() {
                let name = match e {
                    ast::Pat::Ident(n) => n.node.clone(),
                    ast::Pat::Wild => continue,
                    _ => {
                        return Err(LowerError(
                            "only identifier/wildcard elements are supported in `for` tuple patterns"
                                .into(),
                        ))
                    }
                };
                let ct = self
                    .tuple_structs
                    .get(elem_ct)
                    .and_then(|s| s.fields.get(i))
                    .map(|(_, t)| t.clone())
                    .unwrap_or_else(|| "long long".to_string());
                if !self.func.locals.iter().any(|(n, _)| n == &name) {
                    self.func.locals.push((name.clone(), Some(ct.clone())));
                }
                self.types.insert(name.clone(), ct);
                self.emit(name, mir::Rvalue::Field(elem.clone(), format!("f{i}"), false));
            }
            return Ok(());
        }
        let name = pat_name(pat)?;
        if !self.func.locals.iter().any(|(n, _)| n == &name) {
            self.func.locals.push((name.clone(), Some(elem_ct.to_string())));
        }
        self.types.insert(name.clone(), elem_ct.to_string());
        self.emit(name, mir::Rvalue::Use(elem));
        Ok(())
    }

    /// Lower `for <name> in <start>..(<end>[, inclusive]) { <body> }` into a
    /// C-style index loop. `inclusive` selects `<=` (for `..=`) vs `<` (for `..`).
    fn lower_range_for(
        &mut self,
        l: &Expr,
        r: &Expr,
        inclusive: bool,
        name: &str,
        body: &ast::Block,
    ) -> Result<mir::Operand> {
        let a_op = self.lower_expr(l)?;
        let b_op = self.lower_expr(r)?;
        self.func
            .locals
            .push((name.to_string(), Some("long long".into())));
        self.emit(name.to_string(), mir::Rvalue::Use(a_op));

        let head = self.new_block();
        let body_blk = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let cond = mir::Operand::var(name.to_string());
        let cmp = self.fresh_temp("long long");
        self.emit(
            cmp.clone(),
            mir::Rvalue::Binary(
                if inclusive { mir::BinOp::Le } else { mir::BinOp::Lt },
                cond,
                b_op,
            ),
        );
        self.set_term(mir::Terminator::If(
            mir::Operand::var(cmp),
            body_blk,
            exit,
        ));

        self.current = body_blk;
        self.loops.push((exit, head));
        self.lower_block_value(body)?;
        self.loops.pop();
        // i = i + 1
        let i = mir::Operand::var(name.to_string());
        self.emit(
            name.to_string(),
            mir::Rvalue::Binary(mir::BinOp::Add, i, mir::Operand::Const(1)),
        );
        self.set_term(mir::Terminator::Goto(head));

        self.current = exit;
        Ok(mir::Operand::Unit)
    }

    fn lower_expr(&mut self, e: &Expr) -> Result<mir::Operand> {
        match &e.kind {
            ExprKind::Lit(ast::Lit::Int(v, _)) => Ok(mir::Operand::Const(*v as i128)),
            ExprKind::Lit(ast::Lit::Bool(b)) => Ok(mir::Operand::Const(if *b { 1 } else { 0 })),
            ExprKind::Lit(ast::Lit::Float(text, _)) => {
                // Store as integer bits lossily; demo avoids floats in codegen.
                let f: f64 = text.parse().unwrap_or(0.0);
                Ok(mir::Operand::Const(f as i128))
            }
            ExprKind::Lit(ast::Lit::Str(_)) => {
                // String literals are not yet backed by data in the codegen
                // subset; lower to a zero placeholder operand so type-checked
                // programs (e.g. `s.push_str("hi")`) still lower + borrow-check.
                // Real allocation/string layout is a documented codegen follow-up.
                Ok(mir::Operand::Const(0))
            }
            ExprKind::Lit(ast::Lit::Char(c)) => Ok(mir::Operand::Const(*c as i128)),
            ExprKind::Path(p) => {
                let name = p
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                // A literal top-level constant folds to its value; only fall
                // back to a variable read when it is not a known const (the
                // C backend would otherwise see an undeclared identifier).
                if let Some(v) = self.const_values.get(&name) {
                    return Ok(mir::Operand::Const(*v));
                }
                // A nullary variant (`None`) is the enum's tagged struct with
                // just the tag set.
                if let Some((ename, tag, 0)) = self.variants.get(&name).cloned() {
                    let t = self.fresh_temp(&format!("struct {}", ename));
                    self.emit(
                        t.clone(),
                        mir::Rvalue::StructLit(
                            ename,
                            vec![("tag".to_string(), mir::Operand::Const(tag))],
                        ),
                    );
                    return Ok(mir::Operand::var(t));
                }
                Ok(mir::Operand::var(name))
            }
            ExprKind::Range(..) => Err(LowerError(
                "range `..` / `..=` is only supported as a `for` iterator".into(),
            )),
            ExprKind::Binary(l, abop, r) => {
                // Assignment / compound assignment mutate the left-hand place.
                if let Some(arith) = compound_op(*abop) {
                    let lhs = self.lower_expr(l)?;
                    let rhs = self.lower_expr(r)?;
                    let place = match &lhs {
                        mir::Operand::Var(n) => n.clone(),
                        _ => return Err(LowerError("assignment target must be a variable".into())),
                    };
                    let rvalue = mir::Rvalue::Binary(arith, lhs, rhs);
                    self.emit(place.clone(), rvalue);
                    return Ok(mir::Operand::var(place));
                }
                if matches!(abop, ABinOp::Assign) {
                    let lhs = self.lower_expr(l)?;
                    let rhs = self.lower_expr(r)?;
                    let place = match &lhs {
                        mir::Operand::Var(n) => n.clone(),
                        _ => return Err(LowerError("assignment target must be a variable".into())),
                    };
                    self.emit(place.clone(), mir::Rvalue::Use(rhs));
                    return Ok(mir::Operand::var(place));
                }
                if matches!(abop, ABinOp::Range) {
                    return Err(LowerError(
                        "range `..` is only supported as a `for` iterator".into(),
                    ));
                }
                let mop = map_binop(*abop)?;
                let l = self.lower_expr(l)?;
                let r = self.lower_expr(r)?;
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Binary(mop, l, r));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Unary(aop, e) => {
                let mop = match aop {
                    AUnOp::Neg => mir::UnOp::Neg,
                    AUnOp::Not => mir::UnOp::Not,
                    AUnOp::Deref => mir::UnOp::Deref,
                };
                let o = self.lower_expr(e)?;
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Unary(mop, o));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Call(callee, args) => {
                // A tuple-variant constructor (`Some(5)`, `Ok(v)`, user
                // `MyEnum::Var(x)`) lowers to a tagged-struct literal, not a
                // function call.
                if let ExprKind::Path(p) = &callee.kind {
                    let vname = p
                        .segments
                        .last()
                        .map(|s| s.node.clone())
                        .unwrap_or_default();
                    if let Some((ename, tag, arity)) = self.variants.get(&vname).cloned() {
                        return self.lower_variant_construction(&ename, tag, arity, args);
                    }
                }
                // A closure literal as the callee is invoked immediately: lambda-lift
                // it into a top-level function and call that directly.
                let (callee_operand, result_ty) = if let ExprKind::Closure {
                    params,
                    body,
                    ..
                } = &callee.kind
                {
                    (self.lower_closure(params, body)?, "long long".to_string())
                } else {
                    let name = match &callee.kind {
                        ExprKind::Path(p) => p
                            .segments
                            .last()
                            .map(|s| s.node.clone())
                            .unwrap_or_default(),
                        _ => return Err(LowerError("call target must be a path".into())),
                    };
                    // A call to an `async fn` has no bare body; it lowers either to
                    // the future constructor (`__ctor`, used by `.await`) or to the
                    // sync run-wrapper (`__run`, used everywhere else) which drives
                    // the future to completion and returns its output.
                    let is_async_call = self.async_fns.contains(&name);
                    let callee_name = if is_async_call {
                        if self.expecting_future {
                            format!("{}__ctor", name)
                        } else {
                            format!("{}__run", name)
                        }
                    } else {
                        name.clone()
                    };
                    let result_ty = if is_async_call && self.expecting_future {
                        format!("{}__fut", name.trim_end_matches("__ctor"))
                    } else {
                        "long long".to_string()
                    };
                    // A name that is not a known top-level function is an indirect call
                    // through a function-pointer local (a closure value); everything
                    // else is a direct call to a top-level function.
                    let op = if self.all_fn_names.contains(&name) {
                        mir::Operand::Func(callee_name)
                    } else {
                        mir::Operand::Var(name)
                    };
                    (op, result_ty)
                };
                let mut operands = Vec::new();
                for a in args {
                    operands.push(self.lower_expr(a)?);
                }
                let t = self.fresh_temp(result_ty);
                self.emit(t.clone(), mir::Rvalue::Call(callee_operand, operands));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Field(base, field) => {
                let o = self.lower_expr(base)?;
                let base_ty = self.temp_type_of(&name_of(&o));
                // Tuple-index access `t.0`: the base type is a generated
                // `__tuple_*` struct and the field name is a numeric index; map
                // it to the corresponding `fN` field and look up its C type.
                if base_ty.starts_with("__tuple") && field.node.parse::<usize>().is_ok() {
                    let idx: usize = field.node.parse().unwrap();
                    let field_ty = self
                        .tuple_structs
                        .get(&base_ty)
                        .and_then(|s| s.fields.get(idx))
                        .map(|(_, t)| t.clone())
                        .unwrap_or_else(|| "long long".to_string());
                    let t = self.fresh_temp(field_ty);
                    self.emit(t.clone(), mir::Rvalue::Field(o, format!("f{idx}"), false));
                    return Ok(mir::Operand::var(t));
                }
                let field_ty = lookup_field_type(&self.structs, base_ty.as_str(), &field.node)
                    .unwrap_or_else(|| "long long".to_string());
                let t = self.fresh_temp(field_ty);
                self.emit(t.clone(), mir::Rvalue::Field(o, field.node.clone(), false));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Ref(e, mutable) => {
                let o = self.lower_expr(e)?;
                let inner = self.temp_type_of(&name_of(&o));
                let t = self.fresh_temp(format!("{}*", inner));
                self.emit(t.clone(), mir::Rvalue::Ref(o, *mutable));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Struct(path, fields, _base) => {
                let name = path
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                let mut fvals = Vec::new();
                for (fname, fval) in fields {
                    fvals.push((fname.node.clone(), self.lower_expr(fval)?));
                }
                let t = self.fresh_temp(name.clone());
                self.emit(t.clone(), mir::Rvalue::StructLit(name, fvals));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Block(b) => self.lower_block_value(b),
            ExprKind::If(cond, then_b, else_b) => {
                // `if let pat = init { .. }` is parsed as `If(Block{Local}, ..)`.
                if let ExprKind::Block(cb) = &cond.kind {
                    if cb.stmts.len() == 1 && cb.tail.is_none() {
                        if let StmtKind::Local(local) = &cb.stmts[0].kind {
                            return self.lower_if_let(local, then_b, else_b.as_deref());
                        }
                    }
                }
                let cond_op = self.lower_expr(cond)?;
                let result = self.fresh_temp("long long");
                let then_blk = self.new_block();
                let else_blk = self.new_block();
                let merge = self.new_block();
                self.set_term(mir::Terminator::If(cond_op, then_blk, else_blk));

                self.current = then_blk;
                let v_then = self.lower_block_value(then_b)?;
                self.emit(result.clone(), mir::Rvalue::Use(v_then));
                self.set_term(mir::Terminator::Goto(merge));

                self.current = else_blk;
                let v_else = match else_b {
                    Some(eb) => self.lower_expr(eb)?,
                    None => mir::Operand::Unit,
                };
                self.emit(result.clone(), mir::Rvalue::Use(v_else));
                self.set_term(mir::Terminator::Goto(merge));

                self.current = merge;
                Ok(mir::Operand::var(result))
            }
            ExprKind::While(cond, body) => {
                // `while let pat = init { .. }` is parsed as `While(Block{Local}, ..)`.
                if let ExprKind::Block(cb) = &cond.kind {
                    if cb.stmts.len() == 1 && cb.tail.is_none() {
                        if let StmtKind::Local(local) = &cb.stmts[0].kind {
                            return self.lower_while_let(local, body);
                        }
                    }
                }
                self.lower_loop(Some(&**cond), body)
            }
            ExprKind::Loop(body) => self.lower_loop(None, body),
            ExprKind::For(pat, iter, body) => {
                // Desugar `for pat in iter { ... }`. The iterator is either a
                // range `a..b` (iterate `i` over `[a, b)`) or an array (iterate
                // `pat` over `arr[0..len]` via an index variable). `pat` may be a
                // tuple pattern (`for (a, b) in arr_of_tuples`), in which case each
                // element is destructured into its components.
                let array_len: Option<i128> = match &iter.kind {
                    ExprKind::Array(elems) => Some(elems.len() as i128),
                    ExprKind::Path(p) => {
                        let vname = p
                            .segments
                            .last()
                            .map(|s| s.node.clone())
                            .unwrap_or_default();
                        self.array_lens.get(&vname).map(|&n| n as i128)
                    }
                    _ => None,
                };
                match &iter.kind {
                    ExprKind::Binary(l, ABinOp::Range, r) => {
                        let name = pat_name(pat)?;
                        self.lower_range_for(l, r, false, &name, body)
                    }
                    ExprKind::Range(Some(l), Some(r), inclusive) => {
                        let name = pat_name(pat)?;
                        self.lower_range_for(l, r, *inclusive, &name, body)
                    }
                    _ if array_len.is_some() => {
                        let len = array_len.unwrap();
                        // Lower the array to its operand (a `let`-bound array
                        // resolves to the variable; an array literal lowers to an
                        // inline `long long tN[len] = {..};` temp).
                        let arr_op = self.lower_expr(iter)?;
                        // Element C type: for an array literal, infer it from the
                        // first element (so tuple-element arrays bind to the right
                        // component types); otherwise default to `long long`.
                        let elem_ct = if let ExprKind::Array(elems) = &iter.kind {
                            if !elems.is_empty() {
                                let e0 = self.lower_expr(&elems[0])?;
                                self.temp_type_of(&name_of(&e0))
                            } else {
                                "long long".into()
                            }
                        } else {
                            "long long".into()
                        };
                        let idx = self.fresh_temp("long long");
                        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));

                        let head = self.new_block();
                        let body_blk = self.new_block();
                        let exit = self.new_block();
                        self.set_term(mir::Terminator::Goto(head));

                        self.current = head;
                        let cmp = self.fresh_temp("long long");
                        self.emit(
                            cmp.clone(),
                            mir::Rvalue::Binary(
                                mir::BinOp::Lt,
                                mir::Operand::var(idx.clone()),
                                mir::Operand::Const(len),
                            ),
                        );
                        self.set_term(mir::Terminator::If(
                            mir::Operand::var(cmp),
                            body_blk,
                            exit,
                        ));

                        self.current = body_blk;
                        self.loops.push((exit, head));
                        // Bind `pat` to `arr[idx]` (single name or tuple destructure).
                        let elem = self.fresh_temp(elem_ct.clone());
                        self.emit(
                            elem.clone(),
                            mir::Rvalue::Index(arr_op.clone(), mir::Operand::var(idx.clone())),
                        );
                        self.lower_bind_pattern(pat, mir::Operand::var(elem), &elem_ct)?;
                        self.lower_block_value(body)?;
                        self.loops.pop();
                        // idx = idx + 1
                        self.emit(
                            idx.clone(),
                            mir::Rvalue::Binary(
                                mir::BinOp::Add,
                                mir::Operand::var(idx.clone()),
                                mir::Operand::Const(1),
                            ),
                        );
                        self.set_term(mir::Terminator::Goto(head));

                        self.current = exit;
                        Ok(mir::Operand::Unit)
                    }
                    _ => Err(LowerError(
                        "`for` iterator must be a range `a..b`, an array literal `[..]`, or a `let`-bound array"
                            .into(),
                    )),
                }
            }
            ExprKind::Return(opt) => {
                let v = match opt {
                    Some(e) => Some(self.lower_expr(e)?),
                    None => None,
                };
                self.set_term(mir::Terminator::Return(v));
                // Subsequent statements land in a fresh unreachable block.
                self.current = self.new_block();
                Ok(mir::Operand::Unit)
            }
            ExprKind::Break => {
                let (exit, _cont) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| LowerError("`break` outside of a loop".into()))?;
                self.set_term(mir::Terminator::Goto(exit));
                self.current = self.new_block();
                Ok(mir::Operand::Unit)
            }
            ExprKind::Continue => {
                let (_exit, cont) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| LowerError("`continue` outside of a loop".into()))?;
                self.set_term(mir::Terminator::Goto(cont));
                self.current = self.new_block();
                Ok(mir::Operand::Unit)
            }
            ExprKind::Match(scrutinee, arms) => self.lower_match(scrutinee, arms),
            ExprKind::MethodCall(recv, method, args) => {
                // Lower `recv.method(args)` to a plain call `method(recv, args)`;
                // the receiver becomes the first operand. This lets type-checked
                // prelude programs (e.g. `v.push(1)`, `x.is_some()`) lower to MIR
                // and pass the borrow checker. Real method resolution / receiver
                // adjustment is a documented codegen follow-up.
                let recv_op = self.lower_expr(recv)?;
                let mut operands = vec![recv_op];
                for a in args {
                    operands.push(self.lower_expr(a)?);
                }
                let t = self.fresh_temp("long long");
                self.emit(
                    t.clone(),
                    mir::Rvalue::Call(mir::Operand::Func(method.node.clone()), operands),
                );
                Ok(mir::Operand::var(t))
            }
            ExprKind::Index(base, idx) => {
                let b = self.lower_expr(base)?;
                let i = self.lower_expr(idx)?;
                // Element read `base[idx]`; the destination temp carries the
                // (structural) element type, and the C backend emits `t = base[idx]`.
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Index(b, i));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Array(elems) => {
                let mut operands = Vec::with_capacity(elems.len());
                for e in elems {
                    operands.push(self.lower_expr(e)?);
                }
                // Lowering is structural (it has no type info from the inference
                // engine), so the array element C type defaults to `long long`.
                // The array temp is intentionally *not* registered in `temp_types`;
                // the C backend emits its `elem_ty name[len] = {..}` declaration inline.
                let t = format!("t{}", self.temp_counter);
                self.temp_counter += 1;
                self.emit(t.clone(), mir::Rvalue::Array("long long".to_string(), operands));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Unsafe(..) => Err(LowerError("`unsafe` is not in the codegen subset".into())),
            ExprKind::Closure { params, body, .. } => self.lower_closure(params, body),
            ExprKind::Cast(e, ty) => {
                let operand = self.lower_expr(e)?;
                let cty = type_to_c(ty);
                let t = self.fresh_temp(cty);
                self.emit(t.clone(), mir::Rvalue::Cast(operand, type_to_c(ty)));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Macro(m) => self.lower_print_macro(m),
            ExprKind::Tuple(elems) => {
                // Build a tuple value: lower each element, derive its C type
                // (constants fall back to `long long`), register the generated
                // struct type for this shape, and emit a `Tuple` rvalue.
                let mut operands = Vec::with_capacity(elems.len());
                let mut c_tys = Vec::with_capacity(elems.len());
                for e in elems {
                    let o = self.lower_expr(e)?;
                    let ct = match &o {
                        mir::Operand::Var(n) => self.temp_type_of(n),
                        _ => "long long".to_string(),
                    };
                    c_tys.push(ct);
                    operands.push(o);
                }
                let tuple_ty = self.tuple_type(c_tys);
                let t = self.fresh_temp(tuple_ty.clone());
                self.emit(t.clone(), mir::Rvalue::Tuple(tuple_ty, operands));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Try(..) => Err(LowerError(
                "the `?` operator is not in the codegen subset".into(),
            )),
            ExprKind::Await(inner) => {
                if self.async_ctx.is_some() {
                    self.lower_await(inner)
                } else {
                    self.lower_await_sync(inner)
                }
            }
            ExprKind::Deref(e) => {
                let o = self.lower_expr(e)?;
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Unary(mir::UnOp::Deref, o));
                Ok(mir::Operand::var(t))
            }
        }
    }

    /// Lower `.await inner` inside an `async fn`: a single suspension point of the
    /// state machine.
    ///
    /// 1. Lower `inner` (which must be a call to another async fn) to a future
    ///    struct value, and stash it into `self->__awt{k}`.
    /// 2. Persist every live slot and set `state = k+1`, then `return PENDING`
    ///    (suspension).
    /// 3. The resume block (selected by `state == k+1` in the entry `switch`)
    ///    reloads the live slots, drives the stored future to completion by
    ///    polling it in a loop, and yields its `__out` as this `.await`'s value.
    fn lower_await(&mut self, inner: &Expr) -> Result<mir::Operand> {
        let ctx = self
            .async_ctx
            .as_mut()
            .ok_or_else(|| LowerError("`.await` outside of an `async fn`".into()))?;
        let k = ctx.await_count;
        ctx.await_count += 1;
        let self_local = ctx.self_local.clone();
        let slots = ctx.slots.clone();

        // Determine the callee's poll function and future struct to drive on
        // resume.
        let (callee_poll, callee_fut) = match &inner.kind {
            ExprKind::Call(c, _) => match &c.kind {
                ExprKind::Path(p) => {
                    let name = p
                        .segments
                        .last()
                        .map(|s| s.node.clone())
                        .ok_or_else(|| LowerError("cannot resolve awaited call".into()))?;
                    (format!("{}__poll", name), format!("{}__fut", name))
                }
                _ => return Err(LowerError("`.await` operand must be an async call".into())),
            },
            _ => return Err(LowerError("`.await` operand must be an async call".into())),
        };

        // 1. Lower the inner future (call yields the future struct via `__ctor`).
        self.expecting_future = true;
        let futval = self.lower_expr(inner)?;
        self.expecting_future = false;

        // 2. Suspend: stash the future, persist slots, set state, return PENDING.
        let awt_field = format!("__awt{}", k);
        self.emit(
            "__s".to_string(),
            mir::Rvalue::StoreField(
                mir::Operand::var(&self_local),
                awt_field.clone(),
                futval,
                true,
            ),
        );
        for (sname, _sty) in &slots {
            self.emit(
                "__s".to_string(),
                mir::Rvalue::StoreField(
                    mir::Operand::var(&self_local),
                    sname.clone(),
                    mir::Operand::var(sname),
                    true,
                ),
            );
        }
        let _state_t = self.fresh_temp("long long");
        self.emit(
            "__s".to_string(),
            mir::Rvalue::StoreField(
                mir::Operand::var(&self_local),
                "__state".to_string(),
                mir::Operand::Const((k + 1) as i128),
                true,
            ),
        );
        // `return PENDING` (1). Subsequent statements continue in a resume block.
        self.set_term(mir::Terminator::Return(Some(mir::Operand::Const(1))));

        // 3. Resume block: reload the live slots, then drive the stored nested
        //    future (`self->__awt{k}`) to completion by polling it in a loop, and
        //    finally yield its `__out` field as this `.await`'s value.
        let resume = self.new_block();
        // Record the resume block so the entry `switch` (built after the whole
        // body is lowered) can target it via the `k+1` label.
        if let Some(ctx) = self.async_ctx.as_mut() {
            ctx.resume_blocks.push(resume);
        }
        self.current = resume;
        for (sname, _sty) in &slots {
            let t = self.fresh_temp("long long");
            self.emit(
                t.clone(),
                mir::Rvalue::Field(mir::Operand::var(&self_local), sname.clone(), true),
            );
            self.emit(sname.clone(), mir::Rvalue::Use(mir::Operand::var(t)));
        }

        // Copy the suspended nested future out of `self` so we can take its
        // address and poll it in place, then drive it to READY in a loop.
        let awt = self.fresh_temp(callee_fut.clone());
        self.emit(
            awt.clone(),
            mir::Rvalue::Field(mir::Operand::var(&self_local), awt_field.clone(), true),
        );
        let awt_addr = self.fresh_temp(format!("{}*", callee_fut));
        self.emit(
            awt_addr.clone(),
            mir::Rvalue::Ref(mir::Operand::var(awt.clone()), false),
        );

        // Poll loop: `while (callee_poll(&awt) == PENDING) {}`.
        let poll_blk = self.new_block();
        self.set_term(mir::Terminator::Goto(poll_blk));
        self.current = poll_blk;
        let status = self.fresh_temp("long long");
        self.emit(
            status.clone(),
            mir::Rvalue::Call(
                mir::Operand::Func(callee_poll.clone()),
                vec![mir::Operand::var(awt_addr)],
            ),
        );
        let cmp = self.fresh_temp("long long");
        self.emit(
            cmp.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Eq,
                mir::Operand::var(status),
                mir::Operand::Const(1),
            ),
        );
        let end_blk = self.new_block();
        self.set_term(mir::Terminator::If(
            mir::Operand::var(cmp),
            poll_blk,
            end_blk,
        ));

        self.current = end_blk;
        let out_t = self.fresh_temp("long long");
        self.emit(
            out_t.clone(),
            mir::Rvalue::Field(mir::Operand::var(awt), "_out".to_string(), false),
        );
        // A dedicated continuation block holds the await's value and everything
        // that follows, so the suspension point (which `return`s PENDING) and the
        // poll-loop tail never get clobbered by the next statement's terminator.
        let cont = self.new_block();
        self.set_term(mir::Terminator::Goto(cont));
        self.current = cont;
        Ok(mir::Operand::var(out_t))
    }

    /// Lower `.await inner` in a *synchronous* (non-`async`) context: the awaited
    /// async call is driven to completion immediately by lowering it to its sync
    /// `run` wrapper (`__run`), which polls the future until READY and returns
    /// its output. No suspension point is created.
    fn lower_await_sync(&mut self, inner: &Expr) -> Result<mir::Operand> {
        // The inner async call lowers to `<callee>__run` (because `async_ctx` is
        // `None`, `expecting_future` stays false) which already returns the
        // future's output value.
        self.lower_expr(inner)
    }

    /// Lower `if let pat = init { then } else { else }` (parsed as
    /// `If(Block{Local}, ..)`). Identifier patterns are irrefutable (the branch
    /// is always taken); enum-variant patterns (`Some(x)`, `None`, …) lower a
    /// real tag-test guard and take the `else` branch when the tag mismatches.
    fn lower_if_let(
        &mut self,
        local: &ast::Local,
        then_b: &ast::Block,
        else_b: Option<&Expr>,
    ) -> Result<mir::Operand> {
        let init_op = match &local.init {
            Some(e) => self.lower_expr(e)?,
            None => mir::Operand::Unit,
        };
        // Variant / nullary-variant patterns get a real guard + payload binds.
        if matches!(
            local.pat,
            ast::Pat::Variant(..) | ast::Pat::Path(_)
        ) {
            let (cond, binds) = self.pattern_guard(&init_op, &local.pat)?;
            let cond = cond.unwrap_or(mir::Operand::Const(1));
            let result = self.fresh_temp("long long");
            let then_blk = self.new_block();
            let else_blk = self.new_block();
            let merge = self.new_block();
            self.set_term(mir::Terminator::If(cond, then_blk, else_blk));

            self.current = then_blk;
            for (name, val) in binds {
                if !self.func.locals.iter().any(|(n, _)| n == &name) {
                    self.func.locals.push((name.clone(), Some("long long".into())));
                }
                self.emit(name, mir::Rvalue::Use(val));
            }
            let v_then = self.lower_block_value(then_b)?;
            self.emit(result.clone(), mir::Rvalue::Use(v_then));
            self.set_term(mir::Terminator::Goto(merge));

            self.current = else_blk;
            let v_else = match else_b {
                Some(eb) => self.lower_expr(eb)?,
                None => mir::Operand::Unit,
            };
            self.emit(result.clone(), mir::Rvalue::Use(v_else));
            self.set_term(mir::Terminator::Goto(merge));

            self.current = merge;
            return Ok(mir::Operand::var(result));
        }
        if !matches!(local.pat, ast::Pat::Ident(_)) {
            return Err(LowerError(
                "`if let`/`while let` with non-identifier patterns is not yet supported (enum destructuring needs `match`)"
                    .into(),
            ));
        }
        let name = pat_name(&local.pat)?;
        let init_op = match &local.init {
            Some(e) => self.lower_expr(e)?,
            None => mir::Operand::Unit,
        };
        self.func
            .locals
            .push((name.clone(), Some("long long".into())));
        self.emit(name.clone(), mir::Rvalue::Use(init_op));

        // Irrefutable: always taken. Lower as `if (1) { then } else { else }`.
        let result = self.fresh_temp("long long");
        let then_blk = self.new_block();
        let else_blk = self.new_block();
        let merge = self.new_block();
        self.set_term(mir::Terminator::If(
            mir::Operand::Const(1),
            then_blk,
            else_blk,
        ));

        self.current = then_blk;
        let v_then = self.lower_block_value(then_b)?;
        self.emit(result.clone(), mir::Rvalue::Use(v_then));
        self.set_term(mir::Terminator::Goto(merge));

        self.current = else_blk;
        let v_else = match else_b {
            Some(eb) => self.lower_expr(eb)?,
            None => mir::Operand::Unit,
        };
        self.emit(result.clone(), mir::Rvalue::Use(v_else));
        self.set_term(mir::Terminator::Goto(merge));

        self.current = merge;
        Ok(mir::Operand::var(result))
    }

    /// Lower `while let pat = init { body }` (parsed as `While(Block{Local}, ..)`).
    /// The binding is emitted once before the loop (its initializer does not
    /// change across iterations for an identifier pattern) and the loop runs
    /// until a `break` inside the body.
    fn lower_while_let(&mut self, local: &ast::Local, body: &ast::Block) -> Result<mir::Operand> {
        if !matches!(local.pat, ast::Pat::Ident(_)) {
            return Err(LowerError(
                "`if let`/`while let` with non-identifier patterns is not yet supported (enum destructuring needs `match`)"
                    .into(),
            ));
        }
        let name = pat_name(&local.pat)?;
        let init_op = match &local.init {
            Some(e) => self.lower_expr(e)?,
            None => mir::Operand::Unit,
        };
        self.func
            .locals
            .push((name.clone(), Some("long long".into())));
        self.emit(name.clone(), mir::Rvalue::Use(init_op));

        let head = self.new_block();
        let body_blk = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        self.set_term(mir::Terminator::If(
            mir::Operand::Const(1),
            body_blk,
            exit,
        ));

        self.current = body_blk;
        self.loops.push((exit, head));
        self.lower_block_value(body)?;
        self.loops.pop();
        self.set_term(mir::Terminator::Goto(head));

        self.current = exit;
        Ok(mir::Operand::Unit)
    }

    /// Lower `match scrutinee { pat => body, .. }` into a chain of conditional
    /// blocks. Each arm's pattern guards entry into its body: identifier / wild /
    /// path / ref / or-patterns are irrefutable (always taken), and literal
    /// patterns lower to an equality comparison against the scrutinee. Pattern
    /// bindings (identifier in `Ident`/`Tuple`) are assigned the scrutinee (or
    /// the matching tuple element) before the body runs. The match value is the
    /// aggregated arm-body value, like `if`/`else`.
    fn lower_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[(ast::Pat, Expr)],
    ) -> Result<mir::Operand> {
        let scrut_op = self.lower_expr(scrutinee)?;
        let result = self.fresh_temp("long long");
        let merge = self.new_block();

        let mut next_test = self.new_block();
        self.set_term(mir::Terminator::Goto(next_test));

        for (pat, body) in arms {
            let test_blk = next_test;
            self.current = test_blk;
            let (cond, binds): (Option<mir::Operand>, Vec<(String, mir::Operand)>) =
                self.pattern_guard(&scrut_op, pat)?;
            match cond {
                Some(c) => {
                    let body_blk = self.new_block();
                    next_test = self.new_block();
                    self.set_term(mir::Terminator::If(c, body_blk, next_test));
                    self.current = body_blk;
                }
                None => {
                    // Irrefutable (ident / `_` / path / or) — fall straight through.
                    next_test = self.new_block();
                    self.set_term(mir::Terminator::Goto(next_test));
                    // `next_test` is the *following* arm's test; the body runs in a
                    // block we create now and then jumps to `next_test`.
                    let body_blk = self.new_block();
                    self.set_term(mir::Terminator::Goto(body_blk));
                    self.current = body_blk;
                }
            }
            // Emit bound pattern names (assigned the scrutinee / element value).
            for (name, val) in binds {
                if !self.func.locals.iter().any(|(n, _)| n == &name) {
                    self.func.locals.push((name.clone(), Some("long long".into())));
                }
                self.emit(name, mir::Rvalue::Use(val));
            }
            let v = self.lower_expr(body)?;
            self.emit(result.clone(), mir::Rvalue::Use(v));
            self.set_term(mir::Terminator::Goto(merge));
        }

        // The final fall-through block (no arm matched): yield unit.
        self.current = next_test;
        self.emit(result.clone(), mir::Rvalue::Use(mir::Operand::Unit));
        self.set_term(mir::Terminator::Goto(merge));

        self.current = merge;
        Ok(mir::Operand::var(result))
    }

    /// The C type of an expression that constructs an enum variant
    /// (`Some(..)` / `None` / user variants), i.e. `struct <Enum>` — `None`
    /// when the expression is not a variant construction.
    fn variant_c_type(&self, e: &ast::Expr) -> Option<String> {
        let vname = match &e.kind {
            ExprKind::Call(callee, _) => match &callee.kind {
                ExprKind::Path(p) => p.segments.last().map(|s| s.node.clone())?,
                _ => return None,
            },
            ExprKind::Path(p) => p.segments.last().map(|s| s.node.clone())?,
            _ => return None,
        };
        self.variants
            .get(&vname)
            .map(|(ename, _, _)| format!("struct {}", ename))
    }

    /// Lower a builtin `println!`/`eprintln!`/`print!`/`eprint!` invocation.
    ///
    /// The format string is split on `{}` placeholders; literal segments lower
    /// to `print_str("<segment>")` calls and each placeholder consumes the next
    /// argument, printed with the runtime helper chosen from the argument's C
    /// type (`print_str` for `char*`, `print_f64` for `double`, `print_i64`
    /// otherwise). The `ln` variants append a trailing newline. `format!`
    /// evaluates its arguments for side effects and yields the string-literal
    /// placeholder (real formatted-string construction is a documented
    /// follow-up — the subset has no heap string runtime yet).
    fn lower_print_macro(&mut self, m: &ast::MacroCall) -> Result<mir::Operand> {
        let name = m
            .path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default();
        let args = tpt_moira_clotho::parser::Parser::parse_macro_args(m.tokens.clone());
        match name.as_str() {
            "println" | "eprintln" | "print" | "eprint" => {
                let newline = name.ends_with("ln");
                let stderr = name.starts_with('e');
                // `eprintln!`/`eprint!` route through the stderr helpers when
                // available; the subset runtime prints to stdout, so the
                // prefix marks intent and the call targets the same helper.
                let _ = stderr;
                let mut fmt: Option<String> = None;
                let mut rest: Vec<&ast::Expr> = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    if i == 0 {
                        if let ExprKind::Lit(ast::Lit::Str(s)) = &a.kind {
                            fmt = Some(s.clone());
                            continue;
                        }
                    }
                    rest.push(a);
                }
                let mut placeholders: Vec<&ast::Expr> = rest.drain(..).collect();
                let segments: Vec<String> = fmt
                    .as_deref()
                    .map(|f| f.split("{}").map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                let has_fmt = fmt.is_some();
                let emit_str = |s: &str, l: &mut Self| -> Result<()> {
                    if s.is_empty() {
                        return Ok(());
                    }
                    let seg = l.fresh_temp("char*");
                    l.emit(
                        seg.clone(),
                        mir::Rvalue::Call(
                            mir::Operand::Func("print_str".to_string()),
                            vec![mir::Operand::CStr(s.to_string())],
                        ),
                    );
                    Ok(())
                };
                // Interleave literal segments with placeholder arguments.
                if has_fmt {
                    emit_str(&segments[0], self)?;
                    for (i, a) in placeholders.iter_mut().enumerate() {
                        let op = self.lower_expr(a)?;
                        let printer = match self.temp_type_of(match &op {
                            mir::Operand::Var(n) => n.as_str(),
                            _ => "",
                        })
                        .as_str()
                        {
                            t if t.contains("char*") => "print_str",
                            "double" => "print_f64",
                            _ => "print_i64",
                        };
                        let seg_idx = i + 1;
                        let after = segments.get(seg_idx).cloned().unwrap_or_default();
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Call(
                                mir::Operand::Func(printer.to_string()),
                                vec![op],
                            ),
                        );
                        emit_str(&after, self)?;
                    }
                } else {
                    // No format string: print each argument in order.
                    for a in placeholders.drain(..) {
                        let op = self.lower_expr(a)?;
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Call(
                                mir::Operand::Func("print_i64".to_string()),
                                vec![op],
                            ),
                        );
                    }
                }
                if newline {
                    let seg = self.fresh_temp("char*");
                    self.emit(
                        seg.clone(),
                        mir::Rvalue::Call(
                            mir::Operand::Func("print_str".to_string()),
                            vec![mir::Operand::CStr("\n".to_string())],
                        ),
                    );
                }
                Ok(mir::Operand::Unit)
            }
            "format" => {
                // Evaluate for side effects; the value is the string-literal
                // placeholder (no heap string runtime yet).
                for a in args {
                    self.lower_expr(&a)?;
                }
                Ok(mir::Operand::Const(0))
            }
            other => Err(LowerError(format!(
                "macro `{}` is not in the codegen subset",
                other
            ))),
        }
    }

    /// Lower a tuple-variant constructor call (`Some(5)`, `Ok(v)`) to a
    /// tagged-struct literal: `{ tag: <variant tag>, f0: <first payload>, .. }`.
    fn lower_variant_construction(
        &mut self,
        ename: &str,
        tag: i128,
        arity: usize,
        args: &[ast::Expr],
    ) -> Result<mir::Operand> {
        if args.len() != arity {
            return Err(LowerError(format!(
                "variant constructor expects {} payload value(s), found {}",
                arity,
                args.len()
            )));
        }
        let mut fields = vec![("tag".to_string(), mir::Operand::Const(tag))];
        for (i, a) in args.iter().enumerate() {
            let v = self.lower_expr(a)?;
            fields.push((format!("f{}", i), v));
        }
        let t = self.fresh_temp(&format!("struct {}", ename));
        self.emit(t.clone(), mir::Rvalue::StructLit(ename.to_string(), fields));
        Ok(mir::Operand::var(t))
    }

    /// The tag-test guard and payload bindings for a variant pattern
    /// (`Some(x)`, `MyEnum::Var(a, b)`): the arm is taken when the scrutinee's
    /// `tag` field equals the variant's index; each sub-pattern binds against
    /// the corresponding payload field projection.
    fn variant_guard(
        &mut self,
        scrut: &mir::Operand,
        path: &ast::Path,
        subpats: &[ast::Pat],
    ) -> Result<(Option<mir::Operand>, Vec<(String, mir::Operand)>)> {
        let vname = path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default();
        let Some((_, tag, _arity)) = self.variants.get(&vname).cloned() else {
            return Err(LowerError(format!(
                "unknown enum variant `{}` in pattern",
                vname
            )));
        };
        // Guard: scrut.tag == tag.
        let tag_val = self.fresh_temp("long long");
        self.emit(
            tag_val.clone(),
            mir::Rvalue::Field(scrut.clone(), "tag".to_string(), false),
        );
        let cond = self.fresh_temp("long long");
        self.emit(
            cond.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Eq,
                mir::Operand::var(tag_val),
                mir::Operand::Const(tag),
            ),
        );
        // Payload bindings: sub-pattern i matches field `f{i}`.
        let mut binds = Vec::new();
        for (i, sp) in subpats.iter().enumerate() {
            let field_t = self.fresh_temp("long long");
            self.emit(
                field_t.clone(),
                mir::Rvalue::Field(scrut.clone(), format!("f{}", i), false),
            );
            let fv = mir::Operand::var(field_t);
            let (_c, mut b) = self.pattern_guard(&fv, sp)?;
            binds.append(&mut b);
        }
        Ok((Some(mir::Operand::var(cond)), binds))
    }

    /// Build the guard condition and binding assignments for a pattern against the
    /// scrutinee operand. Returns `(None, binds)` for irrefutable patterns (the
    /// arm is always taken) or `(Some(cond), binds)` for a literal comparison.
    fn pattern_guard(
        &mut self,
        scrut: &mir::Operand,
        pat: &ast::Pat,
    ) -> Result<(Option<mir::Operand>, Vec<(String, mir::Operand)>)> {
        match pat {
            ast::Pat::Ident(n) => {
                // A bare name that is a known *nullary* variant (`None`) is a
                // tag test, not a binding — the parser cannot distinguish it
                // from an identifier pattern.
                if let Some((_, tag, 0)) = self.variants.get(&n.node).cloned() {
                    let tag_val = self.fresh_temp("long long");
                    self.emit(
                        tag_val.clone(),
                        mir::Rvalue::Field(scrut.clone(), "tag".to_string(), false),
                    );
                    let cond = self.fresh_temp("long long");
                    self.emit(
                        cond.clone(),
                        mir::Rvalue::Binary(
                            mir::BinOp::Eq,
                            mir::Operand::var(tag_val),
                            mir::Operand::Const(tag),
                        ),
                    );
                    return Ok((Some(mir::Operand::var(cond)), Vec::new()));
                }
                Ok((
                    None,
                    vec![(n.node.clone(), scrut.clone())],
                ))
            }
            ast::Pat::Wild => Ok((None, Vec::new())),
            ast::Pat::Ref(inner) => self.pattern_guard(scrut, inner),
            ast::Pat::Or(a, b) => {
                // `a | b`: take arm if either sub-pattern matches. We lower this to
                // `(a matches) || (b matches)` by computing each guard; bindings
                // from either side are assigned the scrutinee (names must be
                // identical across the or-pattern for this to be sound).
                let (ca, ba) = self.pattern_guard(scrut, a)?;
                let (cb, bb) = self.pattern_guard(scrut, b)?;
                let cond = match (ca, cb) {
                    (Some(x), Some(y)) => {
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Binary(mir::BinOp::Or, x, y),
                        );
                        Some(mir::Operand::var(t))
                    }
                    (Some(x), None) | (None, Some(x)) => Some(x),
                    (None, None) => None,
                };
                let mut binds = ba;
                binds.extend(bb);
                Ok((cond, binds))
            }
            ast::Pat::Lit(lit) => {
                let lit_op = self.lower_lit_operand(lit);
                let t = self.fresh_temp("long long");
                self.emit(
                    t.clone(),
                    mir::Rvalue::Binary(mir::BinOp::Eq, scrut.clone(), lit_op),
                );
                Ok((Some(mir::Operand::var(t)), Vec::new()))
            }
            ast::Pat::Range(lo, hi, inclusive) => {
                // `lo..hi` / `lo..=hi` / `..hi` / `lo..` / `..`. Lower to the
                // conjunction `scrut >= lo && scrut <= hi` (with the upper bound
                // `<` for the exclusive `..` form). Each bound must be a literal.
                let mut conds: Vec<mir::Operand> = Vec::new();
                if let Some(lo_pat) = lo {
                    if let ast::Pat::Lit(l) = &**lo_pat {
                        let lo_op = self.lower_lit_operand(l);
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Binary(mir::BinOp::Ge, scrut.clone(), lo_op),
                        );
                        conds.push(mir::Operand::var(t));
                    } else {
                        return Err(LowerError(
                            "range pattern lower bound must be a literal".into(),
                        ));
                    }
                }
                if let Some(hi_pat) = hi {
                    if let ast::Pat::Lit(l) = &**hi_pat {
                        let hi_op = self.lower_lit_operand(l);
                        let op = if *inclusive {
                            mir::BinOp::Le
                        } else {
                            mir::BinOp::Lt
                        };
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Binary(op, scrut.clone(), hi_op),
                        );
                        conds.push(mir::Operand::var(t));
                    } else {
                        return Err(LowerError(
                            "range pattern upper bound must be a literal".into(),
                        ));
                    }
                }
                let cond = match conds.len() {
                    0 => None,
                    1 => Some(conds.pop().unwrap()),
                    _ => {
                        let mut acc = conds[0].clone();
                        for c in &conds[1..] {
                            let t = self.fresh_temp("long long");
                            self.emit(
                                t.clone(),
                                mir::Rvalue::Binary(mir::BinOp::And, acc, c.clone()),
                            );
                            acc = mir::Operand::var(t);
                        }
                        Some(acc)
                    }
                };
                Ok((cond, Vec::new()))
            }
            ast::Pat::Tuple(elems) => {
                // Bind each identifier element to a field projection of the
                // scrutinee tuple. The pattern is irrefutable (always taken).
                let mut binds = Vec::new();
                for (i, e) in elems.iter().enumerate() {
                    if let ast::Pat::Ident(n) = e {
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Field(scrut.clone(), format!("f{i}"), false),
                        );
                        binds.push((n.node.clone(), mir::Operand::var(t)));
                    }
                }
                Ok((None, binds))
            }
            ast::Pat::Variant(path, subpats) => self.variant_guard(scrut, path, subpats),
            ast::Pat::Path(path) => {
                // A nullary variant path (`None`) is a tag test; any other
                // path pattern stays unsupported in the codegen subset.
                let vname = path
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                if let Some((_, tag, 0)) = self.variants.get(&vname).cloned() {
                    let tag_val = self.fresh_temp("long long");
                    self.emit(
                        tag_val.clone(),
                        mir::Rvalue::Field(scrut.clone(), "tag".to_string(), false),
                    );
                    let cond = self.fresh_temp("long long");
                    self.emit(
                        cond.clone(),
                        mir::Rvalue::Binary(
                            mir::BinOp::Eq,
                            mir::Operand::var(tag_val),
                            mir::Operand::Const(tag),
                        ),
                    );
                    Ok((Some(mir::Operand::var(cond)), Vec::new()))
                } else {
                    Err(LowerError(
                        "this match pattern is not supported in the codegen subset".into(),
                    ))
                }
            }
            ast::Pat::Struct(_, _) => Err(LowerError(
                "this match pattern is not supported in the codegen subset".into(),
            )),
            ast::Pat::Bind(name, sub) => {
                // `name @ subpat`: build the guard from the sub-pattern and also
                // bind `name` to the whole scrutinee (irrefutably).
                let (sub_cond, mut sub_binds) = self.pattern_guard(scrut, sub)?;
                sub_binds.push((name.clone(), scrut.clone()));
                Ok((sub_cond, sub_binds))
            }
        }
    }

    /// Lower a literal pattern to a constant operand.
    fn lower_lit_operand(&mut self, lit: &ast::Lit) -> mir::Operand {
        match lit {
            ast::Lit::Int(v, _) => mir::Operand::Const(*v as i128),
            ast::Lit::Bool(b) => mir::Operand::Const(if *b { 1 } else { 0 }),
            ast::Lit::Char(c) => mir::Operand::Const(*c as i128),
            // Strings/other literals are not yet backed by data in the codegen
            // subset; lower to zero (type-checked programs still lower).
            _ => mir::Operand::Const(0),
        }
    }

    fn lower_loop(&mut self, cond: Option<&Expr>, body: &ast::Block) -> Result<mir::Operand> {
        let head = self.new_block();
        let body_blk = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let entry = if let Some(c) = cond {
            let c_op = self.lower_expr(c)?;
            self.set_term(mir::Terminator::If(c_op, body_blk, exit));
            body_blk
        } else {
            self.set_term(mir::Terminator::Goto(body_blk));
            body_blk
        };

        self.current = entry;
        self.loops.push((exit, head));
        self.lower_block_value(body)?;
        self.loops.pop();
        self.set_term(mir::Terminator::Goto(head));

        self.current = exit;
        Ok(mir::Operand::Unit)
    }
}

/// The MIR produced for one `async fn`: its future struct metadata plus the
/// generated `poll` coroutine and sync `run` executor.
struct AsyncLower {
    def: mir::AsyncDef,
    poll_fn: mir::Function,
    run_fn: mir::Function,
    ctor_fn: mir::Function,
    /// Closures lambda-lifted out of the async body (none in the supported linear
    /// async subset, but threaded through so they are never silently dropped).
    closures: Vec<mir::Function>,
}

/// Collect the names + C types of every `let` binding in a (linear) block, used
/// as the set of slots a suspending `async fn` must persist across `.await`.
fn collect_locals(b: &ast::Block, out: &mut Vec<(String, String)>) {
    for s in &b.stmts {
        if let StmtKind::Local(local) = &s.kind {
            if let Ok(name) = pat_name(&local.pat) {
                let ty = local
                    .ty
                    .as_ref()
                    .map(type_to_c)
                    .unwrap_or_else(|| "long long".to_string());
                out.push((name, ty));
            }
        }
    }
}

/// Walk a (linear) body collecting the inner expression of every `.await` (used
/// to size the future struct's nested-future slots) and rejecting awaits nested
/// inside control flow — the supported async subset is linear.
fn scan_awaits(b: &ast::Block, points: &mut Vec<Expr>, err: &mut Option<LowerError>) {
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Local(local) => {
                if let Some(init) = &local.init {
                    scan_await_expr(init, points, err);
                }
            }
            StmtKind::Semi(e) | StmtKind::Expr(e) => scan_await_expr(e, points, err),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &b.tail {
        scan_await_expr(tail, points, err);
    }
}

fn scan_await_expr(e: &Expr, points: &mut Vec<Expr>, err: &mut Option<LowerError>) {
    match &e.kind {
        ExprKind::Await(inner) => {
            points.push((**inner).clone());
            // The awaited future itself may not contain further control flow.
            scan_await_expr(inner, points, err);
        }
        ExprKind::Binary(l, _, r) => {
            scan_await_expr(l, points, err);
            scan_await_expr(r, points, err);
        }
        ExprKind::Unary(_, e2) | ExprKind::Ref(e2, _) | ExprKind::Deref(e2) => {
            scan_await_expr(e2, points, err)
        }
        ExprKind::Call(c, args) => {
            scan_await_expr(c, points, err);
            for a in args {
                scan_await_expr(a, points, err);
            }
        }
        ExprKind::If(cond, then_b, else_b) => {
            let mut inner = Vec::new();
            let mut e2 = None;
            scan_await_expr(cond, &mut inner, &mut e2);
            scan_await_block(then_b, &mut inner, &mut e2);
            if let Some(eb) = else_b {
                scan_await_expr(eb, &mut inner, &mut e2);
            }
            if !inner.is_empty() && err.is_none() {
                *err = Some(LowerError(
                    "`.await` inside `if` is not supported by the async subset".into(),
                ));
            }
        }
        ExprKind::While(cond, body) | ExprKind::For(_, cond, body) => {
            let mut inner = Vec::new();
            let mut e2 = None;
            scan_await_expr(cond, &mut inner, &mut e2);
            scan_await_block(body, &mut inner, &mut e2);
            if !inner.is_empty() && err.is_none() {
                *err = Some(LowerError(
                    "`.await` inside a loop is not supported by the async subset".into(),
                ));
            }
        }
        ExprKind::Loop(body) => {
            let mut inner = Vec::new();
            let mut e2 = None;
            scan_await_block(body, &mut inner, &mut e2);
            if !inner.is_empty() && err.is_none() {
                *err = Some(LowerError(
                    "`.await` inside a loop is not supported by the async subset".into(),
                ));
            }
        }
        ExprKind::Block(b) => {
            let mut inner = Vec::new();
            let mut e2 = None;
            scan_await_block(b, &mut inner, &mut e2);
            if !inner.is_empty() && err.is_none() {
                *err = Some(LowerError(
                    "`.await` inside a nested block is not supported by the async subset".into(),
                ));
            }
        }
        _ => {}
    }
}

fn scan_await_block(b: &ast::Block, points: &mut Vec<Expr>, err: &mut Option<LowerError>) {
    scan_awaits(b, points, err);
}

/// Build the sync `run` executor for an async fn: `(args) -> T`. It constructs
/// the future, polls it in a loop until READY, and returns `__out`. The future
/// constructor is inlined as a struct literal initialised from the parameters.
fn build_run_wrapper(
    f: &ast::Function,
    fut_struct: &str,
    poll_name: &str,
    output_ty: &str,
    slots: &[(String, String)],
) -> mir::Function {
    let mut func = mir::Function {
        name: format!("{}__run", f.name.node),
        params: Vec::new(),
        ret: Some(output_ty.to_string()),
        locals: Vec::new(),
        temp_types: HashMap::new(),
        blocks: Vec::new(),
    };
    // Parameters become locals of the wrapper.
    for p in &f.params {
        let name = pat_name(&p.pat).unwrap_or_default();
        let ty = type_to_c(&p.ty);
        func.params.push(name.clone());
        func.locals.push((name, Some(ty)));
    }

    let mut t = 0usize;
    let mut fresh = |func: &mut mir::Function, ty: &str| -> String {
        let name = format!("t{}", t);
        t += 1;
        func.temp_types.insert(name.clone(), ty.to_string());
        name
    };

    let b0 = func.blocks.len();
    func.blocks.push(mir::Block::default());
    // `fut = (fut_struct){ .__state = 0, .param = arg, ... };`
    let fut = "t_fut".to_string();
    func.temp_types.insert(fut.clone(), fut_struct.to_string());
    let mut inits: Vec<(String, mir::Operand)> =
        vec![("__state".to_string(), mir::Operand::Const(0))];
    for (sname, _sty) in slots {
        if f.params
            .iter()
            .any(|p| pat_name(&p.pat).ok().as_deref() == Some(sname))
        {
            inits.push((sname.clone(), mir::Operand::var(sname.clone())));
        } else {
            inits.push((sname.clone(), mir::Operand::Const(0)));
        }
    }
    func.blocks[b0].stmts.push(mir::Stmt {
        place: fut.clone(),
        rvalue: mir::Rvalue::StructLit(fut_struct.to_string(), inits),
    });

    // Poll loop: `while (fut_poll(&fut) == PENDING) {}`.
    let loop_b = func.blocks.len();
    func.blocks.push(mir::Block::default());
    let end_b = func.blocks.len();
    func.blocks.push(mir::Block::default());

    let fut_ptr = fresh(&mut func, &format!("{}*", fut_struct));
    func.blocks[b0].stmts.push(mir::Stmt {
        place: fut_ptr.clone(),
        rvalue: mir::Rvalue::Ref(mir::Operand::var(fut.clone()), false),
    });
    func.blocks[b0].term = mir::Terminator::Goto(loop_b);

    let status = fresh(&mut func, "long long");
    func.blocks[loop_b].stmts.push(mir::Stmt {
        place: status.clone(),
        rvalue: mir::Rvalue::Call(
            mir::Operand::Func(poll_name.to_string()),
            vec![mir::Operand::var(fut_ptr.clone())],
        ),
    });
    let cmp = fresh(&mut func, "long long");
    func.blocks[loop_b].stmts.push(mir::Stmt {
        place: cmp.clone(),
        rvalue: mir::Rvalue::Binary(
            mir::BinOp::Eq,
            mir::Operand::var(status.clone()),
            mir::Operand::Const(1),
        ),
    });
    func.blocks[loop_b].term = mir::Terminator::If(mir::Operand::var(cmp), loop_b, end_b);

    let out = fresh(&mut func, output_ty);
    func.blocks[end_b].stmts.push(mir::Stmt {
        place: out.clone(),
        rvalue: mir::Rvalue::Field(mir::Operand::var(fut.clone()), "_out".to_string(), false),
    });
    func.blocks[end_b].term = mir::Terminator::Return(Some(mir::Operand::var(out)));
    func
}

/// Build the future *constructor* for an async fn: `(args) -> fut_struct`. It
/// returns a future struct literal initialised with `state = 0` and the
/// parameters copied from the call arguments. This is the value produced when an
/// async call is used inside `.await` (as opposed to `__run`, which drives the
/// future to completion).
fn build_ctor_wrapper(f: &ast::Function, fut_struct: &str) -> mir::Function {
    let mut func = mir::Function {
        name: format!("{}__ctor", f.name.node),
        params: Vec::new(),
        ret: Some(fut_struct.to_string()),
        locals: Vec::new(),
        temp_types: HashMap::new(),
        blocks: Vec::new(),
    };
    let mut inits: Vec<(String, mir::Operand)> =
        vec![("__state".to_string(), mir::Operand::Const(0))];
    for p in &f.params {
        let name = pat_name(&p.pat).unwrap_or_default();
        let ty = type_to_c(&p.ty);
        func.params.push(name.clone());
        func.locals.push((name.clone(), Some(ty)));
        inits.push((name.clone(), mir::Operand::var(name)));
    }
    let b0 = func.blocks.len();
    func.blocks.push(mir::Block::default());
    let fut = "t_fut".to_string();
    func.temp_types.insert(fut.clone(), fut_struct.to_string());
    func.blocks[b0].stmts.push(mir::Stmt {
        place: fut.clone(),
        rvalue: mir::Rvalue::StructLit(fut_struct.to_string(), inits),
    });
    func.blocks[b0].term = mir::Terminator::Return(Some(mir::Operand::var(fut)));
    func
}

fn pat_name(p: &Pat) -> Result<String> {
    match p {
        Pat::Ident(n) => Ok(n.node.clone()),
        Pat::Wild => Ok("_".to_string()),
        Pat::Bind(n, _) => Ok(n.clone()),
        Pat::Path(path) | Pat::Variant(path, _) => Ok(path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_else(|| "_".to_string())),
        _ => Err(LowerError("only identifier patterns are supported".into())),
    }
}

fn name_of(o: &mir::Operand) -> String {
    match o {
        mir::Operand::Var(n) => n.clone(),
        _ => String::new(),
    }
}

fn compound_op(op: ABinOp) -> Option<mir::BinOp> {
    Some(match op {
        ABinOp::AddAssign => mir::BinOp::Add,
        ABinOp::SubAssign => mir::BinOp::Sub,
        ABinOp::MulAssign => mir::BinOp::Mul,
        ABinOp::DivAssign => mir::BinOp::Div,
        ABinOp::RemAssign => mir::BinOp::Rem,
        ABinOp::BitAndAssign => mir::BinOp::BitAnd,
        ABinOp::BitOrAssign => mir::BinOp::BitOr,
        ABinOp::BitXorAssign => mir::BinOp::BitXor,
        ABinOp::ShlAssign => mir::BinOp::Shl,
        ABinOp::ShrAssign => mir::BinOp::Shr,
        _ => return None,
    })
}

fn map_binop(op: ABinOp) -> Result<mir::BinOp> {
    Ok(match op {
        ABinOp::Add => mir::BinOp::Add,
        ABinOp::Sub => mir::BinOp::Sub,
        ABinOp::Mul => mir::BinOp::Mul,
        ABinOp::Div => mir::BinOp::Div,
        ABinOp::Rem => mir::BinOp::Rem,
        ABinOp::And => mir::BinOp::And,
        ABinOp::Or => mir::BinOp::Or,
        ABinOp::BitAnd => mir::BinOp::BitAnd,
        ABinOp::BitOr => mir::BinOp::BitOr,
        ABinOp::BitXor => mir::BinOp::BitXor,
        ABinOp::Shl => mir::BinOp::Shl,
        ABinOp::Shr => mir::BinOp::Shr,
        ABinOp::Eq => mir::BinOp::Eq,
        ABinOp::Ne => mir::BinOp::Ne,
        ABinOp::Lt => mir::BinOp::Lt,
        ABinOp::Gt => mir::BinOp::Gt,
        ABinOp::Le => mir::BinOp::Le,
        ABinOp::Ge => mir::BinOp::Ge,
        other => {
            return Err(LowerError(format!(
                "operator {:?} is not supported in the codegen subset",
                other
            )))
        }
    })
}

fn lookup_field_type(structs: &[mir::StructDef], base_ty: &str, field: &str) -> Option<String> {
    structs
        .iter()
        .find(|s| s.name == base_ty)
        .and_then(|s| s.fields.iter().find(|(f, _)| f == field))
        .map(|(_, t)| t.clone())
}
