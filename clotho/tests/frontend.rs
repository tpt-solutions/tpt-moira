//! Integration tests for the Clotho frontend (lexer + parser).

use clotho::ast::{BinOp, ExprKind, ItemKind, Lit, Pat, StmtKind};
use clotho::parse_source;
use clotho::token::TokenKind;
use clotho::tokenize_source;

fn collect(src: &str) -> Vec<TokenKind> {
    tokenize_source(src)
        .into_iter()
        .map(|t| t.kind)
        .filter(|k| !matches!(k, TokenKind::Eof))
        .collect()
}

#[test]
fn lex_keywords_and_idents() {
    let kinds = collect("fn let if x_1 while");
    assert_eq!(
        kinds,
        vec![
            TokenKind::Fn,
            TokenKind::Let,
            TokenKind::If,
            TokenKind::Ident("x_1".to_string()),
            TokenKind::While,
        ]
    );
}

#[test]
fn lex_numeric_literals() {
    let kinds = collect("0xFFu8 0b101 0o17 42 3.14f32");
    assert_eq!(
        kinds,
        vec![
            TokenKind::Int {
                value: 0xFF,
                suffix: Some("u8".into())
            },
            TokenKind::Int {
                value: 0b101,
                suffix: None
            },
            TokenKind::Int {
                value: 0o17,
                suffix: None
            },
            TokenKind::Int {
                value: 42,
                suffix: None
            },
            TokenKind::Float {
                text: "3.14".into(),
                suffix: Some("f32".into())
            },
        ]
    );
}

#[test]
fn lex_operators_and_delimiters() {
    let kinds = collect("-> :: .. => && || <<= ");
    assert_eq!(
        kinds,
        vec![
            TokenKind::Arrow,
            TokenKind::ColonColon,
            TokenKind::DotDot,
            TokenKind::FatArrow,
            TokenKind::AmpAmp,
            TokenKind::PipePipe,
            TokenKind::LtLtEq,
        ]
    );
}

#[test]
fn lex_comments_and_trivia() {
    let kinds = collect("/* block /* nested */ */ let // line\n x");
    assert_eq!(kinds, vec![TokenKind::Let, TokenKind::Ident("x".into())]);
}

#[test]
fn lex_string_escapes() {
    let kinds = collect("\"a\\nb\\u{41}\"");
    assert_eq!(kinds, vec![TokenKind::Str("a\nbA".into())]);
}

#[test]
fn parse_empty_function() {
    let prog = parse_source("fn main() {}").unwrap();
    assert_eq!(prog.items.len(), 1);
    match &prog.items[0].kind {
        ItemKind::Function(f) => {
            assert_eq!(f.name.node, "main");
            assert!(f.params.is_empty());
            assert!(f.ret.is_none());
            assert!(f.body.stmts.is_empty());
        }
        other => panic!("expected function, got {:?}", other),
    }
}

#[test]
fn parse_let_with_type_and_init() {
    let prog = parse_source("fn f() { let x: i32 = 1 + 2; }").unwrap();
    if let ItemKind::Function(f) = &prog.items[0].kind {
        assert_eq!(f.body.stmts.len(), 1);
        match &f.body.stmts[0].kind {
            StmtKind::Local(local) => {
                assert!(matches!(local.pat, Pat::Ident(_)));
                assert!(local.ty.is_some());
                assert!(local.init.is_some());
            }
            other => panic!("expected local, got {:?}", other),
        }
    } else {
        panic!("expected function");
    }
}

#[test]
fn parse_binary_precedence() {
    // 1 + 2 * 3 should parse as 1 + (2 * 3)
    let prog = parse_source("fn f() -> i32 { 1 + 2 * 3 }").unwrap();
    if let ItemKind::Function(f) = &prog.items[0].kind {
        let tail = f.body.tail.as_ref().unwrap();
        match &tail.kind {
            ExprKind::Binary(lhs, BinOp::Add, rhs) => {
                assert!(matches!(lhs.kind, ExprKind::Lit(Lit::Int(1, _))));
                assert!(matches!(rhs.kind, ExprKind::Binary(_, BinOp::Mul, _)));
            }
            other => panic!("expected add at top, got {:?}", other),
        }
    } else {
        panic!("expected function");
    }
}

#[test]
fn parse_if_else_and_calls() {
    let src = "fn f() { if x > 0 { g(1, 2) } else { 0 } }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_struct_and_impl_literal() {
    let src = "struct Point { x: i32, y: i32 } fn f() { let p = Point { x: 1, y: 2 }; }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 2);
}

#[test]
fn parse_for_loop_with_range() {
    let src = "fn f() { for i in 0..10 { let _ = i; } }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_error_reports_diagnostic() {
    let res = parse_source("fn f( {");
    assert!(res.is_err());
}

#[test]
fn parse_use_with_as_rename() {
    // `use foo::Bar as Baz;` — the trailing `as` rename must parse (the
    // documented `use ... as` self-host gap for `atropos`). The rename is
    // recorded; `use` is otherwise dropped by the Phase-1 subset.
    let src = "use foo::Bar as Baz;";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
    match &prog.items[0].kind {
        ItemKind::Use(tree) => {
            assert_eq!(tree.prefix.len(), 2);
            assert_eq!(tree.prefix[0].node, "foo");
            assert_eq!(tree.prefix[1].node, "Bar");
            assert!(matches!(tree.kind, clotho::ast::UseKind::Leaf));
            let alias = tree.alias.as_ref().expect("expected an `as` alias");
            assert_eq!(alias.node, "Baz");
        }
        other => panic!("expected a use item, got {other:?}"),
    }
}

#[test]
fn parse_use_without_as_has_no_alias() {
    let src = "use foo::Bar;";
    let prog = parse_source(src).unwrap();
    match &prog.items[0].kind {
        ItemKind::Use(tree) => assert!(tree.alias.is_none()),
        other => panic!("expected a use item, got {other:?}"),
    }
}
