//! Inlining of higher-order helpers at the call sites that pass them a closure literal.
//!
//! A top-level function that takes a closure as an `impl Fn(A) -> R` (or `&mut impl FnMut`,
//! or `fn(A) -> R`) parameter cannot be lowered as a plain function when the closure captures
//! variables: a function pointer carries no environment, and a let-bound closure takes its
//! captures by value. For a call such as
//!
//! ```text
//! each(&[1, 2], &mut |a, b| n = n + a);
//! ```
//!
//! this pass expands the call in place. The closure literal is never bound to a value: each
//! call of the helper's closure parameter inside the helper's body is replaced by the closure's
//! body, with its parameters bound to the call's arguments. The body refers to the call site's
//! variables directly, so a write to a captured variable reaches it. The helper's own parameters
//! and locals are renamed to fresh names, so they cannot capture anything the closure uses.
//!
//! The pass is syntactic and conservative. A helper is expanded only when its body has no
//! `return`, `?`, macro, `match`, closure or non-identifier pattern; its closure parameter is
//! only ever called (never passed on or stored); and it calls no other helper that takes a
//! closure, which keeps the expansion finite. A call is expanded only when the closure is its one
//! closure argument and the closure body meets the same rules. Helpers are kept, so a call that
//! is not expanded still lowers as before.

use crate::ast::*;
use crate::closure_return::{visit_expr_with, visit_item_exprs};
use crate::token::Span;
use std::collections::{HashMap, HashSet};

/// A helper that may be expanded: its function, the index and name of its closure parameter,
/// and the `impl Fn` type's parameter types followed by its result type.
struct Helper {
    func: Function,
    closure_idx: usize,
    closure_name: String,
    fn_args: Vec<Type>,
}

/// Expand the closure-taking helper calls of `prog` (see the module docs).
pub fn inline_closure_calls(mut prog: Program) -> Program {
    let closure_fns: HashSet<String> = prog
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Function(f) if closure_param(f).is_some() => Some(f.name.node.clone()),
            _ => None,
        })
        .collect();
    let helpers: HashMap<String, Helper> = prog
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Function(f) => eligible(f, &closure_fns).map(|h| (f.name.node.clone(), h)),
            _ => None,
        })
        .collect();
    if helpers.is_empty() {
        return prog;
    }
    let mut fresh = 0usize;
    for item in &mut prog.items {
        visit_item_exprs(item, &mut |e: &mut Expr| {
            expand_call(e, &helpers, &mut fresh);
        });
    }
    prog
}

/// The index and `impl Fn` type of the closure parameter of `f`, if it has one.
fn closure_param(f: &Function) -> Option<(usize, Vec<Type>)> {
    f.params.iter().enumerate().find_map(|(i, p)| {
        let mut ty = &p.ty;
        while let TypeKind::Ref(inner) | TypeKind::RefMut(inner) = &ty.kind {
            ty = inner;
        }
        match &ty.kind {
            TypeKind::Path(path, args)
                if path.segments.len() == 1
                    && path.segments[0].node == "fn"
                    && !args.is_empty() =>
            {
                Some((i, args.clone()))
            }
            _ => None,
        }
    })
}

/// Whether a body meets the expansion rules: no `return`, `?`, macro, `match`, closure, call of
/// a closure-taking helper, or non-identifier binding.
fn body_is_expandable(body: &Block, closure_fns: &HashSet<String>, allow_match: bool) -> bool {
    let mut e = Expr {
        kind: ExprKind::Block(body.clone()),
        span: Span::new(0, 0),
    };
    let mut ok = true;
    visit_expr_with(
        &mut e,
        &mut ok,
        &mut |x: &mut Expr, ok: &mut bool| match &x.kind {
            ExprKind::Return(_)
            | ExprKind::Try(_)
            | ExprKind::Macro(_)
            | ExprKind::Closure { .. } => *ok = false,
            ExprKind::Match(..) if !allow_match => *ok = false,
            ExprKind::For(pat, _, _) if !matches!(pat, Pat::Ident(_)) => *ok = false,
            ExprKind::Call(callee, _) => {
                if let ExprKind::Path(p) = &callee.kind {
                    if p.segments.len() == 1 && closure_fns.contains(&p.segments[0].node) {
                        *ok = false;
                    }
                }
            }
            _ => {}
        },
        &mut |b: &mut Block, ok: &mut bool| {
            for s in &b.stmts {
                if let StmtKind::Local(l) = &s.kind {
                    if !matches!(l.pat, Pat::Ident(_)) {
                        *ok = false;
                    }
                }
            }
        },
    );
    ok
}

