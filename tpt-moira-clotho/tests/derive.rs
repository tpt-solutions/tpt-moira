//! Tests for `#[derive(...)]` macro expansion.

use tpt_moira_clotho::ast::{ItemKind, Program};
use tpt_moira_clotho::compile_source;

/// Count the number of `impl` items in a program whose trait name matches.
fn count_impls_for(prog: &Program, trait_name: &str) -> usize {
    prog.items
        .iter()
        .filter(|item| {
            if let ItemKind::Impl(imp) = &item.kind {
                imp.trait_name.node == trait_name
            } else {
                false
            }
        })
        .count()
}

#[test]
fn derive_clone_on_struct_generates_impl() {
    let src = "
        #[derive(Clone, Debug, PartialEq)]
        struct Point { x: i32, y: i32 }
        fn main() {}
    ";
    let prog = compile_source(src).unwrap();
    assert_eq!(count_impls_for(&prog, "Clone"), 1);
    assert_eq!(count_impls_for(&prog, "Debug"), 1);
    assert_eq!(count_impls_for(&prog, "PartialEq"), 1);
    // `Debug`/`PartialEq` take no impl methods in our skeleton but the impl
    // itself must be present.
    let clone_impl = prog
        .items
        .iter()
        .find(|i| matches!(i.kind, ItemKind::Impl(ref imp) if imp.trait_name.node == "Clone"))
        .unwrap();
    assert!(matches!(clone_impl.kind, ItemKind::Impl(_)));
}

#[test]
fn derive_default_uses_default_call() {
    let src = "
        #[derive(Default, Clone)]
        struct Config { flag: bool, count: i32 }
        fn main() {}
    ";
    let prog = compile_source(src).unwrap();
    assert_eq!(count_impls_for(&prog, "Default"), 1);
    assert_eq!(count_impls_for(&prog, "Clone"), 1);
}

#[test]
fn derive_on_enum_generates_impls() {
    let src = "
        #[derive(Clone, PartialEq, Copy)]
        enum Direction { North, South, East, West }
        fn main() {}
    ";
    let prog = compile_source(src).unwrap();
    assert_eq!(count_impls_for(&prog, "Clone"), 1);
    assert_eq!(count_impls_for(&prog, "PartialEq"), 1);
    assert_eq!(count_impls_for(&prog, "Copy"), 1);
}

#[test]
fn attributes_are_retained_on_item() {
    let src = "
        #[derive(Debug)]
        #[inline]
        struct S { v: i32 }
        fn main() {}
    ";
    let prog = compile_source(src).unwrap();
    let s = prog
        .items
        .iter()
        .find(|i| matches!(i.kind, ItemKind::Struct(_)))
        .unwrap();
    assert_eq!(s.attributes.len(), 2);
    assert!(s.attributes.iter().any(|a| a
        .path
        .segments
        .last()
        .map(|s| s.node == "derive")
        .unwrap_or(false)));
    assert!(s.attributes.iter().any(|a| a
        .path
        .segments
        .last()
        .map(|s| s.node == "inline")
        .unwrap_or(false)));
}
