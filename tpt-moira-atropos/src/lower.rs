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
        || hint.starts_with("Option<")
        || hint.starts_with("Result<")
        || hint.starts_with("Map<")
        || hint.starts_with("Set<")
        || hint.starts_with("SortedSet<")
        || hint.starts_with("Tuple<")
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
    lower_program_with_enums(prog, hints, &[])
}

/// Lower `prog`, which may match on the enums of `extra` programs (other crates of a
/// bundle). Those enums' variants are registered, but their structs are not emitted:
/// the program that defines them emits them.
pub fn lower_program_with_enums(
    prog: &Program,
    hints: &TypeHints,
    extra: &[&Program],
) -> Result<mir::MirProgram> {
    let shared = std::sync::Arc::new(hints.clone());
    // The enum items of `prog`, then those of `extra`.
    let mut enum_items: Vec<&ast::Item> = prog.items.iter().collect();
    let mut extra_enums: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in extra {
        for it in &p.items {
            if let ItemKind::Enum(e) = &it.kind {
                if !prog
                    .items
                    .iter()
                    .any(|own| matches!(&own.kind, ItemKind::Enum(o) if o.name.node == e.name.node))
                {
                    extra_enums.insert(e.name.node.clone());
                }
            }
            enum_items.push(it);
        }
    }
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
    for item in &enum_items {
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
    // User enums: the C type of each variant's payload fields, by enum and tag. A
    // generic parameter of the enum has no C type here, so such a field stays `long long`.
    let mut payload_tys: HashMap<(String, i128), Vec<String>> = HashMap::new();
    for item in &enum_items {
        if let ItemKind::Enum(e) = &item.kind {
            let params: Vec<String> = e
                .generics
                .params
                .iter()
                .map(|p| p.name.node.clone())
                .collect();
            for (tag, v) in e.variants.iter().enumerate() {
                let tys: Vec<String> = v
                    .fields
                    .as_ref()
                    .map(|fs| {
                        fs.iter()
                            .map(|f| {
                                let c = type_to_c(&f.ty, &aliases);
                                if mentions_any(&c, &params) {
                                    "long long".to_string()
                                } else {
                                    c
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                payload_tys.insert((e.name.node.clone(), tag as i128), tys);
            }
        }
    }
    // Struct-like variants: the field names of each, in declaration order.
    let mut variant_fields: HashMap<(String, i128), Vec<String>> = HashMap::new();
    for item in &enum_items {
        if let ItemKind::Enum(e) = &item.kind {
            for (tag, v) in e.variants.iter().enumerate() {
                // A tuple variant's fields are unnamed; only struct-like variants have names.
                if let Some(fs) = v
                    .fields
                    .as_ref()
                    .filter(|fs| !fs.is_empty() && fs.iter().all(|f| !f.name.node.is_empty()))
                {
                    variant_fields.insert(
                        (e.name.node.clone(), tag as i128),
                        fs.iter().map(|f| f.name.node.clone()).collect(),
                    );
                }
            }
        }
    }
    // A struct literal or pattern naming a struct is not a variant, even when a
    // variant shares its name.
    let struct_names: std::collections::HashSet<String> = prog
        .items
        .iter()
        .chain(extra.iter().flat_map(|p| p.items.iter()))
        .filter_map(|it| match &it.kind {
            ItemKind::Struct(s) => Some(s.name.node.clone()),
            _ => None,
        })
        .collect();
    // The methods of every `impl` block (this crate's and the extra crates'): a method
    // `m` of type `T` lowers to the function `T__m`.
    let mut methods: HashMap<(String, String), (String, u8, String)> = HashMap::new();
    add_impl_methods(&prog.items, &aliases, &mut methods);
    for p in extra {
        add_impl_methods(&p.items, &aliases, &mut methods);
    }
    // The C parameter types of every top-level function and impl method.
    let mut fn_param_tys: HashMap<String, Vec<String>> = HashMap::new();
    add_fn_params(&prog.items, &aliases, &mut fn_param_tys);
    for p in extra {
        add_fn_params(&p.items, &aliases, &mut fn_param_tys);
    }
    // The top-level functions (not async), so a call passing a capturing closure can specialise them.
    let fn_asts: std::sync::Arc<HashMap<String, ast::Function>> = std::sync::Arc::new(
        prog.items
            .iter()
            .filter_map(|it| match &it.kind {
                ItemKind::Function(f) if !f.is_async => Some((f.name.node.clone(), f.clone())),
                _ => None,
            })
            .collect(),
    );
    let mut enum_structs: Vec<mir::StructDef> = Vec::new();
    let mut variants: HashMap<String, (String, i128, usize)> = HashMap::new();
    let mut qualified: HashMap<String, (String, i128, usize)> = HashMap::new();
    for (ename, vars) in &enum_defs {
        let max_arity = vars.iter().map(|(_, a)| *a).max().unwrap_or(0);
        let mut fields = vec![("tag".to_string(), "long long".to_string())];
        if is_prelude_enum(ename) {
            for i in 0..max_arity {
                fields.push((format!("f{}", i), "long long".to_string()));
            }
            if ename == "Result" {
                fields.push(("e0".to_string(), "long long".to_string()));
            }
        } else {
            // A user enum has a typed slot per payload field of every variant.
            for (tag, (_, arity)) in vars.iter().enumerate() {
                for i in 0..*arity {
                    let ty = payload_tys
                        .get(&(ename.clone(), tag as i128))
                        .and_then(|v| v.get(i))
                        .cloned()
                        .unwrap_or_else(|| "long long".to_string());
                    fields.push((payload_field(ename, tag as i128, i), ty));
                }
            }
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
        if extra_enums.contains(&st.name) {
            continue;
        }
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
            // Each method is a function `Type__method`; `Self` names the impl's type.
            ItemKind::Impl(imp) => {
                let Some(ty) = impl_type_name(&imp.for_ty) else {
                    continue;
                };
                let impl_aliases = aliases.with_alias("Self", imp.for_ty.clone());
                for sub in &imp.items {
                    let ItemKind::Function(f) = &sub.kind else {
                        continue;
                    };
                    // A method with its own type parameters is emitted as its instances only.
                    if !f.generics.params.is_empty() || sub.is_cfg_test() {
                        continue;
                    }
                    // A trait method whose signature names a type the subset does not
                    // model (`&mut Formatter`) is not emitted.
                    if !imp.trait_name.node.is_empty() && !signature_is_modeled(f, &struct_names) {
                        continue;
                    }
                    let mut g = f.clone();
                    g.name.node = format!("{ty}__{}", f.name.node);
                    let mut l = Lower::new(&out.structs, shared.clone());
                    l.async_fns = async_names.clone();
                    l.all_fn_names = all_fn_names.clone();
                    l.const_values = const_values.clone();
                    l.variants = variants.clone();
                    l.qualified_variants = qualified.clone();
                    l.payload_tys = payload_tys.clone();
                    l.variant_fields = variant_fields.clone();
                    l.struct_names = struct_names.clone();
                    l.methods = methods.clone();
                    l.fn_param_tys = fn_param_tys.clone();
                    l.self_ty = Some(ty.clone());
                    l.aliases = impl_aliases.clone();
                    let func = match l.lower_function(&g) {
                        Ok(func) => func,
                        // A trait method outside the codegen subset (e.g. one that uses
                        // `write!`) is not emitted; the inherent methods must lower.
                        Err(_) if !imp.trait_name.node.is_empty() => continue,
                        Err(e) => return Err(in_fn(&g.name.node, e)),
                    };
                    let tuple_structs: Vec<mir::StructDef> =
                        l.tuple_structs.values().cloned().collect();
                    out.functions.push(func);
                    for c in l.closures.drain(..) {
                        out.functions.push(c);
                    }
                    out.functions.extend(l.specs.drain(..));
                    out.structs.extend(tuple_structs);
                }
            }
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
                    l.payload_tys = payload_tys.clone();
                    l.variant_fields = variant_fields.clone();
                    l.struct_names = struct_names.clone();
                    l.methods = methods.clone();
                    l.fn_param_tys = fn_param_tys.clone();
                    l.fn_asts = fn_asts.clone();
                    l.aliases = aliases.clone();
                    let func = l.lower_function(f).map_err(|e| in_fn(&f.name.node, e))?;
                    let tuple_structs: Vec<mir::StructDef> =
                        l.tuple_structs.values().cloned().collect();
                    out.functions.push(func);
                    for f in l.closures.drain(..) {
                        out.functions.push(f);
                    }
                    out.functions.extend(l.specs.drain(..));
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
    /// `v.drain(..)`: the sequence is emptied after the loop.
    drain: bool,
    /// `s.char_indices()`: each item is `(byte index, byte)`.
    indices: bool,
    /// `s.lines()`: each item is one line of the string (without its newline).
    lines: bool,
}

fn seq_iter(iter: &Expr) -> SeqIter<'_> {
    let mut e = iter;
    let mut si = SeqIter {
        seq: iter,
        enumerate: false,
        rev: false,
        mutable: false,
        chars: false,
        drain: false,
        indices: false,
        lines: false,
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
        if m.node == "drain" && args.len() == 1 {
            si.drain = true;
            e = &**r;
        }
    }
    if let ExprKind::MethodCall(r, m, args) = &e.kind {
        if m.node == "lines" && args.is_empty() {
            si.lines = true;
            e = &**r;
        }
    }
    if let ExprKind::MethodCall(r, m, args) = &e.kind {
        if (m.node == "chars" || m.node == "bytes" || m.node == "char_indices") && args.is_empty() {
            si.chars = true;
            si.indices = m.node == "char_indices";
            e = &**r;
        }
    }
    if let ExprKind::MethodCall(r, m, args) = &e.kind {
        if (m.node == "iter" || m.node == "iter_mut" || m.node == "into_iter") && args.is_empty() {
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
    /// A string source `s.chars()` / `s.bytes()`: its items are its characters' codes.
    chars: Option<&'a Expr>,
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

/// Whether a struct is one of the prelude enums, whose payload slots stay untyped
/// (`f{i}` of `long long`) because their types depend on the instantiation.
fn is_prelude_enum(name: &str) -> bool {
    name == "Option" || name == "Result"
}

/// The struct field holding payload `i` of variant `tag` of enum `ename`. User enums
/// name the slot by variant; the prelude enums keep the uniform `f{i}`.
fn payload_field(ename: &str, tag: i128, i: usize) -> String {
    if ename == "Result" && tag == 1 {
        // `Result`'s error payload has its own slot, so `Result<T, E>` can type it.
        format!("e{i}")
    } else if is_prelude_enum(ename) {
        format!("f{i}")
    } else {
        format!("v{tag}_{i}")
    }
}

/// Whether the C type string `ty` names any of `names` as an identifier.
fn mentions_any(ty: &str, names: &[String]) -> bool {
    ty.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|tok| names.iter().any(|n| n == tok))
}

/// Which part of a map a `for` loop yields.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MapIter {
    /// `(key, value)` pairs (`&m`, `m.iter()`), or the keys of a set.
    Pairs,
    /// `m.keys()`.
    Keys,
    /// `m.values()`.
    Values,
}

/// The key and value types of a `Map<K,V>` type string.
fn map_args(ty: &str) -> Option<(String, String)> {
    match mir::split_generic(ty) {
        Some(("Map", a)) | Some(("Sorted", a)) if a.len() == 2 => {
            Some((a[0].to_string(), a[1].to_string()))
        }
        // A set is a map to `bool`.
        Some(("Set", a)) | Some(("SortedSet", a)) if a.len() == 1 => {
            Some((a[0].to_string(), "bool".to_string()))
        }
        _ => None,
    }
}

/// Whether `e` is `HashMap::new()` or `HashMap::with_capacity(..)`.
fn is_hashmap_ctor(e: &Expr) -> bool {
    if let ExprKind::Call(callee, _) = &e.kind {
        if let ExprKind::Path(p) = &callee.kind {
            let segs: Vec<&str> = p.segments.iter().map(|s| s.node.as_str()).collect();
            return segs.len() == 2
                && (segs[0] == "HashMap"
                    || segs[0] == "HashSet"
                    || segs[0] == "BTreeMap"
                    || segs[0] == "BTreeSet")
                && (segs[1] == "new" || segs[1] == "with_capacity");
        }
    }
    false
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
/// Record the C parameter types of the functions and impl methods in `items`.
fn add_fn_params(
    items: &[ast::Item],
    aliases: &AliasTable,
    out: &mut HashMap<String, Vec<String>>,
) {
    for it in items {
        match &it.kind {
            ItemKind::Function(f) => {
                let tys = f.params.iter().map(|p| type_to_c(&p.ty, aliases)).collect();
                out.insert(f.name.node.clone(), tys);
            }
            ItemKind::Impl(imp) => {
                if let Some(ty) = impl_type_name(&imp.for_ty) {
                    let impl_aliases = aliases.with_alias("Self", imp.for_ty.clone());
                    for sub in &imp.items {
                        if let ItemKind::Function(f) = &sub.kind {
                            if !f.generics.params.is_empty() {
                                continue;
                            }
                            let tys = f
                                .params
                                .iter()
                                .map(|p| type_to_c(&p.ty, &impl_aliases))
                                .collect();
                            out.insert(format!("{ty}__{}", f.name.node), tys);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Record the methods of the `impl` blocks in `items` (see `Lower::methods`).
fn add_impl_methods(
    items: &[ast::Item],
    aliases: &AliasTable,
    methods: &mut HashMap<(String, String), (String, u8, String)>,
) {
    for it in items {
        if let ItemKind::Impl(imp) = &it.kind {
            if let Some(ty) = impl_type_name(&imp.for_ty) {
                let impl_aliases = aliases.with_alias("Self", imp.for_ty.clone());
                for sub in &imp.items {
                    if let ItemKind::Function(f) = &sub.kind {
                        if !f.generics.params.is_empty() {
                            continue;
                        }
                        let ret = match &f.ret {
                            Some(t) => type_to_c(t, &impl_aliases),
                            None => "void".to_string(),
                        };
                        methods.insert(
                            (ty.clone(), f.name.node.clone()),
                            (format!("{ty}__{}", f.name.node), self_kind(f), ret),
                        );
                    }
                }
            }
        }
    }
}

/// Whether every type in a function's signature is one the subset models: a primitive,
/// a struct or enum of the program, a generic parameter, or a std container it knows.
fn signature_is_modeled(
    f: &ast::Function,
    struct_names: &std::collections::HashSet<String>,
) -> bool {
    const KNOWN: &[&str] = &[
        "bool", "char", "str", "String", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16",
        "u32", "u64", "u128", "usize", "f32", "f64", "Self", "Option", "Result", "Vec", "HashMap",
        "HashSet", "BTreeMap", "BTreeSet", "Box",
    ];
    fn known(
        t: &ast::Type,
        struct_names: &std::collections::HashSet<String>,
        generics: &[String],
    ) -> bool {
        match &t.kind {
            ast::TypeKind::Path(p, args) => {
                let name = p.segments.last().map(|s| s.node.as_str()).unwrap_or("");
                (KNOWN.contains(&name)
                    || struct_names.contains(name)
                    || generics.iter().any(|g| g == name))
                    && args.iter().all(|a| known(a, struct_names, generics))
            }
            ast::TypeKind::Ref(i) | ast::TypeKind::RefMut(i) | ast::TypeKind::Slice(i) => {
                known(i, struct_names, generics)
            }
            ast::TypeKind::Array(i, _) => known(i, struct_names, generics),
            ast::TypeKind::Tuple(ts) => ts.iter().all(|t| known(t, struct_names, generics)),
            ast::TypeKind::Infer | ast::TypeKind::Never => true,
            ast::TypeKind::Future(_) => false,
        }
    }
    let generics: Vec<String> = f
        .generics
        .params
        .iter()
        .map(|g| g.name.node.clone())
        .collect();
    f.params
        .iter()
        .all(|p| known(&p.ty, struct_names, &generics))
        && f.ret
            .as_ref()
            .map_or(true, |t| known(t, struct_names, &generics))
}

/// Rename the variables of a block (its statements and terminator) by `map`.
fn rename_block(block: &mut mir::Block, map: &HashMap<String, String>) {
    for stmt in block.stmts.iter_mut() {
        if let Some(n) = map.get(&stmt.place) {
            stmt.place = n.clone();
        }
        rename_rvalue(&mut stmt.rvalue, map);
    }
    match &mut block.term {
        mir::Terminator::If(op, _, _) | mir::Terminator::Switch(op, _, _) => rename_op(op, map),
        mir::Terminator::Return(Some(op)) => rename_op(op, map),
        _ => {}
    }
}

fn rename_op(op: &mut mir::Operand, map: &HashMap<String, String>) {
    if let mir::Operand::Var(n) = op {
        if let Some(m) = map.get(n) {
            *n = m.clone();
        }
    }
}

fn rename_rvalue(rv: &mut mir::Rvalue, map: &HashMap<String, String>) {
    use mir::Rvalue as R;
    match rv {
        R::Use(a)
        | R::Unary(_, a)
        | R::Field(a, _, _)
        | R::FieldAddr(a, _, _)
        | R::Ref(a, _)
        | R::Cast(a, _) => {
            rename_op(a, map)
        }
        R::Binary(_, a, b) | R::Index(a, b) | R::IndexRef(a, b) => {
            rename_op(a, map);
            rename_op(b, map);
        }
        R::Call(c, args) => {
            rename_op(c, map);
            for a in args.iter_mut() {
                rename_op(a, map);
            }
        }
        R::StructLit(_, fields) => {
            for (_, a) in fields.iter_mut() {
                rename_op(a, map);
            }
        }
        R::StoreIndex(a, b, c) => {
            rename_op(a, map);
            rename_op(b, map);
            rename_op(c, map);
        }
        R::StoreField(a, _, b, _) => {
            rename_op(a, map);
            rename_op(b, map);
        }
        R::Array(_, ops) | R::Tuple(_, ops) => {
            for a in ops.iter_mut() {
                rename_op(a, map);
            }
        }
    }
}

/// Whether `e` is the bare name `self`.
fn is_self_path(e: &Expr) -> bool {
    matches!(&e.kind, ExprKind::Path(p) if p.segments.len() == 1 && p.segments[0].node == "self")
}

/// The scope of a loop's pattern bindings (see `open_pattern_scope`).
struct PatternScope {
    /// The block the bindings are made in (the body's first block).
    cur: usize,
    /// The first block created after the bindings: the body's own blocks start here.
    created_from: usize,
    renames: HashMap<String, String>,
    saved: Vec<(String, Option<String>)>,
}

/// How a sort orders two items: the items themselves, a key of each item, or a comparator
/// that returns a positive number when its first item comes after its second.
enum SortCmp {
    Natural,
    Key(String, Vec<mir::Operand>),
    Pair(String, Vec<mir::Operand>),
}

/// The name of the type an `impl` block is for (its last path segment).
fn impl_type_name(t: &ast::Type) -> Option<String> {
    match &t.kind {
        ast::TypeKind::Path(p, _) => p.segments.last().map(|s| s.node.clone()),
        _ => None,
    }
}

/// How a method takes its receiver: 0 for an associated function (no `self`), 1 for
/// `&self` / `&mut self`, 2 for `self` by value.
fn self_kind(f: &ast::Function) -> u8 {
    match f.params.first() {
        Some(p) if matches!(&p.pat, ast::Pat::Ident(n) if n.node == "self") => match &p.ty.kind {
            ast::TypeKind::Ref(_) | ast::TypeKind::RefMut(_) => 1,
            _ => 2,
        },
        _ => 0,
    }
}

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
            let arg = |i: usize| type_to_c_expanded(&args[i]);
            match (last.as_deref(), args.len()) {
                (Some("Vec"), 1) => mir::vec_type(&arg(0)),
                // A box is a heap pointer to its value (recursive types need it).
                (Some("Box"), 1) => format!("{}*", arg(0)),
                // `impl Into<String>` / `impl AsRef<str>` parameters take the named type.
                (Some("Into" | "AsRef"), 1) => arg(0),
                // The parser drops the arguments of `impl Trait<..>`; a bare `Into`/`AsRef`
                // parameter is a string conversion in this crate (`impl Into<String>`).
                (Some("Into" | "AsRef"), 0) => "str".to_string(),
                (Some("Option"), 1) => format!("Option<{}>", arg(0)),
                (Some("Result"), 2) => format!("Result<{},{}>", arg(0), arg(1)),
                (Some("HashMap"), 2) => format!("Map<{},{}>", arg(0), arg(1)),
                // A set is a map to `bool` (keys only).
                (Some("HashSet"), 1) => format!("Map<{},bool>", arg(0)),
                // The sorted collections are sorted maps (a set maps to `bool`).
                (Some("BTreeMap"), 2) => format!("Sorted<{},{}>", arg(0), arg(1)),
                (Some("BTreeSet"), 1) => format!("Sorted<{},bool>", arg(0)),
                // `String` and `str` are one C type (`char*`).
                (Some("String"), 0) => "str".to_string(),
                // A function-pointer type `fn(A, B) -> R`: its arguments are the parameters
                // then the result, and every one is a `long long` (see `func_ptr_c_type`).
                // A function type keeps its signature: `Fn<params..,result>`, which the C
                // backend writes as a function pointer.
                (Some("fn"), n) if n >= 1 => {
                    let parts: Vec<String> = (0..n).map(|i| arg(i)).collect();
                    format!("Fn<{}>", parts.join(","))
                }
                (Some("fn"), n) => func_ptr_c_type(n.saturating_sub(1)),
                _ => last.unwrap_or_else(|| "long long".to_string()),
            }
        }
        ast::TypeKind::Tuple(elems) if !elems.is_empty() => {
            let parts: Vec<String> = elems.iter().map(type_to_c_expanded).collect();
            format!("Tuple<{}>", parts.join(","))
        }
        ast::TypeKind::Ref(inner) | ast::TypeKind::RefMut(inner) => {
            // A reference to a slice is the slice value itself.
            let c = type_to_c_expanded(inner);
            // A string is already a pointer (`char*`): a reference to it is the same.
            if mir::slice_elem(&c).is_some() || c == "str" {
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
    /// The C element type of each `let`-bound array literal (an enum for an array of
    /// variants, else `long long`), so a `for` over it binds elements of that type.
    array_elems: HashMap<String, String>,
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
    /// The C type of each user-enum payload field, by `(enum, variant tag)`.
    payload_tys: HashMap<(String, i128), Vec<String>>,
    /// The field names of each struct-like variant, by `(enum, variant tag)`, in
    /// declaration order (the payload slots are positional).
    variant_fields: HashMap<(String, i128), Vec<String>>,
    /// The item types the next closure lifted takes as its parameters (set by the
    /// iterator chain that applies it, consumed by `lower_closure`).
    closure_param_hint: Option<Vec<String>>,
    /// The result type of the closure literal being lowered, from its `Fn` hint.
    closure_ret_hint: Option<String>,
    /// A counter that makes the names of match-arm bindings unique.
    arm_counter: usize,
    /// The C name of each variable that a `let` has given a new name (by its Rust name).
    var_renames: HashMap<String, String>,
    /// Counter for the names of `let` bindings that shadow an existing variable.
    let_counter: usize,
    /// The previous `var_renames` entries of each open pattern scope (restored on close).
    renames_stack: Vec<Vec<(String, Option<String>)>>,
    /// The C return type of each closure lifted so far (by its lifted function name).
    closure_rets: HashMap<String, String>,
    /// The C type a value is expected to have, when its context decides it (the element
    /// type of a `vec!` item). Consumed by the first tuple lowered under it.
    expect_ty: Option<String>,
    /// The C parameter types of each top-level function and impl method, by C name.
    fn_param_tys: HashMap<String, Vec<String>>,
    /// The methods of every `impl` block: `(type, method) -> (C function name, kind)`,
    /// kind 0 for an associated function, 1 for a `&self` method, 2 for `self`.
    methods: HashMap<(String, String), (String, u8, String)>,
    /// The type of the `impl` block being lowered (what `Self` names), if any.
    self_ty: Option<String>,
    /// The names of the structs in the program and its extra crates.
    struct_names: std::collections::HashSet<String>,
    /// Captured variables per lambda-lifted closure (lifted fn name ->
    /// captured local names, in the order they appear as leading parameters).
    closure_captures: HashMap<String, Vec<String>>,
    /// Local variables bound to capturing closures: local name -> lifted fn
    /// name. Used to rewrite direct calls `f(args)` into
    /// `__moira_closure_N(captures…, args)`.
    closure_bindings: HashMap<String, String>,
    /// The top-level non-async functions by name (specialised for capturing closure arguments).
    fn_asts: std::sync::Arc<HashMap<String, ast::Function>>,
    /// The specialised copies already lowered or in progress, by name.
    spec_done: std::collections::HashSet<String>,
    /// Lowered specialised copies, emitted with the program.
    specs: Vec<mir::Function>,
    /// Captured variables (name and C type) that a specialised copy takes as leading parameters.
    leading_caps: Vec<(String, String)>,
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
        // A generic component (`Tuple<usize,usize>`) names its instance struct.
        let t = if t.contains('<') {
            mir::inst_c_name(t)
        } else {
            t.clone()
        };
        name.push_str(t.replace(' ', "_").replace('*', "p").as_str());
    }
    name
}

/// Whether a hint names a pointer to a scalar (`i64*`, `bool*`, ...).
fn is_scalar_pointer_hint(h: &str) -> bool {
    match h.strip_suffix('*') {
        Some(base) => matches!(
            base,
            "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64"
                | "u128" | "usize" | "f32" | "f64" | "bool" | "char"
        ),
        None => false,
    }
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
                // Every name a parameter pattern binds (a tuple pattern binds several).
                bind_pat_names_in(&p.pat, &mut b2);
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
            array_elems: HashMap::new(),
            const_values: HashMap::new(),
            variants: HashMap::new(),
            qualified_variants: HashMap::new(),
            payload_tys: HashMap::new(),
            variant_fields: HashMap::new(),
            methods: HashMap::new(),
            fn_param_tys: HashMap::new(),
            self_ty: None,
            expect_ty: None,
            closure_rets: HashMap::new(),
            arm_counter: 0,
            var_renames: HashMap::new(),
            let_counter: 0,
            renames_stack: Vec::new(),
            closure_param_hint: None,
            closure_ret_hint: None,
            struct_names: std::collections::HashSet::new(),
            closure_captures: HashMap::new(),
            closure_bindings: HashMap::new(),
            fn_asts: std::sync::Arc::new(HashMap::new()),
            spec_done: std::collections::HashSet::new(),
            specs: Vec::new(),
            leading_caps: Vec::new(),
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
        // The captured variables of a specialised copy are its leading parameters.
        for (cap, ty) in self.leading_caps.clone() {
            self.func.params.push(cap.clone());
            self.func.locals.push((cap.clone(), Some(ty.clone())));
            if ty != "long long" {
                self.types.insert(cap, ty);
            }
        }
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
        let mut await_points: Vec<Expr> = Vec::new();
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
        let resume_blocks: Vec<usize> = self
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
        // A `let` in the block scopes its names to the block.
        let saved = self.var_renames.clone();
        let mut result = Ok(());
        for s in &b.stmts {
            result = self.lower_stmt(s);
            if result.is_err() {
                break;
            }
        }
        let value = match result {
            Err(e) => Err(e),
            Ok(()) => match &b.tail {
                Some(tail) => self.lower_expr(tail),
                None => Ok(mir::Operand::Unit),
            },
        };
        self.var_renames = saved;
        value
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
    fn lower_closure(&mut self, params: &Vec<ast::Param>, body: &Expr) -> Result<mir::Operand> {
        let param_tys_hint: Vec<String> = self.closure_param_hint.take().unwrap_or_default();
        let captures = self.literal_captures(params, body)?;

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
        sub.payload_tys = self.payload_tys.clone();
        sub.variant_fields = self.variant_fields.clone();
        sub.methods = self.methods.clone();
        sub.fn_param_tys = self.fn_param_tys.clone();
        sub.closure_rets = self.closure_rets.clone();
        sub.self_ty = self.self_ty.clone();
        sub.struct_names = self.struct_names.clone();
        sub.const_values = self.const_values.clone();
        sub.closure_counter = self.closure_counter;
        sub.fn_asts = self.fn_asts.clone();
        sub.spec_done = self.spec_done.clone();
        sub.func.name = name.clone();
        // A closure returns the type its body has (a `long long` unless the checker
        // says otherwise, e.g. a `String` built by `to_string()`).
        // The body's own type, or the closure's declared result (its `Fn` hint), which
        // names an aggregate the body alone does not (`Err(vec![..])`).
        let declared_ret = self.closure_ret_hint.take();
        let body_ret = self
            .hint_of_expr(body)
            .filter(|h| h != "long long" && !h.is_empty() && h != "Result" && h != "Option")
            .or(declared_ret.filter(|h| h != "long long" && !h.is_empty()))
            .unwrap_or_else(|| "long long".to_string());
        self.closure_rets.insert(name.clone(), body_ret.clone());
        sub.func.ret = Some(body_ret);
        // Captured variables become leading `long long` parameters.
        for cap in &captures {
            // A capture has the type of the variable it captures.
            let cap_ty = match self.types.get(cap).cloned() {
                Some(t) => t,
                None => self
                    .func
                    .locals
                    .iter()
                    .find(|(n, _)| n == cap)
                    .and_then(|(_, t)| t.clone())
                    .unwrap_or_else(|| "long long".to_string()),
            };
            sub.func.params.push(cap.clone());
            sub.func.locals.push((cap.clone(), Some(cap_ty.clone())));
            if cap_ty != "long long" {
                sub.types.insert(cap.clone(), cap_ty);
            }
        }
        // The item types the closure is applied to (set by the iterator chain that uses it).
        let param_tys: Vec<String> = param_tys_hint;
        for (k, p) in params.iter().enumerate() {
            // A parameter names one value, typed by the hint when there is one; a tuple
            // pattern (`|(i, x)|`) binds several untyped names.
            let names_of_p: Vec<String> = closure_param_names(&p.pat)?;
            let ty = match (names_of_p.len(), param_tys.get(k)) {
                (1, Some(t)) => t.clone(),
                _ => "long long".to_string(),
            };
            for n in names_of_p {
                sub.func.params.push(n.clone());
                sub.func.locals.push((n.clone(), Some(ty.clone())));
                if ty != "long long" {
                    sub.types.insert(n, ty.clone());
                }
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
        self.closures.extend(sub.closures.drain(..));
        self.specs.extend(sub.specs.drain(..));
        self.spec_done = sub.spec_done.clone();
        // Record the captures so call sites through a bound local can pass them.
        self.closure_captures.insert(name.clone(), captures);
        Ok(mir::Operand::Func(name))
    }

    /// The captured variables of a closure literal: the names its body uses that are
    /// variables of the enclosing function.
    fn literal_captures(&self, params: &[ast::Param], body: &Expr) -> Result<Vec<String>> {
        let mut param_names = std::collections::HashSet::new();
        for p in params {
            for n in closure_param_names(&p.pat)? {
                param_names.insert(n);
            }
        }
        // Structural capture analysis: a single-segment path name that is neither a
        // closure parameter nor a function is a captured variable. Variant constructors
        // (`Some`, `None`) are not captured variables either.
        let mut not_captured = self.all_fn_names.clone();
        not_captured.extend(self.variants.keys().cloned());
        Ok(closure_captures(body, &param_names, &not_captured))
    }

    /// A call to a top-level function that passes a capturing closure (a local bound to
    /// one, or a closure literal). The callee is lowered once per closure as a copy that
    /// takes the captured variables as leading parameters and calls the closure's lifted
    /// function directly, so the captures never go through a function pointer.
    fn lower_specialized_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<Option<mir::Operand>> {
        let ExprKind::Path(p) = &callee.kind else {
            return Ok(None);
        };
        if p.segments.len() != 1 {
            return Ok(None);
        }
        let fname = p.segments[0].node.clone();
        let Some(f) = self.fn_asts.get(&fname).cloned() else {
            return Ok(None);
        };
        if f.params.len() != args.len() {
            return Ok(None);
        }
        // The capturing closure argument: its position and its lifted function.
        let mut found: Option<(usize, String)> = None;
        for (i, a) in args.iter().enumerate() {
            let lifted = match &a.kind {
                ExprKind::Path(ap) if ap.segments.len() == 1 => {
                    self.closure_bindings.get(&ap.segments[0].node).cloned()
                }
                ExprKind::Closure { params, body, .. } => {
                    if self.literal_captures(params, body)?.is_empty() {
                        None
                    } else {
                        match self.lower_closure(params, body)? {
                            mir::Operand::Func(n) => Some(n),
                            _ => None,
                        }
                    }
                }
                _ => None,
            };
            if let Some(lifted) = lifted {
                if found.is_some() {
                    return Err(LowerError(
                        "a call passes more than one capturing closure (not supported)".into(),
                    ));
                }
                found = Some((i, lifted));
            }
        }
        let Some((pidx, lifted)) = found else {
            return Ok(None);
        };
        let caps = self.closure_captures.get(&lifted).cloned().unwrap_or_default();
        let spec_name = format!("{fname}__with_{lifted}");
        if !self.spec_done.contains(&spec_name) {
            self.spec_done.insert(spec_name.clone());
            self.build_spec(&f, pidx, &lifted, &caps, &spec_name)?;
        }
        let mut operands = Vec::new();
        for c in &caps {
            operands.push(mir::Operand::var(self.var_name(c)));
        }
        for (i, a) in args.iter().enumerate() {
            if i != pidx {
                operands.push(self.lower_expr(a)?);
            }
        }
        let ret = self
            .closure_rets
            .get(&spec_name)
            .cloned()
            .unwrap_or_else(|| "long long".to_string());
        let t = self.fresh_temp(ret);
        self.emit(t.clone(), mir::Rvalue::Call(mir::Operand::Func(spec_name), operands));
        Ok(Some(mir::Operand::var(t)))
    }

    /// Lower the copy of `f` for one capturing closure (`lifted`, capturing `caps`) that
    /// is passed as parameter `pidx`. The closure parameter is bound to `lifted` in the copy.
    fn build_spec(
        &mut self,
        f: &ast::Function,
        pidx: usize,
        lifted: &str,
        caps: &[String],
        spec_name: &str,
    ) -> Result<()> {
        let param_name = pat_name(&f.params[pidx].pat)?;
        let mut g = f.clone();
        g.name.node = spec_name.to_string();
        g.params.remove(pidx);
        let mut leading = Vec::new();
        for cap in caps {
            let cap_ty = match self.types.get(cap).cloned() {
                Some(t) => t,
                None => self
                    .func
                    .locals
                    .iter()
                    .find(|(n, _)| n == cap)
                    .and_then(|(_, t)| t.clone())
                    .unwrap_or_else(|| "long long".to_string()),
            };
            leading.push((cap.clone(), cap_ty));
        }
        let mut l = Lower::new(&self.structs, self.hints.clone());
        l.async_fns = self.async_fns.clone();
        l.all_fn_names = self.all_fn_names.clone();
        l.aliases = self.aliases.clone();
        l.variants = self.variants.clone();
        l.qualified_variants = self.qualified_variants.clone();
        l.payload_tys = self.payload_tys.clone();
        l.variant_fields = self.variant_fields.clone();
        l.methods = self.methods.clone();
        l.fn_param_tys = self.fn_param_tys.clone();
        l.closure_rets = self.closure_rets.clone();
        l.struct_names = self.struct_names.clone();
        l.const_values = self.const_values.clone();
        l.closure_captures = self.closure_captures.clone();
        l.closure_bindings.insert(param_name, lifted.to_string());
        l.fn_asts = self.fn_asts.clone();
        l.spec_done = self.spec_done.clone();
        l.closure_counter = self.closure_counter;
        l.leading_caps = leading;
        let func = l.lower_function(&g)?;
        self.closure_counter = l.closure_counter;
        self.spec_done = l.spec_done;
        self.closures.extend(l.closures.drain(..));
        self.specs.extend(l.specs.drain(..));
        self.closure_rets.insert(
            spec_name.to_string(),
            func.ret.clone().unwrap_or_else(|| "long long".to_string()),
        );
        self.specs.push(func);
        Ok(())
    }

    /// `let x = init;` where `x` is already a variable: the initializer is lowered under the
    /// old `x`, and the new `x` is a variable of its own with a new C name.
    fn lower_rebinding_let(&mut self, plain: &str, local: &ast::Local) -> Result<()> {
        let init = local.init.as_ref().expect("checked by the caller");
        let v = self.lower_expr(init)?;
        let ty = match &local.ty {
            Some(t) => type_to_c(t, &self.aliases),
            None => {
                let actual = match &v {
                    mir::Operand::Var(n) => self.temp_type_of(n),
                    mir::Operand::CStr(_) => "char*".to_string(),
                    _ => "long long".to_string(),
                };
                if actual != "long long" {
                    actual
                } else {
                    match self.hint_of_expr(init) {
                        Some(h)
                            if h == "str"
                                || self.struct_names.contains(&h)
                                || ["Option<", "Result<", "Vec<", "Map<", "Sorted<", "Set<", "SortedSet<"]
                                    .iter()
                                    .any(|p| h.starts_with(p)) =>
                        {
                            h
                        }
                        _ => "long long".to_string(),
                    }
                }
            }
        };
        let unique = format!("{plain}__let{}", self.let_counter);
        self.let_counter += 1;
        self.func.locals.push((unique.clone(), Some(ty.clone())));
        self.types.insert(unique.clone(), ty);
        self.emit(unique.clone(), mir::Rvalue::Use(v));
        self.var_renames.insert(plain.to_string(), unique);
        Ok(())
    }

    fn lower_stmt(&mut self, s: &Stmt) -> Result<()> {
        match &s.kind {
            StmtKind::Local(local) => {
                // Tuple destructuring `let (a, b, ..) = init;` binds each
                // identifier in the pattern to the matching tuple component.
                if matches!(local.pat, Pat::Tuple(_)) {
                    return self.lower_tuple_destructure(local);
                }
                // A `let` that declares a name already in use in this function gets a new C
                // name, so its own type does not clash with the earlier variable.
                if let (Pat::Ident(n), Some(init)) = (&local.pat, &local.init) {
                    let shadows = self.func.locals.iter().any(|(l, _)| *l == n.node);
                    let plain_init =
                        !matches!(init.kind, ExprKind::Closure { .. } | ExprKind::Array(_));
                    if shadows && plain_init && self.async_ctx.is_none() {
                        return self.lower_rebinding_let(&n.node, local);
                    }
                }
                // `let Some(x) = e;` (a refutable pattern, the `else` branch dropped by the
                // parser): the payload is read out of the value and bound to its names.
                if matches!(local.pat, ast::Pat::Variant(..) | ast::Pat::Struct(..)) {
                    return self.lower_refutable_let(local);
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
                    let elem_ct = self.array_elem_ct(elems);
                    self.array_lens.insert(name.clone(), elems.len());
                    self.array_elems.insert(name.clone(), elem_ct.clone());
                    self.emit(name.clone(), mir::Rvalue::Array(elem_ct, operands));
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
                            // A struct-like variant is typed by its enum.
                            let segs: Vec<&str> =
                                path.segments.iter().map(|s| s.node.as_str()).collect();
                            match self.struct_variant_of(&segs) {
                                Some((ename, _, _)) => Some(ename),
                                None => path.segments.last().map(|s| s.node.clone()),
                            }
                        }
                        // A variant construction typed by the checker as an instance ().
                        _ if self.variant_c_type(init).is_some()
                            && self.hint_of_expr(init).map_or(false, |h| {
                                mir::split_generic(&h)
                                    .map_or(false, |(b, _)| b == "Option" || b == "Result")
                            }) =>
                        {
                            self.hint_of_expr(init)
                        }
                        _ if self.variant_c_type(init).is_some() => {
                            // A variant construction (`let o = Some(5);`) types
                            // the local as the enum's tagged struct.
                            self.variant_c_type(init)
                        }
                        ExprKind::Closure { params, .. } => {
                            // A closure value is a function pointer with the signature the
                            // checker gave it (`Fn<..>`), matching the lambda-lifted function.
                            match self.hint_of_expr(init) {
                                Some(h) if h.starts_with("Fn<") => Some(h),
                                _ => Some(func_ptr_c_type(params.len())),
                            }
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
                        // A compound value (`let f = v.first();`, `let v = Vec::new();`)
                        // is typed by the checker's hint for its initializer.
                        _ if self.user_call_ret(init).is_some() => self.user_call_ret(init),
                        // A local bound to a variable that holds a pointer (`let q = p;` with
                        // `p: &i64`) is a pointer too.
                        _ if self.pointer_local_type(init).is_some() => self.pointer_local_type(init),
                        _ => self.hint_of_expr(init).filter(|h| {
                            h == "str"
                                || is_scalar_pointer_hint(h)
                                || self.struct_names.contains(h)
                                || ["Option<", "Result<", "Vec<", "Map<", "Sorted<", "Set<", "SortedSet<"]
                                    .iter()
                                    .any(|p| h.starts_with(p))
                        }),
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
                    // `let m: HashMap<K, V> = HashMap::new()`: the declared type names the map.
                    if let (Some((k, v)), true) =
                        (ty.as_deref().and_then(map_args), is_hashmap_ctor(init))
                    {
                        let t = self.fresh_temp(format!("Map<{k},{v}>"));
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Call(
                                mir::Operand::Func(mir::map_fn_name(&k, &v, "new")),
                                vec![],
                            ),
                        );
                        self.emit(name.clone(), mir::Rvalue::Use(mir::Operand::var(t)));
                        return Ok(());
                    }
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
                                // Calls through the local pass the captures, so the local
                                // holds no function pointer (its C type has no capture slots).
                                self.closure_bindings.insert(name.clone(), lifted.clone());
                                return Ok(());
                            }
                        }
                    }
                    // An unannotated `let` takes the type of its initializer when that
                    // is known (a slice or a vector), so later uses can index it.
                    if ty.is_none() {
                        if let mir::Operand::Var(src) = &v {
                            let t = self.temp_type_of(src);
                            if mir::slice_elem(&t).is_some()
                                || mir::vec_elem(&t).is_some()
                                || t == "str"
                            {
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
                .or_else(|| match mir::split_generic(&base_ty) {
                    Some(("Tuple", args)) => args.get(i).map(|s| s.to_string()),
                    _ => None,
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
        // The return type may be an instance (`Result<i32, str>`); its base names the enum.
        let ret_base = match mir::split_generic(&ret) {
            Some((base, _)) => base.to_string(),
            None => ret.clone(),
        };
        let err_variant = match ret_base.as_str() {
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
        // The error value is returned as is when the function returns the same
        // instance; otherwise a value of the return type carries the error (`e0`).
        let op_ty = self.temp_type_of(&name);
        if op_ty == ret || ret == ret_base && op_ty == ret_base {
            self.set_term(mir::Terminator::Return(Some(mir::Operand::var(
                name.clone(),
            ))));
        } else {
            let mut fields = vec![("tag".to_string(), mir::Operand::Const(err_tag))];
            if ret_base == "Result" {
                let e = self.fresh_temp("long long");
                self.emit(
                    e.clone(),
                    mir::Rvalue::Field(mir::Operand::var(name.clone()), "e0".into(), false),
                );
                fields.push(("e0".to_string(), mir::Operand::var(e)));
            }
            let t = self.fresh_temp(ret.clone());
            let lit_name = if mir::split_generic(&ret).is_some() {
                mir::inst_c_name(&ret)
            } else {
                ret_base.clone()
            };
            self.emit(t.clone(), mir::Rvalue::StructLit(lit_name, fields));
            self.set_term(mir::Terminator::Return(Some(mir::Operand::var(t))));
        }
        self.current = ok_blk;
        let op_ty = self.temp_type_of(&name);
        let payload_ty = match mir::split_generic(&op_ty) {
            Some((_, args)) if !args.is_empty() => args[0].to_string(),
            _ => "long long".to_string(),
        };
        let payload = self.fresh_temp(payload_ty);
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
        // A string slice `&s[a..b]` is a new NUL-terminated copy of those bytes.
        let base_str = match &base.kind {
            ExprKind::Path(p) if p.segments.len() == 1 => {
                let n = p.segments[0].node.clone();
                self.temp_type_of(&n) == "str" || self.hint_of_expr(base).as_deref() == Some("str")
            }
            _ => self.hint_of_expr(base).as_deref() == Some("str"),
        };
        if base_str {
            let s = match self.lower_expr(base)? {
                mir::Operand::Var(n) => n,
                _ => return Err(LowerError("a string slice needs a named string".into())),
            };
            let lo = match start {
                Some(e) => self.lower_expr(e)?,
                None => mir::Operand::Const(0),
            };
            let hi = match end {
                Some(e) => {
                    let h = self.lower_expr(e)?;
                    if inclusive {
                        let t = self.fresh_temp("long long");
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Binary(mir::BinOp::Add, h, mir::Operand::Const(1)),
                        );
                        mir::Operand::var(t)
                    } else {
                        h
                    }
                }
                None => {
                    let t = self.fresh_temp("long long");
                    self.emit(
                        t.clone(),
                        mir::Rvalue::Call(
                            mir::Operand::Func("strlen".into()),
                            vec![mir::Operand::var(s.clone())],
                        ),
                    );
                    mir::Operand::var(t)
                }
            };
            let t = self.fresh_temp("str");
            self.emit(
                t.clone(),
                mir::Rvalue::Call(
                    mir::Operand::Func("moira_str_sub".into()),
                    vec![mir::Operand::var(s), lo, hi],
                ),
            );
            return Ok(mir::Operand::var(t));
        }
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
        let name = self.var_name(&p.segments[0].node);
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
                    chars: None,
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
                            chars: None,
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
                // `s.chars()` / `s.bytes()` on a string: a source of characters.
                "chars" | "bytes"
                    if args.is_empty() && self.hint_of_expr(&**r).as_deref() == Some("str") =>
                {
                    stages.reverse();
                    return Some(IterChain {
                        seq: None,
                        chars: Some(&**r),
                        range: None,
                        stages,
                    });
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
            // An associated function used as the adapter's function (`map(T::f)`).
            ExprKind::Path(p) if p.segments.len() >= 2 => {
                // `mir::Operand::var`: the type and the function are its last two segments.
                let n = p.segments.len();
                let mut segs: Vec<String> = vec![
                    p.segments[n - 2].node.clone(),
                    p.segments[n - 1].node.clone(),
                ];
                if segs[0] == "Self" {
                    segs[0] = self.self_ty.clone().unwrap_or_default();
                }
                match self.methods.get(&(segs[0].clone(), segs[1].clone())) {
                    Some((cname, _, _)) => cname.clone(),
                    None => {
                        return Err(LowerError(format!(
                            "`{}::{}` is not a closure or a function",
                            segs[0], segs[1]
                        )))
                    }
                }
            }
            _ => match self.lower_expr(arg)? {
                mir::Operand::Func(name) => name,
                _ => {
                    return Err(LowerError(format!(
                        "iterator adapters take a closure (argument at bytes {}..{})",
                        arg.span.start, arg.span.end
                    )))
                }
            },
        };
        let caps = self
            .closure_captures
            .get(&lifted)
            .cloned()
            .unwrap_or_default();
        let mut cap_ops = Vec::new();
        for c in caps {
            cap_ops.push(mir::Operand::var(self.var_name(&c)));
        }
        Ok((lifted, cap_ops))
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
            (None, None) => match chain.chars {
                Some(recv) => Ok(self.chars_to_vec(recv)?),
                None => return Err(LowerError("empty iterator chain".into())),
            },
        };
        let elem = match &source {
            Ok((_, elem, _)) => elem.clone(),
            Err(_) => "long long".to_string(),
        };
        // Each closure takes the item that reaches it: the source's element, then what the
        // previous stage produced (a `map`'s result type; an `enumerate` pair is untyped).
        let mut cur = elem.clone();
        let mut stage_callees: Vec<Option<(String, Vec<mir::Operand>)>> = Vec::new();
        for (name, arg) in &chain.stages {
            let callee = match arg {
                Some(a) => {
                    self.closure_param_hint = Some(vec![cur.clone()]);
                    let r = self.closure_callee(a);
                    self.closure_param_hint = None;
                    Some(r?)
                }
                None => None,
            };
            match (name.as_str(), &callee) {
                ("map", Some((f, _))) => {
                    cur = self
                        .closure_rets
                        .get(f)
                        .cloned()
                        .unwrap_or_else(|| "long long".to_string())
                }
                ("enumerate", _) => cur = "long long".to_string(),
                _ => {}
            }
            stage_callees.push(callee);
        }
        let term_callee = match term_args.first() {
            Some(a) => {
                self.closure_param_hint = Some(vec![cur.clone()]);
                let r = self.closure_callee(a);
                self.closure_param_hint = None;
                Some(r?)
            }
            None => None,
        };
        if matches!(terminal, "any" | "all" | "find") && term_callee.is_none() {
            return Err(LowerError(format!("`{terminal}` needs a closure")));
        }

        // The result, initialised before the loop.
        // `collect()` into a `String` (the checker's type for the chain is `str`).
        let string_collect =
            terminal == "collect" && self.hint_of_expr(e).as_deref() == Some("str");
        // `collect()` into a `HashSet<K>` (`Map<K,bool>`) or a `HashMap<K,V>` (`Map<K,V>`).
        let map_collect: Option<(String, String)> = if terminal == "collect" {
            self.hint_of_expr(e).and_then(|h| map_args(&h))
        } else {
            None
        };
        let out_elem = self
            .hint_of_expr(e)
            .and_then(|h| mir::vec_elem(&h).map(|s| s.to_string()))
            .unwrap_or_else(|| elem.clone());
        let mut out: Option<String> = None;
        let mut res: Option<String> = None;
        match terminal {
            "collect" if map_collect.is_some() => {
                let (k, v) = map_collect.clone().unwrap();
                let m = self.fresh_temp(format!("Map<{k},{v}>"));
                self.emit(
                    m.clone(),
                    mir::Rvalue::Call(mir::Operand::Func(mir::map_fn_name(&k, &v, "new")), vec![]),
                );
                out = Some(m);
            }
            "collect" if string_collect => {
                // `collect()` into a `String`: an empty heap string that the characters are
                // appended to.
                let v = self.heap_string(mir::Operand::CStr(String::new()))?;
                out = Some(match v {
                    mir::Operand::Var(n) => n,
                    _ => return Err(LowerError("`collect` into a string".into())),
                });
            }
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
                let r = self.fresh_temp(format!("Option<{elem}>"));
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
                    let args =
                        self.chain_args(caps, &pos, &item, arg.expect("map has a closure"))?;
                    let ret = self
                        .closure_rets
                        .get(&f)
                        .cloned()
                        .unwrap_or_else(|| "long long".to_string());
                    let r = self.fresh_temp(ret);
                    self.emit(r.clone(), mir::Rvalue::Call(mir::Operand::Func(f), args));
                    item = r;
                    pos = None;
                }
                "filter" => {
                    let (f, caps) = callee.expect("filter has a closure");
                    let args =
                        self.chain_args(caps, &pos, &item, arg.expect("filter has a closure"))?;
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
            "collect" if map_collect.is_some() => {
                let (k, v) = map_collect.clone().unwrap();
                let m = out.clone().expect("collect has a map");
                let ptr = self.fresh_temp(format!("Map<{k},{v}>*"));
                self.emit(ptr.clone(), mir::Rvalue::Ref(mir::Operand::var(m), true));
                let (key, val) = if v == "bool" {
                    (mir::Operand::var(item.clone()), mir::Operand::Const(1))
                } else {
                    let kf = self.fresh_temp(k.clone());
                    self.emit(
                        kf.clone(),
                        mir::Rvalue::Field(mir::Operand::var(item.clone()), "f0".into(), false),
                    );
                    let vf = self.fresh_temp(v.clone());
                    self.emit(
                        vf.clone(),
                        mir::Rvalue::Field(mir::Operand::var(item.clone()), "f1".into(), false),
                    );
                    (mir::Operand::var(kf), mir::Operand::var(vf))
                };
                // `insert` returns the value it replaced, as an `Option`.
                let inserted = self.fresh_temp(format!("Option<{v}>"));
                self.emit(
                    inserted,
                    mir::Rvalue::Call(
                        mir::Operand::Func(mir::map_fn_name(&k, &v, "insert")),
                        vec![mir::Operand::var(ptr), key, val],
                    ),
                );
                self.set_term(mir::Terminator::Goto(step));
            }
            "collect" if string_collect => {
                let v = out.clone().expect("collect has a string");
                let ptr = self.fresh_temp("char**");
                self.emit(ptr.clone(), mir::Rvalue::Ref(mir::Operand::var(v), true));
                let pushed = self.fresh_temp("long long");
                self.emit(
                    pushed,
                    mir::Rvalue::Call(
                        mir::Operand::Func("moira_str_push_char".to_string()),
                        vec![mir::Operand::var(ptr), mir::Operand::var(item.clone())],
                    ),
                );
                self.set_term(mir::Terminator::Goto(step));
            }
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
                let args = self.chain_args(caps, &pos, &item, arg)?;
                let t = self.fresh_temp("long long");
                self.emit(t, mir::Rvalue::Call(mir::Operand::Func(f), args));
                self.set_term(mir::Terminator::Goto(step));
            }
            _ => {
                // any / all / find: the closure decides, and a hit leaves the loop.
                let (f, caps) = term_callee.clone().expect("checked above");
                let arg = term_args.first().expect("checked above");
                let args = self.chain_args(caps, &pos, &item, arg)?;
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
            "collect" => mir::Operand::var(out.expect("collect has a result")),
            "for_each" => mir::Operand::Unit,
            _ => mir::Operand::var(res.expect("has a result")),
        })
    }

    /// Lower `for c in s.chars()` (or `bytes()`) over a string as an index loop
    /// bounded by `strlen`. The items are bytes, which is exact for ASCII text.
    /// `for line in s.lines()`: walks the string line by line. Each step copies one line
    /// (without its `\n`, nor a `\r` before it) through the runtime helpers.
    fn lower_lines_for(
        &mut self,
        seq: &Expr,
        pat: &ast::Pat,
        body: &ast::Block,
    ) -> Result<mir::Operand> {
        let s = match self.lower_expr(seq)? {
            mir::Operand::Var(n) => n,
            _ => return Err(LowerError("`lines()` needs a named string".into())),
        };
        let idx = self.fresh_temp("long long");
        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
        let next = self.fresh_temp("long long");
        let head = self.new_block();
        let body_blk = self.new_block();
        let step = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        // `while s[idx] != 0`.
        self.current = head;
        let ch = self.fresh_temp("char");
        self.emit(
            ch.clone(),
            mir::Rvalue::Index(mir::Operand::var(s.clone()), mir::Operand::var(idx.clone())),
        );
        let more = self.fresh_temp("long long");
        self.emit(
            more.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Ne,
                mir::Operand::var(ch),
                mir::Operand::Const(0),
            ),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(more), body_blk, exit));

        self.current = body_blk;
        self.emit(
            next.clone(),
            mir::Rvalue::Call(
                mir::Operand::Func("moira_line_next".to_string()),
                vec![mir::Operand::var(s.clone()), mir::Operand::var(idx.clone())],
            ),
        );
        let line = self.fresh_temp("char*");
        self.emit(
            line.clone(),
            mir::Rvalue::Call(
                mir::Operand::Func("moira_line_text".to_string()),
                vec![
                    mir::Operand::var(s),
                    mir::Operand::var(idx.clone()),
                    mir::Operand::var(next.clone()),
                ],
            ),
        );
        self.lower_bind_pattern(pat, mir::Operand::var(line), "char*")?;
        self.loops.push((exit, step));
        self.lower_block_value(body)?;
        self.loops.pop();
        self.set_term(mir::Terminator::Goto(step));

        self.current = step;
        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::var(next)));
        self.set_term(mir::Terminator::Goto(head));

        self.current = exit;
        Ok(mir::Operand::Unit)
    }

    fn lower_str_for(
        &mut self,
        seq: &Expr,
        pat: &ast::Pat,
        body: &ast::Block,
        indices: bool,
    ) -> Result<mir::Operand> {
        // `char_indices()`: the pattern is `(index, byte)`.
        let (ip, cp) = match (indices, pat) {
            (true, ast::Pat::Tuple(ps)) if ps.len() == 2 => (Some(&ps[0]), &ps[1]),
            (true, _) => {
                return Err(LowerError(
                    "`char_indices` needs an `(index, char)` pattern".into(),
                ))
            }
            (false, other) => (None, other),
        };
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
        if let Some(ip) = ip {
            self.lower_bind_pattern(ip, mir::Operand::var(idx.clone()), "long long")?;
        }
        self.lower_bind_pattern(cp, mir::Operand::var(ch), "char")?;
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

    /// The map a receiver names: a local (or parameter) of type `Map<K,V>` or
    /// `&mut Map<K,V>`, as `(name, K, V, is_pointer)`.
    fn map_local(&self, e: &Expr) -> Option<(String, String, String, bool)> {
        let p = match &e.kind {
            ExprKind::Path(p) if p.segments.len() == 1 => p,
            _ => return None,
        };
        let name = self.var_name(&p.segments[0].node);
        let ty = self.types.get(&name).cloned().or_else(|| {
            self.func
                .locals
                .iter()
                .find(|(n, _)| *n == name)
                .and_then(|(_, t)| t.clone())
        })?;
        if let Some((k, v)) = map_args(&ty) {
            return Some((name, k, v, false));
        }
        let base = ty.strip_suffix('*')?;
        map_args(base).map(|(k, v)| (name, k, v, true))
    }

    /// A map the checker types (a field, a call result): lowered, and typed by its hint.
    fn map_from_hint(&mut self, e: &Expr) -> Result<Option<(String, String, String, bool)>> {
        let hint = match self.hint_of_expr(e) {
            Some(h) if h.starts_with("Map<") || h.starts_with("Sorted<") || h.starts_with("Set<") || h.starts_with("SortedSet<") => h,
            _ => return Ok(None),
        };
        let (k, v) = match map_args(&hint) {
            Some(kv) => kv,
            None => return Ok(None),
        };
        let name = match self.lower_expr(e)? {
            mir::Operand::Var(n) => n,
            _ => return Err(LowerError("a map must be a named value".into())),
        };
        if self.temp_type_of(&name) == "long long" {
            self.types.insert(name.clone(), hint.clone());
        }
        let is_ptr = self.temp_type_of(&name).ends_with('*');
        Ok(Some((name, k, v, is_ptr)))
    }

    /// Whether a map expression is a sorted collection (a `BTreeMap` or `BTreeSet`).
    fn is_sorted_expr(&self, e: &Expr) -> bool {
        let by_hint = self
            .hint_of_expr(e)
            .map_or(false, |h| h.starts_with("Sorted<") || h.starts_with("SortedSet<"));
        let by_local = match &e.kind {
            ExprKind::Path(p) if p.segments.len() == 1 => {
                let name = &p.segments[0].node;
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
                    .map_or(false, |t| t.starts_with("Sorted<") || t.starts_with("SortedSet<"))
            }
            _ => false,
        };
        by_hint || by_local
    }

    /// A key argument of a map method: `&k` passes `k` itself.
    fn lower_key_arg(&mut self, e: &Expr, key_ty: &str) -> Result<mir::Operand> {
        // The key has the map's key type (a tuple literal takes it).
        self.expect_ty = Some(key_ty.to_string());
        let r = match &e.kind {
            ExprKind::Ref(inner, _) => self.lower_expr(inner),
            _ => self.lower_expr(e),
        };
        self.expect_ty = None;
        r
    }

    /// `m.insert(k, v)`, `m.get(&k)`, `m.contains_key(&k)`, `m.len()`, `m.is_empty()`
    /// on a map local: calls to the generated helpers. `None` for other methods.
    fn lower_map_method(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<mir::Operand>> {
        let (mname, k, v, is_ptr) = match self.map_local(recv) {
            Some(m) => m,
            // A map read from a field (): the read-only methods work on a
            // copy of the map, which shares its storage.
            None if matches!(recv.kind, ExprKind::Field(..))
                && matches!(method, "get" | "contains_key" | "len" | "is_empty") =>
            {
                match self.map_from_hint(recv)? {
                    Some(m) => m,
                    None => return Ok(None),
                }
            }
            None => return Ok(None),
        };
        let (helper, ret_ty) = match (method, args.len()) {
            // A set's `insert(x)` and `contains(&x)` are the map's with the value `true`.
            ("insert", 1) => ("insert", format!("Option<{v}>")),
            ("insert", 2) => ("insert", format!("Option<{v}>")),
            ("get", 1) => ("get", format!("Option<{v}>")),
            ("contains" | "contains_key", 1) => ("contains_key", "long long".to_string()),
            ("len", 0) => ("len", "long long".to_string()),
            ("is_empty", 0) => ("is_empty", "bool".to_string()),
            _ => return Ok(None),
        };
        let recv_ptr = if is_ptr {
            mir::Operand::var(mname.clone())
        } else {
            let p = self.fresh_temp(format!("Map<{k},{v}>*"));
            self.emit(
                p.clone(),
                mir::Rvalue::Ref(mir::Operand::var(mname.clone()), true),
            );
            mir::Operand::var(p)
        };
        let mut ops = vec![recv_ptr];
        match helper {
            "insert" => {
                ops.push(self.lower_key_arg(&args[0], &k)?);
                if args.len() == 2 {
                    self.expect_ty = Some(v.clone());
                    let value = self.lower_expr(&args[1]);
                    self.expect_ty = None;
                    ops.push(value?);
                } else {
                    ops.push(mir::Operand::Const(1));
                }
            }
            "get" | "contains_key" => ops.push(self.lower_key_arg(&args[0], &k)?),
            _ => {}
        }
        let t = self.fresh_temp(ret_ty);
        self.emit(
            t.clone(),
            mir::Rvalue::Call(mir::Operand::Func(mir::map_fn_name(&k, &v, helper)), ops),
        );
        Ok(Some(mir::Operand::var(t)))
    }

    /// `for (k, v) in &m` (or `m.iter()`): an index loop over the map's keys and values.
    fn lower_map_for(
        &mut self,
        mname: &str,
        k: &str,
        v: &str,
        is_ptr: bool,
        pat: &ast::Pat,
        body: &ast::Block,
        which: MapIter,
        sorted: bool,
    ) -> Result<mir::Operand> {
        // A set (`HashSet<K>`, a map to `bool`) iterates its keys: a single pattern.
        // `values()` binds each value; `keys()` and a set bind each key; a map's
        // `(key, value)` pattern binds both.
        // A set (its value type is `bool`) yields its keys, whatever the pattern.
        let is_set = v == "bool";
        let (kp, vp) = match (which, pat) {
            (MapIter::Values, other) => (other, None),
            (MapIter::Keys, other) => (other, None),
            (MapIter::Pairs, ast::Pat::Tuple(ps)) if ps.len() == 2 && !is_set => {
                (&ps[0], Some(&ps[1]))
            }
            (MapIter::Pairs, other) => (other, None),
        };
        let values_only = which == MapIter::Values;
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(mname), "len".into(), is_ptr),
        );
        let keys = self.fresh_temp(format!("{k}*"));
        self.emit(
            keys.clone(),
            mir::Rvalue::Field(mir::Operand::var(mname), "keys".into(), is_ptr),
        );
        let vals = self.fresh_temp(format!("{v}*"));
        self.emit(
            vals.clone(),
            mir::Rvalue::Field(mir::Operand::var(mname), "vals".into(), is_ptr),
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
        // A sorted map (`BTreeMap`, `BTreeSet`) yields its entries by ascending key: the
        // slot read is the position of the `idx`-th smallest key.
        let at = if sorted {
            let ptr = if is_ptr {
                mir::Operand::var(mname)
            } else {
                let p = self.fresh_temp(format!("Map<{k},{v}>*"));
                self.emit(p.clone(), mir::Rvalue::Ref(mir::Operand::var(mname), true));
                mir::Operand::var(p)
            };
            let j = self.fresh_temp("long long");
            self.emit(
                j.clone(),
                mir::Rvalue::Call(
                    mir::Operand::Func(mir::map_fn_name(k, v, "rank")),
                    vec![ptr, mir::Operand::var(idx.clone())],
                ),
            );
            mir::Operand::var(j)
        } else {
            mir::Operand::var(idx.clone())
        };
        let kv = self.fresh_temp(k);
        self.emit(
            kv.clone(),
            mir::Rvalue::Index(mir::Operand::var(keys), at.clone()),
        );
        let vv = self.fresh_temp(v);
        self.emit(vv.clone(), mir::Rvalue::Index(mir::Operand::var(vals), at));
        if values_only {
            self.lower_bind_pattern(kp, mir::Operand::var(vv.clone()), v)?;
        } else {
            self.lower_bind_pattern(kp, mir::Operand::var(kv), k)?;
        }
        if let Some(vp) = vp {
            self.lower_bind_pattern(vp, mir::Operand::var(vv), v)?;
        }
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

    /// The argument list of an adapter closure call: its captured variables, then the
    /// item. After `enumerate` the item is the pair `(position, item)`, which a closure
    /// with an `(index, item)` pattern takes as two arguments. Without `enumerate` a
    /// closure with a pair pattern takes the two fields of a tuple item.
    fn chain_args(
        &mut self,
        caps: Vec<mir::Operand>,
        pos: &Option<String>,
        item: &str,
        arg: &Expr,
    ) -> Result<Vec<mir::Operand>> {
        let mut args = caps;
        let pair = closure_takes_pair(arg);
        match (pair, pos) {
            (true, Some(p)) => {
                args.push(mir::Operand::var(p.clone()));
                args.push(mir::Operand::var(item));
            }
            (true, None) => {
                let item_ty = self.temp_type_of(item);
                let parts: Vec<String> = match mir::split_generic(&item_ty) {
                    Some(("Tuple", a)) if a.len() == 2 => a.iter().map(|s| s.to_string()).collect(),
                    _ => vec!["long long".to_string(), "long long".to_string()],
                };
                for (i, pty) in parts.into_iter().enumerate() {
                    let t = self.fresh_temp(pty);
                    self.emit(
                        t.clone(),
                        mir::Rvalue::Field(mir::Operand::var(item), format!("f{i}"), false),
                    );
                    args.push(mir::Operand::var(t));
                }
            }
            (false, None) => args.push(mir::Operand::var(item)),
            (false, Some(_)) => {
                return Err(LowerError(
                    "an `enumerate` item needs a closure with an `(index, item)` pattern".into(),
                ))
            }
        }
        Ok(args)
    }

    /// `for (a, b) in x.iter().zip(y.iter())` over two named sequences: an index loop
    /// to the shorter length, binding each side's item.
    fn lower_zip_for(
        &mut self,
        left: (String, String, bool),
        right: (String, String, bool),
        pat: &ast::Pat,
        body: &ast::Block,
    ) -> Result<mir::Operand> {
        let (an, ae, ap) = left;
        let (bn, be, bp) = right;
        let (p1, p2) = match pat {
            ast::Pat::Tuple(ps) if ps.len() == 2 => (&ps[0], &ps[1]),
            _ => return Err(LowerError("`zip` needs a `(a, b)` pattern".into())),
        };
        let len_a = self.fresh_temp("long long");
        self.emit(
            len_a.clone(),
            mir::Rvalue::Field(mir::Operand::var(an.clone()), "len".into(), ap),
        );
        let len_b = self.fresh_temp("long long");
        self.emit(
            len_b.clone(),
            mir::Rvalue::Field(mir::Operand::var(bn.clone()), "len".into(), bp),
        );
        let data_a = self.fresh_temp(format!("{ae}*"));
        self.emit(
            data_a.clone(),
            mir::Rvalue::Field(mir::Operand::var(an), "data".into(), ap),
        );
        let data_b = self.fresh_temp(format!("{be}*"));
        self.emit(
            data_b.clone(),
            mir::Rvalue::Field(mir::Operand::var(bn), "data".into(), bp),
        );
        let idx = self.fresh_temp("long long");
        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));

        let head = self.new_block();
        let body_blk = self.new_block();
        let step = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let in_a = self.fresh_temp("long long");
        self.emit(
            in_a.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Lt,
                mir::Operand::var(idx.clone()),
                mir::Operand::var(len_a),
            ),
        );
        let in_b = self.fresh_temp("long long");
        self.emit(
            in_b.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Lt,
                mir::Operand::var(idx.clone()),
                mir::Operand::var(len_b),
            ),
        );
        let both = self.fresh_temp("long long");
        self.emit(
            both.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::And,
                mir::Operand::var(in_a),
                mir::Operand::var(in_b),
            ),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(both), body_blk, exit));

        self.current = body_blk;
        self.loops.push((exit, step));
        let av = self.fresh_temp(ae.clone());
        self.emit(
            av.clone(),
            mir::Rvalue::Index(mir::Operand::var(data_a), mir::Operand::var(idx.clone())),
        );
        let bv = self.fresh_temp(be.clone());
        self.emit(
            bv.clone(),
            mir::Rvalue::Index(mir::Operand::var(data_b), mir::Operand::var(idx.clone())),
        );
        self.lower_bind_pattern(p1, mir::Operand::var(av), &ae)?;
        self.lower_bind_pattern(p2, mir::Operand::var(bv), &be)?;
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

    /// `opt.is_some()`, `.unwrap()`, `.unwrap_or(d)` and the like on an `Option` or
    /// `Result` value, typed by the checker's hint for the receiver. `None` for any
    /// other receiver or method (nothing is lowered then).
    /// Pass an argument as the parameter's C type `pt` (a value where a pointer is
    /// expected takes its address; a pointer where a value is expected is read through).
    fn adapt_arg(&mut self, v: mir::Operand, pt: &str) -> mir::Operand {
        let at = match &v {
            mir::Operand::Var(n) => self.temp_type_of(n),
            _ => return v,
        };
        if pt.ends_with('*')
            && !at.ends_with('*')
            && at != "long long"
            && pt.trim_end_matches('*') == at
        {
            let p = self.fresh_temp(format!("{at}*"));
            self.emit(p.clone(), mir::Rvalue::Ref(v, true));
            return mir::Operand::var(p);
        }
        if !pt.ends_with('*')
            && at.ends_with('*')
            && at.trim_end_matches('*') == pt
            && at != "char*"
        {
            let d = self.fresh_temp(pt.to_string());
            self.emit(d.clone(), mir::Rvalue::Unary(mir::UnOp::Deref, v));
            return mir::Operand::var(d);
        }
        v
    }

    /// The declared return type of a call to an `impl` method or associated function
    /// (`Type::f(..)`, `recv.m(..)`), if `e` is one.
    fn user_call_ret(&self, e: &Expr) -> Option<String> {
        match &e.kind {
            ExprKind::Call(callee, _) => match &callee.kind {
                ExprKind::Path(p) if p.segments.len() == 2 => {
                    let mut ty = p.segments[0].node.clone();
                    if ty == "Self" {
                        ty = self.self_ty.clone()?;
                    }
                    self.methods
                        .get(&(ty, p.segments[1].node.clone()))
                        .map(|(_, _, r)| r.clone())
                }
                _ => None,
            },
            ExprKind::MethodCall(recv, m, _) => {
                let hint = self.hint_of_expr(recv)?;
                let ty = hint.trim_end_matches('*').to_string();
                self.methods
                    .get(&(ty, m.node.clone()))
                    .filter(|(_, kind, _)| *kind != 0)
                    .map(|(_, _, r)| r.clone())
            }
            _ => None,
        }
    }

    /// `s.strip_prefix(p)` (or `strip_suffix`): an `Option<str>` built from a branch on the
    /// test, with the rest of the string as its payload.
    /// The character codes of a string, as a new vector of `long long` (the source of a
    /// `chars()` chain). The loop runs to the string's terminating NUL.
    fn chars_to_vec(&mut self, recv: &Expr) -> Result<(String, String, bool)> {
        let s = match self.lower_expr(recv)? {
            mir::Operand::Var(n) => n,
            other => {
                // A literal is copied into a named string first.
                let t = self.fresh_temp("char*");
                self.emit(t.clone(), mir::Rvalue::Use(other));
                t
            }
        };
        let out = self.fresh_temp(mir::vec_type("char"));
        self.emit(
            out.clone(),
            mir::Rvalue::Call(mir::Operand::Func(mir::vec_fn_name("char", "new")), vec![]),
        );
        let idx = self.fresh_temp("long long");
        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
        let head = self.new_block();
        let body = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));
        self.current = head;
        let item = self.fresh_temp("char");
        self.emit(
            item.clone(),
            mir::Rvalue::Index(mir::Operand::var(s.clone()), mir::Operand::var(idx.clone())),
        );
        let more = self.fresh_temp("long long");
        self.emit(
            more.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Ne,
                mir::Operand::var(item.clone()),
                mir::Operand::Const(0),
            ),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(more), body, exit));
        self.current = body;
        self.emit_vec_push(&out, "char", mir::Operand::var(item));
        self.emit(
            idx.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Add,
                mir::Operand::var(idx),
                mir::Operand::Const(1),
            ),
        );
        self.set_term(mir::Terminator::Goto(head));
        self.current = exit;
        Ok((out, "char".to_string(), false))
    }

    /// `s.find(p)`: an `Option<usize>` of the first match of `p` (a string or a `char`).
    fn lower_find(&mut self, recv: &Expr, pat: &Expr) -> Result<mir::Operand> {
        let s = self.lower_expr(recv)?;
        let p = self.pattern_string(pat)?;
        let idx = self.fresh_temp("long long");
        self.emit(
            idx.clone(),
            mir::Rvalue::Call(mir::Operand::Func("moira_str_find".to_string()), vec![s, p]),
        );
        let found = self.fresh_temp("long long");
        self.emit(
            found.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Ge,
                mir::Operand::var(idx.clone()),
                mir::Operand::Const(0),
            ),
        );
        let r = self.fresh_temp("Option<usize>");
        let some = self.new_block();
        let none = self.new_block();
        let join = self.new_block();
        self.set_term(mir::Terminator::If(mir::Operand::var(found), some, none));
        self.current = some;
        self.emit(
            r.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![
                    ("tag".to_string(), mir::Operand::Const(0)),
                    ("f0".to_string(), mir::Operand::var(idx)),
                ],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));
        self.current = none;
        self.emit(
            r.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![("tag".to_string(), mir::Operand::Const(1))],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));
        self.current = join;
        Ok(mir::Operand::var(r))
    }

    /// A string pattern as a C string: a `char` literal becomes a one-character string.
    fn pattern_string(&mut self, pat: &Expr) -> Result<mir::Operand> {
        if let ExprKind::Lit(ast::Lit::Char(_)) = &pat.kind {
            let c = self.lower_expr(pat)?;
            let t = self.fresh_temp("char*");
            self.emit(
                t.clone(),
                mir::Rvalue::Call(mir::Operand::Func("moira_char_str".to_string()), vec![c]),
            );
            return Ok(mir::Operand::var(t));
        }
        self.lower_expr(pat)
    }

    fn lower_strip(
        &mut self,
        e: &Expr,
        recv: &Expr,
        pat: &Expr,
        suffix: bool,
    ) -> Result<mir::Operand> {
        let _ = e;
        let s = self.lower_expr(recv)?;
        let p = self.pattern_string(pat)?;
        let test = if suffix {
            "moira_str_ends_with"
        } else {
            "moira_str_starts_with"
        };
        let cond = self.fresh_temp("long long");
        self.emit(
            cond.clone(),
            mir::Rvalue::Call(
                mir::Operand::Func(test.to_string()),
                vec![s.clone(), p.clone()],
            ),
        );
        let r = self.fresh_temp("Option<str>");
        let some = self.new_block();
        let none = self.new_block();
        let join = self.new_block();
        self.set_term(mir::Terminator::If(mir::Operand::var(cond), some, none));
        self.current = some;
        let n = self.fresh_temp("long long");
        self.emit(
            n.clone(),
            mir::Rvalue::Call(mir::Operand::Func("moira_str_len".to_string()), vec![p]),
        );
        let rest = self.fresh_temp("char*");
        if suffix {
            let total = self.fresh_temp("long long");
            self.emit(
                total.clone(),
                mir::Rvalue::Call(
                    mir::Operand::Func("moira_str_len".to_string()),
                    vec![s.clone()],
                ),
            );
            let keep = self.fresh_temp("long long");
            self.emit(
                keep.clone(),
                mir::Rvalue::Binary(
                    mir::BinOp::Sub,
                    mir::Operand::var(total),
                    mir::Operand::var(n),
                ),
            );
            self.emit(
                rest.clone(),
                mir::Rvalue::Call(
                    mir::Operand::Func("moira_str_before".to_string()),
                    vec![s, mir::Operand::var(keep)],
                ),
            );
        } else {
            self.emit(
                rest.clone(),
                mir::Rvalue::Call(
                    mir::Operand::Func("moira_str_after".to_string()),
                    vec![s, mir::Operand::var(n)],
                ),
            );
        }
        self.emit(
            r.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![
                    ("tag".to_string(), mir::Operand::Const(0)),
                    ("f0".to_string(), mir::Operand::var(rest)),
                ],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));
        self.current = none;
        self.emit(
            r.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![("tag".to_string(), mir::Operand::Const(1))],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));
        self.current = join;
        Ok(mir::Operand::var(r))
    }

    /// A heap copy of a string value (so a `String` can grow in place).
    fn heap_string(&mut self, v: mir::Operand) -> Result<mir::Operand> {
        let t = self.fresh_temp("char*");
        self.emit(
            t.clone(),
            mir::Rvalue::Call(mir::Operand::Func("moira_str_dup".to_string()), vec![v]),
        );
        Ok(mir::Operand::var(t))
    }

    /// Whether a receiver is a string: the checker's hint says so, or its local's C type does.
    fn recv_is_str(&self, recv: &Expr) -> bool {
        match self.hint_of_expr(recv) {
            Some(h) => matches!(h.trim_end_matches('*'), "str" | "String"),
            None => match &recv.kind {
                ExprKind::Path(p) if p.segments.len() == 1 => {
                    let n = &self.var_name(&p.segments[0].node);
                    self.types
                        .get(n)
                        .map_or(false, |t| t == "char*" || t == "str")
                        || self.func.locals.iter().any(|(l, t)| {
                            l == n && matches!(t.as_deref(), Some("char*") | Some("str"))
                        })
                }
                _ => false,
            },
        }
    }

    /// Append a string value to the heap string held by the variable `out`.
    fn push_to_string(&mut self, out: &str, piece: mir::Operand) {
        let ptr = self.fresh_temp("char**");
        self.emit(ptr.clone(), mir::Rvalue::Ref(mir::Operand::var(out), true));
        let done = self.fresh_temp("long long");
        self.emit(
            done,
            mir::Rvalue::Call(
                mir::Operand::Func("moira_str_push_str".into()),
                vec![mir::Operand::var(ptr), piece],
            ),
        );
    }

    /// The C name of the variable a Rust name currently refers to.
    fn var_name(&self, plain: &str) -> String {
        self.var_renames
            .get(plain)
            .cloned()
            .unwrap_or_else(|| plain.to_string())
    }

    /// The element type of a vector-typed field expression, from the checker's hint.
    fn field_vec_elem(&self, e: &Expr) -> Option<String> {
        let h = self.hint_of_expr(e)?;
        let h = h.trim_end_matches('*').to_string();
        mir::vec_elem(&h).map(|s| s.to_string())
    }

    /// Whether `a` should come after `b` under `cmp` (a `long long` operand, 1 or 0).
    fn emit_greater(&mut self, cmp: &SortCmp, a: mir::Operand, b: mir::Operand) -> mir::Operand {
        let t = self.fresh_temp("long long");
        match cmp {
            SortCmp::Natural => {
                self.emit(t.clone(), mir::Rvalue::Binary(mir::BinOp::Gt, a, b));
            }
            SortCmp::Key(f, caps) => {
                let mut ka_args = caps.clone();
                ka_args.push(a);
                let ka = self.fresh_temp("long long");
                self.emit(
                    ka.clone(),
                    mir::Rvalue::Call(mir::Operand::Func(f.clone()), ka_args),
                );
                let mut kb_args = caps.clone();
                kb_args.push(b);
                let kb = self.fresh_temp("long long");
                self.emit(
                    kb.clone(),
                    mir::Rvalue::Call(mir::Operand::Func(f.clone()), kb_args),
                );
                self.emit(
                    t.clone(),
                    mir::Rvalue::Binary(
                        mir::BinOp::Gt,
                        mir::Operand::var(ka),
                        mir::Operand::var(kb),
                    ),
                );
            }
            SortCmp::Pair(f, caps) => {
                let mut args = caps.clone();
                args.push(a);
                args.push(b);
                let r = self.fresh_temp("long long");
                self.emit(
                    r.clone(),
                    mir::Rvalue::Call(mir::Operand::Func(f.clone()), args),
                );
                self.emit(
                    t.clone(),
                    mir::Rvalue::Binary(
                        mir::BinOp::Gt,
                        mir::Operand::var(r),
                        mir::Operand::Const(0),
                    ),
                );
            }
        }
        mir::Operand::var(t)
    }

    /// Sort the vector `recv` in place with an insertion sort: each item is moved left past
    /// the items that should come after it.
    fn lower_vec_sort(&mut self, recv: &Expr, cmp: SortCmp) -> Result<mir::Operand> {
        let (vname, elem, is_ptr) = match self.vec_local(recv) {
            Some(v) => v,
            None => return Err(LowerError("`sort` needs a named vector".into())),
        };
        if matches!(cmp, SortCmp::Natural) && elem == "char*" {
            return Err(LowerError(
                "sorting strings is not in the codegen subset".into(),
            ));
        }
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(vname.clone()), "len".into(), is_ptr),
        );
        let data = self.fresh_temp(format!("{elem}*"));
        self.emit(
            data.clone(),
            mir::Rvalue::Field(mir::Operand::var(vname.clone()), "data".into(), is_ptr),
        );
        let i = self.fresh_temp("long long");
        self.emit(i.clone(), mir::Rvalue::Use(mir::Operand::Const(1)));
        let j = self.fresh_temp("long long");
        let outer_head = self.new_block();
        let outer_body = self.new_block();
        let inner_head = self.new_block();
        let inner_check = self.new_block();
        let swap = self.new_block();
        let outer_step = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(outer_head));

        self.current = outer_head;
        let more = self.fresh_temp("long long");
        self.emit(
            more.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Lt,
                mir::Operand::var(i.clone()),
                mir::Operand::var(len),
            ),
        );
        self.set_term(mir::Terminator::If(
            mir::Operand::var(more),
            outer_body,
            exit,
        ));

        self.current = outer_body;
        self.emit(j.clone(), mir::Rvalue::Use(mir::Operand::var(i.clone())));
        self.set_term(mir::Terminator::Goto(inner_head));

        self.current = inner_head;
        let j_pos = self.fresh_temp("long long");
        self.emit(
            j_pos.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Gt,
                mir::Operand::var(j.clone()),
                mir::Operand::Const(0),
            ),
        );
        self.set_term(mir::Terminator::If(
            mir::Operand::var(j_pos),
            inner_check,
            outer_step,
        ));

        self.current = inner_check;
        let prev = self.fresh_temp("long long");
        self.emit(
            prev.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Sub,
                mir::Operand::var(j.clone()),
                mir::Operand::Const(1),
            ),
        );
        let a = self.fresh_temp(elem.clone());
        self.emit(
            a.clone(),
            mir::Rvalue::Index(
                mir::Operand::var(data.clone()),
                mir::Operand::var(prev.clone()),
            ),
        );
        let b = self.fresh_temp(elem.clone());
        self.emit(
            b.clone(),
            mir::Rvalue::Index(
                mir::Operand::var(data.clone()),
                mir::Operand::var(j.clone()),
            ),
        );
        let gt = self.emit_greater(
            &cmp,
            mir::Operand::var(a.clone()),
            mir::Operand::var(b.clone()),
        );
        self.set_term(mir::Terminator::If(gt, swap, outer_step));

        self.current = swap;
        // data[j-1] = b; data[j] = a; j -= 1.
        self.emit(
            "__scratch".to_string(),
            mir::Rvalue::StoreIndex(
                mir::Operand::var(data.clone()),
                mir::Operand::var(prev),
                mir::Operand::var(b),
            ),
        );
        self.emit(
            "__scratch".to_string(),
            mir::Rvalue::StoreIndex(
                mir::Operand::var(data.clone()),
                mir::Operand::var(j.clone()),
                mir::Operand::var(a),
            ),
        );
        self.emit(
            j.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Sub,
                mir::Operand::var(j.clone()),
                mir::Operand::Const(1),
            ),
        );
        self.set_term(mir::Terminator::Goto(inner_head));

        self.current = outer_step;
        self.emit(
            i.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Add,
                mir::Operand::var(i.clone()),
                mir::Operand::Const(1),
            ),
        );
        self.set_term(mir::Terminator::Goto(outer_head));

        self.current = exit;
        Ok(mir::Operand::Unit)
    }

    /// `base.f.push(x)` / `base.f.extend(v)` on a vector field: the field is read into a local,
    /// the local grows, and the field is written back. `None` for other receivers.
    fn lower_vec_field_method(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<mir::Operand>> {
        let ExprKind::Field(base, field) = &recv.kind else {
            return Ok(None);
        };
        if !(method == "push" || method == "extend") || args.len() != 1 {
            return Ok(None);
        }
        //  takes a named vector or field (an iterator chain stays on its own path).
        // `extend(x.drain(..))` is also a plain sequence source (it empties `x`).
        let arg0: &Expr = &args[0];
        let mut drain_inner: &Expr = arg0;
        let mut draining = false;
        if let ExprKind::MethodCall(r, m, a) = &arg0.kind {
            if m.node == "drain" && a.len() == 1 {
                drain_inner = r;
                draining = true;
            }
        }
        if method == "extend"
            && !draining
            && !matches!(args[0].kind, ExprKind::Path(_) | ExprKind::Field(..))
        {
            return Ok(None);
        }
        let Some(hint) = self.hint_of_expr(recv) else {
            return Ok(None);
        };
        let Some(elem) = mir::vec_elem(&hint).map(|s| s.to_string()) else {
            return Ok(None);
        };
        let b = self.lower_expr(base)?;
        let bname = match &b {
            mir::Operand::Var(n) => n.clone(),
            _ => return Err(LowerError("a vector field needs a named struct".into())),
        };
        let ptr_base = self.scrutinee_is_pointer(&b);
        let vec_ty = mir::vec_type(&elem);
        let cur = self.fresh_temp(vec_ty.clone());
        self.emit(
            cur.clone(),
            mir::Rvalue::Field(
                mir::Operand::var(bname.clone()),
                field.node.clone(),
                ptr_base,
            ),
        );
        if method == "push" {
            let v = self.lower_expr(&args[0])?;
            self.emit_vec_push(&cur, &elem, v);
        } else {
            // `extend(v)`: every item of the vector `v`, in order.
            let (src, src_elem, src_ptr) = self.chain_sequence(drain_inner)?;
            let _ = src_elem;
            let len = self.fresh_temp("long long");
            self.emit(
                len.clone(),
                mir::Rvalue::Field(mir::Operand::var(src.clone()), "len".into(), src_ptr),
            );
            let data = self.fresh_temp(format!("{elem}*"));
            self.emit(
                data.clone(),
                mir::Rvalue::Field(mir::Operand::var(src.clone()), "data".into(), src_ptr),
            );
            let idx = self.fresh_temp("long long");
            self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
            let head = self.new_block();
            let body = self.new_block();
            let exit = self.new_block();
            self.set_term(mir::Terminator::Goto(head));
            self.current = head;
            let more = self.fresh_temp("long long");
            self.emit(
                more.clone(),
                mir::Rvalue::Binary(
                    mir::BinOp::Lt,
                    mir::Operand::var(idx.clone()),
                    mir::Operand::var(len),
                ),
            );
            self.set_term(mir::Terminator::If(mir::Operand::var(more), body, exit));
            self.current = body;
            let item = self.fresh_temp(elem.clone());
            self.emit(
                item.clone(),
                mir::Rvalue::Index(mir::Operand::var(data), mir::Operand::var(idx.clone())),
            );
            self.emit_vec_push(&cur, &elem, mir::Operand::var(item));
            self.emit(
                idx.clone(),
                mir::Rvalue::Binary(
                    mir::BinOp::Add,
                    mir::Operand::var(idx),
                    mir::Operand::Const(1),
                ),
            );
            self.set_term(mir::Terminator::Goto(head));
            self.current = exit;
            if draining {
                // `drain(..)` empties its source once its items are pushed.
                self.emit(
                    "__scratch".to_string(),
                    mir::Rvalue::StoreField(
                        mir::Operand::var(src),
                        "len".to_string(),
                        mir::Operand::Const(0),
                        src_ptr,
                    ),
                );
            }
        }
        self.emit(
            "__scratch".to_string(),
            mir::Rvalue::StoreField(
                mir::Operand::var(bname),
                field.node.clone(),
                mir::Operand::var(cur),
                ptr_base,
            ),
        );
        Ok(Some(mir::Operand::Unit))
    }

    /// `v.join(sep)` on a vector of strings: a new string with `sep` between the items.
    fn lower_join(&mut self, recv: &Expr, sep: &Expr) -> Result<mir::Operand> {
        let (vname, _elem, is_ptr) = match self.vec_local(recv) {
            Some(v) => v,
            None => {
                let (n, e, p) = self.chain_sequence(recv)?;
                (n, e, p)
            }
        };
        let sep_op = self.lower_expr(sep)?;
        let out = self.heap_string(mir::Operand::CStr(String::new()))?;
        let out_name = match &out {
            mir::Operand::Var(n) => n.clone(),
            _ => return Err(LowerError("`join` into a string".into())),
        };
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(vname.clone()), "len".into(), is_ptr),
        );
        let data = self.fresh_temp("char**");
        self.emit(
            data.clone(),
            mir::Rvalue::Field(mir::Operand::var(vname.clone()), "data".into(), is_ptr),
        );
        let idx = self.fresh_temp("long long");
        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
        let head = self.new_block();
        let body = self.new_block();
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
        self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body, exit));
        self.current = body;
        // A separator before every item but the first.
        let first = self.fresh_temp("long long");
        self.emit(
            first.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Eq,
                mir::Operand::var(idx.clone()),
                mir::Operand::Const(0),
            ),
        );
        let sep_blk = self.new_block();
        let item_blk = self.new_block();
        self.set_term(mir::Terminator::If(
            mir::Operand::var(first),
            item_blk,
            sep_blk,
        ));
        self.current = sep_blk;
        let out_ptr = self.fresh_temp("char**");
        self.emit(
            out_ptr.clone(),
            mir::Rvalue::Ref(mir::Operand::var(out_name.clone()), true),
        );
        let sepped = self.fresh_temp("long long");
        self.emit(
            sepped.clone(),
            mir::Rvalue::Call(
                mir::Operand::Func("moira_str_push_str".into()),
                vec![mir::Operand::var(out_ptr), sep_op.clone()],
            ),
        );
        self.set_term(mir::Terminator::Goto(item_blk));
        self.current = item_blk;
        let item = self.fresh_temp("char*");
        self.emit(
            item.clone(),
            mir::Rvalue::Index(mir::Operand::var(data), mir::Operand::var(idx.clone())),
        );
        let out_ptr2 = self.fresh_temp("char**");
        self.emit(
            out_ptr2.clone(),
            mir::Rvalue::Ref(mir::Operand::var(out_name), true),
        );
        let pushed = self.fresh_temp("long long");
        self.emit(
            pushed.clone(),
            mir::Rvalue::Call(
                mir::Operand::Func("moira_str_push_str".into()),
                vec![mir::Operand::var(out_ptr2), mir::Operand::var(item)],
            ),
        );
        self.emit(
            idx.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Add,
                mir::Operand::var(idx),
                mir::Operand::Const(1),
            ),
        );
        self.set_term(mir::Terminator::Goto(head));
        self.current = exit;
        Ok(out)
    }

    /// `let P = init;` for a refutable pattern `P` (`Some(x)`, `E::V { .. }`): each name the
    /// pattern binds is read out of the value. The pattern is assumed to match, as an
    /// irrefutable `let` requires.
    fn lower_refutable_let(&mut self, local: &ast::Local) -> Result<()> {
        let Some(init) = &local.init else {
            return Err(LowerError("a refutable `let` needs an initializer".into()));
        };
        let value = self.lower_expr(init)?;
        let (_, binds) = self.pattern_guard(&value, &local.pat)?;
        for (name, val) in binds {
            let ty = match &val {
                mir::Operand::Var(n) => self.temp_type_of(n),
                _ => "long long".to_string(),
            };
            if !self.func.locals.iter().any(|(n, _)| n == &name) {
                self.func.locals.push((name.clone(), Some(ty.clone())));
            }
            if ty != "long long" {
                self.types.insert(name.clone(), ty);
            }
            self.emit(name, mir::Rvalue::Use(val));
        }
        Ok(())
    }

    /// `seq.get(i)` on a vector or slice: `Some(&seq[i])` while `i < len`, else `None`.
    /// A payload typed as a pointer (`Option<T*>`, the checker's `Option<&T>`) points into
    /// the sequence's data, so a struct element is not copied.
    fn lower_seq_get(
        &mut self,
        e: &Expr,
        recv: &Expr,
        index: &Expr,
    ) -> Result<Option<mir::Operand>> {
        let Some(out_ty) = self.hint_of_expr(e) else {
            return Ok(None);
        };
        let Some(("Option", targs)) = mir::split_generic(&out_ty) else {
            return Ok(None);
        };
        if targs.len() != 1 {
            return Ok(None);
        }
        let payload = targs[0].to_string();
        // The receiver is a vector or slice: a local or parameter declared as one, or an
        // expression the checker typed as one.
        let is_seq = self.vec_local(recv).is_some()
            || self.hint_of_expr(recv).map_or(false, |h| {
                mir::vec_elem(&h).is_some() || mir::slice_elem(&h).is_some()
            });
        if !is_seq {
            return Ok(None);
        }
        let (sname, _, sptr) = self.chain_sequence(recv)?;
        let idx = self.lower_expr(index)?;
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(sname.clone()), "len".into(), sptr),
        );
        // The data pointer points at the elements, so its element type is the payload's
        // type without the reference's `*` (a pointer payload `Tok*` has data `Tok*`).
        let elem = payload.strip_suffix('*').unwrap_or(&payload).to_string();
        let data = self.fresh_temp(format!("{elem}*"));
        self.emit(
            data.clone(),
            mir::Rvalue::Field(mir::Operand::var(sname), "data".into(), sptr),
        );
        let in_range = self.fresh_temp("long long");
        self.emit(
            in_range.clone(),
            mir::Rvalue::Binary(mir::BinOp::Lt, idx.clone(), mir::Operand::var(len)),
        );
        let result = self.fresh_temp(out_ty);
        let some_blk = self.new_block();
        let none_blk = self.new_block();
        let join = self.new_block();
        self.set_term(mir::Terminator::If(
            mir::Operand::var(in_range),
            some_blk,
            none_blk,
        ));

        self.current = some_blk;
        let item = self.fresh_temp(payload.clone());
        let elem_is_ptr = payload.ends_with('*');
        let item_rv = if elem_is_ptr {
            mir::Rvalue::IndexRef(mir::Operand::var(data), idx)
        } else {
            mir::Rvalue::Index(mir::Operand::var(data), idx)
        };
        self.emit(item.clone(), item_rv);
        self.emit(
            result.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![
                    ("tag".to_string(), mir::Operand::Const(0)),
                    (payload_field("Option", 0, 0), mir::Operand::var(item)),
                ],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));

        self.current = none_blk;
        self.emit(
            result.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![("tag".to_string(), mir::Operand::Const(1))],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));

        self.current = join;
        Ok(Some(mir::Operand::var(result)))
    }

    /// `opt.map(|x| body)` on an `Option<T>` value: a branch on the tag. `Some` applies
    /// the closure to the payload (typed `T` by the checker's hint, so a reference payload
    /// stays a pointer) and wraps the result; `None` stays `None`.
    fn lower_option_map(
        &mut self,
        e: &Expr,
        recv: &Expr,
        payload: &str,
        closure: &Expr,
    ) -> Result<Option<mir::Operand>> {
        let Some(out_ty) = self.hint_of_expr(e) else {
            return Ok(None);
        };
        let opt = self.lower_expr(recv)?;
        let oname = match &opt {
            mir::Operand::Var(n) => n.clone(),
            _ => return Err(LowerError("`map` on an option needs a named value".into())),
        };
        let ptr = self.scrutinee_is_pointer(&opt);
        let tag = self.fresh_temp("long long");
        self.emit(
            tag.clone(),
            mir::Rvalue::Field(mir::Operand::var(oname.clone()), "tag".into(), ptr),
        );
        let cond = self.fresh_temp("long long");
        self.emit(
            cond.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Eq,
                mir::Operand::var(tag),
                mir::Operand::Const(0),
            ),
        );
        let result = self.fresh_temp(out_ty);
        let some_blk = self.new_block();
        let none_blk = self.new_block();
        let join = self.new_block();
        self.set_term(mir::Terminator::If(mir::Operand::var(cond), some_blk, none_blk));

        self.current = some_blk;
        self.closure_param_hint = Some(vec![payload.to_string()]);
        let callee = self.closure_callee(closure);
        self.closure_param_hint = None;
        let (f, caps) = callee?;
        let payload_val = self.fresh_temp(payload.to_string());
        self.emit(
            payload_val.clone(),
            mir::Rvalue::Field(mir::Operand::var(oname), payload_field("Option", 0, 0), ptr),
        );
        let mut args = caps;
        args.push(mir::Operand::var(payload_val));
        let ret = self
            .closure_rets
            .get(&f)
            .cloned()
            .unwrap_or_else(|| "long long".to_string());
        let mapped = self.fresh_temp(ret);
        self.emit(
            mapped.clone(),
            mir::Rvalue::Call(mir::Operand::Func(f), args),
        );
        self.emit(
            result.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![
                    ("tag".to_string(), mir::Operand::Const(0)),
                    (payload_field("Option", 0, 0), mir::Operand::var(mapped)),
                ],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));

        self.current = none_blk;
        self.emit(
            result.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![("tag".to_string(), mir::Operand::Const(1))],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));

        self.current = join;
        Ok(Some(mir::Operand::var(result)))
    }

    /// `v.clone()` on a value: a vector is copied element by element into a new vector, a
    /// string is duplicated, and any other value is copied. A user type's own `clone`
    /// method is dispatched by `lower_user_method` instead.
    fn lower_value_method(
        &mut self,
        e: &Expr,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<mir::Operand>> {
        // `seq.get(i)` on a vector or slice with a checker-typed `Option<&T>` result.
        if method == "get" && args.len() == 1 {
            if let Some(r) = self.lower_seq_get(e, recv, &args[0])? {
                return Ok(Some(r));
            }
        }
        // `opt.map(|x| ..)` on an `Option<T>` with a checker-typed payload.
        if method == "map" && args.len() == 1 && matches!(args[0].kind, ExprKind::Closure { .. }) {
            if let Some(h) = self.hint_of_expr(recv) {
                if let Some(("Option", targs)) = mir::split_generic(&h) {
                    if targs.len() == 1 {
                        let payload = targs[0].to_string();
                        if let Some(r) = self.lower_option_map(e, recv, &payload, &args[0])? {
                            return Ok(Some(r));
                        }
                    }
                }
            }
        }
        // `v.sort()`, `v.sort_by_key(f)`, `v.sort_by(f)` on a named vector.
        if matches!(method, "sort" | "sort_by" | "sort_by_key") {
            if let Some((_, elem, _)) = self.vec_local(recv) {
                let cmp = match (method, args.len()) {
                    ("sort", 0) => SortCmp::Natural,
                    ("sort_by_key", 1) => {
                        self.closure_param_hint = Some(vec![elem.clone()]);
                        let r = self.closure_callee(&args[0]);
                        self.closure_param_hint = None;
                        let (f, caps) = r?;
                        SortCmp::Key(f, caps)
                    }
                    ("sort_by", 1) => {
                        self.closure_param_hint = Some(vec![elem.clone(), elem.clone()]);
                        let r = self.closure_callee(&args[0]);
                        self.closure_param_hint = None;
                        let (f, caps) = r?;
                        SortCmp::Pair(f, caps)
                    }
                    _ => return Ok(None),
                };
                return self.lower_vec_sort(recv, cmp).map(Some);
            }
        }
        // `a.cmp(b)`: a three-way result (-1, 0, 1); `strcmp` for strings.
        if method == "cmp" && args.len() == 1 {
            let is_string = self.recv_is_str(recv);
            let a = self.lower_expr(recv)?;
            let b = self.lower_expr(&args[0])?;
            let t = self.fresh_temp("long long");
            if is_string {
                self.emit(
                    t.clone(),
                    mir::Rvalue::Call(mir::Operand::Func("moira_str_cmp".into()), vec![a, b]),
                );
            } else {
                let gt = self.fresh_temp("long long");
                self.emit(
                    gt.clone(),
                    mir::Rvalue::Binary(mir::BinOp::Gt, a.clone(), b.clone()),
                );
                let lt = self.fresh_temp("long long");
                self.emit(lt.clone(), mir::Rvalue::Binary(mir::BinOp::Lt, a, b));
                self.emit(
                    t.clone(),
                    mir::Rvalue::Binary(
                        mir::BinOp::Sub,
                        mir::Operand::var(gt),
                        mir::Operand::var(lt),
                    ),
                );
            }
            return Ok(Some(mir::Operand::var(t)));
        }
        if let Some(v) = self.lower_vec_field_method(recv, method, args)? {
            return Ok(Some(v));
        }
        if method == "join" && args.len() == 1 {
            let is_vec_of_str = self.hint_of_expr(recv).as_deref() == Some("Vec<str>");
            if is_vec_of_str {
                return self.lower_join(recv, &args[0]).map(Some);
            }
            return Ok(None);
        }
        if method != "clone" || !args.is_empty() {
            return Ok(None);
        }
        let hint = self.hint_of_expr(recv);
        if let Some(h) = &hint {
            let base = h.trim_end_matches('*');
            if self
                .methods
                .contains_key(&(base.to_string(), "clone".to_string()))
            {
                return Ok(None);
            }
        }
        let _ = e;
        let op = self.lower_expr(recv)?;
        // The representation is the operand's own C type when it is known: a binding
        // through `&self` holds the payload by value even though the checker types it `&T`.
        let actual = match &op {
            mir::Operand::Var(n) => Some(self.temp_type_of(n)).filter(|t| t != "long long"),
            _ => None,
        };
        let ty = match (actual, hint) {
            (Some(t), _) => t,
            (None, Some(h)) => h,
            (None, None) => "long long".to_string(),
        };
        // A pointer is read through (a clone of `&T` is a `T`).
        let (base, is_ptr) = match ty.strip_suffix('*') {
            Some(b) => (b.to_string(), true),
            None => (ty.clone(), false),
        };
        if base == "str" || base == "char*" {
            let value = if is_ptr {
                let v = self.fresh_temp("char*");
                self.emit(v.clone(), mir::Rvalue::Unary(mir::UnOp::Deref, op));
                mir::Operand::var(v)
            } else {
                op
            };
            return Ok(Some(self.heap_string(value)?));
        }
        if let Some(elem) = mir::vec_elem(&base).map(|s| s.to_string()) {
            let name = match &op {
                mir::Operand::Var(n) => n.clone(),
                _ => return Err(LowerError("`clone` of an unnamed vector".into())),
            };
            return Ok(Some(self.emit_vec_clone(&name, &elem, is_ptr)));
        }
        if is_ptr {
            let v = self.fresh_temp(base);
            self.emit(v.clone(), mir::Rvalue::Unary(mir::UnOp::Deref, op));
            return Ok(Some(mir::Operand::var(v)));
        }
        Ok(Some(op))
    }

    /// A new vector holding the items of `src` (a vector, or a pointer to one).
    fn emit_vec_clone(&mut self, src: &str, elem: &str, is_ptr: bool) -> mir::Operand {
        let out = self.fresh_temp(mir::vec_type(elem));
        self.emit(
            out.clone(),
            mir::Rvalue::Call(mir::Operand::Func(mir::vec_fn_name(elem, "new")), vec![]),
        );
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(src), "len".into(), is_ptr),
        );
        let data = self.fresh_temp(format!("{elem}*"));
        self.emit(
            data.clone(),
            mir::Rvalue::Field(mir::Operand::var(src), "data".into(), is_ptr),
        );
        let idx = self.fresh_temp("long long");
        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
        let head = self.new_block();
        let body = self.new_block();
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
        self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body, exit));
        self.current = body;
        let item = self.fresh_temp(elem.to_string());
        self.emit(
            item.clone(),
            mir::Rvalue::Index(mir::Operand::var(data), mir::Operand::var(idx.clone())),
        );
        self.emit_vec_push(&out, elem, mir::Operand::var(item));
        self.emit(
            idx.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Add,
                mir::Operand::var(idx),
                mir::Operand::Const(1),
            ),
        );
        self.set_term(mir::Terminator::Goto(head));
        self.current = exit;
        mir::Operand::var(out)
    }

    /// `T::default()` / `Default::default()` / `Vec::default()` for a type the checker
    /// names: an empty vector, an empty string, zero for a number, or a zeroed struct.
    fn lower_default_call(&mut self, e: &Expr, args: &[Expr]) -> Result<Option<mir::Operand>> {
        if !args.is_empty() {
            return Ok(None);
        }
        let Some(hint) = self.hint_of_expr(e) else {
            return Ok(None);
        };
        if let Some(elem) = mir::vec_elem(&hint).map(|s| s.to_string()) {
            let t = self.fresh_temp(mir::vec_type(&elem));
            self.emit(
                t.clone(),
                mir::Rvalue::Call(mir::Operand::Func(mir::vec_fn_name(&elem, "new")), vec![]),
            );
            return Ok(Some(mir::Operand::var(t)));
        }
        if hint == "str" {
            return Ok(Some(self.heap_string(mir::Operand::CStr(String::new()))?));
        }
        if matches!(
            hint.as_str(),
            "bool"
                | "long long"
                | "int64_t"
                | "int"
                | "double"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "f32"
                | "f64"
        ) {
            return Ok(Some(mir::Operand::Const(0)));
        }
        if self.struct_names.contains(&hint) {
            let t = self.fresh_temp(hint.clone());
            self.emit(t.clone(), mir::Rvalue::StructLit(hint, Vec::new()));
            return Ok(Some(mir::Operand::var(t)));
        }
        // An enum: its `#[default]` variant (the attribute is not kept in the AST, so the
        // variant named `Unreachable` is used, else the first variant).
        if self.variants.values().any(|(e, _, _)| *e == hint) {
            let tag = self
                .qualified_variants
                .get(&format!("{hint}::Unreachable"))
                .map(|v| v.1)
                .unwrap_or(0);
            let t = self.fresh_temp(hint.clone());
            self.emit(
                t.clone(),
                mir::Rvalue::StructLit(hint, vec![("tag".to_string(), mir::Operand::Const(tag))]),
            );
            return Ok(Some(mir::Operand::var(t)));
        }
        Ok(None)
    }

    /// `s.m(args)` on a `str`/`String` receiver for a method of the C string runtime
    /// (`moira_str_<m>`, see the codegen prelude). `None` otherwise.
    fn lower_str_method(
        &mut self,
        e: &Expr,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<mir::Operand>> {
        // An integer's `to_string()` formats it (the runtime's `moira_int_to_string`).
        if method == "to_string" && args.is_empty() {
            // The receiver's type: the checker's hint, else the lowering's own (a closure
            // parameter's type comes from the iterator item).
            let recv_ty = self.hint_of_expr(recv).or_else(|| match &recv.kind {
                ExprKind::Path(p) if p.segments.len() == 1 => {
                    let n = &self.var_name(&p.segments[0].node);
                    self.types.get(n).cloned().or_else(|| {
                        self.func
                            .locals
                            .iter()
                            .find(|(l, _)| l == n)
                            .and_then(|(_, t)| t.clone())
                    })
                }
                _ => None,
            });
            let is_int = recv_ty.map_or(false, |h| {
                matches!(
                    h.as_str(),
                    "i8" | "i16"
                        | "i32"
                        | "i64"
                        | "i128"
                        | "isize"
                        | "u8"
                        | "u16"
                        | "u32"
                        | "u64"
                        | "u128"
                        | "usize"
                        | "long long"
                        | "int64_t"
                        | "int"
                        | "unsigned long long"
                )
            });
            if is_int {
                let v = self.lower_expr(recv)?;
                let t = self.fresh_temp("char*");
                self.emit(
                    t.clone(),
                    mir::Rvalue::Call(
                        mir::Operand::Func("moira_int_to_string".to_string()),
                        vec![v],
                    ),
                );
                return Ok(Some(mir::Operand::var(t)));
            }
        }
        // A method of a `char` (the C runtime's `moira_char_<method>`).
        let recv_is_char = match self.hint_of_expr(recv).as_deref() {
            Some("char") => true,
            // The lowering's own type for a local (a closure parameter over characters) counts too.
            _ => match &recv.kind {
                ExprKind::Path(p) if p.segments.len() == 1 => {
                    let n = &self.var_name(&p.segments[0].node);
                    self.types.get(n).map_or(false, |t| t == "char")
                        || self
                            .func
                            .locals
                            .iter()
                            .any(|(l, t)| l == n && t.as_deref() == Some("char"))
                }
                _ => false,
            },
        };
        if recv_is_char && args.is_empty() {
            const CHAR_METHODS: &[&str] = &[
                "is_ascii_alphanumeric",
                "is_ascii_digit",
                "is_ascii_alphabetic",
                "is_ascii_whitespace",
                "is_ascii_uppercase",
                "is_ascii_lowercase",
                "is_alphanumeric",
                "is_numeric",
                "to_ascii_lowercase",
                "to_ascii_uppercase",
            ];
            if CHAR_METHODS.contains(&method) {
                let c = self.lower_expr(recv)?;
                let t = self.fresh_temp(if method.starts_with("to_") {
                    "char"
                } else {
                    "long long"
                });
                self.emit(
                    t.clone(),
                    mir::Rvalue::Call(mir::Operand::Func(format!("moira_char_{method}")), vec![c]),
                );
                return Ok(Some(mir::Operand::var(t)));
            }
        }
        // A string converts to itself with `into()`.
        if method == "into" && args.is_empty() {
            let is_string = match self.hint_of_expr(recv) {
                Some(h) => h == "str",
                None => match &recv.kind {
                    ExprKind::Path(p) if p.segments.len() == 1 => {
                        let n = &self.var_name(&p.segments[0].node);
                        self.types
                            .get(n)
                            .map_or(false, |t| t == "char*" || t == "str")
                            || self.func.locals.iter().any(|(l, t)| {
                                l == n && matches!(t.as_deref(), Some("char*") | Some("str"))
                            })
                    }
                    _ => false,
                },
            };
            if is_string {
                return Ok(Some(self.lower_expr(recv)?));
            }
        }
        const STR_METHODS: &[&str] = &[
            "trim",
            "trim_start",
            "trim_end",
            "starts_with",
            "ends_with",
            "contains",
            "to_string",
            "to_owned",
            "to_ascii_lowercase",
            "replace",
            "replacen",
            "len",
            "is_empty",
            "into",
            "as_str",
            "push_str",
            "push",
            "strip_prefix",
            "strip_suffix",
            "find",
        ];
        if !STR_METHODS.contains(&method) {
            return Ok(None);
        }
        let is_str = self.recv_is_str(recv);
        if !is_str {
            return Ok(None);
        }
        // `s.as_str()` is the string itself.
        if method == "as_str" && args.is_empty() {
            return Ok(Some(self.lower_expr(recv)?));
        }
        // `s.strip_prefix(p)` / `s.strip_suffix(p)`: `Some(rest)` when `s` starts (ends) with
        // `p`, else `None`. A `char` pattern is a one-character string.
        // `s.find(p)`: `Some(index)` of the first match, else `None`.
        if method == "find" && args.len() == 1 {
            return self.lower_find(recv, &args[0]).map(Some);
        }
        if (method == "strip_prefix" || method == "strip_suffix") && args.len() == 1 {
            return self
                .lower_strip(e, recv, &args[0], method == "strip_suffix")
                .map(Some);
        }
        // A `char` pattern of `starts_with` / `ends_with` / `contains`.
        if matches!(method, "starts_with" | "ends_with" | "contains")
            && args.len() == 1
            && matches!(args[0].kind, ExprKind::Lit(ast::Lit::Char(_)))
            && self.hint_of_expr(recv).map_or(false, |h| h == "str")
        {
            let s = self.lower_expr(recv)?;
            let c = self.lower_expr(&args[0])?;
            let t = self.fresh_temp("long long");
            self.emit(
                t.clone(),
                mir::Rvalue::Call(
                    mir::Operand::Func(format!("moira_str_{method}_char")),
                    vec![s, c],
                ),
            );
            return Ok(Some(mir::Operand::var(t)));
        }
        // `s.push_str(t)` / `s.push(c)` grow the string's heap buffer in place: the
        // runtime takes the string's address (a `char**`).
        if method == "replacen" && args.len() == 3 {
            let s = self.lower_expr(recv)?;
            let from = self.pattern_string(&args[0])?;
            let to = self.lower_expr(&args[1])?;
            let n = self.lower_expr(&args[2])?;
            let t = self.fresh_temp("char*");
            self.emit(
                t.clone(),
                mir::Rvalue::Call(
                    mir::Operand::Func("moira_str_replacen".into()),
                    vec![s, from, to, n],
                ),
            );
            return Ok(Some(mir::Operand::var(t)));
        }
        if method == "push_str" || method == "push" {
            if args.len() != 1 {
                return Ok(None);
            }
            // A string field (`self.out.push_str(..)`): the field is read into a local, the
            // local grows, and the field is written back.
            if let ExprKind::Field(base, field) = &recv.kind {
                let b = self.lower_expr(base)?;
                let bname = match &b {
                    mir::Operand::Var(n) => n.clone(),
                    _ => return Err(LowerError("a string field needs a named struct".into())),
                };
                let ptr_base = self.scrutinee_is_pointer(&b);
                let cur = self.fresh_temp("char*");
                self.emit(
                    cur.clone(),
                    mir::Rvalue::Field(
                        mir::Operand::var(bname.clone()),
                        field.node.clone(),
                        ptr_base,
                    ),
                );
                let cur_ptr = self.fresh_temp("char**");
                self.emit(
                    cur_ptr.clone(),
                    mir::Rvalue::Ref(mir::Operand::var(cur.clone()), true),
                );
                let arg = self.lower_expr(&args[0])?;
                let helper = if method == "push" {
                    "moira_str_push_char"
                } else {
                    "moira_str_push_str"
                };
                let done = self.fresh_temp("long long");
                self.emit(
                    done,
                    mir::Rvalue::Call(
                        mir::Operand::Func(helper.to_string()),
                        vec![mir::Operand::var(cur_ptr), arg],
                    ),
                );
                self.emit(
                    "__scratch".to_string(),
                    mir::Rvalue::StoreField(
                        mir::Operand::var(bname),
                        field.node.clone(),
                        mir::Operand::var(cur),
                        ptr_base,
                    ),
                );
                return Ok(Some(mir::Operand::Unit));
            }
            let place = self.lower_expr(recv)?;
            let ptr = match &place {
                mir::Operand::Var(n) if self.temp_type_of(n) == "char**" => place.clone(),
                mir::Operand::Var(_) => {
                    let p = self.fresh_temp("char**");
                    self.emit(p.clone(), mir::Rvalue::Ref(place.clone(), true));
                    mir::Operand::var(p)
                }
                _ => {
                    return Err(LowerError(
                        "a string can only be pushed to a named place".into(),
                    ))
                }
            };
            let mut arg = self.lower_expr(&args[0])?;
            // A reference to a string (`&s`) is read through: the helper takes the value.
            if let mir::Operand::Var(n) = &arg {
                if matches!(self.temp_type_of(n).as_str(), "char**" | "str*") {
                    let d = self.fresh_temp("char*");
                    self.emit(d.clone(), mir::Rvalue::Unary(mir::UnOp::Deref, arg.clone()));
                    arg = mir::Operand::var(d);
                }
            }
            let helper = if method == "push" {
                "moira_str_push_char"
            } else {
                "moira_str_push_str"
            };
            let t = self.fresh_temp("long long");
            self.emit(
                t.clone(),
                mir::Rvalue::Call(mir::Operand::Func(helper.to_string()), vec![ptr, arg]),
            );
            return Ok(Some(mir::Operand::Unit));
        }
        let mut ops = vec![self.lower_expr(recv)?];
        for a in args {
            ops.push(self.lower_expr(a)?);
        }
        let ret = self
            .hint_of_expr(e)
            .unwrap_or_else(|| "long long".to_string());
        let t = self.fresh_temp(ret);
        self.emit(
            t.clone(),
            mir::Rvalue::Call(mir::Operand::Func(format!("moira_str_{method}")), ops),
        );
        Ok(Some(mir::Operand::var(t)))
    }

    /// `recv.m(args)` where the receiver's type (from the checker's hint) has a method
    /// `m` in an `impl` block: a call to `Type__m`, with the receiver as its first
    /// argument (a pointer for `&self`, the value for `self`). `None` otherwise.
    fn lower_user_method(
        &mut self,
        _e: &Expr,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<mir::Operand>> {
        // A `clone()` the checker could not type: the receiver's C type decides it.
        if method == "clone" && args.is_empty() && self.hint_of_expr(recv).is_none() {
            let op = self.lower_expr(recv)?;
            let ty = match &op {
                mir::Operand::Var(n) => self.temp_type_of(n),
                _ => "long long".to_string(),
            };
            if ty == "char*" {
                let t = self.fresh_temp("char*");
                self.emit(
                    t.clone(),
                    mir::Rvalue::Call(mir::Operand::Func("moira_str_dup".to_string()), vec![op]),
                );
                return Ok(Some(mir::Operand::var(t)));
            }
            if let Some(pointee) = ty.strip_suffix('*') {
                let t = self.fresh_temp(pointee.to_string());
                self.emit(t.clone(), mir::Rvalue::Unary(mir::UnOp::Deref, op));
                return Ok(Some(mir::Operand::var(t)));
            }
            return Ok(Some(op));
        }
        // `self` names the impl's type (the checker does not type the receiver parameter).
        let hint = match self.hint_of_expr(recv) {
            Some(h) => h,
            None if is_self_path(recv) => match &self.self_ty {
                Some(t) => t.clone(),
                None => return Ok(None),
            },
            None => return Ok(None),
        };
        let ty = hint.trim_end_matches('*').to_string();
        // A derived `clone()` of a user struct or enum is a copy of the value.
        let user_type =
            self.struct_names.contains(&ty) || self.variants.values().any(|(e, _, _)| *e == ty);
        if method == "clone" && args.is_empty() && user_type {
            return Ok(Some(self.lower_expr(recv)?));
        }
        let Some((cname, kind, ret)) = self.methods.get(&(ty.clone(), method.to_string())).cloned()
        else {
            return Ok(None);
        };
        if kind == 0 {
            return Ok(None);
        }
        let op = self.lower_expr(recv)?;
        let is_ptr = match &op {
            mir::Operand::Var(n) => self.temp_type_of(n).ends_with('*'),
            _ => false,
        };
        let self_arg = if kind == 1 {
            if is_ptr {
                op
            } else {
                let p = self.fresh_temp(format!("{ty}*"));
                self.emit(p.clone(), mir::Rvalue::Ref(op, true));
                mir::Operand::var(p)
            }
        } else if is_ptr {
            let v = self.fresh_temp(ty.clone());
            self.emit(v.clone(), mir::Rvalue::Unary(mir::UnOp::Deref, op));
            mir::Operand::var(v)
        } else {
            op
        };
        let mut ops = vec![self_arg];
        for a in args {
            ops.push(self.lower_expr(a)?);
        }
        // A unit-returning function is called as a statement (codegen knows it is void).
        if ret == "void" {
            let t = self.fresh_temp("long long");
            self.emit(t, mir::Rvalue::Call(mir::Operand::Func(cname), ops));
            return Ok(Some(mir::Operand::Unit));
        }
        let t = self.fresh_temp(ret);
        self.emit(t.clone(), mir::Rvalue::Call(mir::Operand::Func(cname), ops));
        Ok(Some(mir::Operand::var(t)))
    }

    /// `Type::f(args)` / `Self::f(args)` for an associated function (or a method called
    /// with its receiver first) of an `impl` block. `None` for other paths.
    fn lower_assoc_call(
        &mut self,
        _e: &Expr,
        path: &ast::Path,
        args: &[Expr],
    ) -> Result<Option<mir::Operand>> {
        // `mir::Operand::var(..)`: the type and the function are the last two segments.
        let n = path.segments.len();
        if n < 2 {
            return Ok(None);
        }
        let mut segs: Vec<String> = vec![
            path.segments[n - 2].node.clone(),
            path.segments[n - 1].node.clone(),
        ];
        if segs[0] == "Self" {
            match &self.self_ty {
                Some(t) => segs[0] = t.clone(),
                None => return Ok(None),
            }
        }
        let Some((cname, _kind, ret)) = self
            .methods
            .get(&(segs[0].clone(), segs[1].clone()))
            .cloned()
        else {
            return Ok(None);
        };
        let mut ops = Vec::new();
        for a in args {
            ops.push(self.lower_expr(a)?);
        }
        // A unit-returning function is called as a statement (codegen knows it is void).
        if ret == "void" {
            let t = self.fresh_temp("long long");
            self.emit(t, mir::Rvalue::Call(mir::Operand::Func(cname), ops));
            return Ok(Some(mir::Operand::Unit));
        }
        let t = self.fresh_temp(ret);
        self.emit(t.clone(), mir::Rvalue::Call(mir::Operand::Func(cname), ops));
        Ok(Some(mir::Operand::var(t)))
    }

    fn lower_enum_method(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<mir::Operand>> {
        let known = matches!(
            method,
            "is_some"
                | "is_none"
                | "is_ok"
                | "is_err"
                | "unwrap"
                | "expect"
                | "unwrap_or"
                | "unwrap_or_else"
                | "or_else"
                | "as_deref"
                | "as_ref"
                | "copied"
                | "cloned"
        );
        if !known {
            return Ok(None);
        }
        let hint = match self.hint_of_expr(recv) {
            Some(h) => h,
            None => return Ok(None),
        };
        let (_base, targs) = match mir::split_generic(&hint) {
            Some((b, a)) if b == "Option" || b == "Result" => (
                b.to_string(),
                a.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            ),
            _ => return Ok(None),
        };
        // The tag of the variant that holds a value: `Some` and `Ok` are variant 0 (the first declared).
        let ok_tag: i128 = 0;
        let payload_ty = targs
            .first()
            .cloned()
            .unwrap_or_else(|| "long long".to_string());
        let op = self.lower_expr(recv)?;
        let ptr = self.scrutinee_is_pointer(&op);
        let tag = self.fresh_temp("long long");
        self.emit(
            tag.clone(),
            mir::Rvalue::Field(op.clone(), "tag".into(), ptr),
        );
        let test = |this: &mut Self, want: i128| -> mir::Operand {
            let c = this.fresh_temp("long long");
            this.emit(
                c.clone(),
                mir::Rvalue::Binary(
                    mir::BinOp::Eq,
                    mir::Operand::var(tag.clone()),
                    mir::Operand::Const(want),
                ),
            );
            mir::Operand::var(c)
        };
        match method {
            // A borrowed view of an option is the option itself (its C value is the same).
            "as_deref" | "as_ref" | "copied" | "cloned" => Ok(Some(op)),
            "is_some" | "is_ok" => Ok(Some(test(self, ok_tag))),
            "is_none" | "is_err" => Ok(Some(test(self, 1 - ok_tag))),
            "unwrap" | "expect" => {
                let v = self.fresh_temp(payload_ty);
                self.emit(v.clone(), mir::Rvalue::Field(op, "f0".into(), ptr));
                Ok(Some(mir::Operand::var(v)))
            }
            "unwrap_or_else" | "or_else" => {
                // The fallback is a closure with no parameters, called when the value is absent.
                let Some(f) = args.first() else {
                    return Err(LowerError(format!("`{method}` needs a closure")));
                };
                self.closure_param_hint = Some(Vec::new());
                let callee = self.closure_callee(f);
                self.closure_param_hint = None;
                let (lifted, caps) = callee?;
                let r = self.fresh_temp(if method == "or_else" {
                    hint.clone()
                } else {
                    payload_ty.clone()
                });
                let cond = test(self, ok_tag);
                let then_blk = self.new_block();
                let else_blk = self.new_block();
                let join = self.new_block();
                self.set_term(mir::Terminator::If(cond, then_blk, else_blk));
                self.current = then_blk;
                if method == "or_else" {
                    self.emit(r.clone(), mir::Rvalue::Use(op.clone()));
                } else {
                    let v = self.fresh_temp(payload_ty.clone());
                    self.emit(v.clone(), mir::Rvalue::Field(op.clone(), "f0".into(), ptr));
                    self.emit(r.clone(), mir::Rvalue::Use(mir::Operand::var(v)));
                }
                self.set_term(mir::Terminator::Goto(join));
                self.current = else_blk;
                let args_now = caps;
                self.emit(
                    r.clone(),
                    mir::Rvalue::Call(mir::Operand::Func(lifted), args_now),
                );
                self.set_term(mir::Terminator::Goto(join));
                self.current = join;
                Ok(Some(mir::Operand::var(r)))
            }
            _ => {
                // `unwrap_or(d)`: the payload when present, else `d`.
                let Some(default) = args.first() else {
                    return Err(LowerError("`unwrap_or` needs a default value".into()));
                };
                let r = self.fresh_temp(payload_ty.clone());
                let cond = test(self, ok_tag);
                let then_blk = self.new_block();
                let else_blk = self.new_block();
                let join = self.new_block();
                self.set_term(mir::Terminator::If(cond, then_blk, else_blk));
                self.current = then_blk;
                let v = self.fresh_temp(payload_ty);
                self.emit(v.clone(), mir::Rvalue::Field(op, "f0".into(), ptr));
                self.emit(r.clone(), mir::Rvalue::Use(mir::Operand::var(v)));
                self.set_term(mir::Terminator::Goto(join));
                self.current = else_blk;
                let d = self.lower_expr(default)?;
                self.emit(r.clone(), mir::Rvalue::Use(d));
                self.set_term(mir::Terminator::Goto(join));
                self.current = join;
                Ok(Some(mir::Operand::var(r)))
            }
        }
    }

    /// `v.first()` / `v.last()` on a sequence: `Some(&v[0])` / `Some(&v[len-1])`, or
    /// `None` when empty. `None` for other receivers.
    fn lower_first_last(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<mir::Operand>> {
        let is_len = method == "len";
        if !(is_len || method == "first" || method == "last")
            || !args.is_empty()
            || !self.is_sequence(recv)
        {
            return Ok(None);
        }
        let (an, ae, ap) = self.chain_sequence(recv)?;
        if is_len {
            // `seq.len()` on a sequence reached through a field or a pattern binding.
            let n = self.fresh_temp("long long");
            self.emit(
                n.clone(),
                mir::Rvalue::Field(mir::Operand::var(an), "len".into(), ap),
            );
            return Ok(Some(mir::Operand::var(n)));
        }
        let r = self.fresh_temp(format!("Option<{ae}>"));
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(an.clone()), "len".into(), ap),
        );
        let data = self.fresh_temp(format!("{ae}*"));
        self.emit(
            data.clone(),
            mir::Rvalue::Field(mir::Operand::var(an), "data".into(), ap),
        );
        let nonempty = self.fresh_temp("long long");
        self.emit(
            nonempty.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Lt,
                mir::Operand::Const(0),
                mir::Operand::var(len.clone()),
            ),
        );
        let some_blk = self.new_block();
        let none_blk = self.new_block();
        let join = self.new_block();
        self.set_term(mir::Terminator::If(
            mir::Operand::var(nonempty),
            some_blk,
            none_blk,
        ));
        self.current = some_blk;
        let idx = if method == "first" {
            mir::Operand::Const(0)
        } else {
            let i = self.fresh_temp("long long");
            self.emit(
                i.clone(),
                mir::Rvalue::Binary(
                    mir::BinOp::Sub,
                    mir::Operand::var(len),
                    mir::Operand::Const(1),
                ),
            );
            mir::Operand::var(i)
        };
        let item = self.fresh_temp(ae.clone());
        self.emit(
            item.clone(),
            mir::Rvalue::Index(mir::Operand::var(data), idx),
        );
        self.emit(
            r.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![
                    ("tag".to_string(), mir::Operand::Const(0)),
                    ("f0".to_string(), mir::Operand::var(item)),
                ],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));
        self.current = none_blk;
        self.emit(
            r.clone(),
            mir::Rvalue::StructLit(
                "Option".to_string(),
                vec![("tag".to_string(), mir::Operand::Const(1))],
            ),
        );
        self.set_term(mir::Terminator::Goto(join));
        self.current = join;
        Ok(Some(mir::Operand::var(r)))
    }

    /// `v.extend(seq)` on a vector local: a loop that pushes each item of `seq` (a
    /// sequence, or `seq.drain(..)`, which also empties its source).
    fn lower_vec_extend(&mut self, recv: &Expr, arg: &Expr) -> Result<Option<mir::Operand>> {
        let (vname, elem, vptr) = match self.vec_local(recv) {
            Some(v) => v,
            None => return Ok(None),
        };
        // The source: `x.drain(..)` (emptied afterwards) or a sequence.
        // Adapters that only re-walk the same items (`x.iter().cloned()`) are skipped.
        let mut src = arg;
        let mut drain = false;
        loop {
            match &src.kind {
                ExprKind::MethodCall(r, m, a) if m.node == "drain" && a.len() == 1 => {
                    src = &**r;
                    drain = true;
                    break;
                }
                ExprKind::MethodCall(r, m, a)
                    if a.is_empty()
                        && matches!(
                            m.node.as_str(),
                            "iter" | "cloned" | "copied" | "into_iter"
                        ) =>
                {
                    src = &**r;
                }
                _ => break,
            }
        }
        if !self.is_sequence(src) {
            return Err(LowerError("`extend` needs a sequence argument".into()));
        }
        let (an, ae, ap) = self.chain_sequence(src)?;
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(an.clone()), "len".into(), ap),
        );
        let data = self.fresh_temp(format!("{ae}*"));
        self.emit(
            data.clone(),
            mir::Rvalue::Field(mir::Operand::var(an.clone()), "data".into(), ap),
        );
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
                mir::Operand::var(len),
            ),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));
        self.current = body_blk;
        let item = self.fresh_temp(ae.clone());
        self.emit(
            item.clone(),
            mir::Rvalue::Index(mir::Operand::var(data), mir::Operand::var(idx.clone())),
        );
        // The receiver's pointer, then the push.
        let ptr = if vptr {
            mir::Operand::var(vname.clone())
        } else {
            let p = self.fresh_temp(format!("{}*", mir::vec_type(&elem)));
            self.emit(
                p.clone(),
                mir::Rvalue::Ref(mir::Operand::var(vname.clone()), true),
            );
            mir::Operand::var(p)
        };
        let pushed = self.fresh_temp("long long");
        self.emit(
            pushed,
            mir::Rvalue::Call(
                mir::Operand::Func(mir::vec_fn_name(&elem, "push")),
                vec![ptr, mir::Operand::var(item)],
            ),
        );
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
        if drain {
            self.emit(
                "__scratch".to_string(),
                mir::Rvalue::StoreField(
                    mir::Operand::var(an),
                    "len".to_string(),
                    mir::Operand::Const(0),
                    ap,
                ),
            );
        }
        Ok(Some(mir::Operand::Unit))
    }

    /// Copy the vector `src` (a local or a field) into a fresh vector, item by item.
    /// Returns the copy's name and element type, or `None` when `src` is not a
    /// vector the lowering can see.
    fn copy_sequence(&mut self, src: &Expr) -> Result<Option<(String, String)>> {
        let Some(hint) = self.hint_of_expr(src) else {
            return Ok(None);
        };
        let Some(elem) = mir::vec_elem(&hint).map(|s| s.to_string()) else {
            return Ok(None);
        };
        let (sname, _, sptr) = self.chain_sequence(src)?;
        let copy = self.fresh_temp(mir::vec_type(&elem));
        self.emit(
            copy.clone(),
            mir::Rvalue::Call(mir::Operand::Func(mir::vec_fn_name(&elem, "new")), vec![]),
        );
        let len = self.fresh_temp("long long");
        self.emit(
            len.clone(),
            mir::Rvalue::Field(mir::Operand::var(sname.clone()), "len".into(), sptr),
        );
        let data = self.fresh_temp(format!("{elem}*"));
        self.emit(
            data.clone(),
            mir::Rvalue::Field(mir::Operand::var(sname), "data".into(), sptr),
        );
        let idx = self.fresh_temp("long long");
        self.emit(idx.clone(), mir::Rvalue::Use(mir::Operand::Const(0)));
        let head = self.new_block();
        let body = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));
        self.current = head;
        let more = self.fresh_temp("long long");
        self.emit(
            more.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Lt,
                mir::Operand::var(idx.clone()),
                mir::Operand::var(len),
            ),
        );
        self.set_term(mir::Terminator::If(mir::Operand::var(more), body, exit));
        self.current = body;
        let item = self.fresh_temp(elem.clone());
        self.emit(
            item.clone(),
            mir::Rvalue::Index(mir::Operand::var(data), mir::Operand::var(idx.clone())),
        );
        self.emit_vec_push(&copy, &elem, mir::Operand::var(item));
        self.emit(
            idx.clone(),
            mir::Rvalue::Binary(
                mir::BinOp::Add,
                mir::Operand::var(idx),
                mir::Operand::Const(1),
            ),
        );
        self.set_term(mir::Terminator::Goto(head));
        self.current = exit;
        Ok(Some((copy, elem)))
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
        let mut scope: Option<PatternScope> = None;
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
            scope = Some(self.open_pattern_scope(pat, item, &item_ty)?);
        }
        self.lower_block_value(body)?;
        if let Some(sc) = scope {
            self.close_pattern_scope(sc);
        }
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
        if si.drain {
            // `drain(..)` removes the items: the vector is empty afterwards.
            self.emit(
                "__scratch".to_string(),
                mir::Rvalue::StoreField(
                    mir::Operand::var(name),
                    "len".to_string(),
                    mir::Operand::Const(0),
                    is_ptr,
                ),
            );
        }
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
                    .or_else(|| match mir::split_generic(elem_ct) {
                        // A tuple type string names its element types.
                        Some(("Tuple", args)) => args.get(i).map(|s| s.to_string()),
                        _ => None,
                    })
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
        // A constructor (`Ok(x)`) is typed by its bare enum; a function returning an
        // instance of that enum gives the arguments (`return Ok(x)`, a tail `Ok(x)`).
        let hint = match (hint.as_str(), self.func.ret.clone()) {
            ("Option" | "Result", Some(ret))
                if mir::split_generic(&ret).map(|(b, _)| b) == Some(hint.as_str()) =>
            {
                ret
            }
            _ => hint,
        };
        let current = self.temp_type_of(&name);
        let refines_enum =
            (current == "Option" || current == "Result") && hint.starts_with(&current);
        // A struct the program defines is hinted by its name.
        let defined = self.structs.iter().any(|s| s.name == hint);
        if (hint_applies(&hint) || defined) && (current == "long long" || refines_enum) {
            self.types.insert(name, hint);
        }
    }

    /// The type hint of `e`, if Lachesis recorded one.
    /// The pointer type of the variable a single-name expression names, if it is a pointer.
    fn pointer_local_type(&self, e: &Expr) -> Option<String> {
        let ExprKind::Path(p) = &e.kind else {
            return None;
        };
        if p.segments.len() != 1 {
            return None;
        }
        let n = self.var_name(&p.segments[0].node);
        self.types
            .get(&n)
            .cloned()
            .or_else(|| {
                self.func
                    .locals
                    .iter()
                    .find(|(l, _)| *l == n)
                    .and_then(|(_, t)| t.clone())
            })
            .filter(|t| t.ends_with('*'))
    }

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
            // A string literal is a C string constant (`"hi"` is a `char*`).
            ExprKind::Lit(ast::Lit::Str(text)) => Ok(mir::Operand::CStr(text.clone())),
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
                    // `None` as an `Option<T>` (the checker's type names the instance).
                    let ty = match self.hint_of_expr(e) {
                        Some(h) if mir::split_generic(&h).map_or(false, |(b, _)| b == ename) => h,
                        _ => ename.clone(),
                    };
                    let t = self.fresh_temp(ty);
                    self.emit(
                        t.clone(),
                        mir::Rvalue::StructLit(
                            ename,
                            vec![("tag".to_string(), mir::Operand::Const(tag))],
                        ),
                    );
                    return Ok(mir::Operand::var(t));
                }
                Ok(mir::Operand::var(self.var_name(&name)))
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
                if matches!(mop, mir::UnOp::Deref) {
                    // Reading through a non-pointer (a match-ergonomics binding) is the value.
                    let oty = match &o {
                        mir::Operand::Var(n) => self.temp_type_of(n),
                        _ => String::new(),
                    };
                    if oty != "long long" && oty.strip_suffix('*').is_none() {
                        return Ok(o);
                    }
                    let pointee = oty.strip_suffix('*').unwrap_or("long long");
                    let t = self.fresh_temp(pointee.to_string());
                    self.emit(t.clone(), mir::Rvalue::Unary(mop, o));
                    return Ok(mir::Operand::var(t));
                }
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Unary(mop, o));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Call(callee, args) => {
                if let ExprKind::Path(p) = &callee.kind {
                    // `std::fs::read_to_string(path)`: a `Result<String, _>` built from the
                    // runtime's read (NULL when the file cannot be read).
                    let segs: Vec<&str> = p.segments.iter().map(|s| s.node.as_str()).collect();
                    if args.len() == 1
                        && segs.last() == Some(&"read_to_string")
                        && (segs.len() == 1 || segs[segs.len() - 2] == "fs")
                    {
                        let path = self.lower_expr(&args[0])?;
                        let text = self.fresh_temp("char*");
                        self.emit(
                            text.clone(),
                            mir::Rvalue::Call(
                                mir::Operand::Func("moira_fs_read_to_string".to_string()),
                                vec![path],
                            ),
                        );
                        let result = self.fresh_temp("Result<str,long long>");
                        let failed = self.fresh_temp("long long");
                        self.emit(
                            failed.clone(),
                            mir::Rvalue::Binary(
                                mir::BinOp::Eq,
                                mir::Operand::var(text.clone()),
                                mir::Operand::Const(0),
                            ),
                        );
                        let ok_blk = self.new_block();
                        let err_blk = self.new_block();
                        let join = self.new_block();
                        self.set_term(mir::Terminator::If(
                            mir::Operand::var(failed),
                            err_blk,
                            ok_blk,
                        ));
                        self.current = ok_blk;
                        self.emit(
                            result.clone(),
                            mir::Rvalue::StructLit(
                                "Result".to_string(),
                                vec![
                                    ("tag".to_string(), mir::Operand::Const(0)),
                                    ("f0".to_string(), mir::Operand::var(text)),
                                ],
                            ),
                        );
                        self.set_term(mir::Terminator::Goto(join));
                        self.current = err_blk;
                        self.emit(
                            result.clone(),
                            mir::Rvalue::StructLit(
                                "Result".to_string(),
                                vec![
                                    ("tag".to_string(), mir::Operand::Const(1)),
                                    (payload_field("Result", 1, 0), mir::Operand::Const(0)),
                                ],
                            ),
                        );
                        self.set_term(mir::Terminator::Goto(join));
                        self.current = join;
                        return Ok(mir::Operand::var(result));
                    }
                    if let Some(v) = self.lower_assoc_call(e, p, args)? {
                        return Ok(v);
                    }
                }
                // `T::default()` / `Default::default()`, typed by the checker.
                if let ExprKind::Path(p) = &callee.kind {
                    if p.segments.last().map_or(false, |s| s.node == "default") {
                        if let Some(v) = self.lower_default_call(e, args)? {
                            return Ok(v);
                        }
                    }
                }
                // `String::from(s)`: a string is already its `char*`, so the value is `s`.
                if let ExprKind::Path(p) = &callee.kind {
                    let segs: Vec<&str> = p.segments.iter().map(|s| s.node.as_str()).collect();
                    if segs == ["String", "from"] && args.len() == 1 {
                        let v = self.lower_expr(&args[0])?;
                        return self.heap_string(v);
                    }
                    if segs == ["String", "new"] && args.is_empty() {
                        return self.heap_string(mir::Operand::CStr(String::new()));
                    }
                }
                // `Box::new(v)`: a heap copy of `v`, typed as a pointer to its type.
                if let ExprKind::Path(p) = &callee.kind {
                    let segs: Vec<&str> = p.segments.iter().map(|s| s.node.as_str()).collect();
                    if segs == ["Box", "new"] && args.len() == 1 {
                        let v = self.lower_expr(&args[0])?;
                        let elem = match &v {
                            mir::Operand::Var(n) => self.temp_type_of(n),
                            _ => "long long".to_string(),
                        };
                        let t = self.fresh_temp(format!("{elem}*"));
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Call(mir::Operand::Func("moira_box_new".to_string()), vec![v]),
                        );
                        return Ok(mir::Operand::var(t));
                    }
                }
                // `Vec::new()` / `Vec::with_capacity(n)` in an expression position (a
                // struct field, an argument): the checker's hint gives the element type.
                if let Some(vec_ty) = vec_constructor(e) {
                    if let Some(elem) = self.hint_of_expr(e).as_deref().and_then(mir::vec_elem) {
                        let elem = elem.to_string();
                        let t = self.fresh_temp(mir::vec_type(&elem));
                        self.emit(
                            t.clone(),
                            mir::Rvalue::Call(
                                mir::Operand::Func(mir::vec_fn_name(&elem, vec_ty)),
                                vec![],
                            ),
                        );
                        return Ok(mir::Operand::var(t));
                    }
                }
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
                            return self.lower_variant_construction(&ename, tag, arity, args, self.hint_of_expr(e));
                        }
                    }
                }
                if let Some(v) = self.lower_specialized_call(callee, args)? {
                    return Ok(v);
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
                            _ => {
                                return Err(LowerError(format!(
                                    "call target must be a path (bytes {}..{})",
                                    callee.span.start, callee.span.end
                                )))
                            }
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
                            operands.push(mir::Operand::Var(self.var_name(&cap)));
                        }
                    }
                }
                let param_tys = match &callee_operand {
                    mir::Operand::Func(f) => self.fn_param_tys.get(f).cloned(),
                    _ => None,
                };
                for a in args {
                    let v = self.lower_expr(a)?;
                    let pt = param_tys.as_ref().and_then(|p| p.get(operands.len())).cloned();
                    let v = match pt {
                        Some(pt) => self.adapt_arg(v, &pt),
                        None => v,
                    };
                    operands.push(v);
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
                // `&p.field` through a pointer `p` points into the struct, rather than
                // at a copy of the field (which would dangle once returned).
                if let ExprKind::Field(base, field) = &e.kind {
                    if let ExprKind::Path(p) = &base.kind {
                        if p.segments.len() == 1 {
                            let bn = self.var_name(&p.segments[0].node);
                            if self.temp_type_of(&bn).ends_with('*') {
                                let v = self.lower_expr(e)?;
                                let inner = self.temp_type_of(&name_of(&v));
                                let t = self.fresh_temp(format!("{}*", inner));
                                self.emit(
                                    t.clone(),
                                    mir::Rvalue::FieldAddr(
                                        mir::Operand::var(bn),
                                        field.node.clone(),
                                        true,
                                    ),
                                );
                                return Ok(mir::Operand::var(t));
                            }
                        }
                    }
                }
                let o = self.lower_expr(e)?;
                // A reference needs a place: a constant is first copied into a temporary.
                let o = match o {
                    mir::Operand::Var(_) => o,
                    other => {
                        let ty = match &other {
                            mir::Operand::CStr(_) => "char*",
                            _ => "long long",
                        };
                        let t = self.fresh_temp(ty);
                        self.emit(t.clone(), mir::Rvalue::Use(other));
                        mir::Operand::var(t)
                    }
                };
                let inner = self.temp_type_of(&name_of(&o));
                if mir::slice_elem(&inner).is_some() {
                    return Ok(o);
                }
                let t = self.fresh_temp(format!("{}*", inner));
                self.emit(t.clone(), mir::Rvalue::Ref(o, *mutable));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Struct(path, fields, _base) => {
                // `E::V { a, b }`: a struct-like variant, built in declaration order.
                let segs: Vec<&str> = path.segments.iter().map(|s| s.node.as_str()).collect();
                if let Some((ename, tag, arity)) = self.struct_variant_of(&segs) {
                    let names = self
                        .variant_fields
                        .get(&(ename.clone(), tag))
                        .cloned()
                        .unwrap_or_default();
                    let mut args: Vec<ast::Expr> = Vec::new();
                    for n in &names {
                        match fields.iter().find(|(f, _)| f.node == *n) {
                            Some((_, e)) => args.push(e.clone()),
                            None => {
                                return Err(LowerError(format!(
                                    "variant literal is missing field `{n}`"
                                )))
                            }
                        }
                    }
                    return self.lower_variant_construction(&ename, tag, arity, &args, self.hint_of_expr(e));
                }
                let mut name = path
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                if name == "Self" {
                    if let Some(t) = &self.self_ty {
                        name = t.clone();
                    }
                }
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
                // `for x in &arr` iterates the same array as `for x in arr`.
                let arr_expr: &Expr = match &iter.kind {
                    ExprKind::Ref(i, _) => &**i,
                    _ => iter,
                };
                let array_len: Option<i128> = match &arr_expr.kind {
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
                // `for x in a.iter().chain(b.iter())`: the first sequence's loop, then the second's.
                if let ExprKind::MethodCall(cr, cm, cargs) = &iter.kind {
                    if cm.node == "chain" && cargs.len() == 1 {
                        let left_e: &Expr = match &cr.kind {
                            ExprKind::MethodCall(r, m, a) if m.node == "iter" && a.is_empty() => &**r,
                            _ => &**cr,
                        };
                        let right_e: &Expr = match &cargs[0].kind {
                            ExprKind::MethodCall(r, m, a) if m.node == "iter" && a.is_empty() => &**r,
                            _ => &cargs[0],
                        };
                        if self.is_sequence(left_e) && self.is_sequence(right_e) {
                            let (ln, le, lp) = self.chain_sequence(left_e)?;
                            let first = SeqIter { seq: left_e, enumerate: false, rev: false, mutable: false, chars: false, drain: false, indices: false, lines: false };
                            self.lower_seq_for(&ln, &le, lp, pat, body, &first)?;
                            let (rn, re, rp) = self.chain_sequence(right_e)?;
                            let second = SeqIter { seq: right_e, enumerate: false, rev: false, mutable: false, chars: false, drain: false, indices: false, lines: false };
                            return self.lower_seq_for(&rn, &re, rp, pat, body, &second);
                        }
                    }
                }
                // `for (a, b) in x.iter().zip(y.iter())` over two named sequences.
                if let ExprKind::MethodCall(zr, zm, zargs) = &iter.kind {
                    if zm.node == "zip" && zargs.len() == 1 {
                        let left_e = match &zr.kind {
                            ExprKind::MethodCall(r, m, a) if m.node == "iter" && a.is_empty() => &**r,
                            _ => &**zr,
                        };
                        let right_e = match &zargs[0].kind {
                            ExprKind::MethodCall(r, m, a) if m.node == "iter" && a.is_empty() => &**r,
                            _ => &zargs[0],
                        };
                        if self.is_sequence(left_e) && self.is_sequence(right_e) {
                            let l = self.chain_sequence(left_e)?;
                            let r = self.chain_sequence(right_e)?;
                            return self.lower_zip_for(l, r, pat, body);
                        }
                    }
                }
                // A map: `for (k, v) in &m`, `m.iter()`, `m.keys()`, `m.values()`.
                let (map_inner, which): (&Expr, MapIter) = match &iter.kind {
                    ExprKind::Ref(i, _) => (&**i, MapIter::Pairs),
                    ExprKind::MethodCall(r, m, a) if m.node == "iter" && a.is_empty() => {
                        (&**r, MapIter::Pairs)
                    }
                    ExprKind::MethodCall(r, m, a) if m.node == "keys" && a.is_empty() => {
                        (&**r, MapIter::Keys)
                    }
                    ExprKind::MethodCall(r, m, a) if m.node == "values" && a.is_empty() => {
                        (&**r, MapIter::Values)
                    }
                    _ => (iter, MapIter::Pairs),
                };
                let map_here = match self.map_local(map_inner) {
                    Some(m) => Some(m),
                    None => self.map_from_hint(map_inner)?,
                };
                if let Some((mname, k, v, is_ptr)) = map_here {
                    let sorted = self.is_sorted_expr(map_inner);
                    return self.lower_map_for(&mname, &k, &v, is_ptr, pat, body, which, sorted);
                }
                let si = seq_iter(iter);
                // `for x in seq.clone()`: iterate a snapshot, so the body may still change `seq`.
                if let ExprKind::MethodCall(r, m, a) = &iter.kind {
                    if m.node == "clone" && a.is_empty() {
                        if let Some((copy, elem)) = self.copy_sequence(r)? {
                            return self.lower_seq_for(&copy, &elem, false, pat, body, &si);
                        }
                    }
                }
                if si.lines {
                    return self.lower_lines_for(si.seq, pat, body);
                }
                if si.chars {
                    return self.lower_str_for(si.seq, pat, body, si.indices);
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
                        let arr_op = self.lower_expr(arr_expr)?;
                        // Element C type: for an array literal, infer it from the
                        // first element (so tuple-element arrays bind to the right
                        // component types); otherwise default to `long long`.
                        let elem_ct = match &arr_expr.kind {
                            ExprKind::Array(elems) if !elems.is_empty() => {
                                let e0 = self.lower_expr(&elems[0])?;
                                self.temp_type_of(&name_of(&e0))
                            }
                            // A `let`-bound array remembers its element type (an enum for
                            // an array of variants).
                            ExprKind::Path(p) => p
                                .segments
                                .last()
                                .and_then(|s| self.array_elems.get(&s.node).cloned())
                                .unwrap_or_else(|| "long long".into()),
                            _ => "long long".into(),
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
                        let scope = self.open_pattern_scope(pat, mir::Operand::var(elem), &elem_ct)?;
                        self.lower_block_value(body)?;
                        self.close_pattern_scope(scope);
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
                if let Some(v) = self.lower_value_method(e, recv, &method.node, args)? {
                    return Ok(v);
                }
                if let Some(v) = self.lower_str_method(e, recv, &method.node, args)? {
                    return Ok(v);
                }
                if let Some(v) = self.lower_user_method(e, recv, &method.node, args)? {
                    return Ok(v);
                }
                if let Some(v) = self.lower_map_method(recv, &method.node, args)? {
                    return Ok(v);
                }
                if method.node == "extend" && args.len() == 1 {
                    if let Some(v) = self.lower_vec_extend(recv, &args[0])? {
                        return Ok(v);
                    }
                }
                if let Some(v) = self.lower_enum_method(recv, &method.node, args)? {
                    return Ok(v);
                }
                if let Some(v) = self.lower_first_last(recv, &method.node, args)? {
                    return Ok(v);
                }
                // `opt.take()` on an `Option<T>` local: the value, leaving `None` behind.
                if method.node == "take" && args.is_empty() {
                    if let ExprKind::Path(p) = &recv.kind {
                        if p.segments.len() == 1 {
                            let name = p.segments[0].node.clone();
                            let ty = self.temp_type_of(&name);
                            if mir::split_generic(&ty).map(|(b, _)| b) == Some("Option") {
                                let old = self.fresh_temp(ty.clone());
                                self.emit(old.clone(), mir::Rvalue::Use(mir::Operand::var(name.clone())));
                                self.emit(
                                    name.clone(),
                                    mir::Rvalue::StructLit(
                                        "Option".to_string(),
                                        vec![("tag".to_string(), mir::Operand::Const(1))],
                                    ),
                                );
                                return Ok(mir::Operand::var(old));
                            }
                        }
                    }
                }
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
                            // The pushed value has the element type (a tuple takes it).
                            self.expect_ty = Some(elem.clone());
                            let o = self.lower_expr(a);
                            self.expect_ty = None;
                            operands.push(o?);
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
            // `s.field[i]` on a vector field: its data is read from the field's value.
            ExprKind::Index(base, idx)
                if matches!(base.kind, ExprKind::Field(..)) && self.field_vec_elem(base).is_some() =>
            {
                let elem = self.field_vec_elem(base).unwrap();
                let bv = self.lower_expr(base)?;
                let bname = match &bv {
                    mir::Operand::Var(n) => n.clone(),
                    _ => return Err(LowerError("a vector field index needs a named value".into())),
                };
                let i = self.lower_expr(idx)?;
                let data = self.fresh_temp(format!("{elem}*"));
                self.emit(
                    data.clone(),
                    mir::Rvalue::Field(mir::Operand::var(bname), "data".into(), false),
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
                let elem_ct = self.array_elem_ct(elems);
                self.emit(
                    t.clone(),
                    mir::Rvalue::Array(elem_ct, operands),
                );
                Ok(mir::Operand::var(t))
            }
            // `unsafe` only gates what the block may do; codegen treats it as a plain block.
            ExprKind::Unsafe(b) => self.lower_block_value(b),
            ExprKind::Closure { params, body, .. } => {
                // The lifted function takes the parameter types the closure was given.
                if let Some(h) = self.hint_of_expr(e) {
                    if let Some(("Fn", tys)) = mir::split_generic(&h) {
                        if tys.len() == params.len() + 1 {
                            self.closure_param_hint =
                                Some(tys[..params.len()].iter().map(|t| t.to_string()).collect());
                            self.closure_ret_hint = tys.last().map(|t| t.to_string());
                        }
                    }
                }
                self.lower_closure(params, body)
            }
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
                let expected = self.expect_ty.take();
                for e in elems {
                    let o = self.lower_expr(e)?;
                    let ct = match &o {
                        mir::Operand::Var(n) => self.temp_type_of(n),
                        _ => "long long".to_string(),
                    };
                    c_tys.push(ct);
                    operands.push(o);
                }
                // The checker's hint names the components when the context decides them
                // (`vec![(0, n)]` as a `Vec<(usize, usize)>`).
                let want = expected.or_else(|| self.hint_of_expr(e));
                let generic = match want.as_deref().and_then(mir::split_generic) {
                    Some(("Tuple", args)) if args.len() == c_tys.len() => {
                        Some(want.clone().unwrap_or_default())
                    }
                    _ => None,
                };
                // A tuple the context types as `Tuple<..>` is that generic instance.
                let tuple_ty = match generic {
                    Some(g) => g,
                    None => self.tuple_type(c_tys),
                };
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
                // A match-ergonomics binding of a scalar payload is already the value
                // (not a C pointer), so reading it through `*` is the value itself.
                let oty = match &o {
                    mir::Operand::Var(n) => self.temp_type_of(n),
                    _ => String::new(),
                };
                // An operand of unknown type keeps the dereference (it may be a pointer).
                if oty != "long long" && oty.strip_suffix('*').is_none() {
                    return Ok(o);
                }
                let pointee = oty.strip_suffix('*').unwrap_or("long long");
                let t = self.fresh_temp(pointee.to_string());
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
        let slots: Vec<(String, String)> = ctx.slots.clone();

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
    /// Bind a pattern's names as new variables of the current scope. Each name gets a
    /// unique C name; its type is set for the lowering of the scope's body. Returns what
    /// `leave_bindings` needs to end the scope.
    fn enter_bindings(
        &mut self,
        binds: &[(String, mir::Operand)],
    ) -> (HashMap<String, String>, Vec<(String, Option<String>)>) {
        let mut renames: HashMap<String, String> = HashMap::new();
        let mut saved: Vec<(String, Option<String>)> = Vec::new();
        for (name, val) in binds {
            let unique = format!("{name}__arm{}", self.arm_counter);
            self.arm_counter += 1;
            let ty = match val {
                mir::Operand::Var(v) => self.temp_type_of(v),
                _ => "long long".to_string(),
            };
            saved.push((name.clone(), self.types.get(name).cloned()));
            if ty != "long long" {
                self.types.insert(name.clone(), ty);
            } else {
                self.types.remove(name);
            }
            self.declare_bound_name(&unique, val);
            self.emit(unique.clone(), mir::Rvalue::Use(val.clone()));
            renames.insert(name.clone(), unique);
        }
        let mut shadowed: Vec<(String, Option<String>)> = Vec::new();
        for (name, unique) in &renames {
            shadowed.push((name.clone(), self.var_renames.get(name).cloned()));
            self.var_renames.insert(name.clone(), unique.clone());
        }
        self.renames_stack.push(shadowed);
        (renames, saved)
    }

    /// End a scope opened by `enter_bindings`: restore the names' types, and rename the
    /// blocks from `from` on (the scope's body) to the bindings' unique names.
    fn leave_bindings(
        &mut self,
        from: usize,
        renames: &HashMap<String, String>,
        saved: Vec<(String, Option<String>)>,
    ) {
        let blocks: Vec<usize> = (from..self.func.blocks.len()).collect();
        self.leave_bindings_on(&blocks, renames, saved);
    }

    /// `leave_bindings` for an explicit list of blocks.
    fn leave_bindings_on(
        &mut self,
        blocks: &[usize],
        renames: &HashMap<String, String>,
        saved: Vec<(String, Option<String>)>,
    ) {
        let popped: Vec<(String, Option<String>)> = match self.renames_stack.pop() {
            Some(x) => x,
            None => Vec::new(),
        };
        {
            // Each name appears once in a scope, so the order of the restores does not matter.
            for entry in popped {
                let name = entry.0;
                match entry.1 {
                    Some(n) => {
                        self.var_renames.insert(name, n);
                    }
                    None => {
                        self.var_renames.remove(&name);
                    }
                }
            }
        }
        for (name, old) in saved.into_iter().rev() {
            match old {
                Some(t) => {
                    self.types.insert(name, t);
                }
                None => {
                    self.types.remove(&name);
                }
            }
        }
        if !renames.is_empty() {
            for &b in blocks {
                rename_block(&mut self.func.blocks[b], renames);
            }
        }
    }

    /// The variables a pattern binds to `elem` (of C type `elem_ct`): the element itself for a
    /// name, or each named component of a tuple (a value read from the element).
    fn pattern_binds(
        &mut self,
        pat: &ast::Pat,
        elem: mir::Operand,
        elem_ct: &str,
    ) -> Result<Vec<(String, mir::Operand)>> {
        let ast::Pat::Tuple(elems) = pat else {
            let name = pat_name(pat)?;
            if name == "_" {
                return Ok(Vec::new());
            }
            return Ok(vec![(name, elem)]);
        };
        let mut out = Vec::new();
        for (i, e) in elems.iter().enumerate() {
            let name =
                match e {
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
                .or_else(|| match mir::split_generic(elem_ct) {
                    Some(("Tuple", args)) => args.get(i).map(|s| s.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "long long".to_string());
            let t = self.fresh_temp(ct);
            self.emit(
                t.clone(),
                mir::Rvalue::Field(elem.clone(), format!("f{i}"), false),
            );
            out.push((name, mir::Operand::var(t)));
        }
        Ok(out)
    }

    /// Open the scope of a loop's pattern: each bound name becomes a variable of its own.
    /// The scope is closed by `close_pattern_scope` once the body is lowered.
    fn open_pattern_scope(
        &mut self,
        pat: &ast::Pat,
        elem: mir::Operand,
        elem_ct: &str,
    ) -> Result<PatternScope> {
        let binds = self.pattern_binds(pat, elem, elem_ct)?;
        let cur = self.current;
        let (renames, saved) = self.enter_bindings(&binds);
        let created_from = self.func.blocks.len();
        Ok(PatternScope {
            cur,
            created_from,
            renames,
            saved,
        })
    }

    /// Close a scope opened by `open_pattern_scope`: the blocks of its body (the current block
    /// at the open, and every block created since) take the bindings' unique names.
    fn close_pattern_scope(&mut self, scope: PatternScope) {
        let mut blocks = vec![scope.cur];
        let end = self.func.blocks.len();
        for b in scope.created_from..end {
            blocks.push(b);
        }
        self.leave_bindings_on(&blocks, &scope.renames, scope.saved);
    }

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
            let then_start = then_blk;
            let (renames, saved) = self.enter_bindings(&binds);
            let v_then = self.lower_block_value(then_b)?;
            self.leave_bindings(then_start, &renames, saved);
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

        // Lower as a conditional on the pattern guard (always taken when the
        // pattern is irrefutable).
        let result = self.fresh_temp("long long");
        let then_blk = self.new_block();
        let else_blk = self.new_block();
        let merge = self.new_block();
        let cond = cond.unwrap_or(mir::Operand::Const(1));
        self.set_term(mir::Terminator::If(cond, then_blk, else_blk));

        self.current = then_blk;
        let then_start = then_blk;
        let (renames, saved) = self.enter_bindings(&binds);
        let v_then = self.lower_block_value(then_b)?;
        self.leave_bindings(then_start, &renames, saved);
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
        let (renames, saved) = self.enter_bindings(&binds);
        let body_start = body_blk;
        self.loops.push((exit, head));
        self.lower_block_value(body)?;
        self.loops.pop();
        self.leave_bindings(body_start, &renames, saved);
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
            // The blocks of this arm: from its test to the end of its body.
            let arm_start = self.func.blocks.len();
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
            // Each bound name is a new variable of this arm (arms may bind one name at
            // different types). Its uses in the body refer to it: by type while the body is
            // lowered, and by name once the arm's blocks are renamed.
            let mut renames: HashMap<String, String> = HashMap::new();
            let mut saved_types: Vec<(String, Option<String>)> = Vec::new();
            for (name, val) in &binds {
                let unique = format!("{name}__arm{}", self.arm_counter);
                self.arm_counter += 1;
                let ty = match val {
                    mir::Operand::Var(v) => self.temp_type_of(v),
                    _ => "long long".to_string(),
                };
                saved_types.push((name.clone(), self.types.get(name).cloned()));
                if ty != "long long" {
                    self.types.insert(name.clone(), ty);
                } else {
                    self.types.remove(name);
                }
                self.declare_bound_name(&unique, val);
                self.emit(unique.clone(), mir::Rvalue::Use(val.clone()));
                renames.insert(name.clone(), unique);
            }
            let v = self.lower_expr(body)?;
            self.emit(result.clone(), mir::Rvalue::Use(v));
            self.set_term(mir::Terminator::Goto(merge));
            for (name, old) in saved_types.into_iter().rev() {
                match old {
                    Some(t) => {
                        self.types.insert(name, t);
                    }
                    None => {
                        self.types.remove(&name);
                    }
                }
            }
            if !renames.is_empty() {
                for block in self.func.blocks[arm_start..].iter_mut() {
                    rename_block(block, &renames);
                }
            }
        }

        // The final fall-through block: no arm matched, which an exhaustive match never does.
        // It aborts (the result is never read), so no value of the result's type is needed.
        self.current = next_test;
        self.emit_panic_call();
        self.set_term(mir::Terminator::Goto(merge));

        self.current = merge;
        Ok(mir::Operand::var(result))
    }

    /// The C type of an expression that constructs an enum variant
    /// (`Some(..)` / `None` / user variants), i.e. `struct <Enum>` — `None`
    /// when the expression is not a variant construction.
    /// The C element type of an array literal: the enum when its first element is a
    /// variant (`[T::Add, T::Sub]`), else `long long`.
    fn array_elem_ct(&self, elems: &[ast::Expr]) -> String {
        if let Some(ExprKind::Lit(ast::Lit::Str(_))) = elems.first().map(|e| &e.kind) {
            return "char*".to_string();
        }
        let path = match elems.first().map(|e| &e.kind) {
            Some(ExprKind::Path(p)) => p,
            Some(ExprKind::Call(callee, _)) => match &callee.kind {
                ExprKind::Path(p) => p,
                _ => return "long long".to_string(),
            },
            _ => return "long long".to_string(),
        };
        self.variant_info(path)
            .map(|(ename, _, _)| ename)
            .unwrap_or_else(|| "long long".to_string())
    }

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
                    // The element type is the expected type of each item (a tuple takes it).
                    self.expect_ty = Some(elem.clone());
                    let op = self.lower_expr(it);
                    self.expect_ty = None;
                    out_ops.push((op?, 0));
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
                let mut placeholders: Vec<&ast::Expr> = rest;
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
                            mir::Operand::Func("__moira_w_str".to_string()),
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
                            t if t.contains("char*") => "__moira_w_str",
                            "str" => "__moira_w_str",
                            "bool" => "__moira_w_bool",
                            "double" => "__moira_w_f64",
                            _ => "__moira_w_i64",
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
                                mir::Operand::Func("__moira_w_i64".to_string()),
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
                            mir::Operand::Func("__moira_w_str".to_string()),
                            vec![mir::Operand::CStr("\n".to_string())],
                        ),
                    );
                }
                Ok(mir::Operand::Unit)
            }
            "format" => {
                // `format!("a{}b", x)`: a new heap string of the literal pieces, with each
                // placeholder's argument converted to text (`{}` only).
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
                let Some(f) = fmt else {
                    return Err(LowerError("`format!` needs a literal format string".into()));
                };
                let segments: Vec<String> = f.split("{}").map(|s| s.to_string()).collect();
                let out = self.heap_string(mir::Operand::CStr(String::new()))?;
                let out_name = match &out {
                    mir::Operand::Var(n) => n.clone(),
                    _ => return Err(LowerError("`format!` into a string".into())),
                };
                for (i, seg) in segments.iter().enumerate() {
                    if i > 0 {
                        let Some(a) = rest.get(i - 1) else {
                            return Err(LowerError(
                                "`format!` has more placeholders than arguments".into(),
                            ));
                        };
                        let op = self.lower_expr(a)?;
                        // The checker's type for the argument decides (a `true` is a constant).
                        let ty = match self.hint_of_expr(a) {
                            Some(h) => h,
                            None => match &op {
                                mir::Operand::Var(n) => self.temp_type_of(n),
                                _ => "long long".to_string(),
                            },
                        };
                        let piece = if ty.contains("char*") || ty == "str" {
                            op
                        } else {
                            let helper = if ty == "bool" {
                                "moira_bool_str"
                            } else {
                                "moira_int_to_string"
                            };
                            let t = self.fresh_temp("char*");
                            self.emit(
                                t.clone(),
                                mir::Rvalue::Call(mir::Operand::Func(helper.to_string()), vec![op]),
                            );
                            mir::Operand::var(t)
                        };
                        self.push_to_string(&out_name, piece);
                    }
                    if !seg.is_empty() {
                        self.push_to_string(&out_name, mir::Operand::CStr(seg.clone()));
                    }
                }
                Ok(out)
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
        want: Option<String>,
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
            fields.push((payload_field(ename, tag, i), v));
        }
        // `Some(x)` is an `Option<T>` when the payload's type is known; the struct
        // literal takes its C struct from the destination type (see the codegen).
        // The checker's type for the whole expression names the enum instance (`Some(3)`
        // as an `Option<i64>`); failing that, a variable payload's type does.
        let hinted =
            want.filter(|w| mir::split_generic(w).map_or(false, |(base, _)| base == ename));
        let payload_ty = match (hinted, fields.get(1)) {
            (Some(w), _) => Some(w),
            (None, Some((_, mir::Operand::Var(n)))) if ename == "Option" => {
                let t = self.temp_type_of(n);
                if t == "long long" {
                    None
                } else {
                    Some(format!("Option<{t}>"))
                }
            }
            _ => None,
        };
        let t = self.fresh_temp(payload_ty.unwrap_or_else(|| ename.to_string()));
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
        let segs: Vec<&str> = path.segments.iter().map(|s| s.node.as_str()).collect();
        self.variant_info_names(&segs)
    }

    /// `variant_info` for a path given as its segment names.
    fn variant_info_names(&self, segs: &[&str]) -> Option<(String, i128, usize)> {
        let n = segs.len();
        if n >= 2 {
            let key = format!("{}::{}", segs[n - 2], segs[n - 1]);
            if let Some(v) = self.qualified_variants.get(&key) {
                return Some(v.clone());
            }
        }
        segs.last()
            .and_then(|last| self.variants.get(*last).cloned())
    }

    /// A struct-like variant named by a literal or pattern path (`E::V { .. }`), with
    /// its enum, tag and arity. `None` for a struct, or a name that is no variant.
    fn struct_variant_of(&self, segs: &[&str]) -> Option<(String, i128, usize)> {
        if segs.len() == 1 && self.struct_names.contains(segs[0]) {
            return None;
        }
        let (ename, tag, arity) = self.variant_info_names(segs)?;
        self.variant_fields
            .contains_key(&(ename.clone(), tag))
            .then_some((ename, tag, arity))
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

    /// The C type of payload field `i` of variant `tag` of `ename`, for a scrutinee
    /// operand: a prelude `Option<T>`/`Result<T, E>` gives its payload types; a user
    /// enum uses its declared payload types.
    fn payload_ty_of(&self, scrut: &mir::Operand, ename: &str, tag: i128, i: usize) -> String {
        if is_prelude_enum(ename) {
            let st = match scrut {
                mir::Operand::Var(n) => self.temp_type_of(n),
                _ => String::new(),
            };
            // A scrutinee behind a reference (`match &mut opt`) has the pointer type.
            let st = st.trim_end_matches('*').to_string();
            if let Some((base, args)) = mir::split_generic(&st) {
                if base == ename {
                    let k = if ename == "Result" && tag == 1 { 1 } else { 0 };
                    if let Some(t) = args.get(k) {
                        if i == 0 {
                            return t.to_string();
                        }
                    }
                }
            }
            return "long long".to_string();
        }
        self.payload_ty(ename, tag, i)
    }

    /// The C type of payload field `i` of variant `tag` of `ename` (`long long` when
    /// the enum is prelude or the type is not known here).
    fn payload_ty(&self, ename: &str, tag: i128, i: usize) -> String {
        self.payload_tys
            .get(&(ename.to_string(), tag))
            .and_then(|v| v.get(i))
            .cloned()
            .unwrap_or_else(|| "long long".to_string())
    }

    /// Declare a name bound by a pattern to the type of the value it takes, so a
    /// payload of a typed enum keeps its type through the binding.
    fn declare_bound_name(&mut self, name: &str, val: &mir::Operand) {
        let ty = match val {
            mir::Operand::Var(v) => self.temp_type_of(v),
            _ => "long long".to_string(),
        };
        if !self.func.locals.iter().any(|(n, _)| n == name) {
            self.func.locals.push((name.to_string(), Some(ty.clone())));
        }
        if ty != "long long" {
            self.types.insert(name.to_string(), ty);
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
            let pty = self.payload_ty_of(scrut, &ename, tag, i);
            let field_t = self.fresh_temp(pty);
            self.emit(
                field_t.clone(),
                mir::Rvalue::Field(scrut.clone(), payload_field(&ename, tag, i), scrut_ptr),
            );
            let fv = mir::Operand::var(field_t);
            let (_c, mut b) = self.pattern_guard(&fv, sp)?;
            binds.append(&mut b);
        }
        Ok((Some(mir::Operand::var(cond)), binds))
    }

    /// The tag-test guard and bindings for a struct-like variant pattern
    /// (`E::V { a, b: pat, .. }`): each named field reads its payload slot.
    fn struct_variant_guard(
        &mut self,
        scrut: &mir::Operand,
        ename: &str,
        tag: i128,
        names: &[String],
        fields: &[(String, ast::Pat)],
    ) -> Result<(Option<mir::Operand>, Vec<(String, mir::Operand)>)> {
        let scrut_ptr = self.scrutinee_is_pointer(scrut);
        self.type_enum_operand(scrut, ename);
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
        let mut binds = Vec::new();
        for (fname, sp) in fields {
            let i = names
                .iter()
                .position(|n| n == fname)
                .ok_or_else(|| LowerError(format!("variant has no field `{fname}`")))?;
            let pty = self.payload_ty_of(scrut, ename, tag, i);
            let field_t = self.fresh_temp(pty);
            self.emit(
                field_t.clone(),
                mir::Rvalue::Field(scrut.clone(), payload_field(ename, tag, i), scrut_ptr),
            );
            let (_c, mut b) = self.pattern_guard(&mir::Operand::var(field_t), sp)?;
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
                // The scrutinee's element types, when it is a tuple of a known shape.
                let scrut_tuple = match scrut {
                    mir::Operand::Var(n) => self.temp_type_of(n),
                    _ => String::new(),
                };
                let scrut_tuple = scrut_tuple.trim_end_matches('*').to_string();
                for (i, e) in elems.iter().enumerate() {
                    if let ast::Pat::Ident(n) = e {
                        let elem_ty = self
                            .tuple_structs
                            .get(&scrut_tuple)
                            .and_then(|st| st.fields.get(i))
                            .map(|(_, t)| t.clone())
                            .or_else(|| match mir::split_generic(&scrut_tuple) {
                                Some(("Tuple", args)) => args.get(i).map(|t| t.to_string()),
                                _ => None,
                            })
                            .unwrap_or_else(|| "long long".to_string());
                        // (The bound temporary keeps the loose type: a name bound in two patterns of
                        // one function shares one C declaration.)
                        let t = self.fresh_temp(elem_ty);
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
            ast::Pat::Struct(name, fields) => {
                // `E::V { a, .. }`: a struct-like variant is tested by its tag, then its
                // named fields are read from their payload slots.
                let segs: Vec<&str> = name.split("::").collect();
                if let Some((ename, tag, _)) = self.struct_variant_of(&segs) {
                    let names = self
                        .variant_fields
                        .get(&(ename.clone(), tag))
                        .cloned()
                        .unwrap_or_default();
                    return self.struct_variant_guard(scrut, &ename, tag, &names, fields);
                }
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
            ast::Lit::Str(text) => mir::Operand::CStr(text.clone()),
            // Float literals are not yet supported in the codegen subset.
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
