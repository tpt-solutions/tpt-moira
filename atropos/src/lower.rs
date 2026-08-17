//! Lower the Clotho AST into the Atropos MIR (SSA form) for the Phase-1 subset.
//!
//! In addition to the usual synchronous subset, this module lowers `async fn`
//! into a genuine suspending state machine: a future struct holding the
//! coroutine `state` plus every live slot, a `poll` coroutine driven by an
//! entry `switch` on `state` (one resume block per `.await` point), and a sync
//! `run` executor wrapper that drives the future to completion. See
//! [`Lower::lower_async_function`] for the transformation.

use crate::mir;
use clotho::ast::{
    self, BinOp as ABinOp, Expr, ExprKind, ItemKind, Pat, Program, Stmt, StmtKind, UnOp as AUnOp,
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
    for item in &prog.items {
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
            ItemKind::Use(_) => { /* ignored in Phase 1 */ }
            ItemKind::Impl(_) => { /* trait impls are consumed by the type checker */ }
            ItemKind::Module(_) => { /* resolved/inlined by the frontend */ }
            ItemKind::Function(f) => {
                if f.is_async {
                    let mut l = Lower::new(&out.structs);
                    l.async_fns = async_names.clone();
                    let ad = l.lower_async_function(f)?;
                    out.structs.push(mir::StructDef {
                        name: ad.def.fut_name.clone(),
                        fields: ad.def.fut_fields.clone(),
                    });
                    out.functions.push(ad.poll_fn.clone());
                    out.functions.push(ad.run_fn.clone());
                    out.functions.push(ad.ctor_fn.clone());
                    out.async_fns.push(ad.def);
                } else {
                    let mut l = Lower::new(&out.structs);
                    l.async_fns = async_names.clone();
                    let func = l.lower_function(f)?;
                    out.functions.push(func);
                }
            }
            other => {
                return Err(LowerError(format!(
                    "unsupported top-level item in codegen subset: {:?}",
                    matches!(
                        other,
                        ItemKind::Enum(_) | ItemKind::Const(_) | ItemKind::Static(_)
                    )
                )));
            }
        }
    }
    Ok(out)
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
    /// When set, the enclosing `async fn` state-machine bookkeeping: the future
    /// struct name, the `self` parameter local name, the output C type, and the
    /// list of `(slot_name, c_type)` that must be round-tripped across a
    /// suspension (all parameters and locals).
    async_ctx: Option<AsyncCtx>,
    /// True while lowering the inner expression of `.await`, so calls to async
    /// fns yield their future struct (`__ctor`) rather than running to
    /// completion (`__run`).
    expecting_future: bool,
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
            async_fns: std::collections::HashSet::new(),
            async_ctx: None,
            expecting_future: false,
        }
    }

    fn lower_function(mut self, f: &ast::Function) -> Result<mir::Function> {
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
        Ok(self.func)
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
                self.emit(
                    sname.clone(),
                    mir::Rvalue::Use(mir::Operand::Const(0)),
                );
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

    fn lower_stmt(&mut self, s: &Stmt) -> Result<()> {
        match &s.kind {
            StmtKind::Local(local) => {
                let name = pat_name(&local.pat)?;
                // Infer the declared local type: prefer an explicit annotation,
                // otherwise fall back to a struct-literal RHS so `let p = Point{..}`
                // is typed `struct Point` (otherwise it defaults to `long long`
                // and the struct assignment/field access fails to compile).
                let ty = match (&local.ty, &local.init) {
                    (Some(t), _) => Some(type_to_c(t)),
                    (None, Some(init)) => match &init.kind {
                        ExprKind::Struct(path, _, _) => path
                            .segments
                            .last()
                            .map(|s| s.node.clone()),
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
                Ok(mir::Operand::var(name))
            }
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
                let name = if is_async_call {
                    if self.expecting_future {
                        format!("{}__ctor", name)
                    } else {
                        format!("{}__run", name)
                    }
                } else {
                    name
                };
                let mut operands = Vec::new();
                for a in args {
                    operands.push(self.lower_expr(a)?);
                }
                // The result type is the future struct when we are building a
                // future (`__ctor`), otherwise `long long` (the `__run` output
                // or a sync call's result).
                let result_ty = if is_async_call && self.expecting_future {
                    format!("{}__fut", name.trim_end_matches("__ctor"))
                } else {
                    "long long".to_string()
                };
                let t = self.fresh_temp(result_ty);
                self.emit(t.clone(), mir::Rvalue::Call(name, operands));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Field(base, field) => {
                let o = self.lower_expr(base)?;
                let base_ty = self.temp_type_of(&name_of(&o));
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
            ExprKind::While(cond, body) => self.lower_loop(Some(&**cond), body),
            ExprKind::Loop(body) => self.lower_loop(None, body),
            ExprKind::For(pat, iter, body) => {
                // Desugar `for i in a..b { ... }` into a while loop over `i`.
                let name = pat_name(pat)?;
                let (a, b) = match &iter.kind {
                    ExprKind::Binary(l, ABinOp::Range, r) => (l, r),
                    _ => return Err(LowerError("`for` iterator must be a range `a..b`".into())),
                };
                let a_op = self.lower_expr(a)?;
                let b_op = self.lower_expr(b)?;
                self.func
                    .locals
                    .push((name.clone(), Some("long long".into())));
                self.emit(name.clone(), mir::Rvalue::Use(a_op));

                let head = self.new_block();
                let body_blk = self.new_block();
                let exit = self.new_block();
                self.set_term(mir::Terminator::Goto(head));

                self.current = head;
                let cond = mir::Operand::var(name.clone());
                let cmp = self.fresh_temp("long long");
                self.emit(cmp.clone(), mir::Rvalue::Binary(mir::BinOp::Lt, cond, b_op));
                self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));

                self.current = body_blk;
                self.loops.push((exit, head));
                self.lower_block_value(body)?;
                self.loops.pop();
                // i = i + 1
                let i = mir::Operand::var(name.clone());
                self.emit(
                    name.clone(),
                    mir::Rvalue::Binary(mir::BinOp::Add, i, mir::Operand::Const(1)),
                );
                self.set_term(mir::Terminator::Goto(head));

                self.current = exit;
                Ok(mir::Operand::Unit)
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
            ExprKind::Match(..) => Err(LowerError("`match` is not in the codegen subset".into())),
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
                self.emit(t.clone(), mir::Rvalue::Call(method.node.clone(), operands));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Index(..) => Err(LowerError("indexing is not in the codegen subset".into())),
            ExprKind::Array(..) => Err(LowerError("arrays are not in the codegen subset".into())),
            ExprKind::Unsafe(..) => Err(LowerError("`unsafe` is not in the codegen subset".into())),
            ExprKind::Closure { .. } => {
                Err(LowerError("closures are not in the codegen subset".into()))
            }
            ExprKind::Cast(..) => Err(LowerError("casts are not in the codegen subset".into())),
            ExprKind::Macro(..) => Err(LowerError("macros are not in the codegen subset".into())),
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
            mir::Rvalue::Call(callee_poll.clone(), vec![mir::Operand::var(awt_addr)]),
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
            poll_name.to_string(),
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
        Pat::Path(path) => Ok(path
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
