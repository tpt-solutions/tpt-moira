//! Lower the Clotho AST into the Atropos MIR (SSA form) for the Phase-1 subset.
//!
//! In addition to the usual synchronous subset, this module lowers `async fn`
//! into a genuine suspending state machine: a future struct holding the
//! coroutine `state` plus every live slot, a `poll` coroutine driven by an
//! entry `switch` on `state` (one resume block per `.await` point), and a sync
//! `run` executor wrapper that drives the future to completion. See
//! [`Lower::lower_async_function`] for the transformation.

use crate::mir;
use std::collections::HashMap;
use std::fmt;
use tpt_moira_clotho::alias::AliasTable;
use tpt_moira_clotho::ast::{
    self, BinOp as ABinOp, Expr, ExprKind, ItemKind, Pat, Program, Stmt, StmtKind, TypeKind,
    UnOp as AUnOp,
};

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
/// Per-expression type hints: maps an expression's source span `(start, end)` to
/// its C-style type (`bool`, `str`, `Vec<T>` or `Vec<T>*`). Lachesis produces
/// them from the type checker; the untyped lowering cannot infer these itself.
pub type TypeHints = HashMap<(usize, usize), String>;

/// Whether a hint changes lowering or codegen (the other hinted types are not used yet).
fn hint_applies(hint: &str) -> bool {
    hint == "bool"
        || hint == "str"
        || hint == "Option"
        || hint == "Result"
        || mir::vec_elem(hint).is_some()
        || mir::slice_elem(hint).is_some()
        || mir::vec_ptr_elem(hint).is_some()
}

/// Lower a whole program without type hints.
pub fn lower_program(prog: &Program) -> Result<mir::MirProgram> {
    lower_program_typed(prog, &TypeHints::new())
}

/// Lower a whole program, using `hints` for expressions whose type the untyped
/// lowering cannot determine (`for` over a field or pattern binding, `println!`
/// of a bool, and so on).
/// Name the function a lowering error came from.
fn in_fn(name: &str, e: LowerError) -> LowerError {
    LowerError(format!("in fn `{name}`: {}", e.0))
}

pub fn lower_program_typed(prog: &Program, hints: &TypeHints) -> Result<mir::MirProgram> {
    let shared = std::sync::Arc::new(hints.clone());
    let mut out = mir::MirProgram::default();

    let aliases = AliasTable::from_program(prog);
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
    // Each enum becomes a tagged struct `{ long long tag; long long f0; .. }`
    // with one payload slot per field of its widest variant; each variant maps
    // its constructor name to the enum + tag + arity.
    let mut enum_structs: Vec<mir::StructDef> = Vec::new();
    let mut variants: HashMap<String, (String, i128, usize)> = HashMap::new();
    let mut qualified: HashMap<String, (String, i128, usize)> = HashMap::new();
    for (ename, vars) in &enum_defs {
        let max_arity = vars.iter().map(|(_, a)| *a).max().unwrap_or(0);
        let mut fields = vec![("tag".to_string(), "long long".to_string())];
        for i in 0..max_arity {
            fields.push((format!("f{}", i), "long long".to_string()));
        }
        enum_structs.push(mir::StructDef {
            name: ename.clone(),
            fields,
        });
        for (tag, (vname, arity)) in vars.iter().enumerate() {
            variants.insert(vname.clone(), (ename.clone(), tag as i128, *arity));
            qualified.insert(
                format!("{ename}::{vname}"),
                (ename.clone(), tag as i128, *arity),
            );
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
                    .map(|f| (f.name.node.clone(), type_to_c(&f.ty, &aliases)))
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
                    let mut l = Lower::new(&out.structs, shared.clone());
                    l.async_fns = async_names.clone();
                    l.all_fn_names = all_fn_names.clone();
                    l.const_values = const_values.clone();
                    l.aliases = aliases.clone();
                    let ad = l
                        .lower_async_function(f)
                        .map_err(|e| in_fn(&f.name.node, e))?;
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
                    let mut l = Lower::new(&out.structs, shared.clone());
                    l.async_fns = async_names.clone();
                    l.all_fn_names = all_fn_names.clone();
                    l.const_values = const_values.clone();
                    l.variants = variants.clone();
                    l.qualified_variants = qualified.clone();
                    l.aliases = aliases.clone();
                    let func = l.lower_function(f).map_err(|e| in_fn(&f.name.node, e))?;
                    let tuple_structs: Vec<mir::StructDef> =
                        l.tuple_structs.values().cloned().collect();
                    out.functions.push(func);
                    out.functions.extend(l.closures.drain(..));
                    out.structs.extend(tuple_structs);
                }
            }
            // Aliases carry no code: every use is expanded to the aliased type
            // before lowering (see `AliasTable`).
            ItemKind::TypeAlias(_) => {}
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
/// `type` aliases are expanded first.
/// A `for` iterable split into the sequence and the adapters around it:
/// `seq`, `&seq`, `&mut seq`, `seq.iter()`, `seq.iter_mut()`, optionally followed by
/// `.rev()` and `.enumerate()`.
struct SeqIter<'a> {
    seq: &'a Expr,
    enumerate: bool,
    rev: bool,
    /// Items are bound by pointer (`iter_mut()`, `&mut seq`).
    mutable: bool,
    /// The bytes of a string (`s.chars()`, `s.bytes()`).
    chars: bool,
}

fn seq_iter(iter: &Expr) -> SeqIter<'_> {
    let mut e = iter;
    let mut si = SeqIter {
        seq: iter,
        enumerate: false,
        rev: false,
        mutable: false,
        chars: false,
    };
    if let ExprKind::MethodCall(r, m, args) = &e.kind {
        if m.node == "enumerate" && args.is_empty() {
            si.enumerate = true;
            e = &**r;
        }
    }
    if let ExprKind::MethodCall(r, m, args) = &e.kind {
        if m.node == "rev" && args.is_empty() {
            si.rev = true;
            e = &**r;
        }
    }
    if let ExprKind::MethodCall(r, m, args) = &e.kind {
        if (m.node == "chars" || m.node == "bytes") && args.is_empty() {
            si.chars = true;
            e = &**r;
        }
    }
    if let ExprKind::MethodCall(r, m, args) = &e.kind {
        if (m.node == "iter" || m.node == "iter_mut") && args.is_empty() {
            si.mutable = m.node == "iter_mut";
            e = &**r;
        }
    }
    if let ExprKind::Ref(inner, mutable) = &e.kind {
        si.mutable = *mutable;
        e = &**inner;
    }
    si.seq = e;
    si
}

/// An iterator chain: the sequence it starts from (`seq.iter()`) and the adapters
/// applied to each item, in application order (`(method, closure argument)`).
struct IterChain<'a> {
    seq: Option<&'a Expr>,
    /// A range source `lo..hi` (or `lo..=hi`): its items are the integers.
    range: Option<(&'a Expr, &'a Expr, bool)>,
    stages: Vec<(String, Option<&'a Expr>)>,
}

/// The iterator terminals lowered over an iterator chain.
fn is_iter_terminal(name: &str) -> bool {
    matches!(
        name,
        "collect" | "any" | "all" | "find" | "count" | "sum" | "for_each"
    )
}

/// Whether a closure argument takes an `(index, item)` pair as its one parameter.
fn closure_takes_pair(arg: &Expr) -> bool {
    match &arg.kind {
        ExprKind::Closure { params, .. } => {
            params.len() == 1 && matches!(&params[0].pat, Pat::Tuple(ps) if ps.len() == 2)
        }
        _ => false,
    }
}

/// The argument list of an adapter closure call: its captured variables, then the
/// item. After `enumerate` the item is the pair `(position, item)`, which only a
/// closure with an `(index, item)` pattern takes (as two arguments).
fn call_args(
    caps: Vec<mir::Operand>,
    pos: &Option<String>,
    item: &str,
    arg: &Expr,
) -> Result<Vec<mir::Operand>> {
    let mut args = caps;
    if closure_takes_pair(arg) != pos.is_some() {
        return Err(LowerError(
            "an `enumerate` item needs a closure with an `(index, item)` pattern".into(),
        ));
    }
    if let Some(p) = pos {
        args.push(mir::Operand::var(p.clone()));
    }
    args.push(mir::Operand::var(item));
    Ok(args)
}

/// The names a closure parameter binds: one name, or the two names of an
/// `(index, item)` pattern. Other patterns are rejected by `pat_name`.
fn closure_param_names(p: &Pat) -> Result<Vec<String>> {
    match p {
        Pat::Tuple(elems) => {
            let mut names = Vec::new();
            for e in elems {
                names.push(pat_name(e)?);
            }
            Ok(names)
        }
        Pat::Ref(inner) => closure_param_names(inner),
        _ => Ok(vec![pat_name(p)?]),
    }
}

/// The C spelling of a Moira scalar type name (`i32` is `int32_t`, and so on);
/// other names are returned unchanged.
fn c_scalar_name(name: &str) -> String {
    let c = match name {
        "i8" => "int8_t",
        "i16" => "int16_t",
        "i32" => "int32_t",
        "i64" => "int64_t",
        "i128" | "isize" => "long long",
        "u8" => "uint8_t",
        "u16" => "uint16_t",
        "u32" => "uint32_t",
        "u64" => "uint64_t",
        "u128" | "usize" => "unsigned long long",
        "bool" => "int",
        "f32" => "float",
        "f64" => "double",
        other => other,
    };
    c.to_string()
}

/// A short rendering of a pattern for diagnostics.
fn pat_brief(p: &ast::Pat) -> String {
    let s = format!("{p:?}");
    s.chars().take(160).collect()
}

/// The last segment of a macro invocation's path (`vec` for `vec!`).
fn macro_name(m: &ast::MacroCall) -> String {
    m.path
        .segments
        .last()
        .map(|s| s.node.clone())
        .unwrap_or_default()
}

/// The expression inside parentheses: the parser reads `(e)` as a block whose
/// only content is `e`.
fn unparen(e: &Expr) -> &Expr {
    match &e.kind {
        ExprKind::Block(b) if b.stmts.is_empty() => match b.tail.as_deref() {
            Some(inner) => unparen(inner),
            None => e,
        },
        _ => e,
    }
}

