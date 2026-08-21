//! Integration tests for the Clotho frontend (lexer + parser).

use tpt_moira_clotho::ast::{BinOp, ExprKind, ItemKind, Lit, Pat, StmtKind};
use tpt_moira_clotho::parse_source;
use tpt_moira_clotho::token::TokenKind;
use tpt_moira_clotho::tokenize_source;

fn collect(src: &str) -> Vec<TokenKind> {
    tokenize_source(src)
        .into_iter()
        .map(|t| t.kind)
        .filter(|k| !matches!(k, TokenKind::Eof))
        .collect()
}

#[test]
fn lex_keywords_and_idents() {
    // `break` must lex as the `Break` keyword, not be swallowed by the `br"…"`
    // raw-byte-string rule (which collides on the leading `br`).
    let kinds = collect("fn let if break while continue return");
    assert_eq!(
        kinds,
        vec![
            TokenKind::Fn,
            TokenKind::Let,
            TokenKind::If,
            TokenKind::Break,
            TokenKind::While,
            TokenKind::Continue,
            TokenKind::Return,
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
            assert!(matches!(tree.kind, tpt_moira_clotho::ast::UseKind::Leaf));
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

#[test]
fn parse_tuple_literal_value() {
    // Regression: a tuple literal `(a, b, c)` as an expression previously
    // double-consumed the closing `)` (parse_delimited already ate it) and
    // reported "expected ), found ;".
    let src = "fn f() { let x = (1, 2); let y = (1, 2, 3); let _ = x; let _ = y; }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_trailing_comma_tuple_pattern() {
    // Regression: `(a,)` single-element tuple patterns (and trailing commas in
    // general) must parse without "expected pattern, found )".
    let src = "fn f() { let (a,) = (1,); let _ = a; }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_method_call_turbofish() {
    // Regression: a turbofish `::<T>` on a method call (e.g.
    // `.collect::<Vec<_>>()`) must parse. The Phase-1 subset ignores the type
    // arguments, but real Rust code uses them.
    let src = "fn f() { let _ = (0..3).collect::<Vec<i32>>(); }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_enum_struct_variants() {
    // Regression: enum struct variants `Variant { field: Type, .. }` must parse
    // (the compiler's own `ast.rs` defines several, e.g. `ExprKind::Closure {
    // params, ret, body }`). Unit, tuple, and struct variants all parse, with or
    // without a trailing comma.
    let src = r#"
        enum E {
            Unit,
            Tuple(i32, String),
            Struct { a: i32, b: String },
            Trailing { x: i32, },
        }
    "#;
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
    match &prog.items[0].kind {
        ItemKind::Enum(e) => assert_eq!(e.variants.len(), 4),
        other => panic!("expected enum, got {other:?}"),
    }
}

#[test]
fn parse_if_let_and_while_let() {
    // Regression: `if let`/`while let` must parse even when the scrutinee RHS is a
    // bare path or reference (not just a call). Previously the RHS was parsed with
    // struct-literal allowed, so `if let P = queue { .. }` swallowed the then-block
    // as a struct literal `queue { .. }`.
    let src = "fn f() { if let Some(x) = iter { let _ = x; } while let Some(y) = &mut iter { let _ = y; } }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_match_guards() {
    // Regression: match arms with guards `Pat if cond => body` must parse. The
    // Phase-1 subset models the guard only nominally (it is parsed and ignored),
    // but the construct must not break the surrounding `match`.
    let src = "fn f(x: i32) -> i32 { match x { 0 => 1, n if n > 0 => n, _ => 0 } }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_impl_trait_in_type_position() {
    // Regression: `impl Trait` parameter/return types must parse. This is the
    // gating construct for `clotho`/`atropos` self-host; the subset models it as
    // the trait path opaquely. Both angle-bracket and parenthesized trait args
    // (`impl FnMut(&str)`) are accepted.
    let src = "fn error(message: impl Into<String>) {} fn g(v: &mut impl FnMut(&str)) {}";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 2);
}

#[test]
fn parse_tuple_types_and_index() {
    // Regression: tuple *types* `(A, B)` previously double-consumed the closing
    // `)`, and tuple field access `expr.0` was rejected. Both now parse.
    let src = "fn f(t: (i32, i32)) { let _ = t.0; let _ = t.1.cmp(t.0); }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_if_with_binary_condition_does_not_eat_then_block() {
    // Regression: a struct literal is only allowed as a binary operand when the
    // surrounding context permits it. With `allow_struct` re-enabled on the RHS
    // of a comparison, `if a > b { a } else { b }` misparsed `b { a }` as a
    // struct literal and then failed with "expected {, found else". The flag now
    // propagates through to the RHS so the then-block's `{` is never swallowed.
    let src = "fn max(a: i32, b: i32) -> i32 { if a > b { a } else { b } }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
    let src2 = "fn f() { if a == b { 1 } else { 0 } }";
    let prog2 = parse_source(src2).unwrap();
    assert_eq!(prog2.items.len(), 1);
}

#[test]
fn parse_leading_dot_float_and_field_access() {
    // Regression: a `.` immediately followed by a digit is a leading-dot float
    // literal (`.5`), and a `.N` following a receiver is numeric tuple/struct
    // field access (`expr.0`). Both must parse.
    let src = "fn f(t: (i32, i32)) { let _ = t.0; let _ = t.1.cmp(t.0); let _ = .5; }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
}

#[test]
fn parse_generic_type_alias() {
    // Regression: generic type aliases `type Pair<T> = (T, T);` must parse. The
    // `TypeAliasDef` now carries optional `generics`.
    let src = "type Pair<T> = (T, T); fn f() { let _: Pair<i32> = (1, 2); }";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 2);
}
