//! Inlining of closure-returning functions at call sites.
//!
//! A top-level function whose whole body is a closure (`fn make(k: i64) -> impl Fn(i64)
//! -> i64 { move |x| x + k }`) cannot be lowered as-is when the closure captures: the
//! C backends pass captures as extra parameters of the lifted function, which a plain
//! function pointer cannot carry. For the common shape
//!
//! ```text
//! let f = make(4);
//! ```
//!
//! this pass rewrites the call site into argument bindings plus the closure itself:
//!
//! ```text
//! let __ret_make_k_0: i64 = 4;
//! let f = |x| x + __ret_make_k_0;
//! ```
//!
//! The existing capturing-closure lowering then handles `f` like any other let-bound
//! closure. A call used as a pure-argument operand of a statement (`apply(make(4), 1)`)
//! is first hoisted into its own `let` before that statement, and a call inside the
//! arguments of `println!`-like macros is hoisted out of the token stream. Captured
//! candidates left with no references are removed, since they cannot be emitted as C.
//!
//! The pass is purely syntactic and deliberately conservative: a candidate must be a
//! non-generic, non-async function whose body is only a closure with identifier
//! parameters; the closure body must bind no names of its own and contain no macros,
//! so renaming the captured parameters is safe. Hoisted arguments must be pure, so
//! evaluation order does not change.
//!
//! The code sticks to the constructs the Moira frontend itself accepts (no slice
//! patterns, no `dyn` closures), since the compiler must parse its own source.

use crate::ast::*;
use crate::parser::Parser;
use crate::token::{Token, TokenKind};
use std::collections::HashMap;

/// A candidate function: its parameters and the closure its body returns.
struct Candidate {
    params: Vec<Param>,
    closure: Expr,
}

/// Rewrite the closure-returning call sites of `prog` (see the module docs).
pub fn inline_returned_closures(mut prog: Program) -> Program {
    let (cands, capturing) = collect_candidates(&prog);
    if cands.is_empty() {
        return prog;
    }
    let mut fresh = 0usize;
    for item in &mut prog.items {
        visit_item_bodies(item, &mut |b: &mut Block| {
            rewrite_block_lets(b, &cands, &mut fresh);
        });
    }
    // A capturing candidate that is no longer referenced cannot be emitted: drop it.
    // Modules are not flattened here, so their references are unknown; keep everything.
    let has_modules = prog
        .items
        .iter()
        .any(|it| matches!(it.kind, ItemKind::Module(_)));
    if !has_modules {
        let removable: Vec<String> = capturing
            .into_iter()
            .filter(|n| count_refs(&prog, n) == 0)
            .collect();
        let items: Vec<Item> = std::mem::take(&mut prog.items);
        for it in items {
            let drop_it = match &it.kind {
                ItemKind::Function(f) => removable.contains(&f.name.node),
                _ => false,
            };
            if !drop_it {
                prog.items.push(it);
            }
        }
    }
    prog
}

/// The closure-returning top-level functions, and the names of those that capture.
fn collect_candidates(prog: &Program) -> (HashMap<String, Candidate>, Vec<String>) {
    let mut out: HashMap<String, Candidate> = HashMap::new();
    let mut capturing: Vec<String> = Vec::new();
    for item in &prog.items {
        let ItemKind::Function(f) = &item.kind else {
            continue;
        };
        if f.is_async || !f.generics.params.is_empty() || f.name.node == "main" {
            continue;
        }
        if !f.body.stmts.is_empty() {
            continue;
        }
        let Some(tail) = &f.body.tail else {
            continue;
        };
        let ExprKind::Closure { params, body, .. } = &tail.kind else {
            continue;
        };
        let name_opts: Option<Vec<String>> = f
            .params
            .iter()
            .map(|p| match &p.pat {
                Pat::Ident(s) => Some(s.node.clone()),
                _ => None,
            })
            .collect();
        let names: Vec<String> = match name_opts {
            Some(n) => n,
            None => continue,
        };
        // A closure parameter that shadows a function parameter would change what the
        // body refers to; leave such functions alone.
        let mut shadows = false;
        for p in params {
            if let Pat::Ident(s) = &p.pat {
                if names.contains(&s.node) {
                    shadows = true;
                }
            }
        }
        if shadows {
            continue;
        }
        let mut body = (**body).clone();
        if !rename_safe(&mut body) {
            continue;
        }
        if mentions_any(&mut body, &names) {
            capturing.push(f.name.node.clone());
        }
        out.insert(
            f.name.node.clone(),
            Candidate {
                params: f.params.clone(),
                closure: (**tail).clone(),
            },
        );
    }
    (out, capturing)
}