/// The bounds of a range expression `a..b`, `a..=b`, `..b`, `a..` or `..`, as
/// `(start, end, inclusive)`; an elided bound is `None`.
fn range_bounds(e: &Expr) -> Option<(Option<&Expr>, Option<&Expr>, bool)> {
    match &e.kind {
        ExprKind::Binary(l, ABinOp::Range, r) => Some((Some(&**l), Some(&**r), false)),
        ExprKind::Range(l, r, inclusive) => Some((l.as_deref(), r.as_deref(), *inclusive)),
        _ => None,
    }
}

/// If `e` is `Vec::new()` or `Vec::with_capacity(..)`, the constructor name
/// (`"new"` or `"with_capacity"`).
fn vec_constructor(e: &Expr) -> Option<&'static str> {
    let callee = match &e.kind {
        ExprKind::Call(callee, _) => callee,
        _ => return None,
    };
    let p = match &callee.kind {
        ExprKind::Path(p) => p,
        _ => return None,
    };
    if p.segments.len() != 2 || p.segments[0].node != "Vec" {
        return None;
    }
    // `with_capacity` is accepted as a plain constructor: the capacity hint is
    // not used by the runtime yet.
    if p.segments[1].node == "new" || p.segments[1].node == "with_capacity" {
        Some("new")
    } else {
        None
    }
}

fn type_to_c(ty: &ast::Type, aliases: &AliasTable) -> String {
    type_to_c_expanded(&aliases.expand(ty))
}

/// [`type_to_c`] for a type whose aliases are already expanded.
fn type_to_c_expanded(ty: &ast::Type) -> String {
    match &ty.kind {
        ast::TypeKind::Path(p, args) => {
            let last = p.segments.last().map(|s| s.node.clone());
            if last.as_deref() == Some("Vec") && args.len() == 1 {
                mir::vec_type(&type_to_c_expanded(&args[0]))
            } else {
                last.unwrap_or_else(|| "long long".to_string())
            }
        }
        ast::TypeKind::Ref(inner) | ast::TypeKind::RefMut(inner) => {
            // A reference to a slice is the slice value itself.
            let c = type_to_c_expanded(inner);
            if mir::slice_elem(&c).is_some() {
                c
            } else {
                format!("{c}*")
            }
        }
        ast::TypeKind::Slice(inner) => mir::slice_type(&type_to_c_expanded(inner)),
        ast::TypeKind::Future(inner) => type_to_c_expanded(inner),
        _ => "long long".to_string(),
    }
}

struct Lower {
    /// Checked types of expressions (see [`TypeHints`]).
    hints: std::sync::Arc<TypeHints>,
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
    /// The same variants keyed `Enum::Variant`, for paths that name their enum.
    qualified_variants: HashMap<String, (String, i128, usize)>,
    /// Captured variables per lambda-lifted closure (lifted fn name ->
    /// captured local names, in the order they appear as leading parameters).
    closure_captures: HashMap<String, Vec<String>>,
    /// Local variables bound to capturing closures: local name -> lifted fn
    /// name. Used to rewrite direct calls `f(args)` into
    /// `__moira_closure_N(captures…, args)`.
    closure_bindings: HashMap<String, String>,
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
    /// Top-level `type` aliases, expanded before any type is converted to C.
    aliases: AliasTable,
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
        format!(
            "long long (*)({})",
            vec!["long long"; param_count].join(", ")
        )
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
    fn new(structs: &[mir::StructDef], hints: std::sync::Arc<TypeHints>) -> Self {
        Lower {
            hints,
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
            qualified_variants: HashMap::new(),
            closure_captures: HashMap::new(),
            closure_bindings: HashMap::new(),
            async_fns: std::collections::HashSet::new(),
            async_ctx: None,
            expecting_future: false,
            tuple_structs: HashMap::new(),
            closure_counter: 0,
            closures: Vec::new(),
            all_fn_names: std::collections::HashSet::new(),
            aliases: AliasTable::default(),
        }
    }

    fn lower_function(&mut self, f: &ast::Function) -> Result<mir::Function> {
        self.func.name = f.name.node.clone();
        self.func.ret = f.ret.as_ref().map(|t| type_to_c(t, &self.aliases));
        for p in &f.params {
            let name = pat_name(&p.pat)?;
            let ty = type_to_c(&p.ty, &self.aliases);
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
            .map(|t| type_to_c(t, &self.aliases))
            .unwrap_or_else(|| "long long".to_string());

        // Collect every parameter + `let` local as a slot that must persist
        // across a suspension.
        let mut slots: Vec<(String, String)> = Vec::new();
        for p in &f.params {
            let name = pat_name(&p.pat)?;
            let ty = type_to_c(&p.ty, &self.aliases);
            slots.push((name, ty));
        }
        collect_locals(&f.body, &mut slots, &self.aliases);

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
        let run_fn = build_run_wrapper(
            f,
            &fut_struct,
            &poll_name,
            &output_ty,
            &slots,
            &self.aliases,
        );
        let ctor_fn = build_ctor_wrapper(f, &fut_struct, &self.aliases);

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
        // Temporaries and locals bound by `let` are in `types`; parameters and
        // declared locals are only in the function's local list.
        self.types
            .get(name)
            .cloned()
            .or_else(|| {
                self.func
                    .locals
                    .iter()
                    .find(|(n, _)| n == name)
                    .and_then(|(_, t)| t.clone())
            })
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

    /// Lambda-lift a closure `|params| body` into a fresh top-level
    /// `mir::Function` and return a `Func` operand referencing it (the closure
    /// value). The lifted function always returns `long long` (a `()` result is
    /// returned as `0`, which callers discard) so its C signature lines up with the
    /// uniform `long long (*)(long long, …)` function-pointer type used for closure
    /// values.
    ///
    /// **Captures**: variables the body references from the enclosing scope are
    /// lowered as *leading parameters* on the lifted function. Call sites that
    /// invoke the closure through a local it was bound to are rewritten by
    /// [`Lower::closure_bindings`] to pass the captured operands; a closure
    /// passed to an unrelated function still lowers to its bare pointer
    /// (invoking such a pointer with captures is not representable in the
    /// subset and remains a documented limitation).
    fn lower_closure(&mut self, params: &[ast::Param], body: &Expr) -> Result<mir::Operand> {
        let mut param_names = std::collections::HashSet::new();
        for p in params {
            for n in closure_param_names(&p.pat)? {
                param_names.insert(n);
            }
        }
        // Structural capture analysis: any single-segment path name the body
        // references that is neither a closure parameter nor a top-level function
        // is a captured variable.
        let captures = closure_captures(body, &param_names, &self.all_fn_names);

        // Qualified by the enclosing function: each function is lowered with its
        // own counter, so a bare counter would give two closures the same name.
        let name = format!(
            "__moira_closure_{}_{}",
            self.func.name, self.closure_counter
        );
        self.closure_counter += 1;

        // Build the lifted function in its own context, seeded with the enclosing
        // environment so nested closures and async names resolve.
        let mut sub = Lower::new(&self.structs, self.hints.clone());
        sub.async_fns = self.async_fns.clone();
        sub.all_fn_names = self.all_fn_names.clone();
        sub.aliases = self.aliases.clone();
        // Enum variants and constants are program-wide: a closure body names them too.
        sub.variants = self.variants.clone();
        sub.qualified_variants = self.qualified_variants.clone();
        sub.const_values = self.const_values.clone();
        sub.closure_counter = self.closure_counter;
        sub.func.name = name.clone();
        sub.func.ret = Some("long long".to_string());
        // Captured variables become leading `long long` parameters.
        for cap in &captures {
            sub.func.params.push(cap.clone());
            sub.func
                .locals
                .push((cap.clone(), Some("long long".to_string())));
        }
        for p in params {
            // Structural lowering: closure parameters are `long long` so the lifted
            // signature matches the uniform function-pointer type. An `(index, item)`
            // pattern is two parameters.
            for n in closure_param_names(&p.pat)? {
                sub.func.params.push(n.clone());
                sub.func.locals.push((n, Some("long long".to_string())));
            }
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
        // Record the captures so call sites through a bound local can pass them.
        self.closure_captures.insert(name.clone(), captures);
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
                    (Some(t), _) => Some(type_to_c(t, &self.aliases)),
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
                    // `let v: Vec<T> = Vec::new()` / `Vec::with_capacity(n)`: the
                    // declared type names the element type of the constructor.
                    if let (Some(elem), Some(vec_ty)) =
                        (ty.as_deref().and_then(mir::vec_elem), vec_constructor(init))
                    {
                        let t = self.fresh_temp(mir::vec_type(elem));
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Call(
                                mir::Operand::Func(mir::vec_fn_name(elem, vec_ty)),
                                vec![],
                            ),
                        );
                        self.emit(name.clone(), mir::Rvalue::Use(mir::Operand::var(t)));
                        return Ok(());
                    }
                    let v = self.lower_expr(init)?;
                    // `let f = |..| ..;` binds a closure value: remember the
                    // local -> lifted-fn mapping so later direct calls
                    // `f(args)` pass the captured variables.
                    if let ExprKind::Closure { .. } = &init.kind {
                        if let mir::Operand::Func(lifted) = &v {
                            if self.closure_captures.contains_key(lifted) {
                                self.closure_bindings.insert(name.clone(), lifted.clone());
                            }
                        }
                    }
                    // An unannotated `let` takes the type of its initializer when that
                    // is known (a slice or a vector), so later uses can index it.
                    if ty.is_none() {
                        if let mir::Operand::Var(src) = &v {
                            let t = self.temp_type_of(src);
                            if mir::slice_elem(&t).is_some() || mir::vec_elem(&t).is_some() {
                                self.types.insert(name.clone(), t.clone());
                                for (ln, lt) in self.func.locals.iter_mut() {
                                    if *ln == name && lt.is_none() {
                                        *lt = Some(t.clone());
                                    }
                                }
                            }
                        }
                    }
                    self.emit(name.clone(), mir::Rvalue::Use(v));
                }
                Ok(())
            }
            StmtKind::Semi(e) | StmtKind::Expr(e) => {
                self.lower_expr(e)?;
                Ok(())
            }
            StmtKind::Item(item) if matches!(item.kind, ast::ItemKind::Use(_)) => {
                // A function-local `use …` declaration introduces no runtime
                // code (Phase 1 ignores imports).
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
                    Some(ts.iter().map(|t| type_to_c(t, &self.aliases)).collect())
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
                    _ => return Err(LowerError(
                        "only identifier/wildcard elements are supported in `let` tuple patterns"
                            .into(),
                    )),
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
            let name =
                match pat {
                    Pat::Ident(n) => n.node.clone(),
                    Pat::Wild => continue,
                    _ => return Err(LowerError(
                        "only identifier/wildcard elements are supported in `let` tuple patterns"
                            .into(),
                    )),
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

    /// Lower `e?` on an `Option` or `Result` value. The error variant (`None`,
    /// or `Err`) returns the value unchanged from the enclosing function, which
    /// must return the same enum; any other value yields its payload. Option and
    /// Result share one tagged layout (`Some`/`Ok` = tag 0, `None`/`Err` = tag 1),
    /// so the returned value needs no conversion.
    fn lower_try(&mut self, inner: &Expr) -> Result<mir::Operand> {
        let ret = self.func.ret.clone().unwrap_or_default();
        let err_variant = match ret.as_str() {
            "Option" => "None",
            "Result" => "Err",
            _ => {
                return Err(LowerError(
                    "`?` is only supported in functions returning `Option` or `Result`".into(),
                ))
            }
        };
        let err_tag = match self.variants.get(err_variant) {
            Some((_, tag, _)) => *tag,
            None => {
                return Err(LowerError(format!(
                    "`?` needs the `{err_variant}` variant of the prelude enum"
                )))
            }
        };
        let v = self.lower_expr(inner)?;
        let name = match &v {
            mir::Operand::Var(n) => n.clone(),
            _ => return Err(LowerError("`?` must apply to a named value".into())),
        };
        self.type_enum_operand(&v, &ret);
        let tag = self.fresh_temp("long long");
        self.emit(
            tag.clone(),
            mir::Rvalue::Field(mir::Operand::var(name.clone()), "tag".into(), false),
        );
        let is_err = self.fresh_temp("long long");
        self.emit(
            is_err.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Eq,
                mir::Operand::var(tag),
                mir::Operand::Const(err_tag),
            ),
        );
        let err_blk = self.new_block();
        let ok_blk = self.new_block();
        self.set_term(mir::Terminator::If(
            mir::Operand::var(is_err),
            err_blk,
            ok_blk,
        ));
        self.current = err_blk;
        self.set_term(mir::Terminator::Return(Some(mir::Operand::var(
            name.clone(),
        ))));
        self.current = ok_blk;
        let payload = self.fresh_temp("long long");
        self.emit(
            payload.clone(),
            mir::Rvalue::Field(mir::Operand::var(name), "f0".into(), false),
        );
        Ok(mir::Operand::var(payload))
    }

    /// Whether the named local or parameter has a slice type.
    fn local_is_slice(&self, name: &str) -> bool {
        let ty = self.types.get(name).cloned().or_else(|| {
            self.func
                .locals
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, t)| t.clone())
        });
        ty.map_or(false, |t| mir::slice_elem(&t).is_some())
    }

