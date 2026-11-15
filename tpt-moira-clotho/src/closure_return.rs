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
//! arguments of `println!`-like or `assert!`/`vec!` macros is hoisted out of the token
//! stream. Captured
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
    /// Leading `let name = <pure>;` bindings of the function body, in order.
    lets: Vec<Local>,
    closure: Expr,
}

/// Rewrite the closure-returning call sites of `prog` (see the module docs).
pub fn inline_returned_closures(mut prog: Program) -> Program {
    let (cands, capturing, capturing_methods) = collect_candidates(&prog);
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
        // Same for capturing methods: drop them from their impl once no call remains.
        let removable_methods: Vec<String> = capturing_methods
            .into_iter()
            .filter(|n| count_method_refs(&prog, n) == 0)
            .collect();
        if !removable_methods.is_empty() {
            for item in &mut prog.items {
                if let ItemKind::Impl(imp) = &mut item.kind {
                    imp.items.retain(|it| match &it.kind {
                        ItemKind::Function(f) => !removable_methods.contains(&f.name.node),
                        _ => true,
                    });
                }
            }
        }
    }
    prog
}

/// Candidate-map key of the method `name`. A dot cannot start an identifier, so a method
/// never collides with a free function of the same name.
fn method_key(name: &str) -> String {
    format!(".{name}")
}

/// The closure-returning functions and inherent methods, the names of the free functions
/// that capture, and the names of the methods that capture.
fn collect_candidates(prog: &Program) -> (HashMap<String, Candidate>, Vec<String>, Vec<String>) {
    let mut out: HashMap<String, Candidate> = HashMap::new();
    let mut capturing: Vec<String> = Vec::new();
    let mut capturing_methods: Vec<String> = Vec::new();
    for item in &prog.items {
        let ItemKind::Function(f) = &item.kind else {
            continue;
        };
        if f.is_async || !f.generics.params.is_empty() || f.name.node == "main" {
            continue;
        }
        if let Some((cand, captures)) = candidate_of(f) {
            if captures {
                capturing.push(f.name.node.clone());
            }
            out.insert(f.name.node.clone(), cand);
        }
    }
    // A method is only inlined when its name is unique across all impls: without type
    // information, a call `x.name(..)` could otherwise be a different type's method.
    let counts = method_name_counts(prog);
    for item in &prog.items {
        let ItemKind::Impl(imp) = &item.kind else {
            continue;
        };
        if !imp.trait_name.node.is_empty() || !imp.generics.params.is_empty() {
            continue;
        }
        for it in &imp.items {
            let ItemKind::Function(f) = &it.kind else {
                continue;
            };
            let has_self = matches!(
                f.params.first(),
                Some(Param { pat: Pat::Ident(s), .. }) if s.node == "self"
            );
            if f.is_async
                || !f.generics.params.is_empty()
                || !has_self
                || counts.get(&f.name.node) != Some(&1)
            {
                continue;
            }
            if let Some((cand, captures)) = candidate_of(f) {
                if captures {
                    capturing_methods.push(f.name.node.clone());
                }
                out.insert(method_key(&f.name.node), cand);
            }
        }
    }
    (out, capturing, capturing_methods)
}

/// How many impl items (of any impl) carry each function name.
fn method_name_counts(prog: &Program) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in &prog.items {
        if let ItemKind::Impl(imp) = &item.kind {
            for it in &imp.items {
                if let ItemKind::Function(f) = &it.kind {
                    *counts.entry(f.name.node.clone()).or_insert(0) += 1;
                }
            }
        }
    }
    counts
}

