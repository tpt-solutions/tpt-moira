//! Whole-program lowering: several crates become one MIR program, so one C
//! translation unit can be built from them.
//!
//! Each crate is lowered on its own, then its items are renamed so that names
//! from different crates cannot collide:
//!
//! * The **root** crate keeps its names (its `main` stays `main`).
//! * Every other crate's functions and structs are prefixed `<crate>__`.
//! * A reference to a name the unit does not define resolves to its owner: the
//!   root if the root defines it, otherwise the first extern crate that does.
//!
//! Prelude enums (`Option`, `Result`) and generated tuple/vector types are
//! identical in every crate, so they are shared and never renamed.

use crate::lower::{lower_program_with_enums, LowerError, TypeHints};
use crate::mir::{self, Operand, Rvalue};
use std::collections::{BTreeMap, BTreeSet};
use tpt_moira_clotho::ast::Program;

/// Names that are identical in every crate and so are shared, not renamed.
fn is_shared(name: &str) -> bool {
    name == "Option"
        || name == "Result"
        || name.starts_with("__tuple")
        || name.starts_with("moira_vec")
}

/// Where a name lives after bundling.
struct Namer {
    /// `unit index -> prefix` (`None` for the root).
    prefixes: Vec<Option<String>>,
    /// `name -> units that define it` (non-shared functions and structs).
    defined_by: BTreeMap<String, BTreeSet<usize>>,
}

impl Namer {
    /// The bundled name of `name` as seen from unit `unit`.
    fn target(&self, unit: usize, name: &str) -> String {
        let owners = match self.defined_by.get(name) {
            Some(o) => o,
            None => return name.to_string(),
        };
        // A unit's own definition wins.
        if owners.contains(&unit) {
            return self.own_name(unit, name);
        }
        // Otherwise the root's definition, then the single extern owner.
        if owners.contains(&0) {
            return name.to_string();
        }
        // Several extern crates define it (each instantiated the same generic struct, say):
        // it is named after the first of them, which emits the definition.
        match owners.iter().next() {
            Some(&owner) => self.own_name(owner, name),
            None => name.to_string(),
        }
    }

    fn own_name(&self, unit: usize, name: &str) -> String {
        match &self.prefixes[unit] {
            None => name.to_string(),
            Some(p) => format!("{p}__{name}"),
        }
    }
}

/// Rename one C identifier through `f`. A generated name such as `Result_Operand_LowerError`
/// is one identifier, so when it is not itself a name of the bundle, its `_`-separated runs
/// are renamed one by one (a run is renamed when `f` renames it).
fn rename_ident(ident: &str, f: &dyn Fn(&str) -> String) -> String {
    let whole = f(ident);
    if whole != ident {
        return whole;
    }
    let pieces: Vec<&str> = ident.split('_').collect();
    let mut chunks: Vec<String> = Vec::with_capacity(pieces.len());
    let mut i = 0;
    while i < pieces.len() {
        let mut taken = None;
        for j in (i + 1..=pieces.len()).rev() {
            let cand = pieces[i..j].join("_");
            if cand.is_empty() {
                continue;
            }
            let renamed = f(&cand);
            if renamed != cand {
                taken = Some((j, renamed));
                break;
            }
        }
        match taken {
            Some((j, renamed)) => {
                chunks.push(renamed);
                i = j;
            }
            None => {
                chunks.push(pieces[i].to_string());
                i += 1;
            }
        }
    }
    chunks.join("_")
}

/// Rename every identifier of a C-like type string through `f`.
fn rename_type_str(ty: &str, f: &dyn Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(ty.len());
    let mut ident = String::new();
    let flush = |ident: &mut String, out: &mut String| {
        if !ident.is_empty() {
            out.push_str(&rename_ident(ident, f));
            ident.clear();
        }
    };
    for ch in ty.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            ident.push(ch);
        } else {
            flush(&mut ident, &mut out);
            out.push(ch);
        }
    }
    flush(&mut ident, &mut out);
    out
}

fn rename_operand(op: &mut Operand, f: &dyn Fn(&str) -> String) {
    if let Operand::Func(n) = op {
        *n = rename_ident(n, f);
    }
}