/// The helper for `f`, when its body meets the rules of the module docs.
fn eligible(f: &Function, closure_fns: &HashSet<String>) -> Option<Helper> {
    if f.is_async || f.name.node == "main" {
        return None;
    }
    let all_ident = f.params.iter().all(|p| match &p.pat {
        Pat::Ident(s) => s.node != "self",
        _ => false,
    });
    if !all_ident {
        return None;
    }
    let (closure_idx, fn_args) = closure_param(f)?;
    let Pat::Ident(cname) = &f.params[closure_idx].pat else {
        return None;
    };
    let closure_name = cname.node.clone();
    if !body_is_expandable(&f.body, closure_fns, false) {
        return None;
    }
    // The closure parameter is only ever called: every use of its name is a callee.
    let mut body = Expr {
        kind: ExprKind::Block(f.body.clone()),
        span: Span::new(0, 0),
    };
    let mut uses = (0usize, 0usize);
    visit_expr_with(
        &mut body,
        &mut uses,
        &mut |x: &mut Expr, u: &mut (usize, usize)| {
            if let ExprKind::Path(p) = &x.kind {
                if p.segments.len() == 1 && p.segments[0].node == closure_name {
                    u.0 += 1;
                }
            }
            if let ExprKind::Call(callee, _) = &x.kind {
                if let ExprKind::Path(p) = &callee.kind {
                    if p.segments.len() == 1 && p.segments[0].node == closure_name {
                        u.1 += 1;
                    }
                }
            }
        },
        &mut |_: &mut Block, _: &mut (usize, usize)| {},
    );
    if uses.0 != uses.1 {
        return None;
    }
    Some(Helper {
        func: f.clone(),
        closure_idx,
        closure_name,
        fn_args,
    })
}

/// Replace `e` by the expanded block when it is a call of a helper with a closure literal.
fn expand_call(e: &mut Expr, helpers: &HashMap<String, Helper>, fresh: &mut usize) {
    let ExprKind::Call(callee, args) = &e.kind else {
        return;
    };
    let ExprKind::Path(p) = &callee.kind else {
        return;
    };
    if p.segments.len() != 1 {
        return;
    }
    let Some(h) = helpers.get(&p.segments[0].node) else {
        return;
    };
    if args.len() != h.func.params.len() {
        return;
    }
    let closure_arg: &Expr = match &args[h.closure_idx].kind {
        ExprKind::Ref(inner, _) => inner,
        _ => &args[h.closure_idx],
    };
    let ExprKind::Closure { params, ret, body } = &closure_arg.kind else {
        return;
    };
    if params.len() + 1 != h.fn_args.len() {
        return;
    }
    // Only the closure is a closure argument.
    for (i, a) in args.iter().enumerate() {
        let a: &Expr = match &a.kind {
            ExprKind::Ref(inner, _) => inner,
            _ => a,
        };
        if i != h.closure_idx && matches!(a.kind, ExprKind::Closure { .. }) {
            return;
        }
    }
    let closure_body_block = Block {
        stmts: Vec::new(),
        tail: Some(body.clone()),
    };
    if !body_is_expandable(&closure_body_block, &HashSet::new(), true) {
        return;
    }
    let mut closure_arg_names: Vec<String> = Vec::new();
    for p in params {
        match &p.pat {
            Pat::Ident(s) => closure_arg_names.push(s.node.clone()),
            _ => return,
        }
    }
    let k = *fresh;
    *fresh += 1;
    let span = e.span;

    // The helper's parameters become fresh locals, bound to the call's arguments in order.
    let mut renames: HashMap<String, String> = HashMap::new();
    let mut stmts: Vec<Stmt> = Vec::new();
    for (i, p) in h.func.params.iter().enumerate() {
        if i == h.closure_idx {
            continue;
        }
        let Pat::Ident(name) = &p.pat else {
            return;
        };
        let fresh_name = format!("__inl_{}_{k}", name.node);
        let declared = if h.func.generics.params.is_empty() {
            Some(p.ty.clone())
        } else {
            None
        };
        stmts.push(local_typed(&fresh_name, declared, args[i].clone(), span));
        renames.insert(name.node.clone(), fresh_name);
    }
    // The helper's own locals (lets and `for` binders) get fresh names too.
    let mut body_expr = Expr {
        kind: ExprKind::Block(h.func.body.clone()),
        span,
    };
    let mut let_names: Vec<String> = Vec::new();
    visit_expr_with(
        &mut body_expr,
        &mut let_names,
        &mut |x: &mut Expr, names: &mut Vec<String>| {
            if let ExprKind::For(Pat::Ident(n), _, _) = &x.kind {
                names.push(n.node.clone());
            }
        },
        &mut |b: &mut Block, names: &mut Vec<String>| {
            for s in &b.stmts {
                if let StmtKind::Local(l) = &s.kind {
                    if let Pat::Ident(n) = &l.pat {
                        names.push(n.node.clone());
                    }
                }
            }
        },
    );
    for n in let_names {
        renames
            .entry(n.clone())
            .or_insert_with(|| format!("__inl_{n}_{k}"));
    }
    // Rename the helper's names, then replace the calls of its closure parameter.
    visit_expr_with(
        &mut body_expr,
        &mut (),
        &mut |x: &mut Expr, _: &mut ()| {
            if let ExprKind::Path(p) = &mut x.kind {
                if p.segments.len() == 1 {
                    if let Some(new) = renames.get(&p.segments[0].node) {
                        p.segments[0].node = new.clone();
                    }
                }
            }
            if let ExprKind::For(Pat::Ident(n), _, _) = &mut x.kind {
                if let Some(new) = renames.get(&n.node) {
                    n.node = new.clone();
                }
            }
        },
        &mut |b: &mut Block, _: &mut ()| {
            for s in &mut b.stmts {
                if let StmtKind::Local(l) = &mut s.kind {
                    if let Pat::Ident(n) = &mut l.pat {
                        if let Some(new) = renames.get(&n.node) {
                            n.node = new.clone();
                        }
                    }
                }
            }
        },
    );
    let closure_ret: Option<Type> = ret.clone().or_else(|| h.fn_args.last().cloned());
    let closure_param_tys: Vec<Type> = h.fn_args[..closure_arg_names.len()].to_vec();
    let ret_is_unit =
        matches!(&closure_ret, Some(t) if matches!(&t.kind, TypeKind::Tuple(v) if v.is_empty()));
    let mut call_counter = 0usize;
    replace_closure_calls(
        &mut body_expr,
        &ClosureCall {
            name: &h.closure_name,
            arg_names: &closure_arg_names,
            params,
            param_tys: &closure_param_tys,
            body,
            ret: closure_ret.as_ref(),
            ret_is_unit,
            k,
        },
        &mut call_counter,
        span,
    );

    let ExprKind::Block(body_block) = body_expr.kind else {
        return;
    };
    stmts.extend(body_block.stmts);
    e.kind = ExprKind::Block(Block {
        stmts,
        tail: body_block.tail,
    });
}

