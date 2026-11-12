//! Monomorphization: generic functions, structs and impl blocks are specialized for each
//! concrete instantiation the program uses, so the backends only see non-generic code.
//!
//! The checker records each generic call and struct literal with the types it solved for
//! the generic parameters (see [`crate::infer::GenericCall`]). This pass then:
//! - copies a generic function per instantiation (`name__i64`), substituting the concrete
//!   types into its signature and body, and renames each call to its instance;
//! - copies a generic struct per instantiation (`Pair__i64`), along with every generic
//!   impl block for it, and rewrites each type annotation, struct literal and associated
//!   call (`Pair::new`) that names an instantiation to the instance's name.
//!
//! The generic originals are removed from the result. Methods with their own generic
//! parameters are not specialized yet.

use crate::infer::{GenericCall, TypeChecker};
use crate::ty::Ty;
use std::collections::{HashMap, HashSet};
use tpt_moira_clotho::ast::*;
use tpt_moira_clotho::token::{Span, TokenKind};

/// Specialize the generic definitions of `prog` (see the module docs). `externs` are the
/// extern crates, which the checker needs to resolve calls into them.
pub fn monomorphize(prog: &Program, externs: &[(&str, &Program)]) -> Program {
    let mut calls: HashMap<(usize, usize), GenericCall> = HashMap::new();
    let mut calls_by_start: HashMap<usize, GenericCall> = HashMap::new();
    for c in TypeChecker::generic_calls_of(prog, externs) {
        if c.owner.is_some() {
            calls_by_start.insert(c.span.start, c.clone());
        }
        calls.insert((c.span.start, c.span.end), c);
    }

    let closure_caps = TypeChecker::closure_captures_of(prog, externs);

    let mut generic_fns: HashMap<String, Function> = HashMap::new();
    let mut plain_fns: HashMap<String, Function> = HashMap::new();
    let mut generic_structs: HashMap<String, StructDef> = HashMap::new();
    let mut generic_impls: Vec<ImplDef> = Vec::new();
    for item in &prog.items {
        match &item.kind {
            ItemKind::Function(f) if !f.generics.params.is_empty() => {
                generic_fns.insert(f.name.node.clone(), f.clone());
            }
            ItemKind::Function(f) => {
                plain_fns.insert(f.name.node.clone(), f.clone());
            }
            ItemKind::Struct(s) if !s.generics.params.is_empty() => {
                generic_structs.insert(s.name.node.clone(), s.clone());
            }
            ItemKind::Impl(imp) if is_generic_impl(imp, &prog.items) => {
                generic_impls.push(imp.clone());
            }
            _ => {}
        }
    }
    // Generic structs of the extern crates are instantiated here too, as copies of their
    // definitions: a crate that uses another crate's generic struct needs its instances.
    for (_, ext) in externs {
        for item in &ext.items {
            match &item.kind {
                ItemKind::Struct(s) if !s.generics.params.is_empty() => {
                    generic_structs
                        .entry(s.name.node.clone())
                        .or_insert_with(|| s.clone());
                }
                ItemKind::Impl(imp) if is_generic_impl(imp, &ext.items) => {
                    generic_impls.push(imp.clone());
                }
                _ => {}
            }
        }
    }
    let generic_struct_names: HashSet<String> = generic_structs.keys().cloned().collect();
    if generic_fns.is_empty() && generic_structs.is_empty() && !generic_methods_exist(prog) {
        return prog.clone();
    }

    // Methods with their own type parameters, by (owner, name), from the non-generic impls.
    let mut generic_methods: HashMap<(String, String), Function> = HashMap::new();
    for item in &prog.items {
        if let ItemKind::Impl(imp) = &item.kind {
            if imp.generics.params.is_empty() {
                if let Some(owner) = imp_self_name(imp) {
                    for m in &imp.items {
                        if let ItemKind::Function(f) = &m.kind {
                            if !f.generics.params.is_empty() {
                                generic_methods
                                    .insert((owner.to_string(), f.name.node.clone()), f.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut mono = Mono {
        calls,
        calls_by_start,
        closure_caps,
        plain_fns,
        inline: Vec::new(),
        closure_locals: HashMap::new(),
        generic_fns,
        generic_structs,
        generic_struct_names,
        generic_impls,
        generic_methods,
        seen: HashSet::new(),
        queue: Vec::new(),
        out: Vec::new(),
    };

    let empty: HashMap<String, Ty> = HashMap::new();
    for item in &prog.items {
        let mut item = item.clone();
        match &mut item.kind {
            ItemKind::Function(f) => {
                if !f.generics.params.is_empty() {
                    continue;
                }
                mono.rewrite_fn(f, &empty);
            }
            ItemKind::Struct(s) => {
                if !s.generics.params.is_empty() {
                    continue;
                }
                for f in &mut s.fields {
                    mono.ty(&mut f.ty, &empty);
                }
            }
            ItemKind::Impl(imp) => {
                if is_generic_impl(imp, &prog.items) {
                    continue;
                }
                mono.rewrite_impl(imp, &empty);
            }
            _ => {}
        }
        mono.out.push(item);
    }

    // Instances may request further instances: drain the worklist.
    while let Some(work) = mono.queue.pop() {
        match work {
            Work::Fn {
                instance,
                callee,
                sub,
                closure,
            } => {
                let base = mono
                    .generic_fns
                    .get(&callee)
                    .or_else(|| mono.plain_fns.get(&callee))
                    .cloned();
                let Some(mut f) = base else {
                    continue;
                };
                f.name.node = instance;
                f.generics = Generics::default();
                mono.inline = inline_contexts(&mut f, &closure);
                mono.rewrite_fn(&mut f, &sub);
                mono.inline.clear();
                if let Some(spec) = &closure {
                    insert_captures(&mut f, spec);
                }
                mono.out.push(Item {
                    visibility: Visibility::Private,
                    kind: ItemKind::Function(f),
                    attributes: Vec::new(),
                });
            }
            Work::Method {
                owner,
                method,
                instance,
                sub,
                closure,
            } => {
                let Some(mut f) = mono
                    .generic_methods
                    .get(&(owner.clone(), method.clone()))
                    .cloned()
                else {
                    continue;
                };
                f.name.node = instance.clone();
                f.generics = Generics::default();
                mono.inline = inline_contexts(&mut f, &closure);
                mono.rewrite_fn(&mut f, &sub);
                mono.inline.clear();
                if let Some(spec) = &closure {
                    insert_captures(&mut f, spec);
                }
                for item in mono.out.iter_mut() {
                    if let ItemKind::Impl(imp) = &mut item.kind {
                        if imp_self_name(imp) == Some(owner.as_str()) && imp.generics.params.is_empty()
                        {
                            imp.items.push(Item {
                                visibility: Visibility::Private,
                                kind: ItemKind::Function(f.clone()),
                                attributes: Vec::new(),
                            });
                            break;
                        }
                    }
                }
            }
            Work::Struct {
                instance,
                name,
                args,
            } => {
                let Some(generic) = mono.generic_structs.get(&name).cloned() else {
                    continue;
                };
                let mut sub: HashMap<String, Ty> = HashMap::new();
                for (p, a) in generic.generics.params.iter().zip(args.iter()) {
                    sub.insert(p.name.node.clone(), a.clone());
                }
                let mut s = generic;
                s.name.node = instance.clone();
                s.generics = Generics::default();
                for f in &mut s.fields {
                    mono.ty(&mut f.ty, &sub);
                }
                mono.out.push(Item {
                    visibility: Visibility::Private,
                    kind: ItemKind::Struct(s),
                    attributes: Vec::new(),
                });
                // Every generic impl block of this struct gets an instance too.
                let impls: Vec<ImplDef> = mono
                    .generic_impls
                    .iter()
                    .filter(|imp| imp_self_name(imp) == Some(name.as_str()))
                    .cloned()
                    .collect();
                for imp in impls {
                    let mut isub: HashMap<String, Ty> = HashMap::new();
                    if let Some(args_of_self) = self_args(&imp) {
                        for (pname, a) in args_of_self.iter().zip(args.iter()) {
                            isub.insert(pname.clone(), a.clone());
                        }
                    }
                    let mut inst = imp.clone();
                    inst.generics = Generics::default();
                    inst.for_ty = Type {
                        kind: TypeKind::Path(
                            Path {
                                segments: vec![Spanned {
                                    node: instance.clone(),
                                    span: inst.for_ty.span,
                                }],
                            },
                            Vec::new(),
                        ),
                        span: inst.for_ty.span,
                    };
                    mono.rewrite_impl(&mut inst, &isub);
                    mono.out.push(Item {
                        visibility: Visibility::Private,
                        kind: ItemKind::Impl(inst),
                        attributes: Vec::new(),
                    });
                }
            }
        }
    }

    Program { items: mono.out }
}

/// Work waiting to be generated.
enum Work {
    /// A function instance: `instance` is `callee` with `sub` substituted. With a
    /// closure, the closure parameter is inlined and its captures are parameters.
    Fn {
        instance: String,
        callee: String,
        sub: HashMap<String, Ty>,
        closure: Option<ClosureSpec>,
    },
    /// A struct instance: `name` with the concrete `args` for its parameters.
    Struct {
        instance: String,
        name: String,
        args: Vec<Ty>,
    },
    /// A method instance: `method` of `owner`, with `sub` substituted, added to the
    /// owner's impl block under `instance`.
    Method {
        owner: String,
        method: String,
        instance: String,
        sub: HashMap<String, Ty>,
        closure: Option<ClosureSpec>,
    },
}

/// The inlined closures of an instance: its closure parameter, and the closures its
/// closure-valued captures stand for.
fn inline_contexts(f: &mut Function, closure: &Option<ClosureSpec>) -> Vec<InlineCtx> {
    let Some(spec) = closure else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(main) = take_closure_param(f, spec) {
        out.push(main);
    }
    out.extend(spec.inlines.iter().cloned());
    out
}

/// A `let` bound to a closure literal: its span, parameters and body.
#[derive(Clone)]
struct LocalClosure {
    span: Span,
    params: Vec<Param>,
    body: Expr,
}

/// A closure passed to an instance: the parameter at `param_index` is replaced by the
/// closure's body, which is inlined at each call of it; the closure's captured variables
/// become parameters at `caps_at`.
#[derive(Clone)]
struct ClosureSpec {
    param_index: usize,
    caps_at: usize,
    closure_params: Vec<Param>,
    body: Expr,
    caps: Vec<(String, Type)>,
    /// The closure-valued captures' closures, inlined where they are called.
    inlines: Vec<InlineCtx>,
}

/// The closure parameter an instance is inlining: its name, its closure's parameter
/// patterns, and its body.
#[derive(Clone)]
struct InlineCtx {
    fname: String,
    params: Vec<Pat>,
    body: Expr,
}

/// Remove the closure parameter from an instance and return what is inlined for it.
fn take_closure_param(f: &mut Function, spec: &ClosureSpec) -> Option<InlineCtx> {
    let fname = match &f.params.get(spec.param_index)?.pat {
        Pat::Ident(n) => n.node.clone(),
        _ => return None,
    };
    f.params.remove(spec.param_index);
    Some(InlineCtx {
        fname,
        params: spec.closure_params.iter().map(|p| p.pat.clone()).collect(),
        body: spec.body.clone(),
    })
}

/// Add the captured variables of a closure as parameters of the instance.
fn insert_captures(f: &mut Function, spec: &ClosureSpec) {
    let span = f.name.span;
    for (i, (name, ty)) in spec.caps.iter().enumerate() {
        f.params.insert(
            spec.caps_at + i,
            Param {
                pat: Pat::Ident(Spanned {
                    node: name.clone(),
                    span,
                }),
                ty: ty.clone(),
            },
        );
    }
}

struct Mono {
    /// The captured variables of each closure literal, by span.
    closure_caps: HashMap<(usize, usize), Vec<(String, Ty)>>,
    /// Top-level functions that are not generic.
    plain_fns: HashMap<String, Function>,
    /// The closures being inlined into the instance being rewritten.
    inline: Vec<InlineCtx>,
    /// The closures bound by `let` in the function being rewritten, by name.
    closure_locals: HashMap<String, LocalClosure>,
    /// Generic calls and struct literals, by callee/expression span.
    calls: HashMap<(usize, usize), GenericCall>,
    /// Associated calls (`Pair::new`), by the start of their path.
    calls_by_start: HashMap<usize, GenericCall>,
    generic_fns: HashMap<String, Function>,
    generic_structs: HashMap<String, StructDef>,
    generic_struct_names: HashSet<String>,
    generic_impls: Vec<ImplDef>,
    /// Methods with their own type parameters, by (owner, name).
    generic_methods: HashMap<(String, String), Function>,
    /// Instances requested so far (by name), so each is generated once.
    seen: HashSet<String>,
    queue: Vec<Work>,
    out: Vec<Item>,
}

impl Mono {
    /// The instance name of the function `callee` at `params`, requesting it if new.
    fn request_fn(&mut self, callee: &str, params: &[(String, Ty)]) -> Option<String> {
        let instance = instance_name(callee, params)?;
        if self.seen.insert(instance.clone()) {
            let sub: HashMap<String, Ty> = params.iter().cloned().collect();
            self.queue.push(Work::Fn {
                instance: instance.clone(),
                callee: callee.to_string(),
                sub,
                closure: None,
            });
        }
        Some(instance)
    }

    /// `f(args)` where `f` is the closure parameter being inlined: the closure's body, with
    /// its parameters bound to the arguments.
    fn try_inline(&mut self, e: &mut Expr, sub: &HashMap<String, Ty>) -> Option<ExprKind> {
        let called = match &e.kind {
            ExprKind::Call(callee, _) => match &callee.kind {
                ExprKind::Path(p) if p.segments.len() == 1 => p.segments[0].node.clone(),
                _ => return None,
            },
            _ => return None,
        };
        let ctx = self.inline.iter().find(|c| c.fname == called)?.clone();
        let ExprKind::Call(_, args) = &mut e.kind else {
            return None;
        };
        for a in args.iter_mut() {
            self.expr(a, sub);
        }
        let mut stmts = Vec::new();
        for (pat, a) in ctx.params.iter().zip(args.iter()) {
            stmts.push(Stmt {
                kind: StmtKind::Local(Local {
                    pat: pat.clone(),
                    ty: None,
                    init: Some(a.clone()),
                    mutable: false,
                }),
            });
        }
        let mut body = ctx.body.clone();
        self.expr(&mut body, sub);
        Some(ExprKind::Block(Block {
            stmts,
            tail: Some(Box::new(body)),
        }))
    }

    /// A call of a function that takes a closure literal: the function's instance that
    /// inlines the closure, called with the closure's captured variables first.
    fn specialize_call(&mut self, e: &mut Expr, sub: &HashMap<String, Ty>) -> Option<ExprKind> {
        let ExprKind::Call(callee, args) = &e.kind else {
            return None;
        };
        let ExprKind::Path(p) = &callee.kind else {
            return None;
        };
        if p.segments.len() != 1 {
            return None;
        }
        let ci = args
            .iter()
            .position(|a| matches!(a.kind, ExprKind::Closure { .. }))?;
        let name = p.segments[0].node.clone();
        let base = self
            .generic_fns
            .get(&name)
            .or_else(|| self.plain_fns.get(&name))?
            .clone();
        if !is_closure_param(&base, ci) {
            return None;
        }
        let closure = args[ci].clone();
        let (cparams, cbody) = match &closure.kind {
            ExprKind::Closure { params, body, .. } => (params.clone(), (**body).clone()),
            _ => return None,
        };
        let (caps, inlines) = self.captures_of(&closure, sub)?;
        let (base_inst, solved): (String, HashMap<String, Ty>) = if base.generics.params.is_empty() {
            (name.clone(), HashMap::new())
        } else {
            let call = self.calls.get(&(callee.span.start, callee.span.end))?.clone();
            let params: Vec<(String, Ty)> = call
                .params
                .iter()
                .map(|(n, t)| (n.clone(), subst_ty(t, sub)))
                .collect();
            (instance_name(&name, &params)?, params.into_iter().collect())
        };
        let inst = format!("{}__cl{}", base_inst, closure.span.start);
        if self.seen.insert(inst.clone()) {
            self.queue.push(Work::Fn {
                instance: inst.clone(),
                callee: name.clone(),
                sub: solved,
                closure: Some(ClosureSpec {
                    param_index: ci,
                    caps_at: 0,
                    closure_params: cparams,
                    body: cbody,
                    caps: caps.clone(),
                    inlines: inlines.clone(),
                }),
            });
        }
        let (callee_span, args_span) = (callee.span, closure.span);
        let mut new_args = Vec::new();
        for (n, _) in &caps {
            new_args.push(path_expr(n, args_span));
        }
        let rest: Vec<Expr> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != ci)
            .map(|(_, a)| a.clone())
            .collect();
        for mut a in rest {
            self.expr(&mut a, sub);
            new_args.push(a);
        }
        Some(ExprKind::Call(
            Box::new(path_expr(&inst, callee_span)),
            new_args,
        ))
    }

    /// A method call (of a method with its own generics) that takes a closure literal.
    fn specialize_method(&mut self, e: &mut Expr, sub: &HashMap<String, Ty>) -> Option<ExprKind> {
        let ExprKind::MethodCall(recv, method, args) = &e.kind else {
            return None;
        };
        let ci = args
            .iter()
            .position(|a| matches!(a.kind, ExprKind::Closure { .. }))?;
        let call = self
            .calls
            .get(&(method.span.start, method.span.end))?
            .clone();
        if !call.method {
            return None;
        }
        let owner = call.owner.clone()?;
        let base = self
            .generic_methods
            .get(&(owner.clone(), method.node.clone()))?
            .clone();
        // The receiver is the method's first parameter, so the closure is at `ci + 1`.
        if !is_closure_param(&base, ci + 1) {
            return None;
        }
        let closure = args[ci].clone();
        let (cparams, cbody) = match &closure.kind {
            ExprKind::Closure { params, body, .. } => (params.clone(), (**body).clone()),
            _ => return None,
        };
        let (caps, inlines) = self.captures_of(&closure, sub)?;
        let params: Vec<(String, Ty)> = call
            .params
            .iter()
            .map(|(n, t)| (n.clone(), subst_ty(t, sub)))
            .collect();
        let mut parts = Vec::new();
        for (_, t) in &params {
            parts.push(mangle(t)?);
        }
        let base_inst = format!("{}__{}", method.node, parts.join("_"));
        let inst = format!("{}__cl{}", base_inst, closure.span.start);
        if self.seen.insert(format!("{owner}::{inst}")) {
            self.queue.push(Work::Method {
                owner: owner.clone(),
                method: method.node.clone(),
                instance: inst.clone(),
                sub: params.into_iter().collect(),
                closure: Some(ClosureSpec {
                    param_index: ci + 1,
                    caps_at: 1,
                    closure_params: cparams,
                    body: cbody,
                    caps: caps.clone(),
                    inlines: inlines.clone(),
                }),
            });
        }
        let (method_span, args_span) = (method.span, closure.span);
        let mut recv2 = (**recv).clone();
        self.expr(&mut recv2, sub);
        let mut new_args = Vec::new();
        for (n, _) in &caps {
            new_args.push(path_expr(n, args_span));
        }
        let rest: Vec<Expr> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != ci)
            .map(|(_, a)| a.clone())
            .collect();
        for mut a in rest {
            self.expr(&mut a, sub);
            new_args.push(a);
        }
        Some(ExprKind::MethodCall(
            Box::new(recv2),
            Spanned {
                node: inst,
                span: method_span,
            },
            new_args,
        ))
    }

    /// The captured variables of a closure literal, as source types. `None` when a
    /// capture has no source spelling or is itself a closure (not yet supported).
    fn captures_of(
        &self,
        closure: &Expr,
        sub: &HashMap<String, Ty>,
    ) -> Option<(Vec<(String, Type)>, Vec<InlineCtx>)> {
        let mut caps = Vec::new();
        let mut inlines = Vec::new();
        self.collect_captures(closure.span, sub, &mut caps, &mut inlines, 0)?;
        Some((caps, inlines))
    }

    /// The captures of the closure at `span`. A capture that is a closure bound by `let`
    /// is inlined where it is called: its own captures are captures of this closure too.
    fn collect_captures(
        &self,
        span: Span,
        sub: &HashMap<String, Ty>,
        caps: &mut Vec<(String, Type)>,
        inlines: &mut Vec<InlineCtx>,
        depth: usize,
    ) -> Option<()> {
        if depth > 8 {
            return None;
        }
        let recorded = self.closure_caps.get(&(span.start, span.end))?.clone();
        for (n, t) in recorded {
            let st = subst_ty(&t, sub);
            if matches!(st, Ty::Fn(..)) {
                let local = self.closure_locals.get(&n)?.clone();
                if !inlines.iter().any(|c| c.fname == n) {
                    inlines.push(InlineCtx {
                        fname: n.clone(),
                        params: local.params.iter().map(|p| p.pat.clone()).collect(),
                        body: local.body.clone(),
                    });
                    self.collect_captures(local.span, sub, caps, inlines, depth + 1)?;
                }
            } else if !caps.iter().any(|(m, _)| m == &n) {
                caps.push((n, ty_to_ast(&st, span)?));
            }
        }
        Some(())
    }

    /// The instance name of the struct `name` at `args`, requesting it if new.
    fn request_struct(&mut self, name: &str, args: &[Ty]) -> Option<String> {
        let mut parts = Vec::new();
        for a in args {
            parts.push(mangle(a)?);
        }
        let instance = format!("{}__{}", name, parts.join("_"));
        if self.seen.insert(instance.clone()) {
            self.queue.push(Work::Struct {
                instance: instance.clone(),
                name: name.to_string(),
                args: args.to_vec(),
            });
        }
        Some(instance)
    }

    /// The name of the instance of method `method` of `owner` at `params`, requesting it
    /// if new. The instance is added to the owner's impl block when the worklist drains.
    fn request_method(&mut self, owner: &str, method: &str, params: &[(String, Ty)]) -> Option<String> {
        let mut parts = Vec::new();
        for (_, t) in params {
            parts.push(mangle(t)?);
        }
        let instance = format!("{}__{}", method, parts.join("_"));
        if self.seen.insert(format!("{owner}::{instance}")) {
            let sub: HashMap<String, Ty> = params.iter().cloned().collect();
            self.queue.push(Work::Method {
                owner: owner.to_string(),
                method: method.to_string(),
                instance: instance.clone(),
                sub,
                closure: None,
            });
        }
        Some(instance)
    }

    fn rewrite_impl(&mut self, imp: &mut ImplDef, sub: &HashMap<String, Ty>) {
        self.ty(&mut imp.for_ty, sub);
        for m in &mut imp.items {
            if let ItemKind::Function(f) = &mut m.kind {
                self.rewrite_fn(f, sub);
            }
        }
    }

    fn rewrite_fn(&mut self, f: &mut Function, sub: &HashMap<String, Ty>) {
        self.closure_locals.clear();
        for p in &mut f.params {
            self.ty(&mut p.ty, sub);
        }
        if let Some(r) = &mut f.ret {
            self.ty(r, sub);
        }
        self.block(&mut f.body, sub);
    }

    fn block(&mut self, b: &mut Block, sub: &HashMap<String, Ty>) {
        for s in &mut b.stmts {
            match &mut s.kind {
                StmtKind::Local(l) => {
                    if let (Pat::Ident(n), Some(init)) = (&l.pat, &l.init) {
                        if let ExprKind::Closure { params, body, .. } = &init.kind {
                            self.closure_locals.insert(
                                n.node.clone(),
                                LocalClosure {
                                    span: init.span,
                                    params: params.clone(),
                                    body: (**body).clone(),
                                },
                            );
                        }
                    }
                    if let Some(t) = &mut l.ty {
                        self.ty(t, sub);
                    }
                    if let Some(e) = &mut l.init {
                        self.expr(e, sub);
                    }
                }
                StmtKind::Expr(e) | StmtKind::Semi(e) => self.expr(e, sub),
                StmtKind::Item(_) => {}
            }
        }
        if let Some(t) = &mut b.tail {
            self.expr(t, sub);
        }
    }

    /// Rewrite a type: generic parameters become their concrete types, and an
    /// instantiation of a generic struct (`Pair<i64>`) becomes the instance's name.
    fn ty(&mut self, t: &mut Type, sub: &HashMap<String, Ty>) {
        match &mut t.kind {
            TypeKind::Path(p, args) => {
                for a in args.iter_mut() {
                    self.ty(a, sub);
                }
                let last = p.segments.last().map(|s| s.node.clone()).unwrap_or_default();
                if p.segments.len() == 1 && args.is_empty() {
                    if let Some(concrete) = sub.get(&last) {
                        if let Some(written) = ty_to_ast(concrete, t.span) {
                            *t = written;
                            return;
                        }
                    }
                }
                if !args.is_empty() && self.generic_struct_names.contains(&last) {
                    let converted: Option<Vec<Ty>> = args.iter().map(ast_to_ty).collect();
                    if let Some(converted) = converted {
                        if let Some(instance) = self.request_struct(&last, &converted) {
                            let span = t.span;
                            *t = Type {
                                kind: TypeKind::Path(
                                    Path {
                                        segments: vec![Spanned {
                                            node: instance,
                                            span,
                                        }],
                                    },
                                    Vec::new(),
                                ),
                                span,
                            };
                        }
                    }
                }
            }
            TypeKind::Ref(inner)
            | TypeKind::RefMut(inner)
            | TypeKind::Array(inner, _)
            | TypeKind::Slice(inner)
            | TypeKind::Future(inner) => self.ty(inner, sub),
            TypeKind::Tuple(elems) => {
                for e in elems {
                    self.ty(e, sub);
                }
            }
            TypeKind::Infer | TypeKind::Never => {}
        }
    }

    fn expr(&mut self, e: &mut Expr, sub: &HashMap<String, Ty>) {
        // A call of the closure parameter being inlined becomes the closure's body.
        if let Some(kind) = self.try_inline(e, sub) {
            e.kind = kind;
            return;
        }
        // A call passing a closure literal calls an instance that inlines it.
        if let Some(kind) = self.specialize_call(e, sub) {
            e.kind = kind;
            return;
        }
        if let Some(kind) = self.specialize_method(e, sub) {
            e.kind = kind;
            return;
        }
        let key = (e.span.start, e.span.end);
        match &mut e.kind {
            ExprKind::Call(callee, args) => {
                if let ExprKind::Path(p) = &mut callee.kind {
                    if let Some(call) = self.calls.get(&(callee.span.start, callee.span.end)).cloned() {
                        self.rename_call(p, &call, sub);
                    }
                }
                self.expr(callee, sub);
                for a in args {
                    self.expr(a, sub);
                }
            }
            ExprKind::Struct(path, fields, base) => {
                if let Some(call) = self.calls.get(&key).cloned() {
                    let params: Vec<Ty> = call.params.iter().map(|(_, t)| subst_ty(t, sub)).collect();
                    if self.generic_struct_names.contains(&call.callee) {
                        if let Some(instance) = self.request_struct(&call.callee, &params) {
                            if let Some(last) = path.segments.last_mut() {
                                last.node = instance;
                            }
                        }
                    }
                }
                for (_, x) in fields {
                    self.expr(x, sub);
                }
                if let Some(b) = base {
                    self.expr(b, sub);
                }
            }
            ExprKind::Lit(_) | ExprKind::Break | ExprKind::Continue => {}
            ExprKind::Macro(mc) => {
                // Macro arguments are tokens the checker parses later. A callee among them
                // is renamed by its identifier token (a free function), or by the owner
                // identifier of an associated call (`Pair::new`).
                for tok in &mut mc.tokens {
                    if let TokenKind::Ident(name) = &mut tok.kind {
                        if let Some(call) = self.calls.get(&(tok.span.start, tok.span.end)).cloned() {
                            if call.method {
                                // A method's name: the call is on the receiver's type.
                                let params: Vec<(String, Ty)> = call
                                    .params
                                    .iter()
                                    .map(|(n, t)| (n.clone(), subst_ty(t, sub)))
                                    .collect();
                                if let Some(owner) = call.owner.clone() {
                                    if let Some(instance) = self.request_method(&owner, &call.callee, &params) {
                                        *name = instance;
                                    }
                                }
                            } else if call.owner.is_none() {
                                let params: Vec<(String, Ty)> = call
                                    .params
                                    .iter()
                                    .map(|(n, t)| (n.clone(), subst_ty(t, sub)))
                                    .collect();
                                if let Some(instance) = self.request_fn(&call.callee, &params) {
                                    *name = instance;
                                }
                            }
                        } else if let Some(call) = self.calls_by_start.get(&tok.span.start).cloned() {
                            if call.owner.as_deref() == Some(name.as_str()) {
                                let params: Vec<Ty> =
                                    call.params.iter().map(|(_, t)| subst_ty(t, sub)).collect();
                                if let Some(instance) = self.owner_instance(&call, &params) {
                                    *name = instance;
                                }
                            }
                        }
                    }
                }
            }
            ExprKind::Path(_) => {}
            ExprKind::Binary(a, _, b) | ExprKind::Index(a, b) | ExprKind::ArrayRepeat(a, b) => {
                self.expr(a, sub);
                self.expr(b, sub);
            }
            ExprKind::Range(a, b, _) => {
                if let Some(a) = a {
                    self.expr(a, sub);
                }
                if let Some(b) = b {
                    self.expr(b, sub);
                }
            }
            ExprKind::Unary(_, a)
            | ExprKind::Field(a, _)
            | ExprKind::Ref(a, _)
            | ExprKind::Deref(a)
            | ExprKind::Await(a)
            | ExprKind::Try(a) => self.expr(a, sub),
            ExprKind::MethodCall(recv, method, args) => {
                // A method with its own type parameters: call its instance on the receiver's
                // type (the copy is added to that type's impl block). Recorded by the name.
                let name_key = (method.span.start, method.span.end);
                if let Some(call) = self.calls.get(&name_key).cloned() {
                    if call.method {
                        let params: Vec<(String, Ty)> = call
                            .params
                            .iter()
                            .map(|(n, t)| (n.clone(), subst_ty(t, sub)))
                            .collect();
                        if let Some(owner) = &call.owner {
                            if let Some(instance) = self.request_method(owner, &call.callee, &params) {
                                method.node = instance;
                            }
                        }
                    }
                }
                self.expr(recv, sub);
                for a in args {
                    self.expr(a, sub);
                }
            }
            ExprKind::Array(xs) | ExprKind::Tuple(xs) => {
                for x in xs {
                    self.expr(x, sub);
                }
            }
            ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => self.block(b, sub),
            ExprKind::If(c, then, els) => {
                self.expr(c, sub);
                self.block(then, sub);
                if let Some(e2) = els {
                    self.expr(e2, sub);
                }
            }
            ExprKind::While(c, b) => {
                self.expr(c, sub);
                self.block(b, sub);
            }
            ExprKind::For(_, iter, b) => {
                self.expr(iter, sub);
                self.block(b, sub);
            }
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(v, sub);
                }
            }
            ExprKind::Match(scrut, arms) => {
                self.expr(scrut, sub);
                for (_, body) in arms {
                    self.expr(body, sub);
                }
            }
            ExprKind::Closure { params, ret, body } => {
                for p in params {
                    self.ty(&mut p.ty, sub);
                }
                if let Some(r) = ret {
                    self.ty(r, sub);
                }
                self.expr(body, sub);
            }
            ExprKind::Cast(a, t) => {
                self.expr(a, sub);
                self.ty(t, sub);
            }
        }
    }

    /// Rename a call to its instance: a free generic function, or an associated function
    /// whose owner (`Pair` in `Pair::new`) is a generic struct.
    fn rename_call(&mut self, p: &mut Path, call: &GenericCall, sub: &HashMap<String, Ty>) {
        let params: Vec<(String, Ty)> = call
            .params
            .iter()
            .map(|(n, t)| (n.clone(), subst_ty(t, sub)))
            .collect();
        match &call.owner {
            None => {
                if let Some(instance) = self.request_fn(&call.callee, &params) {
                    if let Some(last) = p.segments.last_mut() {
                        last.node = instance;
                    }
                }
            }
            Some(owner) => {
                let owner_params: Vec<Ty> = params.iter().map(|(_, t)| t.clone()).collect();
                let owner_call = GenericCall {
                    span: call.span,
                    callee: call.callee.clone(),
                    owner: Some(owner.clone()),
                    method: false,
                    params: params.clone(),
                };
                if let Some(instance) = self.owner_instance(&owner_call, &owner_params) {
                    let n = p.segments.len();
                    if n >= 2 {
                        p.segments[n - 2].node = instance;
                    }
                }
            }
        }
    }

    /// The struct instance an associated call's owner names: the owner's arguments are
    /// the generic impl's `for` type arguments, solved from the call's parameters.
    fn owner_instance(&mut self, call: &GenericCall, params: &[Ty]) -> Option<String> {
        let owner = call.owner.clone()?;
        if !self.generic_struct_names.contains(&owner) {
            return None;
        }
        let solved: HashMap<String, Ty> = call
            .params
            .iter()
            .map(|(n, _)| n.clone())
            .zip(params.iter().cloned())
            .collect();
        // The impl that defines the method names the owner's arguments.
        let imp = self.generic_impls.iter().find(|imp| {
            imp_self_name(imp) == Some(owner.as_str())
                && imp.items.iter().any(|m| match &m.kind {
                    ItemKind::Function(f) => f.name.node == call.callee,
                    _ => false,
                })
        })?;
        let names = self_args(imp)?;
        let mut args = Vec::new();
        for n in names {
            args.push(solved.get(&n)?.clone());
        }
        self.request_struct(&owner, &args)
    }
}

/// A path expression naming one variable or function.
fn path_expr(name: &str, span: Span) -> Expr {
    Expr {
        kind: ExprKind::Path(Path {
            segments: vec![Spanned {
                node: name.to_string(),
                span,
            }],
        }),
        span,
    }
}

/// Whether parameter `index` of `f` is a closure: a function type, or a generic parameter
/// bounded by one.
fn is_closure_param(f: &Function, index: usize) -> bool {
    let Some(p) = f.params.get(index) else {
        return false;
    };
    match &p.ty.kind {
        TypeKind::Path(path, _) if path.segments.len() == 1 && path.segments[0].node == "fn" => true,
        TypeKind::Path(path, args) if args.is_empty() && path.segments.len() == 1 => {
            let name = &path.segments[0].node;
            let inline = f.generics.params.iter().any(|g| {
                &g.name.node == name && g.bounds.iter().any(|b| matches!(b, TypeBound::Fn(_)))
            });
            // A where-clause closure bound names the parameter too.
            let clause = f.generics.where_clause.iter().any(|w| match &w.ty.kind {
                TypeKind::Path(wp, _) => {
                    wp.segments.len() == 1
                        && &wp.segments[0].node == name
                        && w.bounds.iter().any(|b| matches!(b, TypeBound::Fn(_)))
                }
                _ => false,
            });
            inline || clause
        }
        _ => false,
    }
}

/// Every variable a piece of code refers to by a single name (including inside macro
/// arguments), in order of first appearance.
pub fn referenced_names(e: &Expr, out: &mut Vec<String>) {
    fn push(out: &mut Vec<String>, n: &str) {
        if !out.iter().any(|m| m == n) {
            out.push(n.to_string());
        }
    }
    fn block(b: &Block, out: &mut Vec<String>) {
        for s in &b.stmts {
            match &s.kind {
                StmtKind::Local(l) => {
                    if let Some(e) = &l.init {
                        referenced_names(e, out);
                    }
                }
                StmtKind::Expr(e) | StmtKind::Semi(e) => referenced_names(e, out),
                StmtKind::Item(_) => {}
            }
        }
        if let Some(t) = &b.tail {
            referenced_names(t, out);
        }
    }
    match &e.kind {
        ExprKind::Path(p) => {
            if p.segments.len() == 1 {
                push(out, &p.segments[0].node);
            }
        }
        ExprKind::Lit(_) | ExprKind::Break | ExprKind::Continue => {}
        ExprKind::Macro(mc) => {
            for t in &mc.tokens {
                if let TokenKind::Ident(n) = &t.kind {
                    push(out, n);
                }
            }
        }
        ExprKind::Binary(a, _, b) | ExprKind::Index(a, b) | ExprKind::ArrayRepeat(a, b) => {
            referenced_names(a, out);
            referenced_names(b, out);
        }
        ExprKind::Range(a, b, _) => {
            if let Some(a) = a {
                referenced_names(a, out);
            }
            if let Some(b) = b {
                referenced_names(b, out);
            }
        }
        ExprKind::Unary(_, a)
        | ExprKind::Field(a, _)
        | ExprKind::Ref(a, _)
        | ExprKind::Deref(a)
        | ExprKind::Await(a)
        | ExprKind::Try(a)
        | ExprKind::Cast(a, _) => referenced_names(a, out),
        ExprKind::Call(f, args) => {
            referenced_names(f, out);
            for a in args {
                referenced_names(a, out);
            }
        }
        ExprKind::MethodCall(recv, _, args) => {
            referenced_names(recv, out);
            for a in args {
                referenced_names(a, out);
            }
        }
        ExprKind::Array(xs) | ExprKind::Tuple(xs) => {
            for x in xs {
                referenced_names(x, out);
            }
        }
        ExprKind::Struct(_, fields, base) => {
            for (_, x) in fields {
                referenced_names(x, out);
            }
            if let Some(b) = base {
                referenced_names(b, out);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => block(b, out),
        ExprKind::If(c, then, els) => {
            referenced_names(c, out);
            block(then, out);
            if let Some(e2) = els {
                referenced_names(e2, out);
            }
        }
        ExprKind::While(c, b) => {
            referenced_names(c, out);
            block(b, out);
        }
        ExprKind::For(_, iter, b) => {
            referenced_names(iter, out);
            block(b, out);
        }
        ExprKind::Return(v) => {
            if let Some(v) = v {
                referenced_names(v, out);
            }
        }
        ExprKind::Match(scrut, arms) => {
            referenced_names(scrut, out);
            for (_, body) in arms {
                referenced_names(body, out);
            }
        }
        ExprKind::Closure { body, .. } => referenced_names(body, out),
    }
}

/// The name of the instance of function `callee` at `params`.
fn instance_name(callee: &str, params: &[(String, Ty)]) -> Option<String> {
    let mut parts = Vec::new();
    for (_, t) in params {
        parts.push(mangle(t)?);
    }
    Some(format!("{}__{}", callee, parts.join("_")))
}

/// Whether a non-generic impl block of the program has a method with its own type
/// parameters.
fn generic_methods_exist(prog: &Program) -> bool {
    prog.items.iter().any(|item| match &item.kind {
        ItemKind::Impl(imp) => {
            imp.generics.params.is_empty()
                && imp.items.iter().any(|m| match &m.kind {
                    ItemKind::Function(f) => !f.generics.params.is_empty(),
                    _ => false,
                })
        }
        _ => false,
    })
}

/// The name of the type an `impl` block is for (`Pair` in `impl<T> Pair<T>`).
fn imp_self_name(imp: &ImplDef) -> Option<&str> {
    match &imp.for_ty.kind {
        TypeKind::Path(p, _) => p.segments.last().map(|s| s.node.as_str()),
        _ => None,
    }
}

/// The parameter names an `impl` block's self type uses (`["T"]` for `impl<T> Pair<T>`);
/// `None` if an argument is not a plain generic name.
fn self_args(imp: &ImplDef) -> Option<Vec<String>> {
    match &imp.for_ty.kind {
        TypeKind::Path(_, args) => {
            let mut names = Vec::new();
            for a in args {
                match &a.kind {
                    TypeKind::Path(p, inner) if inner.is_empty() && p.segments.len() == 1 => {
                        names.push(p.segments[0].node.clone());
                    }
                    _ => return None,
                }
            }
            Some(names)
        }
        _ => None,
    }
}

/// A generic impl block: one with its own generic parameters, for a type of the program
/// that is itself generic.
fn is_generic_impl(imp: &ImplDef, items: &[Item]) -> bool {
    if imp.generics.params.is_empty() {
        return false;
    }
    let Some(name) = imp_self_name(imp) else { return false };
    items.iter().any(|i| match &i.kind {
        ItemKind::Struct(s) => s.name.node == name && !s.generics.params.is_empty(),
        _ => false,
    })
}

/// Substitute solved generic parameters inside a checker type.
fn subst_ty(t: &Ty, sub: &HashMap<String, Ty>) -> Ty {
    match t {
        Ty::Generic(n) => sub.get(n).cloned().unwrap_or_else(|| t.clone()),
        Ty::Ref(inner) => Ty::Ref(Box::new(subst_ty(inner, sub))),
        Ty::RefMut(inner) => Ty::RefMut(Box::new(subst_ty(inner, sub))),
        Ty::Apply(n, args) => Ty::Apply(n.clone(), args.iter().map(|a| subst_ty(a, sub)).collect()),
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|a| subst_ty(a, sub)).collect()),
        Ty::Fn(params, ret) => Ty::Fn(
            params.iter().map(|a| subst_ty(a, sub)).collect(),
            Box::new(subst_ty(ret, sub)),
        ),
        other => other.clone(),
    }
}

/// A name for a concrete type, used in instance names. `None` for types that have no
/// spelling here (unsolved, generic, or function types).
fn mangle(t: &Ty) -> Option<String> {
    Some(match t {
        Ty::Int | Ty::I32 => "i32".to_string(),
        Ty::I8 => "i8".to_string(),
        Ty::I16 => "i16".to_string(),
        Ty::I64 => "i64".to_string(),
        Ty::I128 => "i128".to_string(),
        Ty::Isize => "isize".to_string(),
        Ty::U8 => "u8".to_string(),
        Ty::U16 => "u16".to_string(),
        Ty::U32 => "u32".to_string(),
        Ty::U64 => "u64".to_string(),
        Ty::U128 => "u128".to_string(),
        Ty::Usize => "usize".to_string(),
        Ty::F32 => "f32".to_string(),
        Ty::F64 => "f64".to_string(),
        Ty::Bool => "bool".to_string(),
        Ty::Char => "char".to_string(),
        Ty::Str => "str".to_string(),
        Ty::Unit => "unit".to_string(),
        Ty::Struct(n) => n.clone(),
        Ty::Apply(n, args) => {
            let mut parts = vec![n.clone()];
            for a in args {
                parts.push(mangle(a)?);
            }
            parts.join("_")
        }
        Ty::Ref(inner) => format!("ref_{}", mangle(inner)?),
        Ty::RefMut(inner) => format!("refmut_{}", mangle(inner)?),
        Ty::Fn(params, ret) => {
            let mut parts = vec!["fn".to_string()];
            for p in params {
                parts.push(mangle(p)?);
            }
            parts.push("ret".to_string());
            parts.push(mangle(ret)?);
            parts.join("_")
        }
        _ => return None,
    })
}

/// The checker type a source type names, for the arguments of an instantiation written
/// in the source (`Pair<i64>`). `None` for types the checker does not model here.
fn ast_to_ty(t: &Type) -> Option<Ty> {
    Some(match &t.kind {
        TypeKind::Path(p, args) => {
            let name = p.segments.last()?.node.as_str();
            if args.is_empty() {
                match name {
                    "i8" => Ty::I8,
                    "i16" => Ty::I16,
                    "i32" => Ty::I32,
                    "i64" => Ty::I64,
                    "i128" => Ty::I128,
                    "isize" => Ty::Isize,
                    "u8" => Ty::U8,
                    "u16" => Ty::U16,
                    "u32" => Ty::U32,
                    "u64" => Ty::U64,
                    "u128" => Ty::U128,
                    "usize" => Ty::Usize,
                    "f32" => Ty::F32,
                    "f64" => Ty::F64,
                    "bool" => Ty::Bool,
                    "char" => Ty::Char,
                    "str" => Ty::Str,
                    other => Ty::Struct(other.to_string()),
                }
            } else {
                let mut converted = Vec::new();
                for a in args {
                    converted.push(ast_to_ty(a)?);
                }
                Ty::Apply(name.to_string(), converted)
            }
        }
        TypeKind::Ref(inner) => Ty::Ref(Box::new(ast_to_ty(inner)?)),
        TypeKind::RefMut(inner) => Ty::RefMut(Box::new(ast_to_ty(inner)?)),
        TypeKind::Tuple(elems) if elems.is_empty() => Ty::Unit,
        // A function type is the path `fn` whose arguments are its parameters and result.
        TypeKind::Path(p, args) if p.segments.len() == 1 && p.segments[0].node == "fn" && !args.is_empty() => {
            let (ret, params) = args.split_last()?;
            let mut ps = Vec::new();
            for a in params {
                ps.push(ast_to_ty(a)?);
            }
            Ty::Fn(ps, Box::new(ast_to_ty(ret)?))
        }
        _ => return None,
    })
}

/// The source type a concrete checker type is written as, for substitution into the
/// AST. `None` when the type has no source spelling.
fn ty_to_ast(t: &Ty, span: Span) -> Option<Type> {
    let path = |name: &str, args: Vec<Type>| Type {
        kind: TypeKind::Path(
            Path {
                segments: vec![Spanned {
                    node: name.to_string(),
                    span,
                }],
            },
            args,
        ),
        span,
    };
    Some(match t {
        Ty::Int | Ty::I32 => path("i32", vec![]),
        Ty::I8 => path("i8", vec![]),
        Ty::I16 => path("i16", vec![]),
        Ty::I64 => path("i64", vec![]),
        Ty::I128 => path("i128", vec![]),
        Ty::Isize => path("isize", vec![]),
        Ty::U8 => path("u8", vec![]),
        Ty::U16 => path("u16", vec![]),
        Ty::U32 => path("u32", vec![]),
        Ty::U64 => path("u64", vec![]),
        Ty::U128 => path("u128", vec![]),
        Ty::Usize => path("usize", vec![]),
        Ty::F32 => path("f32", vec![]),
        Ty::F64 => path("f64", vec![]),
        Ty::Bool => path("bool", vec![]),
        Ty::Char => path("char", vec![]),
        Ty::Str => path("str", vec![]),
        Ty::Unit => Type {
            kind: TypeKind::Tuple(vec![]),
            span,
        },
        Ty::Struct(n) => path(n, vec![]),
        Ty::Apply(n, args) => {
            let mut written = Vec::new();
            for a in args {
                written.push(ty_to_ast(a, span)?);
            }
            path(n, written)
        }
        Ty::Ref(inner) => Type {
            kind: TypeKind::Ref(Box::new(ty_to_ast(inner, span)?)),
            span,
        },
        Ty::RefMut(inner) => Type {
            kind: TypeKind::RefMut(Box::new(ty_to_ast(inner, span)?)),
            span,
        },
        Ty::Fn(params, ret) => {
            let mut args = Vec::new();
            for p in params {
                args.push(ty_to_ast(p, span)?);
            }
            args.push(ty_to_ast(ret, span)?);
            path("fn", args)
        }
        _ => return None,
    })
}