fn rename_rvalue(rv: &mut Rvalue, f: &dyn Fn(&str) -> String) {
    match rv {
        Rvalue::Use(a)
        | Rvalue::Unary(_, a)
        | Rvalue::Field(a, _, _)
        | Rvalue::FieldAddr(a, _, _)
        | Rvalue::Ref(a, _) => {
            rename_operand(a, f)
        }
        Rvalue::Binary(_, a, b) | Rvalue::Index(a, b) | Rvalue::IndexRef(a, b) => {
            rename_operand(a, f);
            rename_operand(b, f);
        }
        Rvalue::Call(callee, args) => {
            rename_operand(callee, f);
            for a in args {
                rename_operand(a, f);
            }
        }
        Rvalue::StructLit(name, fields) => {
            *name = rename_type_str(name, f);
            for (_, op) in fields {
                rename_operand(op, f);
            }
        }
        Rvalue::StoreIndex(a, b, c) => {
            rename_operand(a, f);
            rename_operand(b, f);
            rename_operand(c, f);
        }
        Rvalue::StoreField(a, _, b, _) => {
            rename_operand(a, f);
            rename_operand(b, f);
        }
        Rvalue::Cast(a, ty) => {
            rename_operand(a, f);
            *ty = rename_type_str(ty, f);
        }
        Rvalue::Array(ty, elems) | Rvalue::Tuple(ty, elems) => {
            *ty = rename_type_str(ty, f);
            for e in elems {
                rename_operand(e, f);
            }
        }
    }
}

/// Rename the type strings of a list of named fields.
fn rename_field_types(fields: &mut Vec<(String, String)>, f: &dyn Fn(&str) -> String) {
    for (_, ty) in fields.iter_mut() {
        *ty = rename_type_str(ty, f);
    }
}

/// Apply `f` to every name in one crate's MIR program.
fn rename_program(p: &mut mir::MirProgram, f: &dyn Fn(&str) -> String) {
    for s in &mut p.structs {
        s.name = rename_ident(&s.name, f);
        for (_, ty) in &mut s.fields {
            *ty = rename_type_str(ty, f);
        }
    }
    for func in &mut p.functions {
        func.name = rename_ident(&func.name, f);
        if let Some(r) = &mut func.ret {
            *r = rename_type_str(r, f);
        }
        for (_, ty) in &mut func.locals {
            if let Some(t) = ty {
                *t = rename_type_str(t, f);
            }
        }
        let renamed_temps: Vec<(String, String)> = func
            .temp_types
            .iter()
            .map(|(k, v)| (k.clone(), rename_type_str(v, f)))
            .collect();
        func.temp_types = renamed_temps.into_iter().collect();
        for block in &mut func.blocks {
            for stmt in &mut block.stmts {
                rename_rvalue(&mut stmt.rvalue, f);
            }
        }
    }
    for a in &mut p.async_fns {
        a.fn_name = f(&a.fn_name);
        a.fut_name = f(&a.fut_name);
        a.poll_name = f(&a.poll_name);
        a.run_name = f(&a.run_name);
        a.output_ty = rename_type_str(&a.output_ty, f);
        rename_field_types(&mut a.fut_fields, f);
    }
}

/// Lowering cannot tell a function defined in another crate from a closure
/// value, so such a call is emitted as an indirect call through a variable.
/// Rewrite a call through a name that is not bound locally in the function, and
/// that names a function somewhere in the bundle, into a direct call.
fn resolve_extern_calls(p: &mut mir::MirProgram, fn_names: &BTreeSet<String>) {
    for func in &mut p.functions {
        let mut bound: BTreeSet<String> = func.params.iter().cloned().collect();
        bound.extend(func.locals.iter().map(|(n, _)| n.clone()));
        for block in &mut func.blocks {
            for stmt in &mut block.stmts {
                if let Rvalue::Call(callee, _) = &mut stmt.rvalue {
                    let target = match callee {
                        Operand::Var(n) if !bound.contains(n) && fn_names.contains(n) => {
                            Some(n.clone())
                        }
                        _ => None,
                    };
                    if let Some(n) = target {
                        *callee = Operand::Func(n);
                    }
                }
            }
        }
    }
}