/// True when the expression binds no names of its own and contains no macro, so that
/// renaming a free variable throughout it is sound.
fn rename_safe(e: &mut Expr) -> bool {
    let mut bad = false;
    visit_expr(
        e,
        &mut |x: &mut Expr| {
            if matches!(
                x.kind,
                ExprKind::Closure { .. }
                    | ExprKind::Match(..)
                    | ExprKind::For(..)
                    | ExprKind::Macro(_)
            ) {
                bad = true;
            }
        },
        &mut |_| {},
    );
    visit_expr(
        e,
        &mut |_| {},
        &mut |b: &mut Block| {
            for s in &b.stmts {
                if matches!(s.kind, StmtKind::Local(_) | StmtKind::Item(_)) {
                    bad = true;
                }
            }
        },
    );
    !bad
}

fn mentions_any(e: &mut Expr, names: &[String]) -> bool {
    let mut found = false;
    visit_expr(
        e,
        &mut |x: &mut Expr| {
            if let ExprKind::Path(p) = &x.kind {
                if p.segments.len() == 1 && names.contains(&p.segments[0].node) {
                    found = true;
                }
            }
        },
        &mut |_| {},
    );
    found
}

/// Apply `f` to every block of an item's bodies (functions, impl methods, consts, statics).
fn visit_item_bodies(item: &mut Item, f: &mut impl FnMut(&mut Block)) {
    match &mut item.kind {
        ItemKind::Function(func) => { visit_block(&mut func.body, &mut |_| {}, f); }
        ItemKind::Impl(imp) => {
            for it in &mut imp.items {
                visit_item_bodies(it, f);
            }
        }
        ItemKind::Const(c) => { visit_expr(&mut c.value, &mut |_| {}, f); }
        ItemKind::Static(s) => { visit_expr(&mut s.value, &mut |_| {}, f); }
        _ => {}
    }
}

/// Rebuild the statements of `b`: hoisted argument lets and inlined closure lets are
/// pushed to the output directly, ahead of the statement that uses them.
fn rewrite_block_lets(b: &mut Block, cands: &HashMap<String, Candidate>, fresh: &mut usize) {
    let mut out: Vec<Stmt> = Vec::with_capacity(b.stmts.len());
    let old: Vec<Stmt> = std::mem::take(&mut b.stmts);
    for mut stmt in old {
        match &mut stmt.kind {
            StmtKind::Local(local) => {
                if let Some(e) = &mut local.init {
                    hoist_call_args(e, cands, &mut out, fresh);
                }
            }
            StmtKind::Expr(e) | StmtKind::Semi(e) => { hoist_call_args(e, cands, &mut out, fresh); }
            StmtKind::Item(_) => {}
        }
        let inlined = match &stmt.kind {
            StmtKind::Local(local) => emit_let(local, cands, fresh, &mut out),
            _ => false,
        };
        if !inlined {
            out.push(stmt);
        }
    }
    b.stmts = out;
}

