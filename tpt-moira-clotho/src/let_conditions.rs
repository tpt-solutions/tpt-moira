//! Desugaring of `if let` and `while let` into `match`.
//!
//! The parser represents the condition `let PAT = EXPR` as a one-statement block
//! (`ExprKind::Block` holding a `Local`). That makes
//!
//! ```text
//! if let PAT = EXPR { THEN } else { ELSE }
//! while let PAT = EXPR { BODY }
//! ```
//!
//! rewrite to the forms the checker and the lowering already handle for any pattern:
//!
//! ```text
//! match EXPR { PAT => { THEN }, _ => { ELSE } }
//! loop { match EXPR { PAT => { BODY }, _ => break } }
//! ```
//!
//! A missing `else` is an empty block. The rewrite is syntactic and keeps every
//! sub-expression (and its span), so the spans the checker keys its types by still
//! name the same expressions.

use crate::ast::*;
use crate::closure_return::visit_item_exprs;

/// Rewrite every `if let` and `while let` in `prog` into `match` / `loop` (see the module
/// docs).
pub fn desugar_let_conditions(mut prog: Program) -> Program {
    for item in &mut prog.items {
        visit_item_exprs(item, &mut |e: &mut Expr| desugar_expr(e));
    }
    prog
}

/// Whether `cond` is the `let PAT = EXPR` form the parser builds for `if let`/`while let`:
/// a block of exactly one initialized `let` and no tail.
fn is_let_condition(cond: &Expr) -> bool {
    match &cond.kind {
        ExprKind::Block(b) => {
            b.tail.is_none()
                && b.stmts.len() == 1
                && match &b.stmts[0].kind {
                    StmtKind::Local(l) => l.init.is_some() && l.ty.is_none(),
                    _ => false,
                }
        }
        _ => false,
    }
}

/// The `let` of a condition that `is_let_condition` accepted.
fn take_let(cond: Expr) -> Local {
    match cond.kind {
        ExprKind::Block(b) => match b.stmts.into_iter().next() {
            Some(Stmt {
                kind: StmtKind::Local(l),
            }) => l,
            _ => unreachable!("is_let_condition checked the block"),
        },
        _ => unreachable!("is_let_condition checked the condition"),
    }
}

fn desugar_expr(e: &mut Expr) {
    let is_let_if = matches!(&e.kind, ExprKind::If(c, _, _) if is_let_condition(c));
    let is_let_while = matches!(&e.kind, ExprKind::While(c, _) if is_let_condition(c));
    if !is_let_if && !is_let_while {
        return;
    }
    let span = e.span;
    let old = std::mem::replace(&mut e.kind, ExprKind::Break);
    e.kind = match old {
        ExprKind::If(cond, then, els) => {
            let local = take_let(*cond);
            let els_e = match els {
                Some(x) => *x,
                None => Expr {
                    kind: ExprKind::Block(Block {
                        stmts: Vec::new(),
                        tail: None,
                    }),
                    span,
                },
            };
            let scrut = local.init.expect("let condition has an initializer");
            ExprKind::Match(
                Box::new(scrut),
                vec![
                    (
                        local.pat,
                        Expr {
                            kind: ExprKind::Block(then),
                            span,
                        },
                    ),
                    (Pat::Wild, els_e),
                ],
            )
        }
        ExprKind::While(cond, body) => {
            let local = take_let(*cond);
            let scrut = local.init.expect("let condition has an initializer");
            let break_block = Expr {
                kind: ExprKind::Block(Block {
                    stmts: vec![Stmt {
                        kind: StmtKind::Semi(Expr {
                            kind: ExprKind::Break,
                            span,
                        }),
                    }],
                    tail: None,
                }),
                span,
            };
            let inner = Expr {
                kind: ExprKind::Match(
                    Box::new(scrut),
                    vec![
                        (
                            local.pat,
                            Expr {
                                kind: ExprKind::Block(body),
                                span,
                            },
                        ),
                        (Pat::Wild, break_block),
                    ],
                ),
                span,
            };
            ExprKind::Loop(Block {
                stmts: vec![Stmt {
                    kind: StmtKind::Semi(inner),
                }],
                tail: None,
            })
        }
        other => other,
    };
}
