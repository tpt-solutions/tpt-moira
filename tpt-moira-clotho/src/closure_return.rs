//! Inlining of closure-returning functions at `let` call sites.
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
//! closure. The function itself is removed once it has no remaining references and
//! captures its parameters (the only case that cannot be emitted as C).
//!
//! The pass is purely syntactic and deliberately conservative: a candidate must be a
//! non-generic, non-async function whose body is only a closure with identifier
//! parameters; the closure body must bind no names of its own and contain no macros,
//! so renaming the captured parameters is safe. Calls that are not a `let` initializer
//! (for example `apply(make(4), 1)`) are left untouched.

use crate::ast::*;
use crate::token::TokenKind;
use std::collections::{HashMap, HashSet};

/// A candidate function: its parameters and the closure its body returns.
struct Candidate {
    params: Vec<Param>,
    closure: Expr,
    /// Whether the closure body refers to one of the parameters.
    captures: bool,
}

/// Rewrite every `let x = f(args)` whose `f` is a closure-returning candidate (see the
/// module docs). `prog` is returned with those call sites inlined.
pub fn inline_returned_closures(mut prog: Program) -> Program {
    let cands = collect_candidates(&prog);
    if cands.is_empty() {
        return prog;
    }
    let mut inlined: HashSet<String> = HashSet::new();
    let mut fresh = 0usize;
    for item in &mut prog.items {
        visit_item_bodies(item, &mut |b| {
            rewrite_block_lets(b, &cands, &mut inlined, &mut fresh);
        });
    }
    // Remove candidates that captured and have no references left: they cannot be emitted.
    let has_modules = prog
        .items
        .iter()
        .any(|it| matches!(it.kind, ItemKind::Module(_)));
    if !has_modules {
        let removable: Vec<String> = inlined
            .iter()
            .filter(|n| cands[*n].captures && count_refs(&prog, n) == 0)
            .cloned()
            .collect();
        prog.items.retain(|it| match &it.kind {
            ItemKind::Function(f) => !removable.contains(&f.name.node),
            _ => true,
        });
    }
    prog
}