    /// Lower `base[a..b]` (any of the range forms) to a slice value. The base is a
    /// `Vec`, a slice, or a `let`-bound array; the bounds default to the start and
    /// the length of the base.
    fn lower_slice_index(&mut self, base: &Expr, idx: &Expr) -> Result<mir::Operand> {
        let (start, end, inclusive) = match range_bounds(idx) {
            Some(b) => b,
            None => return Err(LowerError("slice index must be a range".into())),
        };
        // A sequence the lowering cannot see locally (a field, a call result) is
        // typed from its hint.
        let found = match self.vec_local(base) {
            Some(x) => Some(x),
            None if self.is_sequence(base) => Some(self.chain_sequence(base)?),
            None => None,
        };
        let (data, len, elem) = match found {
            Some((vname, elem, is_ptr)) => {
                let data = self.fresh_temp(format!("{elem}*"));
                self.emit(
                    data.clone(),
                    mir::Rvalue::Field(mir::Operand::var(vname.clone()), "data".into(), is_ptr),
                );
                let len = self.fresh_temp("long long");
                self.emit(
                    len.clone(),
                    mir::Rvalue::Field(mir::Operand::var(vname), "len".into(), is_ptr),
                );
                (mir::Operand::var(data), mir::Operand::var(len), elem)
            }
            None => {
                let name = match &base.kind {
                    ExprKind::Path(p) if p.segments.len() == 1 => p.segments[0].node.clone(),
                    _ => {
                        return Err(LowerError(
                            "slicing needs a named Vec, slice or array".into(),
                        ))
                    }
                };
                let n = match self.array_lens.get(&name) {
                    Some(n) => *n,
                    None => {
                        return Err(LowerError(format!(
                            "cannot slice `{name}`: not a Vec, slice or array"
                        )))
                    }
                };
                (
                    mir::Operand::var(name),
                    mir::Operand::Const(n as i128),
                    "long long".to_string(),
                )
            }
        };
        let lo = match start {
            Some(s) => self.lower_expr(s)?,
            None => mir::Operand::Const(0),
        };
        let mut hi = match end {
            Some(e) => self.lower_expr(e)?,
            None => len.clone(),
        };
        if inclusive && end.is_some() {
            let t = self.fresh_temp("long long");
            self.emit(
                t.clone(),
                mir::Rvalue::Binary(mir::BinOp::Add, hi, mir::Operand::Const(1)),
            );
            hi = mir::Operand::var(t);
        }
        let t = self.fresh_temp(mir::slice_type(&elem));
        self.emit(
            t.clone(),
            mir::Rvalue::Call(
                mir::Operand::Func(mir::slice_fn_name(&elem, "sub")),
                vec![data, len, lo, hi],
            ),
        );
        Ok(mir::Operand::var(t))
    }

    /// If `e` names a local (or parameter) of type `Vec<T>` or `&mut Vec<T>`,
    /// returns `(name, T, is_pointer)`.
    fn vec_local(&self, e: &Expr) -> Option<(String, String, bool)> {
        let p = match &e.kind {
            ExprKind::Path(p) => p,
            _ => return None,
        };
        if p.segments.len() != 1 {
            return None;
        }
        let name = p.segments[0].node.clone();
        let ty = self.types.get(&name).cloned().or_else(|| {
            self.func
                .locals
                .iter()
                .find(|(n, _)| *n == name)
                .and_then(|(_, t)| t.clone())
        })?;
        if let Some(elem) = mir::vec_elem(&ty).or_else(|| mir::slice_elem(&ty)) {
            return Some((name, elem.to_string(), false));
        }
        mir::vec_ptr_elem(&ty).map(|elem| (name, elem.to_string(), true))
    }

