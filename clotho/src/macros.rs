//! Concurrent macro-expansion pipeline for the Phase-1 subset.
//!
//! Phase 1 deliberately ships a *restricted* macro system (full `macro_rules!`
//! expansion is a Phase-3 deliverable). The restricted system supports simple
//! token-substitution macros declared with the `macro` keyword:
//!
//! ```text
//! macro double(x) = x + x;
//! fn main() { let y = double(21); }   // expands to `let y = 21 + 21;`
//! ```
//!
//! Macros are extracted from the token stream before parsing, then their
//! invocations (ordinary call syntax `name(args)`) are expanded. Expansion is a
//! pure tree rewrite, so it parallelizes trivially: [`expand_program_concurrent`]
//! expands each top-level item on its own worker thread, keeping the CPU busy
//! while the next translation unit is still being read from disk (see the
//! [`crate::pipeline`] module for the I/O-concurrent driver).

use crate::ast::{self, Expr, ExprKind, Item, ItemKind, Pat, Program, Stmt, StmtKind};
use crate::error::{Diag, ParseResult};
use crate::parser::Parser;
use crate::token::{Token, TokenKind};
use std::collections::HashMap;

/// A single restricted macro: a list of parameter names and a body expression.
#[derive(Debug, Clone)]
pub struct MacroDef {
    pub params: Vec<String>,
    pub body: Expr,
}

/// A table of macros keyed by name.
#[derive(Debug, Clone, Default)]
pub struct MacroTable {
    pub defs: HashMap<String, MacroDef>,
}