/// The candidate for a function whose body is pure `let`s followed by a closure over its
/// identifier parameters, and whether that closure captures (mentions a parameter or a
/// `let`). `None` when the shape or the renaming rules do not allow inlining.
fn candidate_of(f: &Function) -> Option<(Candidate, bool)> {
    let lets = candidate_lets(f)?;
    let tail = f.body.tail.as_ref()?;
    let ExprKind::Closure { params, body, .. } = &tail.kind else {
        return None;
    };
    let names: Vec<String> = f
        .params
        .iter()
        .map(|p| match &p.pat {
            Pat::Ident(s) => Some(s.node.clone()),
            _ => None,
        })
        .collect::<Option<Vec<String>>>()?;
    let let_names: Vec<String> = lets
        .iter()
        .filter_map(|l| match &l.pat {
            Pat::Ident(s) => Some(s.node.clone()),
            _ => None,
        })
        .collect();
    // A closure parameter that shadows a function parameter or `let` would change what
    // the body refers to; leave such functions alone.
    for p in params {
        if let Pat::Ident(s) = &p.pat {
            if names.contains(&s.node) || let_names.contains(&s.node) {
                return None;
            }
        }
    }
    let mut body = (**body).clone();
    if !rename_safe(&mut body) {
        return None;
    }
    let mut captured_names = names;
    captured_names.extend(let_names);
    let captures = mentions_any(&mut body, &captured_names);
    Some((
        Candidate {
            params: f.params.clone(),
            lets,
            closure: (**tail).clone(),
        },
        captures,
    ))
}

/// The leading `let` bindings of a candidate body, when every statement is a plain
/// `let name = <pure expression>;` with a distinct name not used by a parameter.
/// Returns `None` if the body has any other statement.
fn candidate_lets(f: &Function) -> Option<Vec<Local>> {
    let mut lets: Vec<Local> = Vec::new();
    let mut seen: Vec<String> = f
        .params
        .iter()
        .filter_map(|p| match &p.pat {
            Pat::Ident(s) => Some(s.node.clone()),
            _ => None,
        })
        .collect();
    for st in &f.body.stmts {
        let StmtKind::Local(l) = &st.kind else {
            return None;
        };
        let Pat::Ident(name) = &l.pat else {
            return None;
        };
        let init = l.init.as_ref()?;
        if !is_pure(init) || seen.contains(&name.node) {
            return None;
        }
        seen.push(name.node.clone());
        lets.push(l.clone());
    }
    Some(lets)
}

/// True when the expression binds no names of its own and contains no macro, so that
/// renaming a free variable throughout it is sound.
fn rename_safe(e: &mut Expr) -> bool {
    let mut bad: bool = false;
    visit_expr_with(
        e,
        &mut bad,
        &mut |x: &mut Expr, flag: &mut bool| {
            if matches!(
                x.kind,
                ExprKind::Closure { .. }
                    | ExprKind::Match(..)
                    | ExprKind::For(..)
                    | ExprKind::Macro(_)
            ) {
                *flag = true;
            }
        },
        &mut |_blk: &mut Block, _st: &mut bool| {},
    );
    visit_expr_with(
        e,
        &mut bad,
        &mut |_ex: &mut Expr, _st: &mut bool| {},
        &mut |b: &mut Block, flag: &mut bool| {
            for s in &b.stmts {
                if matches!(s.kind, StmtKind::Local(_) | StmtKind::Item(_)) {
                    *flag = true;
                }
            }
        },
    );
    !bad
}

/// The state of [`mentions_any`]: the names searched for, and whether one was found.
struct MentionScan {
    names: Vec<String>,
    found: bool,
}

fn mentions_any(e: &mut Expr, names: &[String]) -> bool {
    let mut owned: Vec<String> = Vec::new();
    let mut k = 0;
    while k < names.len() {
        owned.push(names[k].clone());
        k += 1;
    }
    let mut scan = MentionScan {
        names: owned,
        found: false,
    };
    visit_expr_with(
        e,
        &mut scan,
        &mut |x: &mut Expr, sc: &mut MentionScan| {
            if let ExprKind::Path(p) = &x.kind {
                if p.segments.len() == 1 && sc.names.contains(&p.segments[0].node) {
                    sc.found = true;
                }
            }
        },
        &mut |_blk: &mut Block, _sc: &mut MentionScan| {},
    );
    scan.found
}