    /// The sequence of an iterator chain `seq.iter()` followed by `map`, `filter`,
    /// `cloned` or `copied` adapters, if `e` is one.
    fn iter_chain_of<'a>(&self, e: &'a Expr) -> Option<IterChain<'a>> {
        let mut stages: Vec<(String, Option<&'a Expr>)> = Vec::new();
        let mut cur: &'a Expr = unparen(e);
        loop {
            // A range source: `(lo..hi).map(..)` iterates the integers.
            if let Some((Some(lo), Some(hi), inclusive)) = range_bounds(cur) {
                stages.reverse();
                return Some(IterChain {
                    seq: None,
                    range: Some((lo, hi, inclusive)),
                    stages,
                });
            }
            let (r, m, args) = match &cur.kind {
                ExprKind::MethodCall(r, m, args) => (r, m, args),
                _ => return None,
            };
            let name = m.node.as_str();
            match name {
                "iter" | "into_iter" if args.is_empty() => {
                    stages.reverse();
                    let seq: &'a Expr = &**r;
                    return if self.is_sequence(seq) {
                        Some(IterChain {
                            seq: Some(seq),
                            range: None,
                            stages,
                        })
                    } else {
                        None
                    };
                }
                "map" | "filter" if args.len() == 1 => {
                    stages.push((name.to_string(), Some(&args[0])));
                }
                "cloned" | "copied" | "enumerate" if args.is_empty() => {
                    stages.push((name.to_string(), None))
                }
                _ => return None,
            }
            cur = unparen(&**r);
        }
    }

    /// Whether `seq` is a vector, slice or array value the chain can index.
    fn is_sequence(&self, seq: &Expr) -> bool {
        self.vec_local(seq).is_some()
            || self.hint_of_expr(seq).map_or(false, |h| {
                mir::vec_elem(&h).is_some()
                    || mir::slice_elem(&h).is_some()
                    || mir::vec_ptr_elem(&h).is_some()
            })
    }

    /// The name, element type and pointer-ness of a chain's sequence. A sequence
    /// reached through a field or a pattern binding is lowered and typed from its hint.
    fn chain_sequence(&mut self, seq: &Expr) -> Result<(String, String, bool)> {
        if let Some(found) = self.vec_local(seq) {
            return Ok(found);
        }
        let hint = match self.hint_of_expr(seq) {
            Some(h) => h,
            None => return Err(LowerError("iterator chain over an unknown sequence".into())),
        };
        let (elem, hinted_ptr) = match mir::vec_elem(&hint)
            .or_else(|| mir::slice_elem(&hint))
            .map(|e| (e.to_string(), false))
            .or_else(|| mir::vec_ptr_elem(&hint).map(|e| (e.to_string(), true)))
        {
            Some(p) => p,
            None => return Err(LowerError("iterator chain over an unknown sequence".into())),
        };
        let op = self.lower_expr(seq)?;
        let n = match op {
            mir::Operand::Var(n) => n,
            _ => return Err(LowerError("iterator chain needs a named sequence".into())),
        };
        if self.temp_type_of(&n) == "long long" {
            self.types.insert(n.clone(), hint.clone());
        }
        let is_ptr = hinted_ptr || self.temp_type_of(&n).ends_with('*');
        Ok((n, elem, is_ptr))
    }

    /// The lifted function of a closure argument and the captured variables it
    /// takes as leading arguments. A closure literal is lifted here, once, so the
    /// call inside the loop does not repeat the lifting.
    fn closure_callee(&mut self, arg: &Expr) -> Result<(String, Vec<mir::Operand>)> {
        let lifted = match &arg.kind {
            ExprKind::Path(p) if p.segments.len() == 1 => {
                let name = p.segments[0].node.clone();
                if let Some(l) = self.closure_bindings.get(&name).cloned() {
                    l
                } else if self.all_fn_names.contains(&name) {
                    return Ok((name, Vec::new()));
                } else {
                    return Err(LowerError(format!(
                        "`{name}` is not a closure or a function"
                    )));
                }
            }
            _ => match self.lower_expr(arg)? {
                mir::Operand::Func(name) => name,
                _ => return Err(LowerError("iterator adapters take a closure".into())),
            },
        };
        let caps = self
            .closure_captures
            .get(&lifted)
            .cloned()
            .unwrap_or_default();
        Ok((lifted, caps.into_iter().map(mir::Operand::var).collect()))
    }

    /// Lower a terminal call (`collect`, `any`, `all`, `find`, `count`, `sum`,
    /// `for_each`) on an iterator chain as one index loop. The source is a sequence
    /// (its items are its elements) or a range (its items are the integers). The
    /// adapters apply to each item in order: `map` replaces it with the closure's
    /// result, `filter` skips the items its closure rejects, and `enumerate` pairs
    /// each item with its position. A paired item is passed to a closure that takes
    /// an `(index, item)` pattern as two arguments. The output of `collect` is a
    /// `Vec` whose element type comes from the checker's hint.
    fn lower_iter_chain(
        &mut self,
        e: &Expr,
        chain: IterChain<'_>,
        terminal: &str,
        term_args: &[Expr],
    ) -> Result<mir::Operand> {
        // The source: a sequence's name, element type and pointer-ness, or a range's
        // bounds (lowered now, before the loop).
        let source = match (chain.seq, chain.range) {
            (Some(seq), _) => Ok(self.chain_sequence(seq)?),
            (None, Some((lo, hi, inclusive))) => {
                let lo_op = self.lower_expr(lo)?;
                let mut hi_op = self.lower_expr(hi)?;
                if inclusive {
                    let t = self.fresh_temp("long long");
                    self.emit(
                        t.clone(),
                        mir::Rvalue::Binary(mir::BinOp::Add, hi_op.clone(), mir::Operand::Const(1)),
                    );
                    hi_op = mir::Operand::var(t);
                }
                Err((lo_op, hi_op))
            }
            (None, None) => return Err(LowerError("empty iterator chain".into())),
        };
        let term_callee = match term_args.first() {
            Some(a) => Some(self.closure_callee(a)?),
            None => None,
        };
        if matches!(terminal, "any" | "all" | "find") && term_callee.is_none() {
            return Err(LowerError(format!("`{terminal}` needs a closure")));
        }
        let mut stage_callees: Vec<Option<(String, Vec<mir::Operand>)>> = Vec::new();
        for (_, arg) in &chain.stages {
            stage_callees.push(match arg {
                Some(a) => Some(self.closure_callee(a)?),
                None => None,
            });
        }

        // The result, initialised before the loop.
        let elem = match &source {
            Ok((_, elem, _)) => elem.clone(),
            Err(_) => "long long".to_string(),
        };
        let out_elem = self
            .hint_of_expr(e)
            .and_then(|h| mir::vec_elem(&h).map(|s| s.to_string()))
            .unwrap_or_else(|| elem.clone());
        let mut out: Option<String> = None;
        let mut res: Option<String> = None;
        match terminal {
            "collect" => {
                let v = self.fresh_temp(mir::vec_type(&out_elem));
                self.emit(
                    v.clone(),
                    mir::Rvalue::Call(
                        mir::Operand::Func(mir::vec_fn_name(&out_elem, "new")),
                        vec![],
                    ),
                );
                out = Some(v);
            }
            "any" | "all" => {
                let r = self.fresh_temp("bool");
                let init = if terminal == "any" { 0 } else { 1 };
                self.emit(r.clone(), mir::Rvalue::Use(mir::Operand::Const(init)));
                res = Some(r);
            }
            "find" => {
                let r = self.fresh_temp("Option");
                self.emit(
                    r.clone(),
                    mir::Rvalue::StructLit(
                        "Option".to_string(),
                        vec![
                            ("tag".to_string(), mir::Operand::Const(1)),
                            ("f0".to_string(), mir::Operand::Const(0)),
                        ],
                    ),
                );
                res = Some(r);
            }
            "count" | "sum" => {
                let r = self.fresh_temp("long long");
                self.emit(r.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
                res = Some(r);
            }
            "for_each" => {}
            _ => {
                return Err(LowerError(format!(
                    "`{terminal}` is not an iterator terminal"
                )))
            }
        }

        // The loop counter, its bound, and (for a sequence) the data pointer.
        let idx = self.fresh_temp("long long");
        let (bound, data) = match &source {
            Ok((vname, elem, is_ptr)) => {
                let len = self.fresh_temp("long long");
                self.emit(
                    len.clone(),
                    mir::Rvalue::Field(mir::Operand::var(vname.clone()), "len".into(), *is_ptr),
                );
                let data = self.fresh_temp(format!("{elem}*"));
                self.emit(
                    data.clone(),
                    mir::Rvalue::Field(mir::Operand::var(vname.clone()), "data".into(), *is_ptr),
                );
                self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
                (mir::Operand::var(len), Some(data))
            }
            Err((lo_op, hi_op)) => {
                self.emit(idx.clone(), mir::Rvalue::Use(lo_op.clone()));
                (hi_op.clone(), None)
            }
        };
        // `enumerate` counts the items that reach it.
        let counter = self.fresh_temp("long long");
        self.emit(counter.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));

        let head = self.new_block();
        let body_blk = self.new_block();
        let step = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let cmp = self.fresh_temp("long long");
        self.emit(
            cmp.clone(),
            mir::Rvalue::Binary(mir::BinOp::Lt, mir::Operand::var(idx.clone()), bound),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));

        self.current = body_blk;
        let mut item = self.fresh_temp(elem.clone());
        match data {
            Some(data) => self.emit(
                item.clone(),
                mir::Rvalue::Index(mir::Operand::var(data), mir::Operand::var(idx.clone())),
            ),
            None => self.emit(
                item.clone(),
                mir::Rvalue::Use(mir::Operand::var(idx.clone())),
            ),
        }
        // The position of the current item, once `enumerate` has passed it.
        let mut pos: Option<String> = None;
        for ((name, arg), callee) in chain.stages.iter().zip(stage_callees) {
            match name.as_str() {
                "map" => {
                    let (f, caps) = callee.expect("map has a closure");
                    let args = call_args(caps, &pos, &item, arg.expect("map has a closure"))?;
                    let r = self.fresh_temp("long long");
                    self.emit(r.clone(), mir::Rvalue::Call(mir::Operand::Func(f), args));
                    item = r;
                    pos = None;
                }
                "filter" => {
                    let (f, caps) = callee.expect("filter has a closure");
                    let args = call_args(caps, &pos, &item, arg.expect("filter has a closure"))?;
                    let keep_test = self.fresh_temp("long long");
                    self.emit(
                        keep_test.clone(),
                        mir::Rvalue::Call(mir::Operand::Func(f), args),
                    );
                    let keep = self.new_block();
                    self.set_term(mir::Terminator::If(
                        mir::Operand::var(keep_test),
                        keep,
                        step,
                    ));
                    self.current = keep;
                }
                "enumerate" => {
                    let p = self.fresh_temp("long long");
                    self.emit(
                        p.clone(),
                        mir::Rvalue::Use(mir::Operand::var(counter.clone())),
                    );
                    self.emit(
                        counter.clone(),
                        mir::Rvalue::Binary(
                            mir::BinOp::Add,
                            mir::Operand::var(counter.clone()),
                            mir::Operand::Const(1),
                        ),
                    );
                    pos = Some(p);
                }
                // `cloned` / `copied` are the identity on the scalar items modelled here.
                _ => {}
            }
        }

        // A position alive at the terminal needs a pair closure; `collect`, `find`
        // and `sum` have no pair form here.
        if pos.is_some() && matches!(terminal, "collect" | "find" | "sum") {
            return Err(LowerError(format!(
                "`{terminal}` on an `enumerate` item is not in the codegen subset"
            )));
        }

        match terminal {
            "collect" => {
                let v = out.clone().expect("collect has a vector");
                let ptr = self.fresh_temp(format!("{}*", mir::vec_type(&out_elem)));
                self.emit(ptr.clone(), mir::Rvalue::Ref(mir::Operand::var(v), true));
                let pushed = self.fresh_temp("long long");
                self.emit(
                    pushed,
                    mir::Rvalue::Call(
                        mir::Operand::Func(mir::vec_fn_name(&out_elem, "push")),
                        vec![mir::Operand::var(ptr), mir::Operand::var(item.clone())],
                    ),
                );
                self.set_term(mir::Terminator::Goto(step));
            }
            "count" => {
                let r = res.clone().expect("count has a result");
                self.emit(
                    r.clone(),
                    mir::Rvalue::Binary(
                        mir::BinOp::Add,
                        mir::Operand::var(r),
                        mir::Operand::Const(1),
                    ),
                );
                self.set_term(mir::Terminator::Goto(step));
            }
            "sum" => {
                let r = res.clone().expect("sum has a result");
                self.emit(
                    r.clone(),
                    mir::Rvalue::Binary(
                        mir::BinOp::Add,
                        mir::Operand::var(r),
                        mir::Operand::var(item.clone()),
                    ),
                );
                self.set_term(mir::Terminator::Goto(step));
            }
            "for_each" => {
                let (f, caps) = term_callee.clone().expect("for_each has a closure");
                let arg = term_args.first().expect("for_each has a closure");
                let args = call_args(caps, &pos, &item, arg)?;
                let t = self.fresh_temp("long long");
                self.emit(t, mir::Rvalue::Call(mir::Operand::Func(f), args));
                self.set_term(mir::Terminator::Goto(step));
            }
            _ => {
                // any / all / find: the closure decides, and a hit leaves the loop.
                let (f, caps) = term_callee.clone().expect("checked above");
                let arg = term_args.first().expect("checked above");
                let args = call_args(caps, &pos, &item, arg)?;
                let c = self.fresh_temp("long long");
                self.emit(c.clone(), mir::Rvalue::Call(mir::Operand::Func(f), args));
                let r = res.clone().expect("has a result");
                let early = self.new_block();
                if terminal == "all" {
                    self.set_term(mir::Terminator::If(mir::Operand::var(c), step, early));
                } else {
                    self.set_term(mir::Terminator::If(mir::Operand::var(c), early, step));
                }
                self.current = early;
                match terminal {
                    "any" => self.emit(r, mir::Rvalue::Use(mir::Operand::Const(1))),
                    "all" => self.emit(r, mir::Rvalue::Use(mir::Operand::Const(0))),
                    _ => self.emit(
                        r,
                        mir::Rvalue::StructLit(
                            "Option".to_string(),
                            vec![
                                ("tag".to_string(), mir::Operand::Const(0)),
                                ("f0".to_string(), mir::Operand::var(item.clone())),
                            ],
                        ),
                    ),
                }
                self.set_term(mir::Terminator::Goto(exit));
            }
        }

        self.current = step;
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
        Ok(match terminal {
            "collect" => mir::Operand::var(out.expect("collect has a vector")),
            "for_each" => mir::Operand::Unit,
            _ => mir::Operand::var(res.expect("has a result")),
        })
    }

    /// Lower `for c in s.chars()` (or `bytes()`) over a string as an index loop
    /// bounded by `strlen`. The items are bytes, which is exact for ASCII text.
    fn lower_str_for(
        &mut self,
        seq: &Expr,
        pat: &ast::Pat,
        body: &ast::Block,
    ) -> Result<mir::Operand> {
        let s = match self.lower_expr(seq)? {
            mir::Operand::Var(n) => n,
            _ => return Err(LowerError("`chars()` needs a named string".into())),
        };
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Call(
                mir::Operand::Func("strlen".to_string()),
                vec![mir::Operand::var(s.clone())],
            ),
        );
        let idx = self.fresh_temp("long long");
        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));

        let head = self.new_block();
        let body_blk = self.new_block();
        let step = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let cmp = self.fresh_temp("long long");
        self.emit(
            cmp.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Lt,
                mir::Operand::var(idx.clone()),
                mir::Operand::var(len),
            ),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));

        self.current = body_blk;
        self.loops.push((exit, step));
        let ch = self.fresh_temp("char");
        self.emit(
            ch.clone(),
            mir::Rvalue::Index(mir::Operand::var(s), mir::Operand::var(idx.clone())),
        );
        self.lower_bind_pattern(pat, mir::Operand::var(ch), "char")?;
        self.lower_block_value(body)?;
        self.loops.pop();
        self.set_term(mir::Terminator::Goto(step));

        self.current = step;
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

    /// Lower a `for` over a sequence (a `Vec`, a slice, or a vector reached
    /// through a field) as an index loop over its `data`/`len` fields. The adapters
    /// in `si` select the order (`rev`), the item (`enumerate` adds a position
    /// counter; `mutable` binds a pointer to each item). The sequence cannot
    /// change length during the loop, so `len` is read once.
    fn lower_seq_for(
        &mut self,
        name: &str,
        elem: &str,
        is_ptr: bool,
        pat: &ast::Pat,
        body: &ast::Block,
        si: &SeqIter,
    ) -> Result<mir::Operand> {
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(name), "len".into(), is_ptr),
        );
        let data = self.fresh_temp(format!("{elem}*"));
        self.emit(
            data.clone(),
            mir::Rvalue::Field(mir::Operand::var(name), "data".into(), is_ptr),
        );
        // `idx` walks the sequence (backwards for `rev`); `pos` counts iterations
        // for `enumerate`, so the index is the position in the iteration order.
        let idx = self.fresh_temp("long long");
        if si.rev {
            self.emit(
                idx.clone(),
                mir::Rvalue::Binary(
                    mir::BinOp::Sub,
                    mir::Operand::var(len.clone()),
                    mir::Operand::Const(1),
                ),
            );
        } else {
            self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
        }
        let pos = self.fresh_temp("long long");
        self.emit(pos.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));

        let head = self.new_block();
        let body_blk = self.new_block();
        // `continue` goes to the step block, so the counters still advance.
        let step = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let cmp = self.fresh_temp("long long");
        let (cmp_op, rhs) = if si.rev {
            (mir::BinOp::Ge, mir::Operand::Const(0))
        } else {
            (mir::BinOp::Lt, mir::Operand::var(len.clone()))
        };
        self.emit(
            cmp.clone(),
            mir::Rvalue::Binary(cmp_op, mir::Operand::var(idx.clone()), rhs),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));

        self.current = body_blk;
        self.loops.push((exit, step));
        let (item, item_ty) = if si.mutable {
            let p = self.fresh_temp(format!("{elem}*"));
            self.emit(
                p.clone(),
                mir::Rvalue::IndexRef(
                    mir::Operand::var(data.clone()),
                    mir::Operand::var(idx.clone()),
                ),
            );
            (mir::Operand::var(p), format!("{elem}*"))
        } else {
            let v = self.fresh_temp(elem.to_string());
            self.emit(
                v.clone(),
                mir::Rvalue::Index(
                    mir::Operand::var(data.clone()),
                    mir::Operand::var(idx.clone()),
                ),
            );
            (mir::Operand::var(v), elem.to_string())
        };
        if si.enumerate {
            let (index_pat, item_pat) = match pat {
                ast::Pat::Tuple(ps) if ps.len() == 2 => (&ps[0], &ps[1]),
                _ => {
                    return Err(LowerError(
                        "`enumerate` needs an `(index, item)` pattern".into(),
                    ))
                }
            };
            let index_name = pat_name(index_pat)?;
            if index_name != "_" {
                if !self.func.locals.iter().any(|(n, _)| *n == index_name) {
                    self.func
                        .locals
                        .push((index_name.clone(), Some("long long".to_string())));
                }
                self.types
                    .insert(index_name.clone(), "long long".to_string());
                self.emit(index_name, mir::Rvalue::Use(mir::Operand::var(pos.clone())));
            }
            self.lower_bind_pattern(item_pat, item, &item_ty)?;
        } else {
            self.lower_bind_pattern(pat, item, &item_ty)?;
        }
        self.lower_block_value(body)?;
        self.loops.pop();
        self.set_term(mir::Terminator::Goto(step));

        self.current = step;
        let step_op = if si.rev {
            mir::BinOp::Sub
        } else {
            mir::BinOp::Add
        };
        self.emit(
            idx.clone(),
            mir::Rvalue::Binary(
                step_op,
                mir::Operand::var(idx.clone()),
                mir::Operand::Const(1),
            ),
        );
        self.emit(
            pos.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Add,
                mir::Operand::var(pos.clone()),
                mir::Operand::Const(1),
            ),
        );
        self.set_term(mir::Terminator::Goto(head));

        self.current = exit;
        Ok(mir::Operand::Unit)
    }

    /// Bind a `for`/loop pattern to a per-iteration element value. A single
    /// identifier binds directly; a tuple pattern `for (a, b) in arr` reads each
    /// element's `.fN` field from the tuple-typed `elem`.
    fn lower_bind_pattern(
        &mut self,
        pat: &ast::Pat,
        elem: mir::Operand,
        elem_ct: &str,
    ) -> Result<()> {
        if let ast::Pat::Tuple(elems) = pat {
            for (i, e) in elems.iter().enumerate() {
                let name = match e {
                    ast::Pat::Ident(n) => n.node.clone(),
                    ast::Pat::Wild => continue,
                    _ => return Err(LowerError(
                        "only identifier/wildcard elements are supported in `for` tuple patterns"
                            .into(),
                    )),
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
                self.emit(
                    name,
                    mir::Rvalue::Field(elem.clone(), format!("f{i}"), false),
                );
            }
            return Ok(());
        }
        let name = pat_name(pat)?;
        if !self.func.locals.iter().any(|(n, _)| n == &name) {
            self.func
                .locals
                .push((name.clone(), Some(elem_ct.to_string())));
        }
        self.types.insert(name.clone(), elem_ct.to_string());
        self.emit(name, mir::Rvalue::Use(elem));
        Ok(())
    }

    /// Lower `for <name> in <start>..(<end>[, inclusive]) { <body> }` into a
    /// C-style index loop. `inclusive` selects `<=` (for `..=`) vs `<` (for `..`).
    /// Lower `for name in (lo..hi).rev()` (or `lo..=hi`): the counter starts at the
    /// last value and steps down to `lo`.
    fn lower_range_for_rev(
        &mut self,
        l: &Expr,
        r: &Expr,
        inclusive: bool,
        name: &str,
        body: &ast::Block,
    ) -> Result<mir::Operand> {
        let lo_op = self.lower_expr(l)?;
        let hi_op = self.lower_expr(r)?;
        let start = if inclusive {
            hi_op
        } else {
            let t = self.fresh_temp("long long");
            self.emit(
                t.clone(),
                mir::Rvalue::Binary(mir::BinOp::Sub, hi_op, mir::Operand::Const(1)),
            );
            mir::Operand::var(t)
        };
        self.func
            .locals
            .push((name.to_string(), Some("long long".into())));
        self.emit(name.to_string(), mir::Rvalue::Use(start));

        let head = self.new_block();
        let body_blk = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let cmp = self.fresh_temp("long long");
        self.emit(
            cmp.clone(),
            mir::Rvalue::Binary(mir::BinOp::Ge, mir::Operand::var(name.to_string()), lo_op),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));

        self.current = body_blk;
        // `continue` goes to the step block, so the counter still moves.
        let step = self.new_block();
        self.loops.push((exit, step));
        self.lower_block_value(body)?;
        self.loops.pop();
        self.set_term(mir::Terminator::Goto(step));
        self.current = step;
        self.emit(
            name.to_string(),
            mir::Rvalue::Binary(
                mir::BinOp::Sub,
                mir::Operand::var(name.to_string()),
                mir::Operand::Const(1),
            ),
        );
        self.set_term(mir::Terminator::Goto(head));

        self.current = exit;
        Ok(mir::Operand::Unit)
    }

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
                if inclusive {
                    mir::BinOp::Le
                } else {
                    mir::BinOp::Lt
                },
                cond,
                b_op,
            ),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));

        self.current = body_blk;
        // `continue` goes to the step block, so the counter still advances.
        let step = self.new_block();
        self.loops.push((exit, step));
        self.lower_block_value(body)?;
        self.loops.pop();
        self.set_term(mir::Terminator::Goto(step));
        self.current = step;
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
        let op = self.lower_expr_inner(e)?;
        self.apply_hint(e, &op);
        Ok(op)
    }

    /// Give a temporary the hinted type of the expression it holds, when the
    /// hint changes lowering (see [`hint_applies`]). Locals keep their declared type.
    fn apply_hint(&mut self, e: &Expr, op: &mir::Operand) {
        let name = match op {
            mir::Operand::Var(n) => n.clone(),
            _ => return,
        };
        if self.func.locals.iter().any(|(n, _)| *n == name) {
            return;
        }
        let hint = match self.hints.get(&(e.span.start, e.span.end)) {
            Some(h) => h.clone(),
            None => return,
        };
        if hint_applies(&hint) && self.temp_type_of(&name) == "long long" {
            self.types.insert(name, hint);
        }
    }

    /// The type hint of `e`, if Lachesis recorded one.
    fn hint_of_expr(&self, e: &Expr) -> Option<String> {
        self.hints.get(&(e.span.start, e.span.end)).cloned()
    }

    fn lower_expr_inner(&mut self, e: &Expr) -> Result<mir::Operand> {
        match &e.kind {
            // Parsed (so crates using `[v; N]` reach the checker), but not yet
            // lowered: the codegen subset has no repeat-array construction.
            ExprKind::ArrayRepeat(..) => Err(LowerError(
                "array repeat `[value; count]` is not in the codegen subset yet".to_string(),
            )),
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
            ExprKind::Range(..) => Err(LowerError(format!(
                "range `..` / `..=` at bytes {}..{} is only supported as a `for` iterator or a slice index",
                e.span.start, e.span.end
            ))),
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
                    // Stores through a place: a field, an element, or a dereference.
                    // They are statements (no destination), like the async state writes.
                    match &l.kind {
                        ExprKind::Field(base, field) => {
                            let b = self.lower_expr(base)?;
                            let rhs = self.lower_expr(r)?;
                            let deref = match &b {
                                mir::Operand::Var(n) => self.temp_type_of(n).ends_with('*'),
                                _ => false,
                            };
                            let fname = match field.node.parse::<usize>() {
                                Ok(i) => format!("f{i}"),
                                Err(_) => field.node.clone(),
                            };
                            self.emit(
                                "__scratch".to_string(),
                                mir::Rvalue::StoreField(b, fname, rhs, deref),
                            );
                            return Ok(mir::Operand::Unit);
                        }
                        ExprKind::Index(base, idx) => {
                            let data = match self.vec_local(base) {
                                Some((vname, elem, is_ptr)) => {
                                    let d = self.fresh_temp(format!("{elem}*"));
                                    self.emit(
                                        d.clone(),
                                        mir::Rvalue::Field(
                                            mir::Operand::var(vname),
                                            "data".into(),
                                            is_ptr,
                                        ),
                                    );
                                    mir::Operand::var(d)
                                }
                                None => self.lower_expr(base)?,
                            };
                            let i = self.lower_expr(idx)?;
                            let rhs = self.lower_expr(r)?;
                            self.emit(
                                "__scratch".to_string(),
                                mir::Rvalue::StoreIndex(data, i, rhs),
                            );
                            return Ok(mir::Operand::Unit);
                        }
                        ExprKind::Deref(inner) | ExprKind::Unary(AUnOp::Deref, inner) => {
                            let p = self.lower_expr(inner)?;
                            let rhs = self.lower_expr(r)?;
                            self.emit(
                                "__scratch".to_string(),
                                mir::Rvalue::StoreIndex(p, mir::Operand::Const(0), rhs),
                            );
                            return Ok(mir::Operand::Unit);
                        }
                        _ => {}
                    }
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
                    if let Some((ename, tag, arity)) = self.variant_info(p) {
                        // A call is a constructor only when its path names the
                        // variant's enum (`MyEnum::Var(..)`, `Self::Var(..)`) or
                        // is a bare name that is not also a function, and the
                        // arity matches. Otherwise it is an ordinary call (e.g. a
                        // function or tuple struct that happens to share the name).
                        let qualifier_ok = if p.segments.len() >= 2 {
                            let q = &p.segments[p.segments.len() - 2].node;
                            *q == ename || q == "Self"
                        } else {
                            !self.all_fn_names.contains(&vname)
                        };
                        if qualifier_ok && arity == args.len() {
                            return self.lower_variant_construction(&ename, tag, arity, args);
                        }
                    }
                }
                // A closure literal as the callee is invoked immediately: lambda-lift
                // it into a top-level function and call that directly.
                let (callee_operand, result_ty) =
                    if let ExprKind::Closure { params, body, .. } = &callee.kind {
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
                        } else if matches!(
                            name.as_str(),
                            "print_i32"
                                | "print_i64"
                                | "print_usize"
                                | "print_f64"
                                | "print_bool"
                                | "print_str"
                        ) {
                            // C runtime builtins are direct calls (never closure values),
                            // so they must be `Func` for the backend's void-call guard.
                            mir::Operand::Func(callee_name)
                        } else if let Some(lifted) = self.closure_bindings.get(&name).cloned() {
                            // A local bound to a *capturing* closure: rewrite to the
                            // direct lifted-fn call so captures can be passed.
                            mir::Operand::Func(lifted)
                        } else {
                            mir::Operand::Var(name)
                        };
                        (op, result_ty)
                    };
                let mut operands = Vec::new();
                // Capturing closures invoked through a bound local get their
                // captured variables as leading arguments.
                if let mir::Operand::Func(fname) = &callee_operand {
                    if let Some(caps) = self.closure_captures.get(fname).cloned() {
                        for cap in caps {
                            operands.push(mir::Operand::Var(cap));
                        }
                    }
                }
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
                // A numeric field is a tuple index even when the base's type is
                // not known here: tuples are `fN` structs in the backend.
                if let Ok(idx) = field.node.parse::<usize>() {
                    let t = self.fresh_temp("long long");
                    self.emit(t.clone(), mir::Rvalue::Field(o, format!("f{idx}"), false));
                    return Ok(mir::Operand::var(t));
                }
                // A field of a reference (`item.attributes` with `item: &Item`) is read
                // through the pointer (`->`); the struct is the pointee type.
                let deref = base_ty.ends_with('*');
                let struct_ty = base_ty.trim_end_matches('*');
                let field_ty = lookup_field_type(&self.structs, struct_ty, &field.node)
                    .unwrap_or_else(|| "long long".to_string());
                let t = self.fresh_temp(field_ty);
                self.emit(t.clone(), mir::Rvalue::Field(o, field.node.clone(), deref));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Ref(e, mutable) => {
                // `&v[a..b]` and `&slice` are slices already: no pointer.
                if let ExprKind::Index(_, idx) = &e.kind {
                    if range_bounds(idx).is_some() {
                        return self.lower_expr(e);
                    }
                }
                let o = self.lower_expr(e)?;
                let inner = self.temp_type_of(&name_of(&o));
                if mir::slice_elem(&inner).is_some() {
                    return Ok(o);
                }
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
                if let ExprKind::MethodCall(r0, m, args) = &iter.kind {
                    if m.node == "rev" && args.is_empty() {
                        if let Some((Some(l), Some(h), inclusive)) = range_bounds(unparen(&**r0)) {
                            let name = pat_name(pat)?;
                            return self.lower_range_for_rev(l, h, inclusive, &name, body);
                        }
                    }
                }
                let si = seq_iter(iter);
                if si.chars {
                    return self.lower_str_for(si.seq, pat, body);
                }
                if let Some((vname, elem, is_ptr)) = self.vec_local(si.seq) {
                    return self.lower_seq_for(&vname, &elem, is_ptr, pat, body, &si);
                }
                // A sequence the lowering cannot see locally (a field, or a pattern
                // binding): iterate it when Lachesis says it is a vector or slice.
                if let Some(hint) = self.hint_of_expr(si.seq) {
                    let vec_form = mir::vec_elem(&hint)
                        .or_else(|| mir::slice_elem(&hint))
                        .map(|e| (e.to_string(), false))
                        .or_else(|| mir::vec_ptr_elem(&hint).map(|e| (e.to_string(), true)));
                    if let Some((elem, _)) = vec_form {
                        let op = self.lower_expr(si.seq)?;
                        if let mir::Operand::Var(n) = op {
                            if self.temp_type_of(&n) == "long long" {
                                self.types.insert(n.clone(), hint.clone());
                                // A pattern-bound local is already declared; give it the hinted type.
                                for (ln, lt) in self.func.locals.iter_mut() {
                                    if *ln == n && lt.as_deref().map_or(true, |t| t == "long long")
                                    {
                                        *lt = Some(hint.clone());
                                    }
                                }
                            }
                            let is_ptr = self.temp_type_of(&n).ends_with('*');
                            return self.lower_seq_for(&n, &elem, is_ptr, pat, body, &si);
                        }
                        return Err(LowerError(
                            "`for` over a vector must iterate a named value".into(),
                        ));
                    }
                }
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
                        let step = self.new_block();
                        self.loops.push((exit, step));
                        // Bind `pat` to `arr[idx]` (single name or tuple destructure).
                        let elem = self.fresh_temp(elem_ct.clone());
                        self.emit(
                            elem.clone(),
                            mir::Rvalue::Index(arr_op.clone(), mir::Operand::var(idx.clone())),
                        );
                        self.lower_bind_pattern(pat, mir::Operand::var(elem), &elem_ct)?;
                        self.lower_block_value(body)?;
                        self.loops.pop();
                        self.set_term(mir::Terminator::Goto(step));
                        self.current = step;
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
                    _ => Err(LowerError(format!(
                        "`for` iterator at bytes {}..{} must be a range `a..b`, an array literal `[..]`, or a `let`-bound array",
                        iter.span.start, iter.span.end
                    ))),
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
            ExprKind::MethodCall(recv, method, args)
                if is_iter_terminal(&method.node) && self.iter_chain_of(recv).is_some() =>
            {
                let chain = self
                    .iter_chain_of(recv)
                    .expect("guard checked the chain");
                self.lower_iter_chain(e, chain, &method.node, args)
            }
            ExprKind::MethodCall(recv, method, args) => {
                // `v[a..b].len()`: the receiver is a slice value; read its length.
                if let ExprKind::Index(_, idx) = &recv.kind {
                    if range_bounds(idx).is_some() && (method.node == "len" || method.node == "is_empty") {
                        let s = self.lower_expr(recv)?;
                        if let mir::Operand::Var(sn) = &s {
                            if mir::slice_elem(&self.temp_type_of(sn)).is_some() {
                                let len = self.fresh_temp("long long");
                                self.emit(
                                    len.clone(),
                                    mir::Rvalue::Field(s.clone(), "len".into(), false),
                                );
                                if method.node == "len" {
                                    return Ok(mir::Operand::var(len));
                                }
                                let t = self.fresh_temp("bool");
                                self.emit(
                                    t.clone(),
                                    mir::Rvalue::Binary(
                                        mir::BinOp::Eq,
                                        mir::Operand::var(len),
                                        mir::Operand::Const(0),
                                    ),
                                );
                                return Ok(mir::Operand::var(t));
                            }
                        }
                    }
                }
                // `v.push(x)`, `v.len()`, `v.is_empty()` on a vector local or
                // `&mut Vec<T>` parameter call the generated `Vec<T>` helpers.
                if let Some((vname, _, _)) = self.vec_local(recv) {
                    // `len` and `is_empty` read a slice's length field directly.
                    if self.local_is_slice(&vname) && method.node == "len" {
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Field(mir::Operand::var(vname), "len".into(), false),
                        );
                        return Ok(mir::Operand::var(t));
                    }
                    if self.local_is_slice(&vname) && method.node == "is_empty" {
                        let len = self.fresh_temp("long long");
                        self.emit(
                            len.clone(),
                            mir::Rvalue::Field(mir::Operand::var(vname), "len".into(), false),
                        );
                        let t = self.fresh_temp("bool");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Binary(
                                mir::BinOp::Eq,
                                mir::Operand::var(len),
                                mir::Operand::Const(0),
                            ),
                        );
                        return Ok(mir::Operand::var(t));
                    }
                    let (vname, elem, is_ptr) = self.vec_local(recv).unwrap();
                    let helper = match method.node.as_str() {
                        "push" | "len" | "is_empty" => Some(method.node.clone()),
                        _ => None,
                    };
                    if let Some(helper) = helper {
                        let recv_ptr = if is_ptr {
                            mir::Operand::var(vname)
                        } else {
                            let p = self.fresh_temp(format!("{}*", mir::vec_type(&elem)));
                            self.emit(p.clone(), mir::Rvalue::Ref(mir::Operand::var(vname), true));
                            mir::Operand::var(p)
                        };
                        let mut operands = vec![recv_ptr];
                        for a in args {
                            operands.push(self.lower_expr(a)?);
                        }
                        let result_ty = if helper == "is_empty" {
                            "bool"
                        } else {
                            "long long"
                        };
                        let t = self.fresh_temp(result_ty);
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Call(
                                mir::Operand::Func(mir::vec_fn_name(&elem, &helper)),
                                operands,
                            ),
                        );
                        return Ok(mir::Operand::var(t));
                    }
                }
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
            ExprKind::Index(base, idx) if range_bounds(idx).is_some() => {
                self.lower_slice_index(base, idx)
            }
            ExprKind::Index(base, idx) if self.vec_local(base).is_some() => {
                let (vname, elem, is_ptr) = self.vec_local(base).unwrap();
                let i = self.lower_expr(idx)?;
                let data = self.fresh_temp(format!("{elem}*"));
                self.emit(
                    data.clone(),
                    mir::Rvalue::Field(mir::Operand::var(vname), "data".into(), is_ptr),
                );
                let t = self.fresh_temp(elem.clone());
                self.emit(t.clone(), mir::Rvalue::Index(mir::Operand::var(data), i));
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
                self.emit(
                    t.clone(),
                    mir::Rvalue::Array("long long".to_string(), operands),
                );
                Ok(mir::Operand::var(t))
            }
            ExprKind::Unsafe(..) => Err(LowerError("`unsafe` is not in the codegen subset".into())),
            ExprKind::Closure { params, body, .. } => self.lower_closure(params, body),
            ExprKind::Cast(e, ty) => {
                let operand = self.lower_expr(e)?;
                let cty = type_to_c(ty, &self.aliases);
                let t = self.fresh_temp(c_scalar_name(&cty));
                self.emit(
                    t.clone(),
                    mir::Rvalue::Cast(operand, c_scalar_name(&cty)),
                );
                Ok(mir::Operand::var(t))
            }
            ExprKind::Macro(m) if macro_name(m) == "vec" => self.lower_vec_macro(e, m),
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
            ExprKind::Try(inner) => self.lower_try(inner),
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
        if matches!(local.pat, ast::Pat::Variant(..) | ast::Pat::Path(_)) {
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
                    self.func
                        .locals
                        .push((name.clone(), Some("long long".into())));
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
        let init_op = match &local.init {
            Some(e) => self.lower_expr(e)?,
            None => mir::Operand::Unit,
        };
        // Variant/literal patterns (`if let Err(e) = r`) are supported via the
        // same discriminant test the `match` lowering uses; identifier patterns
        // are irrefutable and always taken.
        let (cond, binds) = self.pattern_guard(&init_op, &local.pat)?;
        let new_binds: Vec<(String, Option<String>)> = binds
            .iter()
            .filter(|(n, _)| !self.func.locals.iter().any(|(en, _)| en == n))
            .map(|(n, _)| (n.clone(), Some("long long".into())))
            .collect();
        self.func.locals.extend(new_binds);

        // Lower as a conditional on the pattern guard (always taken when the
        // pattern is irrefutable).
        let result = self.fresh_temp("long long");
        let then_blk = self.new_block();
        let else_blk = self.new_block();
        let merge = self.new_block();
        let cond = cond.unwrap_or(mir::Operand::Const(1));
        self.set_term(mir::Terminator::If(cond, then_blk, else_blk));

        self.current = then_blk;
        for (n, val) in &binds {
            self.emit(n.clone(), mir::Rvalue::Use(val.clone()));
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
        Ok(mir::Operand::var(result))
    }

    /// Lower `while let pat = init { body }` (parsed as `While(Block{Local}, ..)`).
    /// The binding is emitted once before the loop (its initializer does not
    /// change across iterations for an identifier pattern) and the loop runs
    /// until a `break` inside the body.
    fn lower_while_let(&mut self, local: &ast::Local, body: &ast::Block) -> Result<mir::Operand> {
        let init_op = match &local.init {
            Some(e) => self.lower_expr(e)?,
            None => mir::Operand::Unit,
        };
        // The pattern guard (possibly a variant discriminant test) is
        // re-evaluated at the loop head each iteration; the initializer itself
        // runs once.
        let head = self.new_block();
        let body_blk = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let (cond, binds) = self.pattern_guard(&init_op, &local.pat)?;
        let cond = cond.unwrap_or(mir::Operand::Const(1));
        self.set_term(mir::Terminator::If(cond, body_blk, exit));

        self.current = body_blk;
        for (n, val) in binds {
            if !self.func.locals.iter().any(|(en, _)| en == &n) {
                self.func.locals.push((n.clone(), Some("long long".into())));
            }
            self.emit(n, mir::Rvalue::Use(val));
        }
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
    fn lower_match(&mut self, scrutinee: &Expr, arms: &[(ast::Pat, Expr)]) -> Result<mir::Operand> {
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
                    self.func
                        .locals
                        .push((name.clone(), Some("long long".into())));
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
    /// Branch to a panic when `cond` is false; execution continues in the ok block.
    fn lower_panic_unless(&mut self, cond: mir::Operand) {
        let ok = self.new_block();
        let fail = self.new_block();
        self.set_term(mir::Terminator::If(cond, ok, fail));
        self.current = fail;
        self.emit_panic_call();
        self.set_term(mir::Terminator::Goto(ok));
        self.current = ok;
    }

    /// Call the runtime panic helper (prints a message and exits).
    /// Lower `vec![a, b, ..]` or `vec![value; count]` to a vector built by pushes.
    /// The element type comes from the checker's hint for the macro's `Vec<T>`.
    fn lower_vec_macro(&mut self, e: &Expr, m: &ast::MacroCall) -> Result<mir::Operand> {
        let elem = self
            .hint_of_expr(e)
            .and_then(|h| mir::vec_elem(&h).map(|s| s.to_string()))
            .unwrap_or_else(|| "long long".to_string());
        let out = self.fresh_temp(mir::vec_type(&elem));
        self.emit(
            out.clone(),
            mir::Rvalue::Call(mir::Operand::Func(mir::vec_fn_name(&elem, "new")), vec![]),
        );
        // `vec![value; count]`: split at the top-level `;`.
        let mut depth = 0i32;
        let mut split: Option<usize> = None;
        for (i, t) in m.tokens.iter().enumerate() {
            match t.kind {
                tpt_moira_clotho::token::TokenKind::LParen
                | tpt_moira_clotho::token::TokenKind::LBracket
                | tpt_moira_clotho::token::TokenKind::LBrace => depth += 1,
                tpt_moira_clotho::token::TokenKind::RParen
                | tpt_moira_clotho::token::TokenKind::RBracket
                | tpt_moira_clotho::token::TokenKind::RBrace => depth -= 1,
                tpt_moira_clotho::token::TokenKind::Semicolon if depth == 0 => {
                    split = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let pushes: Vec<(mir::Operand, usize)> = match split {
            Some(i) => {
                let value_toks = m.tokens[..i].to_vec();
                let count_toks = m.tokens[i + 1..].to_vec();
                let value = tpt_moira_clotho::parser::Parser::segment(value_toks)
                    .parse_expr()
                    .map_err(|_| LowerError("`vec![value; count]`: bad value".into()))?;
                let count = tpt_moira_clotho::parser::Parser::segment(count_toks)
                    .parse_expr()
                    .map_err(|_| LowerError("`vec![value; count]`: bad count".into()))?;
                let v = self.lower_expr(&value)?;
                let n = self.lower_expr(&count)?;
                // Loop `count` times, pushing the value each time.
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
                    mir::Rvalue::Binary(mir::BinOp::Lt, mir::Operand::var(idx.clone()), n),
                );
                self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));
                self.current = body_blk;
                self.emit_vec_push(&out, &elem, v);
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
                Vec::new()
            }
            None => {
                let items = tpt_moira_clotho::parser::Parser::parse_macro_args(m.tokens.clone());
                let mut out_ops = Vec::new();
                for it in &items {
                    out_ops.push((self.lower_expr(it)?, 0));
                }
                out_ops
            }
        };
        for (op, _) in pushes {
            self.emit_vec_push(&out, &elem, op);
        }
        Ok(mir::Operand::var(out))
    }

    /// Append `value` to the vector `out` (a local of type `Vec<elem>`).
    fn emit_vec_push(&mut self, out: &str, elem: &str, value: mir::Operand) {
        let ptr = self.fresh_temp(format!("{}*", mir::vec_type(elem)));
        self.emit(ptr.clone(), mir::Rvalue::Ref(mir::Operand::var(out), true));
        let pushed = self.fresh_temp("long long");
        self.emit(
            pushed,
            mir::Rvalue::Call(
                mir::Operand::Func(mir::vec_fn_name(elem, "push")),
                vec![mir::Operand::var(ptr), value],
            ),
        );
    }

    fn emit_panic_call(&mut self) {
        let t = self.fresh_temp("long long");
        self.emit(
            t,
            mir::Rvalue::Call(mir::Operand::Func("__moira_panic".to_string()), Vec::new()),
        );
    }

    fn lower_print_macro(&mut self, m: &ast::MacroCall) -> Result<mir::Operand> {
        let name = m
            .path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default();
        let args = tpt_moira_clotho::parser::Parser::parse_macro_args(m.tokens.clone());
        match name.as_str() {
            // `assert!(cond)` panics when `cond` is false; any message arguments are
            // not formatted (the runtime prints a fixed panic line).
            "assert" | "debug_assert" => {
                let cond = args
                    .first()
                    .ok_or_else(|| LowerError("assert! needs a condition".to_string()))?;
                let c = self.lower_expr(cond)?;
                self.lower_panic_unless(c);
                Ok(mir::Operand::Unit)
            }
            "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" => {
                if args.len() < 2 {
                    return Err(LowerError(format!("{name}! needs two operands")));
                }
                let a = self.lower_expr(&args[0])?;
                let b = self.lower_expr(&args[1])?;
                let ok = self.fresh_temp("long long");
                let op = if name.ends_with("_eq") {
                    mir::BinOp::Eq
                } else {
                    mir::BinOp::Ne
                };
                self.emit(ok.clone(), mir::Rvalue::Binary(op, a, b));
                self.lower_panic_unless(mir::Operand::var(ok));
                Ok(mir::Operand::Unit)
            }
            // `matches!(expr, pattern)`: a pattern test on the scrutinee.
            "matches" => {
                let (scrut_e, pat) =
                    match tpt_moira_clotho::parser::Parser::parse_matches_args(m.tokens.clone()) {
                        Some(parts) => parts,
                        None => {
                            return Err(LowerError("`matches!` needs `(expr, pattern)`".into()))
                        }
                    };
                let scrut = self.lower_expr(&scrut_e)?;
                let (cond, _) = self.pattern_guard(&scrut, &pat)?;
                Ok(cond.unwrap_or(mir::Operand::Const(1)))
            }
            "panic" | "unreachable" | "todo" | "unimplemented" | "debug_unreachable" => {
                for a in &args {
                    self.lower_expr(a)?;
                }
                self.emit_panic_call();
                Ok(mir::Operand::Unit)
            }
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
                        let printer = match self
                            .temp_type_of(match &op {
                                mir::Operand::Var(n) => n.as_str(),
                                _ => "",
                            })
                            .as_str()
                        {
                            t if t.contains("char*") => "print_str",
                            "str" => "print_str",
                            "bool" => "print_bool",
                            "double" => "print_f64",
                            _ => "print_i64",
                        };
                        let seg_idx = i + 1;
                        let after = segments.get(seg_idx).cloned().unwrap_or_default();
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Call(mir::Operand::Func(printer.to_string()), vec![op]),
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
    /// Give an untyped operand the enum type `ename`, learned from a variant
    /// pattern matched against it (its own type is otherwise unknown, and the
    /// tag/payload reads need a struct). Operands with a known type are left alone.
    /// The enum variant a path names. A path qualified by its enum (`Enum::Variant`)
    /// finds that enum's variant, so equal variant names in two enums do not clash.
    fn variant_info(&self, path: &ast::Path) -> Option<(String, i128, usize)> {
        let n = path.segments.len();
        if n >= 2 {
            let key = format!(
                "{}::{}",
                path.segments[n - 2].node,
                path.segments[n - 1].node
            );
            if let Some(v) = self.qualified_variants.get(&key) {
                return Some(v.clone());
            }
        }
        let last = path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default();
        self.variants.get(&last).cloned()
    }

    /// Whether a scrutinee is a pointer (a match on `&Enum` reads fields with `->`).
    fn scrutinee_is_pointer(&self, op: &mir::Operand) -> bool {
        match op {
            mir::Operand::Var(n) => self.temp_type_of(n).ends_with('*'),
            _ => false,
        }
    }

    fn type_enum_operand(&mut self, op: &mir::Operand, ename: &str) {
        let name = match op {
            mir::Operand::Var(n) => n.clone(),
            _ => return,
        };
        if self.temp_type_of(&name) != "long long" {
            return;
        }
        self.types.insert(name.clone(), ename.to_string());
        for (ln, lt) in self.func.locals.iter_mut() {
            if *ln == name && lt.as_deref().map_or(true, |t| t == "long long") {
                *lt = Some(ename.to_string());
            }
        }
    }

    fn variant_guard(
        &mut self,
        scrut: &mir::Operand,
        path: &ast::Path,
        subpats: &[ast::Pat],
    ) -> Result<(Option<mir::Operand>, Vec<(String, mir::Operand)>)> {
        let scrut_ptr = self.scrutinee_is_pointer(scrut);
        let vname = path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default();
        let Some((ename, tag, _arity)) = self.variant_info(path) else {
            return Err(LowerError(format!(
                "unknown enum variant `{}` in pattern",
                vname
            )));
        };
        self.type_enum_operand(scrut, &ename);
        // Guard: scrut.tag == tag.
        let tag_val = self.fresh_temp("long long");
        self.emit(
            tag_val.clone(),
            mir::Rvalue::Field(scrut.clone(), "tag".to_string(), scrut_ptr),
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
                mir::Rvalue::Field(scrut.clone(), format!("f{}", i), scrut_ptr),
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
        let scrut_ptr = self.scrutinee_is_pointer(scrut);
        match pat {
            ast::Pat::Ident(n) => {
                // A bare name that is a known *nullary* variant (`None`) is a
                // tag test, not a binding — the parser cannot distinguish it
                // from an identifier pattern.
                if let Some((_, tag, 0)) = self.variants.get(&n.node).cloned() {
                    let tag_val = self.fresh_temp("long long");
                    self.emit(
                        tag_val.clone(),
                        mir::Rvalue::Field(scrut.clone(), "tag".to_string(), scrut_ptr),
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
                Ok((None, vec![(n.node.clone(), scrut.clone())]))
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
                        self.emit(t.clone(), mir::Rvalue::Binary(mir::BinOp::Or, x, y));
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
                        self.emit(t.clone(), mir::Rvalue::Binary(op, scrut.clone(), hi_op));
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
                            mir::Rvalue::Field(scrut.clone(), format!("f{i}"), scrut_ptr),
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
                if let Some((ename, tag, 0)) = self.variant_info(path) {
                    self.type_enum_operand(scrut, &ename);
                    let tag_val = self.fresh_temp("long long");
                    self.emit(
                        tag_val.clone(),
                        mir::Rvalue::Field(scrut.clone(), "tag".to_string(), scrut_ptr),
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
                    Err(LowerError(format!(
                        "this match pattern is not supported in the codegen subset: {}",
                        pat_brief(pat)
                    )))
                }
            }
            // `Name { field: pat, .. }`: a struct has no tag, so the pattern tests only
            // its field subpatterns (each on the field's value), and binds their names.
            ast::Pat::Struct(_, fields) => {
                let ptr = self.scrutinee_is_pointer(scrut);
                let mut cond: Option<mir::Operand> = None;
                let mut binds = Vec::new();
                for (fname, sub) in fields {
                    let fv = self.fresh_temp("long long");
                    self.emit(
                        fv.clone(),
                        mir::Rvalue::Field(scrut.clone(), fname.clone(), ptr),
                    );
                    let (c, b) = self.pattern_guard(&mir::Operand::var(fv), sub)?;
                    binds.extend(b);
                    if let Some(c) = c {
                        cond = Some(match cond {
                            None => c,
                            Some(prev) => {
                                let t = self.fresh_temp("long long");
                                self.emit(t.clone(), mir::Rvalue::Binary(mir::BinOp::And, prev, c));
                                mir::Operand::var(t)
                            }
                        });
                    }
                }
                Ok((cond, binds))
            }
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
fn collect_locals(b: &ast::Block, out: &mut Vec<(String, String)>, aliases: &AliasTable) {
    for s in &b.stmts {
        if let StmtKind::Local(local) = &s.kind {
            if let Ok(name) = pat_name(&local.pat) {
                let ty = local
                    .ty
                    .as_ref()
                    .map(|t| type_to_c(t, aliases))
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
    aliases: &AliasTable,
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
        let ty = type_to_c(&p.ty, aliases);
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
fn build_ctor_wrapper(f: &ast::Function, fut_struct: &str, aliases: &AliasTable) -> mir::Function {
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
        let ty = type_to_c(&p.ty, aliases);
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
        // `&x` binds the referenced value; the lowering copies it, as for `x`.
        Pat::Ref(inner) => pat_name(inner),
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