/// Replace each argument `g(pure..)` of a call in `e` (where `g` is a candidate) by a
/// fresh name bound by a `let` pushed onto `out` (inlined like a direct `let`). Only
/// straight-line operands are visited: nothing under a block, branch, loop or closure.
fn hoist_call_args(
    e: &mut Expr,
    cands: &HashMap<String, Candidate>,
    out: &mut Vec<Stmt>,
    fresh: &mut usize,
) {
    match &mut e.kind {
        ExprKind::Call(callee, args) => {
            hoist_call_args(callee, cands, out, fresh);
            for a in args.iter_mut() {
                if let Some(name) = hoistable_name(a, cands) {
                    let bind = format!("__hoist_{}_{}", name, *fresh);
                    *fresh += 1;
                    let local = Local {
                        pat: Pat::Ident(Spanned {
                            node: bind.clone(),
                            span: a.span,
                        }),
                        ty: None,
                        init: Some(a.clone()),
                        mutable: false,
                    };
                    if !emit_let(&local, cands, fresh, out) {
                        out.push(Stmt {
                            kind: StmtKind::Local(local),
                        });
                    }
                    *a = Expr {
                        kind: ExprKind::Path(Path {
                            segments: vec![Spanned {
                                node: bind,
                                span: a.span,
                            }],
                        }),
                        span: a.span,
                    };
                } else {
                    hoist_call_args(a, cands, out, fresh);
                }
            }
        }
        ExprKind::Binary(a, _, b) => {
            hoist_call_args(a, cands, out, fresh);
            hoist_call_args(b, cands, out, fresh);
        }
        ExprKind::Unary(_, a)
        | ExprKind::Ref(a, _)
        | ExprKind::Cast(a, _)
        | ExprKind::Field(a, _) => { hoist_call_args(a, cands, out, fresh); }
        ExprKind::MethodCall(r, _, args) => {
            hoist_call_args(r, cands, out, fresh);
            for a in args {
                hoist_call_args(a, cands, out, fresh);
            }
        }
        ExprKind::Tuple(v) | ExprKind::Array(v) => {
            for a in v {
                hoist_call_args(a, cands, out, fresh);
            }
        }
        ExprKind::Macro(m) => {
            if is_format_macro(&m.path) {
                m.tokens = hoist_macro_tokens(&m.tokens, cands, out, fresh);
            }
        }
        _ => {}
    }
}

/// The candidate name of `e` when it is a call `g(pure..)` with matching arity.
fn hoistable_name(e: &Expr, cands: &HashMap<String, Candidate>) -> Option<String> {
    let ExprKind::Call(c, cargs) = &e.kind else {
        return None;
    };
    let ExprKind::Path(p) = &c.kind else {
        return None;
    };
    if p.segments.len() != 1 {
        return None;
    }
    let name = &p.segments[0].node;
    let cand = cands.get(name)?;
    if cand.params.len() == cargs.len() && cargs.iter().all(is_pure) {
        Some(name.clone())
    } else {
        None
    }
}

fn is_format_macro(path: &Path) -> bool {
    match path.segments.last() {
        Some(s) => {
            let n = s.node.as_str();
            n == "println" || n == "print" || n == "eprintln" || n == "eprint" || n == "format"
        }
        None => false,
    }
}

/// Hoist closure-returning calls out of the token stream of a formatting macro
/// (`println!`, `format!`, ...). Macro arguments stay token streams until the checker and
/// lowering re-parse them, so a call inside them is found by scanning for `name ( .. )`
/// and replaced by a fresh identifier bound by a hoisted `let`.
fn hoist_macro_tokens(
    tokens: &[Token],
    cands: &HashMap<String, Candidate>,
    out: &mut Vec<Stmt>,
    fresh: &mut usize,
) -> Vec<Token> {
    let mut kept: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let mut replaced = false;
        if let TokenKind::Ident(name) = &tokens[i].kind {
            let open = matches!(tokens.get(i + 1).map(|t| &t.kind), Some(TokenKind::LParen));
            if open && cands.contains_key(name) {
                if let Some(j) = matching_paren(tokens, i + 1) {
                    let parsed = Parser::parse_macro_args(tokens[i..=j].to_vec());
                    if parsed.len() == 1 {
                        if let Some(call_name) = hoistable_name(&parsed[0], cands) {
                            let bind = format!("__hoist_{}_{}", call_name, *fresh);
                            *fresh += 1;
                            let local = Local {
                                pat: Pat::Ident(Spanned {
                                    node: bind.clone(),
                                    span: tokens[i].span,
                                }),
                                ty: None,
                                init: Some(parsed[0].clone()),
                                mutable: false,
                            };
                            if !emit_let(&local, cands, fresh, out) {
                                out.push(Stmt {
                                    kind: StmtKind::Local(local),
                                });
                            }
                            kept.push(Token {
                                kind: TokenKind::Ident(bind),
                                span: tokens[i].span,
                            });
                            i = j + 1;
                            replaced = true;
                        }
                    }
                }
            }
        }
        if !replaced {
            kept.push(tokens[i].clone());
            i += 1;
        }
    }
    kept
}