/// The closure parameter being replaced, and the closure literal it stands for.
struct ClosureCall<'a> {
    name: &'a str,
    arg_names: &'a [String],
    params: &'a [Param],
    /// The parameter types of the helper's `impl Fn` type, used where the closure leaves one out.
    param_tys: &'a [Type],
    body: &'a Expr,
    ret: Option<&'a Type>,
    ret_is_unit: bool,
    k: usize,
}

/// Replace each call `name(args..)` in `root` by the closure body, with its parameters bound to
/// `args` (typed by the closure's parameter types). A typed result keeps the closure's result
/// type.
fn replace_closure_calls(root: &mut Expr, call: &ClosureCall<'_>, counter: &mut usize, span: Span) {
    visit_expr_with(
        root,
        &mut (),
        &mut |x: &mut Expr, _: &mut ()| {
            let ExprKind::Call(callee, args) = &x.kind else {
                return;
            };
            let ExprKind::Path(p) = &callee.kind else {
                return;
            };
            if p.segments.len() != 1 || p.segments[0].node != call.name {
                return;
            }
            if args.len() != call.arg_names.len() {
                return;
            }
            let id = *counter;
            *counter += 1;
            let mut stmts: Vec<Stmt> = Vec::new();
            let mut renames: HashMap<String, String> = HashMap::new();
            for (i, name) in call.arg_names.iter().enumerate() {
                let fresh_name = format!("__cl_{name}_{}_{id}", call.k);
                let declared = match &call.params[i].ty.kind {
                    TypeKind::Infer => Some(call.param_tys[i].clone()),
                    _ => Some(call.params[i].ty.clone()),
                };
                stmts.push(local_typed(&fresh_name, declared, args[i].clone(), span));
                renames.insert(name.clone(), fresh_name);
            }
            // The closure body, with its parameters renamed to the bound names.
            let mut inner = call.body.clone();
            visit_expr_with(
                &mut inner,
                &mut (),
                &mut |y: &mut Expr, _: &mut ()| {
                    if let ExprKind::Path(p) = &mut y.kind {
                        if p.segments.len() == 1 {
                            if let Some(new) = renames.get(&p.segments[0].node) {
                                p.segments[0].node = new.clone();
                            }
                        }
                    }
                },
                &mut |_: &mut Block, _: &mut ()| {},
            );
            let expansion = match call.ret {
                Some(t) if !call.ret_is_unit => {
                    let result = format!("__cr_{}_{id}", call.k);
                    stmts.push(local_typed(&result, Some(t.clone()), inner, span));
                    Block {
                        stmts,
                        tail: Some(Box::new(Expr {
                            kind: ExprKind::Path(Path {
                                segments: vec![Spanned { node: result, span }],
                            }),
                            span,
                        })),
                    }
                }
                _ => Block {
                    stmts,
                    tail: Some(Box::new(inner)),
                },
            };
            x.kind = ExprKind::Block(expansion);
        },
        &mut |_: &mut Block, _: &mut ()| {},
    );
}

/// `let name: ty = init;` (or `let name = init;`) as a statement.
fn local_typed(name: &str, ty: Option<Type>, init: Expr, span: Span) -> Stmt {
    Stmt {
        kind: StmtKind::Local(Local {
            pat: Pat::Ident(Spanned {
                node: name.to_string(),
                span,
            }),
            ty,
            init: Some(init),
            mutable: false,
        }),
    }
}
