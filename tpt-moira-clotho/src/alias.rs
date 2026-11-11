//! Type-alias expansion.
//!
//! A top-level `type Name<P..> = Body;` is a pure name for `Body` with its
//! generic parameters substituted. Downstream passes (the Lachesis checker and
//! the Atropos lowering) resolve types structurally, so they expand aliases with
//! this table before converting a type. Only *single-segment* paths are alias
//! uses: a qualified path such as `std::result::Result<T, E>` is never rewritten,
//! which keeps `type Result<T> = std::result::Result<T, E>;` from recursing into
//! itself.

use crate::ast::{Item, ItemKind, ModuleKind, Path, Program, Type, TypeKind, Visibility};
use std::collections::HashMap;

/// Bound on nested alias expansion. A cyclic alias (`type A = A;`) stops here and
/// is left unexpanded, so the downstream checker reports it as an unknown type
/// instead of looping.
const MAX_EXPANSION_DEPTH: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct AliasTable {
    /// Alias name -> (generic parameter names, aliased type).
    defs: HashMap<String, (Vec<String>, Type)>,
}

impl AliasTable {
    /// Collect every top-level alias of `prog` (inline `mod` items are searched
    /// too, so `mod m { type X = i32; }` is visible under `X`). Items gated behind
    /// `#[cfg(test)]` are skipped, matching the checker.
    pub fn from_program(prog: &Program) -> Self {
        let mut table = AliasTable::default();
        table.collect(&prog.items, false);
        table
    }

    /// The aliases another crate may name: only `pub` ones. A private alias such
    /// as atropos's `type Result<T> = ...` must not leak into a dependent crate.
    pub fn from_exports(prog: &Program) -> Self {
        let mut table = AliasTable::default();
        table.collect(&prog.items, true);
        table
    }

    /// Merge the aliases of `other` into `self`. Entries already present win, so
    /// the crate being checked shadows an extern crate's alias of the same name.
    pub fn extend_from(&mut self, other: &AliasTable) {
        for (name, def) in &other.defs {
            self.defs.entry(name.clone()).or_insert_with(|| def.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Return `ty` with every alias use replaced by its aliased type.
    pub fn expand(&self, ty: &Type) -> Type {
        if self.defs.is_empty() {
            return ty.clone();
        }
        self.expand_depth(ty, 0)
    }

    fn collect(&mut self, items: &[Item], exports_only: bool) {
        for item in items {
            if item.is_cfg_test() {
                continue;
            }
            match &item.kind {
                ItemKind::TypeAlias(ta) => {
                    if exports_only && item.visibility != Visibility::Pub {
                        continue;
                    }
                    let params = ta
                        .generics
                        .params
                        .iter()
                        .map(|p| p.name.node.clone())
                        .collect();
                    self.defs
                        .insert(ta.name.node.clone(), (params, ta.ty.clone()));
                }
                ItemKind::Module(m) => {
                    if let ModuleKind::Inline(inline) = &m.kind {
                        self.collect(inline, exports_only);
                    }
                }
                _ => {}
            }
        }
    }

    fn expand_depth(&self, ty: &Type, depth: usize) -> Type {
        if depth > MAX_EXPANSION_DEPTH {
            return ty.clone();
        }
        let map = |t: &Type| self.expand_depth(t, depth);
        let kind = match &ty.kind {
            TypeKind::Path(path, args) => {
                let args: Vec<Type> = args.iter().map(map).collect();
                if let Some(expanded) = self.expand_use(path, &args, depth) {
                    return expanded;
                }
                TypeKind::Path(path.clone(), args)
            }
            TypeKind::Ref(inner) => TypeKind::Ref(Box::new(map(inner))),
            TypeKind::RefMut(inner) => TypeKind::RefMut(Box::new(map(inner))),
            TypeKind::Array(inner, n) => TypeKind::Array(Box::new(map(inner)), *n),
            TypeKind::Slice(inner) => TypeKind::Slice(Box::new(map(inner))),
            TypeKind::Tuple(elems) => TypeKind::Tuple(elems.iter().map(map).collect()),
            TypeKind::Future(inner) => TypeKind::Future(Box::new(map(inner))),
            TypeKind::Infer => TypeKind::Infer,
            TypeKind::Never => TypeKind::Never,
        };
        Type {
            kind,
            span: ty.span,
        }
    }

    /// If `path<args>` names an alias, return its body with the parameters
    /// replaced by `args`, expanded again (aliases may refer to aliases).
    fn expand_use(&self, path: &Path, args: &[Type], depth: usize) -> Option<Type> {
        if path.segments.len() != 1 {
            return None;
        }
        let (params, body) = self.defs.get(&path.segments[0].node)?;
        let subst: HashMap<&str, &Type> =
            params.iter().map(String::as_str).zip(args.iter()).collect();
        let substituted = substitute(body, &subst);
        Some(self.expand_depth(&substituted, depth + 1))
    }
}

/// Replace each single-segment, argument-free path naming a parameter with the
/// corresponding argument type.
fn substitute(ty: &Type, subst: &HashMap<&str, &Type>) -> Type {
    if let TypeKind::Path(path, args) = &ty.kind {
        if args.is_empty() && path.segments.len() == 1 {
            if let Some(arg) = subst.get(path.segments[0].node.as_str()) {
                return (*arg).clone();
            }
        }
    }
    let kind = match &ty.kind {
        TypeKind::Path(path, args) => TypeKind::Path(
            path.clone(),
            args.iter().map(|a| substitute(a, subst)).collect(),
        ),
        TypeKind::Ref(inner) => TypeKind::Ref(Box::new(substitute(inner, subst))),
        TypeKind::RefMut(inner) => TypeKind::RefMut(Box::new(substitute(inner, subst))),
        TypeKind::Array(inner, n) => TypeKind::Array(Box::new(substitute(inner, subst)), *n),
        TypeKind::Slice(inner) => TypeKind::Slice(Box::new(substitute(inner, subst))),
        TypeKind::Tuple(elems) => {
            TypeKind::Tuple(elems.iter().map(|e| substitute(e, subst)).collect())
        }
        TypeKind::Future(inner) => TypeKind::Future(Box::new(substitute(inner, subst))),
        TypeKind::Infer => TypeKind::Infer,
        TypeKind::Never => TypeKind::Never,
    };
    Type {
        kind,
        span: ty.span,
    }
}