/// The index of the `)` matching the `(` at `open`, if any.
fn matching_paren(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut j = open;
    while j < tokens.len() {
        match &tokens[j].kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Operands whose evaluation has no effect, so evaluating them earlier is harmless.
fn is_pure(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Lit(_) => true,
        ExprKind::Path(p) => p.segments.len() == 1,
        ExprKind::Unary(_, a) => is_pure(a),
        ExprKind::Binary(a, _, b) => is_pure(a) && is_pure(b),
        _ => false,
    }
}

/// If `local` is `let name = candidate(args);`, push the argument bindings and then
/// `let name = <closure with captured parameters renamed>;` onto `out` and return true.
fn emit_let(
    local: &Local,
    cands: &HashMap<String, Candidate>,
    fresh: &mut usize,
    out: &mut Vec<Stmt>,
) -> bool {
    let Pat::Ident(_) = &local.pat else {
        return false;
    };
    let Some(init) = &local.init else {
        return false;
    };
    let ExprKind::Call(callee, args) = &init.kind else {
        return false;
    };
    let ExprKind::Path(p) = &callee.kind else {
        return false;
    };
    if p.segments.len() != 1 {
        return false;
    }
    let name = p.segments[0].node.clone();
    let Some(cand) = cands.get(&name) else {
        return false;
    };
    if args.len() != cand.params.len() {
        return false;
    }
    let mut renames: HashMap<String, String> = HashMap::new();
    let mut k = 0;
    while k < args.len() {
        let param = &cand.params[k];
        let Pat::Ident(pname) = &param.pat else {
            return false;
        };
        let new_name = format!("__ret_{}_{}_{}", name, pname.node, *fresh);
        *fresh += 1;
        renames.insert(pname.node.clone(), new_name.clone());
        out.push(Stmt {
            kind: StmtKind::Local(Local {
                pat: Pat::Ident(Spanned {
                    node: new_name,
                    span: pname.span,
                }),
                ty: Some(param.ty.clone()),
                init: Some(args[k].clone()),
                mutable: false,
            }),
        });
        k += 1;
    }
    let mut closure = cand.closure.clone();
    visit_expr(
        &mut closure,
        &mut |x: &mut Expr| {
            if let ExprKind::Path(p) = &mut x.kind {
                if p.segments.len() == 1 {
                    if let Some(new) = renames.get(&p.segments[0].node) {
                        p.segments[0].node = new.clone();
                    }
                }
            }
        },
        &mut |_| {},
    );
    out.push(Stmt {
        kind: StmtKind::Local(Local {
            pat: local.pat.clone(),
            ty: local.ty.clone(),
            init: Some(closure),
            mutable: local.mutable,
        }),
    });
    true
}

/// Count the references to the function `name` anywhere in the program: as a path
/// expression (including callees) or as an identifier token inside a macro.
fn count_refs(prog: &Program, name: &str) -> usize {
    let mut count = 0usize;
    let mut prog = prog.clone();
    for item in &mut prog.items {
        visit_item_exprs(item, &mut |e: &mut Expr| {
            if let ExprKind::Path(p) = &e.kind {
                if p.segments.len() == 1 && p.segments[0].node == name {
                    count += 1;
                }
            }
            if let ExprKind::Macro(m) = &e.kind {
                for t in &m.tokens {
                    if let TokenKind::Ident(s) = &t.kind {
                        if s == name {
                            count += 1;
                        }
                    }
                }
            }
        });
    }
    count
}

pub(crate) fn visit_item_exprs(item: &mut Item, f: &mut impl FnMut(&mut Expr)) {
    match &mut item.kind {
        ItemKind::Function(func) => { visit_block(&mut func.body, f, &mut |_| {}); }
        ItemKind::Impl(imp) => {
            for it in &mut imp.items {
                visit_item_exprs(it, f);
            }
        }
        ItemKind::Const(c) => { visit_expr(&mut c.value, f, &mut |_| {}); }
        ItemKind::Static(s) => { visit_expr(&mut s.value, f, &mut |_| {}); }
        _ => {}
    }
}

/// Visit a block: `fb` first (it may rewrite the statements), then each statement's
/// expressions.
pub(crate) fn visit_block(b: &mut Block, fe: &mut impl FnMut(&mut Expr), fb: &mut impl FnMut(&mut Block)) {
    fb(b);
    for s in &mut b.stmts {
        match &mut s.kind {
            StmtKind::Local(l) => {
                if let Some(e) = &mut l.init {
                    visit_expr(e, fe, fb);
                }
            }
            StmtKind::Expr(e) | StmtKind::Semi(e) => { visit_expr(e, fe, fb); }
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &mut b.tail {
        visit_expr(t, fe, fb);
    }
}

/// Visit an expression tree: `fe` on every expression node (before its children) and
/// `fb` on every block.
pub(crate) fn visit_expr(e: &mut Expr, fe: &mut impl FnMut(&mut Expr), fb: &mut impl FnMut(&mut Block)) {
    fe(e);
    match &mut e.kind {
        ExprKind::Lit(_) | ExprKind::Path(_) | ExprKind::Break | ExprKind::Continue => {}
        ExprKind::Macro(_) => {}
        ExprKind::Binary(a, _, b) | ExprKind::Index(a, b) | ExprKind::ArrayRepeat(a, b) => {
            visit_expr(a, fe, fb);
            visit_expr(b, fe, fb);
        }
        ExprKind::Unary(_, a)
        | ExprKind::Field(a, _)
        | ExprKind::Ref(a, _)
        | ExprKind::Deref(a)
        | ExprKind::Cast(a, _)
        | ExprKind::Try(a)
        | ExprKind::Await(a) => { visit_expr(a, fe, fb); }
        ExprKind::Call(c, args) => {
            visit_expr(c, fe, fb);
            for a in args {
                visit_expr(a, fe, fb);
            }
        }
        ExprKind::MethodCall(r, _, args) => {
            visit_expr(r, fe, fb);
            for a in args {
                visit_expr(a, fe, fb);
            }
        }
        ExprKind::Array(v) | ExprKind::Tuple(v) => {
            for a in v {
                visit_expr(a, fe, fb);
            }
        }
        ExprKind::Struct(_, fields, base) => {
            for (_, v) in fields {
                visit_expr(v, fe, fb);
            }
            if let Some(b) = base {
                visit_expr(b, fe, fb);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => { visit_block(b, fe, fb); }
        ExprKind::If(c, b, els) => {
            visit_expr(c, fe, fb);
            visit_block(b, fe, fb);
            if let Some(x) = els {
                visit_expr(x, fe, fb);
            }
        }
        ExprKind::While(c, b) => {
            visit_expr(c, fe, fb);
            visit_block(b, fe, fb);
        }
        ExprKind::For(_, it, b) => {
            visit_expr(it, fe, fb);
            visit_block(b, fe, fb);
        }
        ExprKind::Return(v) => {
            if let Some(x) = v {
                visit_expr(x, fe, fb);
            }
        }
        ExprKind::Match(s, arms) => {
            visit_expr(s, fe, fb);
            for (_, x) in arms {
                visit_expr(x, fe, fb);
            }
        }
        ExprKind::Closure { body, .. } => { visit_expr(body, fe, fb); }
        ExprKind::Range(a, b, _) => {
            if let Some(x) = a {
                visit_expr(x, fe, fb);
            }
            if let Some(x) = b {
                visit_expr(x, fe, fb);
            }
        }
    }
}
