//! Type propagation for temporaries.
//!
//! The lowering gives a temporary the C type `long long` unless it knows better, but many
//! temporaries hold a pointer or a struct: the result of a call (the callee's return type is
//! known), a field read (the struct's field type), an index into a pointer, a reference, a
//! struct literal. This pass gives such temporaries the type their definition implies, and
//! repeats until nothing changes, so a chain of definitions is typed in order. Locals keep
//! their declared types; only temporaries whose type is the default are changed.

use crate::mir::{MirProgram, Operand, Rvalue};
use std::collections::HashMap;

/// The default C type of a temporary whose type is not known.
const DEFAULT: &str = "long long";

/// `prog` with its temporaries typed from their definitions (see the module docs).
pub fn retype_temps(prog: &MirProgram) -> MirProgram {
    let mut prog = prog.clone();
    for _ in 0..8 {
        if !retype_round(&mut prog) {
            break;
        }
    }
    prog
}

fn retype_round(prog: &mut MirProgram) -> bool {
    let rets: HashMap<String, String> = prog
        .functions
        .iter()
        .filter_map(|f| f.ret.clone().map(|r| (f.name.clone(), r)))
        .collect();
    let fields: HashMap<String, Vec<(String, String)>> = prog
        .structs
        .iter()
        .map(|s| (s.name.clone(), s.fields.clone()))
        .collect();
    let mut changed = false;
    for f in prog.functions.iter_mut() {
        let mut types: HashMap<String, String> = f.temp_types.clone();
        for (name, ty) in &f.locals {
            if let Some(ty) = ty {
                types.insert(name.clone(), ty.clone());
            }
        }
        let locals: Vec<String> = f.locals.iter().map(|(n, _)| n.clone()).collect();
        // A returned value is typed as the function's return type; a function whose return
        // type is only the default takes the pointer or struct type of what it returns (a
        // closure whose body had no hint, for one).
        let mut returned: Vec<String> = Vec::new();
        for b in &f.blocks {
            if let crate::mir::Terminator::Return(Some(Operand::Var(v))) = &b.term {
                returned.push(v.clone());
            }
        }
        for v in returned {
            let vt = types
                .get(&v)
                .cloned()
                .unwrap_or_else(|| DEFAULT.to_string());
            match f.ret.clone() {
                Some(r) if r != DEFAULT && vt == DEFAULT && !locals.contains(&v) => {
                    types.insert(v.clone(), r.clone());
                    f.temp_types.insert(v, r);
                    changed = true;
                }
                Some(r) if r == DEFAULT && is_pointer_or_struct(&vt, &fields) => {
                    f.ret = Some(vt.clone());
                    changed = true;
                }
                _ => {}
            }
        }
        for bi in 0..f.blocks.len() {
            for si in 0..f.blocks[bi].stmts.len() {
                let place = f.blocks[bi].stmts[si].place.clone();
                if locals.contains(&place) {
                    continue;
                }
                let current = types
                    .get(&place)
                    .cloned()
                    .unwrap_or_else(|| DEFAULT.to_string());
                if current != DEFAULT {
                    continue;
                }
                let rv = &f.blocks[bi].stmts[si].rvalue;
                if let Some(ty) = infer(rv, &types, &rets, &fields) {
                    if ty != DEFAULT && !ty.is_empty() {
                        types.insert(place.clone(), ty.clone());
                        f.temp_types.insert(place, ty);
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

/// Whether a C type is a pointer or a struct: a type the default cannot stand for.
fn is_pointer_or_struct(ty: &str, fields: &HashMap<String, Vec<(String, String)>>) -> bool {
    ty.ends_with('*') || fields.contains_key(ty)
}

/// The C type of an operand, when the function's types give it.
fn operand_type(op: &Operand, types: &HashMap<String, String>) -> Option<String> {
    match op {
        Operand::Var(n) => types.get(n).cloned(),
        Operand::CStr(_) => Some("char*".to_string()),
        _ => None,
    }
}

/// The type a definition gives its place, if it is one this pass understands.
fn infer(
    rv: &Rvalue,
    types: &HashMap<String, String>,
    rets: &HashMap<String, String>,
    fields: &HashMap<String, Vec<(String, String)>>,
) -> Option<String> {
    match rv {
        Rvalue::Use(op) => operand_type(op, types),
        Rvalue::Call(Operand::Func(name), _) => rets.get(name).cloned(),
        Rvalue::Field(base, field, _) => {
            let bt = operand_type(base, types)?;
            field_type(bt.trim_end_matches('*'), field, fields)
        }
        Rvalue::FieldAddr(base, field, _) => {
            let bt = operand_type(base, types)?;
            field_type(bt.trim_end_matches('*'), field, fields).map(|t| format!("{t}*"))
        }
        Rvalue::Index(base, _) => operand_type(base, types)?
            .strip_suffix('*')
            .map(|t| t.to_string()),
        Rvalue::IndexRef(base, _) => {
            let bt = operand_type(base, types)?;
            bt.ends_with('*').then_some(bt)
        }
        Rvalue::Ref(op, _) => operand_type(op, types).map(|t| format!("{t}*")),
        Rvalue::Cast(_, ty) => Some(ty.clone()),
        Rvalue::StructLit(name, _) => fields.contains_key(name).then(|| name.clone()),
        // A tuple literal is its generated struct.
        Rvalue::Tuple(ty, _) => Some(ty.clone()),
        _ => None,
    }
}

/// The C type of field `field` of struct `strukt`.
fn field_type(
    strukt: &str,
    field: &str,
    fields: &HashMap<String, Vec<(String, String)>>,
) -> Option<String> {
    fields
        .get(strukt)?
        .iter()
        .find(|(name, _)| name == field)
        .map(|(_, ty)| ty.clone())
}