/// Apply `f` to every block of an item's bodies (functions, impl methods, consts, statics).
fn visit_item_bodies(item: &mut Item, f: &mut impl FnMut(&mut Block)) {
    match &mut item.kind {
        ItemKind::Function(func) => {
            visit_block(&mut func.body, &mut |_| {}, f);
        }
        ItemKind::Impl(imp) => {
            for it in &mut imp.items {
                visit_item_bodies(it, f);
            }
        }
        ItemKind::Const(c) => {
            visit_expr(&mut c.value, &mut |_| {}, f);
        }
        ItemKind::Static(s) => {
            visit_expr(&mut s.value, &mut |_| {}, f);
        }
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
                    let allow = impure_hoist_ok(e, cands);
                    hoist_call_args(e, cands, &mut out, fresh, allow);
                }
            }
            StmtKind::Expr(e) | StmtKind::Semi(e) => {
                let allow = impure_hoist_ok(e, cands);
                hoist_call_args(e, cands, &mut out, fresh, allow);
            }
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
///
/// `allow_impure` is set by `impure_hoist_ok` for a statement whose only effectful
/// operand is one candidate call: then that call's arguments may be impure, because
/// hoisting it runs them before the statement, which nothing else precedes.
fn hoist_call_args(
    e: &mut Expr,
    cands: &HashMap<String, Candidate>,
    out: &mut Vec<Stmt>,
    fresh: &mut usize,
    allow_impure: bool,
) {
    match &mut e.kind {
        ExprKind::Call(callee, args) => {
            hoist_nested(callee, cands, out, fresh);
            for a in args.iter_mut() {
                if let Some(name) = hoistable_name(a, cands, allow_impure) {
                    hoist_one(a, &name, cands, out, fresh);
                } else {
                    hoist_nested(a, cands, out, fresh);
                }
            }
        }
        _ => hoist_nested(e, cands, out, fresh),
    }
}

/// Bind the candidate call `a` (named `name`) to a fresh `let` pushed onto `out`, and
/// replace `a` with a reference to that binding.
fn hoist_one(
    a: &mut Expr,
    name: &str,
    cands: &HashMap<String, Candidate>,
    out: &mut Vec<Stmt>,
    fresh: &mut usize,
) {
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
}

/// Hoisting below the top-level expression of a statement: pure-argument rules only.
fn hoist_nested(
    e: &mut Expr,
    cands: &HashMap<String, Candidate>,
    out: &mut Vec<Stmt>,
    fresh: &mut usize,
) {
    match &mut e.kind {
        ExprKind::Call(callee, args) => {
            hoist_nested(callee, cands, out, fresh);
            for a in args.iter_mut() {
                if let Some(name) = hoistable_name(a, cands, false) {
                    hoist_one(a, &name, cands, out, fresh);
                } else {
                    hoist_nested(a, cands, out, fresh);
                }
            }
        }
        ExprKind::Binary(a, _, b) => {
            hoist_nested(a, cands, out, fresh);
            hoist_nested(b, cands, out, fresh);
        }
        ExprKind::Unary(_, a)
        | ExprKind::Ref(a, _)
        | ExprKind::Cast(a, _)
        | ExprKind::Field(a, _) => {
            hoist_nested(a, cands, out, fresh);
        }
        ExprKind::MethodCall(r, _, args) => {
            hoist_nested(r, cands, out, fresh);
            for a in args {
                hoist_nested(a, cands, out, fresh);
            }
        }
        ExprKind::Tuple(v) | ExprKind::Array(v) => {
            for a in v {
                hoist_nested(a, cands, out, fresh);
            }
        }
        ExprKind::Macro(m) if is_format_macro(&m.path) => {
            m.tokens = hoist_macro_tokens(&m.tokens, cands, out, fresh);
        }
        _ => {}
    }
}

