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

    let mut generic_fns: HashMap<String, Function> = HashMap::new();
    let mut generic_structs: HashMap<String, StructDef> = HashMap::new();
    let mut generic_impls: Vec<ImplDef> = Vec::new();
    for item in &prog.items {
        match &item.kind {
            ItemKind::Function(f) if !f.generics.params.is_empty() => {
                generic_fns.insert(f.name.node.clone(), f.clone());
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
            } => {
                let Some(generic) = mono.generic_fns.get(&callee).cloned() else {
                    continue;
                };
                let mut f = generic;
                f.name.node = instance;
                f.generics = Generics::default();
                mono.rewrite_fn(&mut f, &sub);
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
                mono.rewrite_fn(&mut f, &sub);
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
    /// A function instance: `instance` is `callee` with `sub` substituted.
    Fn {
        instance: String,
        callee: String,
        sub: HashMap<String, Ty>,
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
    },
}

struct Mono {
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
            });
        }
        Some(instance)
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
            });
        }
        Some(instance)
    }

    fn rewrite_impl(&mut self, imp: &mut ImplDef, sub: &HashMap<String, Ty>) {
        // Methods with their own type parameters are instantiated at their call sites.
        imp.items
            .retain(|m| !matches!(&m.kind, ItemKind::Function(f) if !f.generics.params.is_empty()));
        self.ty(&mut imp.for_ty, sub);
        for m in &mut imp.items {
            if let ItemKind::Function(f) = &mut m.kind {
                self.rewrite_fn(f, sub);
            }
        }
    }

    fn rewrite_fn(&mut self, f: &mut Function, sub: &HashMap<String, Ty>) {
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
        _ => return None,
    })
}
