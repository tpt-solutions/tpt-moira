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
// Regression: nested parentheses inside a paren-delimited macro invocation
// (`println!("{}", v.len())`) are macro *content*, not depth markers. They
// must be captured so `v.len()` re-parses as a method call, not a field
// access (the type checker saw "no field `len` on type `Vec`").
#[test]
fn macro_tokens_keep_nested_parens() {
    let src = "fn main() { println!(\"{}\", v.len()); }";
    let prog = parse_source(src).unwrap();
    let mut found = false;
    for item in &prog.items {
        if let ItemKind::Function(f) = &item.kind {
            for stmt in &f.body.stmts {
                if let StmtKind::Semi(e) | StmtKind::Expr(e) = &stmt.kind {
                    if let ExprKind::Macro(m) = &e.kind {
                        let has_parens = m
                            .tokens
                            .iter()
                            .any(|t| matches!(t.kind, TokenKind::LParen | TokenKind::RParen));
                        assert!(has_parens, "nested parens dropped from macro tokens");
                        found = true;
                    }
                }
            }
        }
    }
    assert!(found, "macro call not parsed");
}

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
fn parse_block_body_match_arm_followed_by_tuple_arm() {
    // Regression: a `match` arm whose body is a block `{ .. }` must not treat the
    // next arm's `(pat) =>` pattern as a function call (`block(args)`). A block is
    // not callable in Rust, so the trailing `(` belongs to the following arm.
    let src = "fn f() -> i32 {\n    let r = match (1, 1) {\n        (x, y) => { 1 }\n        (a, b) => 2,\n        _ => 0,\n    };\n    r\n}";
    let prog = parse_source(src).unwrap();
    assert_eq!(prog.items.len(), 1);
    // The same shape with an OR-of-tuples arm (as in atropos/lower.rs).
    let src2 = "fn g() -> i32 {\n    let r = match (1, 1) {\n        (Some(x), Some(y)) => { 1 }\n        (Some(x), None) | (None, Some(x)) => 2,\n        _ => 0,\n    };\n    r\n}";
    let prog2 = parse_source(src2).unwrap();
    assert_eq!(prog2.items.len(), 1);
}

#[test]
fn parse_variant_pattern_with_payload() {
    // Regression: tuple-variant patterns `Some(x)` / `Ok(v)` / `Err(e)` must
    // parse with their payload retained (Previously the payload was discarded,
    // so the bound name `x` was never produced). `Pat::Variant` now carries it.
    let src =
        "fn f(o: i32) -> i32 {\n    match o {\n        Some(x) => x,\n        None => 0,\n    }\n}";
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

#[test]
fn parse_closures() {
    // Regression: closure bodies (block or expression) must parse. The closure
    // parameter list used to consume its own closing `|` as an OR-pattern
    // separator (`|a| body` was read as the single pattern `a | body`), so any
    // closure with a body failed with "expected expression". All of these now
    // parse; the codegen subset still rejects closures at the MIR-lowering stage.
    let cases = [
        // Block body.
        "fn f() { let g = |e| { let x = e + 1; x }; }",
        // Expression body with a binary operator (the `|` must not swallow `+`).
        "fn f() { let g = |e| e + 1; }",
        // Multiple parameters.
        "fn f() { let g = |a, b| a + b; }",
        // Closure nested as a call argument.
        "fn f() { let _ = (0..3).map(|e| e + 1); }",
        // Explicit return type.
        "fn f() { let g = |e| -> i32 { e }; }",
        // Reference-pattern parameter, including used as the first of several call
        // arguments (the `&` inner pattern must not swallow the closing `|`).
        "fn f() { let _ = v.iter().any(|&u| eq(u, x), extra); }",
        "fn f() { let g = |&mut u| u; }",
    ];
    for src in cases {
        let prog = parse_source(src).expect("closure should parse");
        assert_eq!(prog.items.len(), 1);
    }
}

/// Regression: a tuple-index access inside a comma-separated macro argument
/// (`s.1, x`) used to make the macro-argument parser spin forever, because the
/// comma-split segment had no terminating `Eof` and the cursor never advanced.
/// The parse runs on a thread with a timeout so a reintroduced hang fails the
/// test instead of stalling the suite.
#[test]
fn macro_args_with_tuple_index_terminate() {
    use std::sync::mpsc;
    use std::time::Duration;
    use tpt_moira_clotho::lexer::tokenize;
    use tpt_moira_clotho::parser::Parser;

    for src in ["s.0", "s.1, x", "s.0, s.1, s.x", "a, b.2"] {
        let (tx, rx) = mpsc::channel();
        let owned = src.to_string();
        std::thread::spawn(move || {
            let n = Parser::parse_macro_args(tokenize(&owned)).len();
            let _ = tx.send(n);
        });
        let n = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("parse_macro_args hung on `{src}`"));
        let expected = src.split(',').count();
        assert_eq!(n, expected, "wrong argument count for `{src}`");
    }
}

