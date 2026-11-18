//! Fusing an iterator chain bound by a `let` into the `for` loop that consumes it.
//!
//! ```text
//! let tys = a.iter().filter_map(F).chain(b.iter().map(G));
//! for ty in tys { BODY }
//! ```
//!
//! becomes
//!
//! ```text
//! for ty in a.iter().filter_map(F).chain(b.iter().map(G)) { BODY }
//! ```
//!
//! when the `let` is directly followed by the loop, the bound name is a plain identifier, the
//! initializer is an iterator adapter chain, and the name is not mentioned anywhere else in the
//! block. The code lowering reads a `for` over a chain expression, not a chain stored in a
//! local, so the rewrite is what makes such loops compile. The rewrite is syntactic and keeps
//! every sub-expression and its span.

use crate::ast::*;
use crate::closure_return::{visit_block, visit_expr_with};

/// Rewrite every fusable `let` + `for` pair in `prog` (see the module docs).
pub fn inline_chain_lets(mut prog: Program) -> Program {
    for item in &mut prog.items {
        visit_item_blocks(item);
    }
    prog
}

fn visit_item_blocks(item: &mut Item) {
    match &mut item.kind {
        ItemKind::Function(func) => {
            visit_block(&mut func.body, &mut |_| {}, &mut |b: &mut Block| rewrite_block(b));
        }
        ItemKind::Impl(imp) => {
            for it in &mut imp.items {
                visit_item_blocks(it);
            }
        }
        _ => {}
    }
}

/// Whether `e` is an adapter chain that yields an iterator (its outermost call is one of the
/// adapters the `for` lowering walks).
fn is_adapter_chain(e: &Expr) -> bool {
    matches!(
        &e.kind,
        ExprKind::MethodCall(_, m, _)
            if matches!(
                m.node.as_str(),
                "chain" | "map" | "filter" | "filter_map" | "cloned" | "copied" | "zip" | "enumerate"
            )
    )
}

fn rewrite_block(b: &mut Block) {
    let mut i = 0;
    while i + 1 < b.stmts.len() {
        if let Some(name) = fusable_let(&b.stmts[i], &b.stmts[i + 1]) {
            if !mentioned_outside_loop(b, i + 1, &name) {
                let let_stmt = b.stmts.remove(i);
                let StmtKind::Local(local) = let_stmt.kind else {
                    unreachable!("fusable_let checked a local");
                };
                let init = local.init.expect("fusable_let checked an initializer");
                if let StmtKind::Expr(e) | StmtKind::Semi(e) = &mut b.stmts[i].kind {
                    if let ExprKind::For(_, iter, _) = &mut e.kind {
                        **iter = init;
                    }
                }
                continue;
            }
        }
        i += 1;
    }
}

/// The name bound by `stmts[i]` when it is `let NAME = <adapter chain>;` and `stmts[i + 1]` is
/// `for PAT in NAME { .. }`.
fn fusable_let(first: &Stmt, second: &Stmt) -> Option<String> {
    let StmtKind::Local(local) = &first.kind else {
        return None;
    };
    let Pat::Ident(name) = &local.pat else {
        return None;
    };
    let init = local.init.as_ref()?;
    if !is_adapter_chain(init) {
        return None;
    }
    let (StmtKind::Expr(e) | StmtKind::Semi(e)) = &second.kind else {
        return None;
    };
    match &e.kind {
        ExprKind::For(_, iter, _) => match &iter.kind {
            ExprKind::Path(p) if p.segments.len() == 1 && p.segments[0].node == name.node => {
                Some(name.node.clone())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Whether `name` is mentioned in `b` other than as the iterator of the `for` at `loop_idx`
/// (the statement after the `let`, which is removed by the rewrite): in the loop's body, in
/// a later statement, or in the tail.
fn mentioned_outside_loop(b: &mut Block, loop_idx: usize, name: &str) -> bool {
    let names = [name.to_string()];
    for (k, s) in b.stmts.iter_mut().enumerate() {
        if k == loop_idx {
            if let StmtKind::Expr(e) | StmtKind::Semi(e) = &mut s.kind {
                if let ExprKind::For(_, _, body) = &mut e.kind {
                    if block_mentions(body, &names) {
                        return true;
                    }
                }
            }
            continue;
        }
        if stmt_mentions(s, &names) {
            return true;
        }
    }
    if let Some(t) = &mut b.tail {
        if expr_mentions(t, &names) {
            return true;
        }
    }
    false
}

fn stmt_mentions(s: &mut Stmt, names: &[String]) -> bool {
    match &mut s.kind {
        StmtKind::Local(l) => l.init.as_mut().is_some_and(|e| expr_mentions(e, names)),
        StmtKind::Expr(e) | StmtKind::Semi(e) => expr_mentions(e, names),
        StmtKind::Item(_) => false,
    }
}

fn expr_mentions(e: &mut Expr, names: &[String]) -> bool {
    let mut found = false;
    visit_expr_with(
        e,
        &mut found,
        &mut |x: &mut Expr, f: &mut bool| {
            if let ExprKind::Path(p) = &x.kind {
                if p.segments.len() == 1 && names.contains(&p.segments[0].node) {
                    *f = true;
                }
            }
        },
        &mut |_b: &mut Block, _f: &mut bool| {},
    );
    found
}

fn block_mentions(b: &mut Block, names: &[String]) -> bool {
    let mut found = false;
    crate::closure_return::visit_block_with(
        b,
        &mut found,
        &mut |x: &mut Expr, f: &mut bool| {
            if let ExprKind::Path(p) = &x.kind {
                if p.segments.len() == 1 && names.contains(&p.segments[0].node) {
                    *f = true;
                }
            }
        },
        &mut |_b: &mut Block, _f: &mut bool| {},
    );
    found
}