fn collect_candidates(prog: &Program) -> HashMap<String, Candidate> {
    let mut out = HashMap::new();
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
        let ExprKind::Closure { params, .. } = &tail.kind else {
            continue;
        };
        let names: Vec<String> = match f
            .params
            .iter()
            .map(|p| match &p.pat {
                Pat::Ident(s) => Some(s.node.clone()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
        {
            Some(n) => n,
            None => continue,
        };
        // A closure parameter that shadows a function parameter would change what the
        // body refers to; leave such functions alone.
        let closure_param_names: Vec<String> = params
            .iter()
            .filter_map(|p| match &p.pat {
                Pat::Ident(s) => Some(s.node.clone()),
                _ => None,
            })
            .collect();
        if names.iter().any(|n| closure_param_names.contains(n)) {
            continue;
        }
        let mut body = (**tail).clone();
        if !rename_safe(&mut body) {
            continue;
        }
        let captures = mentions_any(&mut body.clone(), &names);
        out.insert(
            f.name.node.clone(),
            Candidate {
                params: f.params.clone(),
                closure: (**tail).clone(),
                captures,
            },
        );
    }
    out
}

/// True when the expression binds no names of its own and contains no macro, so that
/// renaming a free variable throughout it is sound.
fn rename_safe(e: &mut Expr) -> bool {
    let ok = std::cell::Cell::new(true);
    visit_expr(
        e,
        &mut |x| {
            if matches!(
                x.kind,
                ExprKind::Closure { .. } | ExprKind::Match(..) | ExprKind::For(..) | ExprKind::Macro(_)
            ) {
                ok.set(false);
            }
        },
        &mut |b| {
            if b.stmts.iter().any(|s| matches!(s.kind, StmtKind::Local(_) | StmtKind::Item(_))) {
                ok.set(false);
            }
        },
    );
    ok.get()
}

fn mentions_any(e: &mut Expr, names: &[String]) -> bool {
    let mut found = false;
    visit_expr(
        e,
        &mut |x| {
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
fn visit_item_bodies(item: &mut Item, f: &mut dyn FnMut(&mut Block)) {
    match &mut item.kind {
        ItemKind::Function(func) => visit_block(&mut func.body, &mut |_| {}, f),
        ItemKind::Impl(imp) => {
            for it in &mut imp.items {
                visit_item_bodies(it, f);
            }
        }
        ItemKind::Const(c) => visit_expr(&mut c.value, &mut |_| {}, f),
        ItemKind::Static(s) => visit_expr(&mut s.value, &mut |_| {}, f),
        _ => {}
    }
}

/// Replace each `let name = candidate(args);` in `b` by argument bindings followed by
/// the candidate's closure, with the captured parameters renamed to fresh names.
fn rewrite_block_lets(
    b: &mut Block,
    cands: &HashMap<String, Candidate>,
    inlined: &mut HashSet<String>,
    fresh: &mut usize,
) {
    let mut out: Vec<Stmt> = Vec::with_capacity(b.stmts.len());
    for stmt in std::mem::take(&mut b.stmts) {
        let replaced = match &stmt.kind {
            StmtKind::Local(local) => inline_let(local, cands, fresh),
            _ => None,
        };
        match replaced {
            Some((new_stmts, callee)) => {
                inlined.insert(callee);
                out.extend(new_stmts);
            }
            None => out.push(stmt),
        }
    }
    b.stmts = out;
}

fn inline_let(
    local: &Local,
    cands: &HashMap<String, Candidate>,
    fresh: &mut usize,
) -> Option<(Vec<Stmt>, String)> {
    let Pat::Ident(_) = &local.pat else {
        return None;
    };
    let init = local.init.as_ref()?;
    let ExprKind::Call(callee, args) = &init.kind else {
        return None;
    };
    let ExprKind::Path(p) = &callee.kind else {
        return None;
    };
    if p.segments.len() != 1 {
        return None;
    }
    let name = &p.segments[0].node;
    let cand = cands.get(name)?;
    if args.len() != cand.params.len() {
        return None;
    }
    let mut stmts = Vec::with_capacity(args.len() + 1);
    let mut renames: HashMap<String, String> = HashMap::new();
    for (param, arg) in cand.params.iter().zip(args) {
        let Pat::Ident(pname) = &param.pat else {
            return None;
        };
        let new_name = format!("__ret_{}_{}_{}", name, pname.node, *fresh);
        *fresh += 1;
        renames.insert(pname.node.clone(), new_name.clone());
        stmts.push(Stmt {
            kind: StmtKind::Local(Local {
                pat: Pat::Ident(Spanned {
                    node: new_name,
                    span: pname.span,
                }),
                ty: Some(param.ty.clone()),
                init: Some(arg.clone()),
                mutable: false,
            }),
        });
    }
    let mut closure = cand.closure.clone();
    visit_expr(
        &mut closure,
        &mut |x| {
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
    stmts.push(Stmt {
        kind: StmtKind::Local(Local {
            pat: local.pat.clone(),
            ty: local.ty.clone(),
            init: Some(closure),
            mutable: local.mutable,
        }),
    });
    Some((stmts, name.clone()))
}

/// Count the references to the function `name` anywhere in the program: as a path
/// expression (including callees) or as an identifier token inside a macro.
fn count_refs(prog: &Program, name: &str) -> usize {
    let mut count = 0usize;
    let mut count_expr = |e: &mut Expr| {
        match &e.kind {
            ExprKind::Path(p) if p.segments.len() == 1 && p.segments[0].node == name => {
                count += 1
            }
            ExprKind::Macro(m) => {
                count += m
                    .tokens
                    .iter()
                    .filter(|t| matches!(&t.kind, TokenKind::Ident(s) if s == name))
                    .count();
            }
            _ => {}
        }
    };
    let mut prog = prog.clone();
    for item in &mut prog.items {
        visit_item_exprs(item, &mut count_expr);
    }
    count
}

fn visit_item_exprs(item: &mut Item, f: &mut dyn FnMut(&mut Expr)) {
    match &mut item.kind {
        ItemKind::Function(func) => visit_block(&mut func.body, f, &mut |_| {}),
        ItemKind::Impl(imp) => {
            for it in &mut imp.items {
                visit_item_exprs(it, f);
            }
        }
        ItemKind::Const(c) => visit_expr(&mut c.value, f, &mut |_| {}),
        ItemKind::Static(s) => visit_expr(&mut s.value, f, &mut |_| {}),
        _ => {}
    }
}

/// Visit a block: `fb` first (it may rewrite the statements), then each statement's
/// expressions.
fn visit_block(b: &mut Block, fe: &mut dyn FnMut(&mut Expr), fb: &mut dyn FnMut(&mut Block)) {
    fb(b);
    for s in &mut b.stmts {
        match &mut s.kind {
            StmtKind::Local(l) => {
                if let Some(e) = &mut l.init {
                    visit_expr(e, fe, fb);
                }
            }
            StmtKind::Expr(e) | StmtKind::Semi(e) => visit_expr(e, fe, fb),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &mut b.tail {
        visit_expr(t, fe, fb);
    }
}

/// Visit an expression tree: `fe` on every expression node (before its children) and
/// `fb` on every block.
fn visit_expr(e: &mut Expr, fe: &mut dyn FnMut(&mut Expr), fb: &mut dyn FnMut(&mut Block)) {
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
        | ExprKind::Await(a) => visit_expr(a, fe, fb),
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
        ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => visit_block(b, fe, fb),
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
        ExprKind::Closure { body, .. } => visit_expr(body, fe, fb),
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
