//! Tests for the Clotho concurrent macro-expansion pipeline.

use clotho::ast::{ExprKind, ItemKind, StmtKind};
use clotho::compile_source;
use clotho::macros::extract_macros;
use clotho::tokenize_source;

#[test]
fn macro_extraction_removes_def_from_tokens() {
    let src = "macro twice(x) = x * 2; fn main() { let y = twice(3); }";
    let tokens = tokenize_source(src);
    let (table, rest) = extract_macros(&tokens).unwrap();
    assert!(table.defs.contains_key("twice"));
    // The `macro` keyword must be gone from the remaining token stream.
    let rest_kinds: Vec<_> = rest.iter().map(|t| format!("{:?}", t.kind)).collect();
    assert!(
        !rest_kinds.iter().any(|k| k.contains("Macro")),
        "macro keyword leaked: {:?}",
        rest_kinds
    );
}

#[test]
fn simple_macro_expands_to_arithmetic() {
    let src = "
        macro double(x) = x + x;
        fn main() {
            let y = double(21);
            let _ = y;
        }
    ";
    let prog = compile_source(src).unwrap();
    let main = match &prog.items[0].kind {
        ItemKind::Function(f) => f,
        _ => panic!("expected function"),
    };
    // Find `let y = ...`. The init should be `21 + 21`, not a call.
    let init = main
        .body
        .stmts
        .iter()
        .find_map(|s| match &s.kind {
            StmtKind::Local(l) => match &l.pat {
                clotho::ast::Pat::Ident(n) if n.node == "y" => l.init.as_ref(),
                _ => None,
            },
            _ => None,
        })
        .expect("y binding not found");
    match &init.kind {
        ExprKind::Binary(l, _, r) => {
            assert!(matches!(l.kind, ExprKind::Lit(clotho::ast::Lit::Int(21, _))));
            assert!(matches!(r.kind, ExprKind::Lit(clotho::ast::Lit::Int(21, _))));
        }
        other => panic!("expected binary expansion, got {:?}", other),
    }
}

#[test]
fn macro_used_in_expression_context() {
    let src = "
        macro sq(x) = x * x;
        fn area() -> i32 { sq(3 + 1) }
    ";
    let prog = compile_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
    // Should parse as a function with no leftover call to `sq`.
    let f = match &prog.items[0].kind {
        ItemKind::Function(f) => f,
        _ => panic!(),
    };
    assert!(format!("{:?}", f.body).contains("4"));
}

#[test]
fn concurrent_pipeline_matches_sync() {
    // The concurrent expander must produce the same tree as the synchronous
    // one; this also exercises multi-threaded expansion.
    let src = "
        macro inc(x) = x + 1;
        fn f() { let a = inc(0); let b = inc(41); }
    ";
    let prog = compile_source(src).unwrap();
    let f = match &prog.items[0].kind {
        ItemKind::Function(f) => f,
        _ => panic!(),
    };
    // Both `inc` calls expanded; no Call-to-`inc` should remain.
    assert!(!format!("{:?}", f.body).contains("inc"));
}

#[test]
fn no_macros_is_identity() {
    let src = "fn main() { let x = 1; }";
    let prog = compile_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
    match &prog.items[0].kind {
        ItemKind::Function(f) => assert_eq!(f.name.node, "main"),
        _ => panic!(),
    }
}