/// Type aliases expand to their body, with generic parameters substituted, and
/// chained aliases resolve transitively. Qualified paths are never treated as
/// alias uses (so `type Result<T> = std::result::Result<T, E>` cannot recurse).
#[test]
fn alias_table_expands_generic_and_chained_aliases() {
    use tpt_moira_clotho::alias::AliasTable;
    use tpt_moira_clotho::ast::{ItemKind, TypeKind};

    let src = "type Id = i32; type Pair<T> = (T, T); type Id2 = Id; \
               type Res<T> = std::result::Result<T, String>; \
               fn f(a: Id2, p: Pair<bool>, r: Res<u8>) {}";
    let prog = parse_source(src).expect("parses");
    let table = AliasTable::from_program(&prog);
    let f = prog
        .items
        .iter()
        .find_map(|it| match &it.kind {
            ItemKind::Function(f) if f.name.node == "f" => Some(f),
            _ => None,
        })
        .expect("fn f");

    // `Id2` -> `Id` -> `i32`.
    let a = table.expand(&f.params[0].ty);
    assert!(matches!(&a.kind, TypeKind::Path(p, _) if p.segments[0].node == "i32"));

    // `Pair<bool>` -> `(bool, bool)`.
    let p = table.expand(&f.params[1].ty);
    match &p.kind {
        TypeKind::Tuple(elems) => {
            assert_eq!(elems.len(), 2);
            for e in elems {
                assert!(matches!(&e.kind, TypeKind::Path(q, _) if q.segments[0].node == "bool"));
            }
        }
        other => panic!("expected tuple, got {other:?}"),
    }

    // `Res<u8>` -> `std::result::Result<u8, String>` (the qualified path is kept).
    let r = table.expand(&f.params[2].ty);
    match &r.kind {
        TypeKind::Path(q, args) => {
            assert_eq!(q.segments.len(), 3);
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected path, got {other:?}"),
    }
}

/// A self-referential alias must terminate (bounded expansion), not loop.
#[test]
fn cyclic_alias_terminates() {
    use tpt_moira_clotho::alias::AliasTable;
    use tpt_moira_clotho::ast::ItemKind;

    let prog = parse_source("type A = A; fn f(x: A) {}").expect("parses");
    let table = AliasTable::from_program(&prog);
    let f = prog
        .items
        .iter()
        .find_map(|it| match &it.kind {
            ItemKind::Function(f) => Some(f),
            _ => None,
        })
        .expect("fn f");
    let _ = table.expand(&f.params[0].ty);
}

/// Constructs that real crates use and the front end must accept: `const fn`,
/// the never type, string-valued attributes, `core::str` paths, brace-grouped
/// `use`, item-position macro calls, and braced `macro_rules!` followed by items.
#[test]
fn real_crate_item_forms_parse() {
    let cases = [
        "pub const fn new() -> i32 { 1 }",
        "fn abort() -> ! { loop {} }",
        "#[cfg(feature = \"x\")]\nuse core::str;",
        "use { a::b, c };",
        "compile_error!(\"no\");",
        "cfg_if! { if #[cfg(a)] { fn x() {} } }",
        "#[cfg(not(docsrs))]\nmacro_rules! crate_root {\n () => { 1 };\n}\nfn after() {}",
    ];
    for src in cases {
        if let Err(e) = parse_source(src) {
            panic!("`{src}` should parse, got: {e:?}");
        }
    }
    // The dropped forms leave no item behind; the real ones are kept.
    let prog = parse_source("compile_error!(\"no\"); fn kept() {}").expect("parses");
    assert_eq!(prog.items.len(), 1);
}

/// `vis`, `lifetime` and `meta` fragments, and a `vis` that matches nothing.
#[test]
fn macro_fragments_vis_lifetime_meta_match() {
    let src = "macro_rules! make {
        ($v:vis fn $n:ident) => { $v fn $n() -> i32 { 1 } };
    }
    make!(pub fn a);
    make!(fn b);";
    let prog = tpt_moira_clotho::compile_source(src).expect("expands and parses");
    let names: Vec<&str> = prog
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Function(f) => Some(f.name.node.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

/// `#[cfg(..)]` is evaluated for the fixed target (64-bit Windows, no features,
/// test off): satisfied items are kept, unsatisfied ones are dropped at parse.
#[test]
fn cfg_predicates_select_items_for_the_target() {
    let src = r#"
        #[cfg(feature = "std")] fn feature_on() {}
        #[cfg(not(feature = "std"))] fn no_std() {}
        #[cfg(windows)] fn on_windows() {}
        #[cfg(unix)] fn on_unix() {}
        #[cfg(not(unix))] fn not_unix() {}
        #[cfg(all(windows, target_pointer_width = "64"))] fn win64() {}
        #[cfg(any(unix, target_os = "linux"))] fn linuxish() {}
        #[cfg(test)] fn test_only() {}
        #[cfg(feature = "std")] mod missing_file;
    "#;
    let prog = parse_source(src).expect("parses");
    let names: Vec<&str> = prog
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Function(f) => Some(f.name.node.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["no_std", "on_windows", "not_unix", "win64"]);
}

/// `matches!(e, pat)` parses as a `match` whose pattern is a real pattern (so
/// `A | B` is an or-pattern, not a bitwise OR of two paths).
#[test]
fn matches_macro_desugars_to_a_match() {
    let prog =
        tpt_moira_clotho::compile_source("fn f(e: E) -> bool { matches!(e, E::A | E::B(_)) }")
            .expect("parses");
    let body = match &prog.items[0].kind {
        ItemKind::Function(f) => f.body.tail.as_ref().expect("tail").kind.clone(),
        other => panic!("expected fn, got {other:?}"),
    };
    match body {
        ExprKind::Match(_, arms) => {
            assert_eq!(arms.len(), 2);
            assert!(matches!(arms[0].0, tpt_moira_clotho::ast::Pat::Or(_, _)));
        }
        other => panic!("expected a match, got {other:?}"),
    }
}

/// An item declared inside a function body is hoisted to the top level; nested
/// items whose names clash are not hoisted.
#[test]
fn nested_items_are_hoisted_and_clashes_rejected() {
    let prog =
        parse_source("fn outer() -> i32 { fn helper() -> i32 { 1 } helper() }").expect("parses");
    let names: Vec<&str> = prog
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Function(f) => Some(f.name.node.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["outer", "helper"]);

    // Two nested items with one name are both dropped (neither binds the other's
    // calls); the functions around them still parse.
    let clash = parse_source(
        "fn a() -> i32 { fn h() -> i32 { 1 } h() } fn b() -> i32 { fn h() -> i32 { 2 } h() }",
    )
    .expect("parses");
    let clash_names: Vec<&str> = clash
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Function(f) => Some(f.name.node.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(clash_names, vec!["a", "b"]);
}