impl MacroTable {
    pub fn new() -> Self {
        MacroTable {
            defs: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

/// Pull `macro` definitions out of a token stream and return the remaining
/// tokens with the definitions removed. Each body is parsed once into an [`Expr`]
/// so that later expansion is a pure AST rewrite.
pub fn extract_macros(tokens: &[Token]) -> ParseResult<(MacroTable, Vec<Token>)> {
    let mut table = MacroTable::new();
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());

    let mut i = 0;
    while i < tokens.len() {
        // Look for: `macro Ident ( params ) = body ;`
        if matches!(tokens[i].kind, TokenKind::Macro) {
            // `macro` `Ident`
            if i + 1 >= tokens.len() || !matches!(tokens[i + 1].kind, TokenKind::Ident(_)) {
                return Err(Diag::error(
                    "expected macro name after `macro`",
                    Some(tokens[i].span),
                ));
            }
            let name = match &tokens[i + 1].kind {
                TokenKind::Ident(n) => n.clone(),
                _ => unreachable!(),
            };

            // `( params )`
            let mut j = i + 2;
            if j >= tokens.len() || !matches!(tokens[j].kind, TokenKind::LParen) {
                return Err(Diag::error(
                    "expected `(` in macro declaration",
                    Some(tokens[j].span),
                ));
            }
            j += 1;
            let mut params: Vec<String> = Vec::new();
            while j < tokens.len() && !matches!(tokens[j].kind, TokenKind::RParen) {
                match &tokens[j].kind {
                    TokenKind::Ident(p) => params.push(p.clone()),
                    TokenKind::Comma => {}
                    other => {
                        return Err(Diag::error(
                            format!("expected macro parameter, found {}", other),
                            Some(tokens[j].span),
                        ))
                    }
                }
                j += 1;
            }
            if j >= tokens.len() || !matches!(tokens[j].kind, TokenKind::RParen) {
                return Err(Diag::error(
                    "expected `)` to close macro parameters",
                    Some(tokens[j].span),
                ));
            }
            j += 1; // past `)`
            if j >= tokens.len() || !matches!(tokens[j].kind, TokenKind::Eq) {
                return Err(Diag::error(
                    "expected `=` in macro declaration",
                    Some(tokens[j].span),
                ));
            }
            j += 1; // past `=`

            // Body runs until a top-level `;` (balanced delimiters allowed).
            let body_start = j;
            let mut depth: i32 = 0;
            while j < tokens.len() {
                match tokens[j].kind {
                    TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
                    TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => depth -= 1,
                    TokenKind::Semicolon if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            if j >= tokens.len() {
                return Err(Diag::error(
                    "unterminated macro body (missing `;`)",
                    Some(tokens[body_start].span),
                ));
            }
            let body_tokens: Vec<Token> = tokens[body_start..j].to_vec();
            let body = Parser::new(body_tokens).parse_expr()?;

            table.defs.insert(name, MacroDef { params, body });
            // Skip the terminating `;`.
            i = j + 1;
            continue;
        }

        out.push(tokens[i].clone());
        i += 1;
    }

    Ok((table, out))
}

/// Expand every macro invocation in a parsed program (single-threaded reference
/// path). Identical in result to [`expand_program_concurrent`].
pub fn expand_program(prog: &Program, table: &MacroTable) -> Program {
    let items = prog
        .items
        .iter()
        .map(|item| expand_item(item, table))
        .collect();
    Program { items }
}

/// Expand every macro invocation in a parsed program, distributing the work
/// across worker threads so expansion overlaps with the rest of the build.
pub fn expand_program_concurrent(prog: &Program, table: &MacroTable) -> Program {
    if table.is_empty() {
        return prog.clone();
    }
    let expanded: Vec<Item> = std::thread::scope(|s| {
        let handles: Vec<_> = prog
            .items
            .iter()
            .map(|item| s.spawn(|| expand_item(item, table)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    Program { items: expanded }
}

fn expand_item(item: &Item, table: &MacroTable) -> Item {
    let kind = match &item.kind {
        ItemKind::Function(f) => ItemKind::Function(ast::Function {
            name: f.name.clone(),
            generics: f.generics.clone(),
            is_async: f.is_async,
            is_unsafe: f.is_unsafe,
            params: f.params.clone(),
            ret: f.ret.clone(),
            body: expand_block(&f.body, table),
        }),
        other => other.clone(),
    };
    Item {
        visibility: item.visibility,
        kind,
    }
}

fn expand_block(b: &ast::Block, table: &MacroTable) -> ast::Block {
    let stmts = b.stmts.iter().map(|s| expand_stmt(s, table)).collect();
    let tail = b.tail.as_ref().map(|e| Box::new(expand_expr(e, table)));
    ast::Block { stmts, tail }
}

fn expand_stmt(s: &Stmt, table: &MacroTable) -> Stmt {
    let kind = match &s.kind {
        StmtKind::Local(local) => StmtKind::Local(ast::Local {
            pat: local.pat.clone(),
            ty: local.ty.clone(),
            init: local.init.as_ref().map(|e| expand_expr(e, table)),
            mutable: local.mutable,
        }),
        StmtKind::Semi(e) => StmtKind::Semi(expand_expr(e, table)),
        StmtKind::Expr(e) => StmtKind::Expr(expand_expr(e, table)),
        StmtKind::Item(item) => StmtKind::Item(expand_item(item, table)),
    };
    Stmt { kind }
}

fn expand_expr(e: &Expr, table: &MacroTable) -> Expr {
    let span = e.span;
    let kind = match &e.kind {
        ExprKind::Call(callee, args) => {
            if let ExprKind::Path(p) = &callee.kind {
                if let Some(name) = p.segments.last().map(|s| s.node.clone()) {
                    if let Some(def) = table.defs.get(&name) {
                        let expanded_args: Vec<Expr> =
                            args.iter().map(|a| expand_expr(a, table)).collect();
                        let mut subst = HashMap::new();
                        for (param, arg) in def.params.iter().zip(expanded_args.iter()) {
                            subst.insert(param.clone(), arg.clone());
                        }
                        // Substitute parameters in the body, then keep expanding
                        // in case the body (or an argument) contained macros.
                        let substituted = subst_expr(&def.body, &subst);
                        return expand_expr(&substituted, table);
                    }
                }
            }
            ExprKind::Call(
                Box::new(expand_expr(callee, table)),
                args.iter().map(|a| expand_expr(a, table)).collect(),
            )
        }
        ExprKind::Binary(l, op, r) => ExprKind::Binary(
            Box::new(expand_expr(l, table)),
            *op,
            Box::new(expand_expr(r, table)),
        ),
        ExprKind::Unary(op, e2) => ExprKind::Unary(*op, Box::new(expand_expr(e2, table))),
        ExprKind::Ref(e2, _) => ExprKind::Ref(Box::new(expand_expr(e2, table)), false),
        ExprKind::Deref(e2) => ExprKind::Deref(Box::new(expand_expr(e2, table))),
        ExprKind::Field(base, field) => {
            ExprKind::Field(Box::new(expand_expr(base, table)), field.clone())
        }
        ExprKind::Index(base, idx) => ExprKind::Index(
            Box::new(expand_expr(base, table)),
            Box::new(expand_expr(idx, table)),
        ),
        ExprKind::Array(elems) => {
            ExprKind::Array(elems.iter().map(|e2| expand_expr(e2, table)).collect())
        }
        ExprKind::Struct(path, fields, base) => ExprKind::Struct(
            path.clone(),
            fields
                .iter()
                .map(|(n, v)| (n.clone(), expand_expr(v, table)))
                .collect(),
            base.as_ref().map(|b| Box::new(expand_expr(b, table))),
        ),
        ExprKind::Block(b) => ExprKind::Block(expand_block(b, table)),
        ExprKind::If(cond, then_b, else_b) => ExprKind::If(
            Box::new(expand_expr(cond, table)),
            expand_block(then_b, table),
            else_b.as_ref().map(|b| match &b.kind {
                ExprKind::If(..) => Box::new(expand_expr(b, table)),
                ExprKind::Block(b2) => Box::new(Expr {
                    kind: ExprKind::Block(expand_block(b2, table)),
                    span: b.span,
                }),
                _ => Box::new(expand_expr(b, table)),
            }),
        ),
        ExprKind::While(cond, body) => ExprKind::While(
            Box::new(expand_expr(cond, table)),
            expand_block(body, table),
        ),
        ExprKind::Loop(body) => ExprKind::Loop(expand_block(body, table)),
        ExprKind::For(pat, iter, body) => ExprKind::For(
            pat.clone(),
            Box::new(expand_expr(iter, table)),
            expand_block(body, table),
        ),
        ExprKind::Match(scrut, arms) => ExprKind::Match(
            Box::new(expand_expr(scrut, table)),
            arms.iter()
                .map(|(p, b)| (p.clone(), expand_expr(b, table)))
                .collect(),
        ),
        ExprKind::Return(opt) => {
            ExprKind::Return(opt.as_ref().map(|e| Box::new(expand_expr(e, table))))
        }
        other => other.clone(),
    };
    Expr { kind, span }
}

/// Substitute parameter identifiers in `expr` with their argument expressions.
fn subst_expr(expr: &Expr, subst: &HashMap<String, Expr>) -> Expr {
    let span = expr.span;
    let kind = match &expr.kind {
        ExprKind::Path(p) => {
            if p.segments.len() == 1 {
                let name = &p.segments[0].node;
                if let Some(replacement) = subst.get(name) {
                    return replacement.clone();
                }
            }
            ExprKind::Path(p.clone())
        }
        ExprKind::Call(callee, args) => ExprKind::Call(
            Box::new(subst_expr(callee, subst)),
            args.iter().map(|a| subst_expr(a, subst)).collect(),
        ),
        ExprKind::Binary(l, op, r) => ExprKind::Binary(
            Box::new(subst_expr(l, subst)),
            *op,
            Box::new(subst_expr(r, subst)),
        ),
        ExprKind::Unary(op, e2) => ExprKind::Unary(*op, Box::new(subst_expr(e2, subst))),
        ExprKind::Ref(e2, _) => ExprKind::Ref(Box::new(subst_expr(e2, subst)), false),
        ExprKind::Deref(e2) => ExprKind::Deref(Box::new(subst_expr(e2, subst))),
        ExprKind::Field(base, field) => {
            ExprKind::Field(Box::new(subst_expr(base, subst)), field.clone())
        }
        ExprKind::Index(base, idx) => ExprKind::Index(
            Box::new(subst_expr(base, subst)),
            Box::new(subst_expr(idx, subst)),
        ),
        ExprKind::Array(elems) => {
            ExprKind::Array(elems.iter().map(|e2| subst_expr(e2, subst)).collect())
        }
        ExprKind::Struct(path, fields, base) => ExprKind::Struct(
            path.clone(),
            fields
                .iter()
                .map(|(n, v)| (n.clone(), subst_expr(v, subst)))
                .collect(),
            base.as_ref().map(|b| Box::new(subst_expr(b, subst))),
        ),
        ExprKind::Block(b) => ExprKind::Block(subst_block(b, subst)),
        ExprKind::If(cond, then_b, else_b) => ExprKind::If(
            Box::new(subst_expr(cond, subst)),
            subst_block(then_b, subst),
            else_b.as_ref().map(|b| match &b.kind {
                ExprKind::If(..) => Box::new(subst_expr(b, subst)),
                ExprKind::Block(b2) => Box::new(Expr {
                    kind: ExprKind::Block(subst_block(b2, subst)),
                    span: b.span,
                }),
                _ => Box::new(subst_expr(b, subst)),
            }),
        ),
        ExprKind::While(cond, body) => {
            ExprKind::While(Box::new(subst_expr(cond, subst)), subst_block(body, subst))
        }
        ExprKind::Loop(body) => ExprKind::Loop(subst_block(body, subst)),
        ExprKind::For(pat, iter, body) => ExprKind::For(
            pat.clone(),
            Box::new(subst_expr(iter, subst)),
            subst_block(body, subst),
        ),
        ExprKind::Match(scrut, arms) => ExprKind::Match(
            Box::new(subst_expr(scrut, subst)),
            arms.iter()
                .map(|(p, b)| (p.clone(), subst_expr(b, subst)))
                .collect(),
        ),
        ExprKind::Return(opt) => {
            ExprKind::Return(opt.as_ref().map(|e| Box::new(subst_expr(e, subst))))
        }
        other => other.clone(),
    };
    Expr { kind, span }
}

fn subst_block(b: &ast::Block, subst: &HashMap<String, Expr>) -> ast::Block {
    let stmts = b.stmts.iter().map(|s| subst_stmt(s, subst)).collect();
    let tail = b.tail.as_ref().map(|e| Box::new(subst_expr(e, subst)));
    ast::Block { stmts, tail }
}

fn subst_stmt(s: &Stmt, subst: &HashMap<String, Expr>) -> Stmt {
    let kind = match &s.kind {
        StmtKind::Local(local) => StmtKind::Local(ast::Local {
            pat: local.pat.clone(),
            ty: local.ty.clone(),
            init: local.init.as_ref().map(|e| subst_expr(e, subst)),
            mutable: local.mutable,
        }),
        StmtKind::Semi(e) => StmtKind::Semi(subst_expr(e, subst)),
        StmtKind::Expr(e) => StmtKind::Expr(subst_expr(e, subst)),
        StmtKind::Item(item) => StmtKind::Item(item.clone()),
    };
    Stmt { kind }
}

/// Convenience: a pattern is never a macro parameter, so this helper documents
/// that substitution preserves patterns untouched.
#[allow(dead_code)]
fn subst_pat(_p: &Pat) {}