/// True when the statement expression `e` is `f(.., g(impure..), ..)`: a call to a plain
/// name whose only non-pure argument is one candidate call `g(..)` of matching arity.
/// Hoisting `g(..)` before the statement then runs its impure arguments before anything
/// else in the statement, so evaluation order is preserved.
fn impure_hoist_ok(e: &Expr, cands: &HashMap<String, Candidate>) -> bool {
    let ExprKind::Call(callee, args) = &e.kind else {
        return false;
    };
    let ExprKind::Path(p) = &callee.kind else {
        return false;
    };
    if p.segments.len() != 1 {
        return false;
    }
    let mut impure = 0usize;
    for a in args {
        if is_pure(a) {
            continue;
        }
        impure += 1;
        let ExprKind::Call(c, cargs) = &a.kind else {
            return false;
        };
        let ExprKind::Path(cp) = &c.kind else {
            return false;
        };
        if cp.segments.len() != 1 {
            return false;
        }
        match cands.get(&cp.segments[0].node) {
            Some(cand) if cand.params.len() == cargs.len() => {}
            _ => return false,
        }
    }
    impure == 1
}

/// The candidate name of `e` when it is a call `g(pure..)` with matching arity.
fn hoistable_name(
    e: &Expr,
    cands: &HashMap<String, Candidate>,
    allow_impure: bool,
) -> Option<String> {
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
    if cand.params.len() == cargs.len() && (allow_impure || cargs.iter().all(is_pure)) {
        Some(name.clone())
    } else {
        None
    }
}