/// Lower the root crate and its extern crates into one MIR program.
///
/// `externs` are `(crate name, program, hints)` triples; the crate name becomes
/// the prefix of that crate's items.
pub fn lower_bundle(
    root: &Program,
    root_hints: &TypeHints,
    externs: &[(String, Program, TypeHints)],
) -> Result<mir::MirProgram, LowerError> {
    let in_crate = |name: &str, e: LowerError| LowerError(format!("in crate `{name}`: {}", e.0));
    // Each unit also learns the enums of the other units (for patterns on them), but
    // only the owning unit emits an enum's struct.
    let mut asts: Vec<&Program> = vec![root];
    let mut hint_list: Vec<&TypeHints> = vec![root_hints];
    let mut names: Vec<String> = vec!["<root>".to_string()];
    for (name, prog, hints) in externs {
        asts.push(prog);
        hint_list.push(hints);
        names.push(name.clone());
    }
    let mut units: Vec<mir::MirProgram> = Vec::new();
    let mut prefixes: Vec<Option<String>> = Vec::new();
    for i in 0..asts.len() {
        let extra: Vec<&Program> = (0..asts.len())
            .filter(|&j| j != i)
            .map(|j| asts[j])
            .collect();
        let unit = lower_program_with_enums(asts[i], hint_list[i], &extra)
            .map_err(|e| in_crate(&names[i], e))?;
        units.push(unit);
        prefixes.push(if i == 0 { None } else { Some(names[i].clone()) });
    }

    // Which unit defines each non-shared function and struct.
    let mut defined_by: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (idx, unit) in units.iter().enumerate() {
        let mut names: Vec<String> = Vec::new();
        for f in &unit.functions {
            names.push(f.name.clone());
        }
        for s in &unit.structs {
            names.push(s.name.clone());
        }
        for n in names {
            if !is_shared(&n) {
                defined_by.entry(n).or_default().insert(idx);
            }
        }
    }
    let fn_names: BTreeSet<String> = units
        .iter()
        .flat_map(|u| u.functions.iter().map(|f| f.name.clone()))
        .filter(|n| !is_shared(n))
        .collect();
    for unit in units.iter_mut() {
        resolve_extern_calls(unit, &fn_names);
    }
    let namer = Namer {
        prefixes,
        defined_by,
    };

    let mut out = mir::MirProgram::default();
    for (idx, unit) in units.iter_mut().enumerate() {
        rename_program(unit, &|n: &str| namer.target(idx, n));
        for s in unit.structs.drain(..) {
            if !out.structs.iter().any(|t| t.name == s.name) {
                out.structs.push(s);
            }
        }
        out.functions.append(&mut unit.functions);
        out.async_fns.append(&mut unit.async_fns);
    }
    retype_call_results(&mut out);
    Ok(out)
}

/// A call's result is typed by the callee's return type, which only the unit defining
/// the callee knows when the caller was lowered. Give every temporary or local that
/// holds such a result (and was typed `long long` for want of that knowledge) the
/// callee's declared type.
fn retype_call_results(p: &mut mir::MirProgram) {
    let rets: std::collections::HashMap<String, String> = p
        .functions
        .iter()
        .filter_map(|f| f.ret.as_ref().map(|r| (f.name.clone(), r.clone())))
        .collect();
    for func in &mut p.functions {
        let mut found: Vec<(String, String)> = Vec::new();
        for block in &func.blocks {
            for stmt in &block.stmts {
                if let Rvalue::Call(Operand::Func(callee), _) = &stmt.rvalue {
                    if let Some(r) = rets.get(callee) {
                        found.push((stmt.place.clone(), r.clone()));
                    }
                }
            }
        }
        for (place, ty) in found {
            if ty == "long long" {
                continue;
            }
            let temp_untyped = func
                .temp_types
                .get(&place)
                .map_or(true, |t| t == "long long");
            if temp_untyped {
                func.temp_types.insert(place.clone(), ty.clone());
            }
            for (ln, lt) in func.locals.iter_mut() {
                if *ln == place && lt.as_deref().map_or(true, |t| t == "long long") {
                    *lt = Some(ty.clone());
                }
            }
        }
    }
}
