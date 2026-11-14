//! Module-scoped resolution of the std `Path` name.
//!
//! `std::path::Path` and the AST's own `Path` struct are both called `Path`, and the
//! checker sees the crate flattened into one namespace, so a type name cannot say
//! which one it means. Each source file is its own module, so the meaning is decided
//! per file, before flattening: a file that imports `std::path::Path` (and defines no
//! `Path` of its own) has its type positions `Path` rewritten to `PathBuf`. A
//! `PathBuf` is a string in the backend (`char*`), like `str`, so the rewritten type is
//! one the checker and the C backend model.
//!
//! Only type positions are rewritten (signatures, fields, `let` annotations, casts,
//! closure parameters). A std `Path` used in an expression (`Path::new`) is not
//! rewritten; the crates Moira compiles do not use one.

use crate::ast::*;
use crate::closure_return::visit_block;

/// Resolve the std `Path` name of every file in `prog` (see the module docs).
pub fn resolve_std_paths(prog: Program) -> Program {
    Program {
        items: resolve_scope(prog.items),
    }
}

/// Resolve one scope (a file, or an inline module): its own imports decide.
fn resolve_scope(mut items: Vec<Item>) -> Vec<Item> {
    let on = imports_std_path(&items) && !defines_path(&items);
    for item in &mut items {
        resolve_item(item, on);
    }
    items
}

/// Whether a scope imports `std::path::Path` by name (no alias).
fn imports_std_path(items: &[Item]) -> bool {
    items.iter().any(|it| match &it.kind {
        ItemKind::Use(tree) => {
            let mut found = false;
            for_each_use_path(tree, &mut Vec::new(), &mut |segs, alias| {
                if alias.is_none()
                    && segs.len() == 3
                    && segs[0] == "std"
                    && segs[1] == "path"
                    && segs[2] == "Path"
                {
                    found = true;
                }
            });
            found
        }
        _ => false,
    })
}

/// Call `f` on every full path `prefix::..::leaf` of a `use` tree.
fn for_each_use_path(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    f: &mut impl FnMut(&[String], Option<&str>),
) {
    let depth = prefix.len();
    for seg in &tree.prefix {
        prefix.push(seg.node.clone());
    }
    match &tree.kind {
        UseKind::Leaf => f(prefix, tree.alias.as_ref().map(|a| a.node.as_str())),
        UseKind::Glob => {}
        UseKind::Nested(subs) => {
            for sub in subs {
                for_each_use_path(sub, prefix, f);
            }
        }
    }
    prefix.truncate(depth);
}

/// Whether a scope defines its own `Path` type (a struct, enum or alias), which
/// shadows an import.
fn defines_path(items: &[Item]) -> bool {
    items.iter().any(|it| match &it.kind {
        ItemKind::Struct(s) => s.name.node == "Path",
        ItemKind::Enum(e) => e.name.node == "Path",
        ItemKind::TypeAlias(t) => t.name.node == "Path",
        _ => false,
    })
}

fn resolve_item(item: &mut Item, on: bool) {
    match &mut item.kind {
        ItemKind::Function(f) => resolve_fn(f, on),
        ItemKind::Struct(s) => {
            for field in &mut s.fields {
                rewrite_ty(&mut field.ty, on);
            }
        }
        ItemKind::Enum(e) => {
            for v in &mut e.variants {
                if let Some(fields) = &mut v.fields {
                    for field in fields {
                        rewrite_ty(&mut field.ty, on);
                    }
                }
            }
        }
        ItemKind::Const(c) => {
            if let Some(t) = &mut c.ty {
                rewrite_ty(t, on);
            }
        }
        ItemKind::Static(s) => rewrite_ty(&mut s.ty, on),
        ItemKind::TypeAlias(t) => rewrite_ty(&mut t.ty, on),
        ItemKind::Impl(imp) => {
            rewrite_ty(&mut imp.for_ty, on);
            for it in &mut imp.items {
                resolve_item(it, on);
            }
        }
        ItemKind::Module(m) => {
            if let ModuleKind::Inline(sub) = &mut m.kind {
                let taken = std::mem::take(sub);
                *sub = resolve_scope(taken);
            }
        }
        ItemKind::Use(_) => {}
    }
}

fn resolve_fn(f: &mut Function, on: bool) {
    for p in &mut f.params {
        rewrite_ty(&mut p.ty, on);
    }
    if let Some(t) = &mut f.ret {
        rewrite_ty(t, on);
    }
    if !on {
        return;
    }
    visit_block(
        &mut f.body,
        &mut |e: &mut Expr| match &mut e.kind {
            ExprKind::Cast(_, ty) => rewrite_ty(ty, true),
            ExprKind::Closure { params, ret, .. } => {
                for p in params {
                    rewrite_ty(&mut p.ty, true);
                }
                if let Some(t) = ret {
                    rewrite_ty(t, true);
                }
            }
            _ => {}
        },
        &mut |b: &mut Block| {
            for s in &mut b.stmts {
                match &mut s.kind {
                    StmtKind::Local(l) => {
                        if let Some(t) = &mut l.ty {
                            rewrite_ty(t, true);
                        }
                    }
                    StmtKind::Item(it) => resolve_item(it, true),
                    StmtKind::Expr(_) | StmtKind::Semi(_) => {}
                }
            }
        },
    );
}

/// Rename the std `Path` type to `PathBuf` throughout `ty` when `on`.
fn rewrite_ty(ty: &mut Type, on: bool) {
    if !on {
        return;
    }
    match &mut ty.kind {
        TypeKind::Path(p, args) => {
            if p.segments.len() == 1 && p.segments[0].node == "Path" {
                p.segments[0].node = "PathBuf".to_string();
            }
            for a in args {
                rewrite_ty(a, on);
            }
        }
        TypeKind::Ref(inner) | TypeKind::RefMut(inner) | TypeKind::Future(inner) => {
            rewrite_ty(inner, on)
        }
        TypeKind::Array(inner, _) | TypeKind::Slice(inner) => rewrite_ty(inner, on),
        TypeKind::Tuple(elems) => {
            for e in elems {
                rewrite_ty(e, on);
            }
        }
        TypeKind::Infer | TypeKind::Never => {}
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_std_paths;
    use crate::ast::{ItemKind, TypeKind};
    use crate::parser::Parser;

    /// The name of the first parameter type of the function `f` in `src`, after resolution.
    fn first_param_type(src: &str) -> String {
        let prog = Parser::from_source(src).parse_program().expect("parse");
        let prog = resolve_std_paths(prog);
        let f = prog
            .items
            .iter()
            .find_map(|it| match &it.kind {
                ItemKind::Function(f) if f.name.node == "f" => Some(f),
                _ => None,
            })
            .expect("fn f");
        match &f.params[0].ty.kind {
            TypeKind::Ref(inner) | TypeKind::RefMut(inner) => inner.node_name(),
            _ => f.params[0].ty.node_name(),
        }
    }

    #[test]
    fn imported_std_path_becomes_pathbuf() {
        let src = "use std::path::{Path, PathBuf};\nfn f(p: &Path) {}\n";
        assert_eq!(first_param_type(src), "PathBuf");
    }

    #[test]
    fn local_path_struct_is_left_alone() {
        let src =
            "use std::path::{Path, PathBuf};\nstruct Path { segments: i64 }\nfn f(p: &Path) {}\n";
        assert_eq!(first_param_type(src), "Path");
    }

    #[test]
    fn without_the_import_path_is_untouched() {
        let src = "fn f(p: &Path) {}\n";
        assert_eq!(first_param_type(src), "Path");
    }
}