/// Built-in macros whose arguments are evaluated eagerly, in order, once per expansion,
/// so a pure candidate call among them can be bound by a `let` before the statement.
/// Other macros (user `macro_rules!`, `matches!`) may evaluate lazily or repeatedly and
/// are left alone.
fn is_format_macro(path: &Path) -> bool {
    match path.segments.last() {
        Some(s) => matches!(
            s.node.as_str(),
            "println"
                | "print"
                | "eprintln"
                | "eprint"
                | "format"
                | "assert"
                | "debug_assert"
                | "assert_eq"
                | "assert_ne"
                | "debug_assert_eq"
                | "debug_assert_ne"
                | "vec"
        ),
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
                        if let Some(call_name) = hoistable_name(&parsed[0], cands, false) {
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
        ExprKind::Unary(_, a) | ExprKind::Field(a, _) => is_pure(a),
        ExprKind::Binary(a, _, b) => is_pure(a) && is_pure(b),
        _ => false,
    }
}

/// If `local` is `let name = candidate(args);` or `let name = recv.method(args);`, push the
/// argument bindings (a method's receiver is bound to `self`) and then the lets and
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
    // The candidate, the name used for fresh bindings, and the value of each parameter.
    let is_method = matches!(init.kind, ExprKind::MethodCall(..));
    let (name, cand, inits): (String, &Candidate, Vec<Expr>) = match &init.kind {
        ExprKind::Call(callee, args) => {
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
            (name, cand, args.clone())
        }
        ExprKind::MethodCall(recv, mname, args) => {
            let ExprKind::Path(rp) = &recv.kind else {
                return false;
            };
            if rp.segments.len() != 1 {
                return false;
            }
            let Some(cand) = cands.get(&method_key(&mname.node)) else {
                return false;
            };
            if args.len() + 1 != cand.params.len() {
                return false;
            }
            // `&self` and `&mut self` bind a reference to the receiver; `self` binds a copy.
            let self_init = match &cand.params[0].ty.kind {
                TypeKind::Ref(_) => Expr {
                    kind: ExprKind::Ref(recv.clone(), false),
                    span: recv.span,
                },
                TypeKind::RefMut(_) => Expr {
                    kind: ExprKind::Ref(recv.clone(), true),
                    span: recv.span,
                },
                _ => (**recv).clone(),
            };
            let mut inits = vec![self_init];
            inits.extend(args.iter().cloned());
            (mname.node.clone(), cand, inits)
        }
        _ => return false,
    };
    let mut renames: HashMap<String, String> = HashMap::new();
    let mut k = 0;
    while k < inits.len() {
        let param = &cand.params[k];
        let Pat::Ident(pname) = &param.pat else {
            return false;
        };
        let new_name = format!("__ret_{}_{}_{}", name, pname.node, *fresh);
        *fresh += 1;
        renames.insert(pname.node.clone(), new_name.clone());
        // The receiver's type is left to inference: the checker sees the receiver directly.
        let self_arg = is_method && k == 0;
        out.push(Stmt {
            kind: StmtKind::Local(Local {
                pat: Pat::Ident(Spanned {
                    node: new_name,
                    span: pname.span,
                }),
                ty: if self_arg { None } else { Some(param.ty.clone()) },
                init: Some(inits[k].clone()),
                mutable: false,
            }),
        });
        k += 1;
    }
    for l in &cand.lets {
        let Pat::Ident(lname) = &l.pat else {
            return false;
        };
        let new_name = format!("__ret_{}_{}_{}", name, lname.node, *fresh);
        *fresh += 1;
        let Some(init0) = &l.init else {
            return false;
        };
        let mut init = init0.clone();
        rename_paths(&mut init, &renames);
        out.push(Stmt {
            kind: StmtKind::Local(Local {
                pat: Pat::Ident(Spanned {
                    node: new_name.clone(),
                    span: lname.span,
                }),
                ty: l.ty.clone(),
                init: Some(init),
                mutable: false,
            }),
        });
        renames.insert(lname.node.clone(), new_name);
    }
    let mut closure = cand.closure.clone();
    rename_paths(&mut closure, &renames);
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

/// Rename every single-segment path in `e` that `renames` maps.
fn rename_paths(e: &mut Expr, renames: &HashMap<String, String>) {
    visit_expr(
        e,
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
}

/// Count the references to the method `name`: method calls, path expressions whose last
/// segment is `name` (`Type::name`), and identifier tokens inside macros.
fn count_method_refs(prog: &Program, name: &str) -> usize {
    let mut count = 0usize;
    let mut prog = prog.clone();
    for item in &mut prog.items {
        visit_item_exprs(item, &mut |e: &mut Expr| match &e.kind {
            ExprKind::MethodCall(_, m, _) if m.node == name => count += 1,
            ExprKind::Path(p) if p.segments.last().map(|s| s.node == name) == Some(true) => {
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
        });
    }
    count
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
        ItemKind::Function(func) => {
            visit_block(&mut func.body, f, &mut |_| {});
        }
        ItemKind::Impl(imp) => {
            for it in &mut imp.items {
                visit_item_exprs(it, f);
            }
        }
        ItemKind::Const(c) => {
            visit_expr(&mut c.value, f, &mut |_| {});
        }
        ItemKind::Static(s) => {
            visit_expr(&mut s.value, f, &mut |_| {});
        }
        _ => {}
    }
}

/// Visit a block: `fb` first (it may rewrite the statements), then each statement's
/// expressions.
pub(crate) fn visit_block(
    b: &mut Block,
    fe: &mut impl FnMut(&mut Expr),
    fb: &mut impl FnMut(&mut Block),
) {
    fb(b);
    for s in &mut b.stmts {
        match &mut s.kind {
            StmtKind::Local(l) => {
                if let Some(e) = &mut l.init {
                    visit_expr(e, fe, fb);
                }
            }
            StmtKind::Expr(e) | StmtKind::Semi(e) => {
                visit_expr(e, fe, fb);
            }
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &mut b.tail {
        visit_expr(t, fe, fb);
    }
}

/// Visit an expression tree: `fe` on every expression node (before its children) and
/// `fb` on every block.
pub(crate) fn visit_expr(
    e: &mut Expr,
    fe: &mut impl FnMut(&mut Expr),
    fb: &mut impl FnMut(&mut Block),
) {
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
        | ExprKind::Await(a) => {
            visit_expr(a, fe, fb);
        }
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
        ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => {
            visit_block(b, fe, fb);
        }
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
        ExprKind::Closure { body, .. } => {
            visit_expr(body, fe, fb);
        }
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

/// [`visit_block`] with a state `st` passed to every callback (a closure cannot update a
/// captured variable in the Moira subset, so the state is an argument instead).
pub(crate) fn visit_block_with<S>(
    b: &mut Block,
    st: &mut S,
    fe: &mut impl FnMut(&mut Expr, &mut S),
    fb: &mut impl FnMut(&mut Block, &mut S),
) {
    fb(b, st);
    for s in &mut b.stmts {
        match &mut s.kind {
            StmtKind::Local(l) => {
                if let Some(e) = &mut l.init {
                    visit_expr_with(e, st, fe, fb);
                }
            }
            StmtKind::Expr(e) | StmtKind::Semi(e) => {
                visit_expr_with(e, st, fe, fb);
            }
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &mut b.tail {
        visit_expr_with(t, st, fe, fb);
    }
}

/// [`visit_expr`] with a state `st` passed to every callback.
pub(crate) fn visit_expr_with<S>(
    e: &mut Expr,
    st: &mut S,
    fe: &mut impl FnMut(&mut Expr, &mut S),
    fb: &mut impl FnMut(&mut Block, &mut S),
) {
    fe(e, st);
    match &mut e.kind {
        ExprKind::Lit(_) | ExprKind::Path(_) | ExprKind::Break | ExprKind::Continue => {}
        ExprKind::Macro(_) => {}
        ExprKind::Binary(a, _, b) | ExprKind::Index(a, b) | ExprKind::ArrayRepeat(a, b) => {
            visit_expr_with(a, st, fe, fb);
            visit_expr_with(b, st, fe, fb);
        }
        ExprKind::Unary(_, a)
        | ExprKind::Field(a, _)
        | ExprKind::Ref(a, _)
        | ExprKind::Deref(a)
        | ExprKind::Cast(a, _)
        | ExprKind::Try(a)
        | ExprKind::Await(a) => {
            visit_expr_with(a, st, fe, fb);
        }
        ExprKind::Call(c, args) => {
            visit_expr_with(c, st, fe, fb);
            for a in args {
                visit_expr_with(a, st, fe, fb);
            }
        }
        ExprKind::MethodCall(r, _, args) => {
            visit_expr_with(r, st, fe, fb);
            for a in args {
                visit_expr_with(a, st, fe, fb);
            }
        }
        ExprKind::Array(v) | ExprKind::Tuple(v) => {
            for a in v {
                visit_expr_with(a, st, fe, fb);
            }
        }
        ExprKind::Struct(_, fields, base) => {
            for (_, v) in fields {
                visit_expr_with(v, st, fe, fb);
            }
            if let Some(b) = base {
                visit_expr_with(b, st, fe, fb);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => {
            visit_block_with(b, st, fe, fb);
        }
        ExprKind::If(c, b, els) => {
            visit_expr_with(c, st, fe, fb);
            visit_block_with(b, st, fe, fb);
            if let Some(x) = els {
                visit_expr_with(x, st, fe, fb);
            }
        }
        ExprKind::While(c, b) => {
            visit_expr_with(c, st, fe, fb);
            visit_block_with(b, st, fe, fb);
        }
        ExprKind::For(_, it, b) => {
            visit_expr_with(it, st, fe, fb);
            visit_block_with(b, st, fe, fb);
        }
        ExprKind::Return(v) => {
            if let Some(x) = v {
                visit_expr_with(x, st, fe, fb);
            }
        }
        ExprKind::Match(s, arms) => {
            visit_expr_with(s, st, fe, fb);
            for (_, x) in arms {
                visit_expr_with(x, st, fe, fb);
            }
        }
        ExprKind::Closure { body, .. } => {
            visit_expr_with(body, st, fe, fb);
        }
        ExprKind::Range(a, b, _) => {
            if let Some(x) = a {
                visit_expr_with(x, st, fe, fb);
            }
            if let Some(x) = b {
                visit_expr_with(x, st, fe, fb);
            }
        }
    }
}
