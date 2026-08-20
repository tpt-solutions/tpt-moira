//! The Lachesis type-inference engine for the Phase-1 subset.
//!
//! Because the subset has no generics, Lachesis performs *direct* type inference:
//! every expression's type is computed locally (literals default to `i32`,
//! variables resolve from the environment, function calls from their signatures,
//! field accesses from struct definitions) while *constraints* between operands
//! are checked on the fly. This is the "type inference engine" deliverable; it
//! integrates with Clotho (it consumes the AST) and with Atropos (its results
//! gate code generation).

use crate::traits::{BuiltinTrait, TraitDb};
use crate::ty::Ty;
use clotho::ast::{
    BinOp, Block, Expr, ExprKind, Generics, Lit, Local, Param, Pat, Path, StmtKind, Type,
    TypeBound, TypeKind, UnOp,
};
use clotho::error::Diag;
use clotho::token::Span;
use std::collections::HashMap;

/// A struct's resolved field types.
#[derive(Debug, Clone)]
pub struct StructInfo {
    pub fields: Vec<(String, Ty)>,
    /// Generic parameters declared on the struct, e.g. `<T>` in `struct Foo<T>`.
    /// Field types may reference these as `Ty::Generic` parameters.
    pub generics: Generics,
}

/// A function's resolved signature.
#[derive(Debug, Clone)]
pub struct FnSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
    /// Whether the function is declared `async fn`, in which case callers that
    /// do not `.await` the result receive a `Future` wrapper.
    pub is_async: bool,
    /// Generic parameters declared on the function, e.g. `<T: Bound>`. Used to
    /// enforce trait bounds on the parameter types and to infer the concrete
    /// type arguments at each call site.
    pub generics: Generics,
}

/// The Phase-2 type checker.
pub struct TypeChecker {
    structs: HashMap<String, StructInfo>,
    fns: HashMap<String, FnSig>,
    /// Associated functions `Type::fn()` (two-segment call paths), keyed by
    /// `(self_type_name, fn_name)`. Used by the standard-library prelude.
    assoc_fns: HashMap<(String, String), FnSig>,
    /// Methods `receiver.method()`, keyed by `(receiver_type_name, method_name)`.
    /// Used by the standard-library prelude.
    methods: HashMap<(String, String), FnSig>,
    /// User-defined methods collected from `impl` blocks, keyed by
    /// `(self_type_name, method_name)`. These resolve `receiver.method(args)`
    /// calls against the user's own types (in addition to the `std` prelude).
    user_methods: HashMap<(String, String), FnSig>,
    /// The `Self` type name of the item currently being checked (a method inside
    /// an `impl`, or an `impl`'s signature collection). Lets `type_to_ty` resolve
    /// the `Self` path to the concrete self type.
    self_type: Option<String>,
    /// A trait solver seeded with the user `impl` clauses in the program, used to
    /// enforce *user-defined* trait bounds (e.g. `T: Show`) on generic call
    /// sites. Built once per [`TypeChecker::check_program`] run.
    solver: Option<crate::solver::Solver>,
    /// Nullary prelude values usable without a call site, e.g. `None` (a unit
    /// value of type `Option`), mirroring how `None` is a bare variant in Rust.
    values: HashMap<String, Ty>,
    env: Vec<HashMap<String, Ty>>,
    /// Stack of the generic parameter lists of the functions currently being
    /// checked. The top entry scopes the generic type parameters so that a bare
    /// type-name in a signature/body resolves to [`Ty::Generic`] and so that
    /// trait bounds declared on those parameters can be enforced.
    generic_scope: Vec<Generics>,
    ret: Ty,
    ret_span: Span,
    errors: Vec<Diag>,
}

impl Default for TypeChecker {
    fn default() -> Self {
        TypeChecker {
            structs: HashMap::new(),
            fns: HashMap::new(),
            assoc_fns: HashMap::new(),
            methods: HashMap::new(),
            user_methods: HashMap::new(),
            self_type: None,
            solver: None,
            values: HashMap::new(),
            env: Vec::new(),
            generic_scope: Vec::new(),
            ret: Ty::Unit,
            ret_span: Span::default(),
            errors: Vec::new(),
        }
    }
}

impl TypeChecker {
    /// Type-check a whole program. Returns all collected diagnostics on failure.
    pub fn check_program(prog: &clotho::ast::Program) -> Result<(), Vec<Diag>> {
        let mut cx = TypeChecker::default();
        // Seed the trait solver with the user `impl` clauses so user-defined
        // trait bounds (e.g. `T: Show`) can be enforced at generic call sites.
        cx.solver = Some(crate::solver::Solver::new(crate::solver::program_with_impls(
            prog,
        )));
        // Pass 1: collect struct and function signatures so callers may refer to
        // functions defined later in the file (forward references).
        for item in &prog.items {
            match &item.kind {
                clotho::ast::ItemKind::Struct(s) => {
                    // Resolve field types within the struct's own generic scope
                    // so a bare `T` becomes `Ty::Generic("T")`.
                    cx.generic_scope.push(s.generics.clone());
                    let fields = s
                        .fields
                        .iter()
                        .map(|f| (f.name.node.clone(), cx.type_to_ty(&f.ty)))
                        .collect();
                    cx.generic_scope.pop();
                    cx.structs.insert(
                        s.name.node.clone(),
                        StructInfo {
                            fields,
                            generics: s.generics.clone(),
                        },
                    );
                }
                clotho::ast::ItemKind::Impl(imp) => {
                    // Collect the methods declared in user `impl` blocks so that
                    // `receiver.method(args)` calls on the user's own types
                    // resolve. `Self` and the impl's generic params are in scope.
                    let self_name = imp.for_ty.node_name();
                    cx.generic_scope.push(imp.generics.clone());
                    cx.self_type = Some(self_name.clone());
                    for m in &imp.items {
                        if let clotho::ast::ItemKind::Function(f) = &m.kind {
                            let params =
                                f.params.iter().map(|p| cx.type_to_ty(&p.ty)).collect();
                            let ret = f
                                .ret
                                .as_ref()
                                .map(|t| cx.type_to_ty(t))
                                .unwrap_or(Ty::Unit);
                            cx.user_methods.insert(
                                (self_name.clone(), f.name.node.clone()),
                                FnSig {
                                    params,
                                    ret,
                                    is_async: f.is_async,
                                    generics: imp.generics.clone(),
                                },
                            );
                        }
                    }
                    cx.self_type = None;
                    cx.generic_scope.pop();
                }
                clotho::ast::ItemKind::Function(f) => {
                    // Resolve the signature types within the function's own
                    // generic scope so a bare `T` becomes `Ty::Generic("T")`.
                    cx.generic_scope.push(f.generics.clone());
                    let params = f.params.iter().map(|p| cx.type_to_ty(&p.ty)).collect();
                    let ret = f.ret.as_ref().map(|t| cx.type_to_ty(t)).unwrap_or(Ty::Unit);
                    cx.generic_scope.pop();
                    cx.fns.insert(
                        f.name.node.clone(),
                        FnSig {
                            params,
                            ret,
                            is_async: f.is_async,
                            generics: f.generics.clone(),
                        },
                    );
                }
                // Enums/consts/statics/uses are not part of the type-checked subset.
                _ => {}
            }
        }

        // Built-in functions available to the subset.
        cx.fns.insert(
            "print_i32".to_string(),
            FnSig {
                params: vec![Ty::I32],
                ret: Ty::Unit,
                is_async: false,
                generics: Generics::default(),
            },
        );
        cx.fns.insert(
            "print_bool".to_string(),
            FnSig {
                params: vec![Ty::Bool],
                ret: Ty::Unit,
                is_async: false,
                generics: Generics::default(),
            },
        );

        // The standard-library prelude: recognized `std` types, their
        // constructors, associated functions, and methods. This is the "bind the
        // standard library's signatures into the checker" step that lets
        // idiomatic programs (that use `Option`, `Result`, `Vec`, `String`,
        // `Box`, …) type-check against Moira. Full compilation of those types
        // (heap-backed `Vec`, etc.) remains a codegen follow-up.
        cx.register_prelude();

        // Pass 2: check each function body.
        for item in &prog.items {
            if let clotho::ast::ItemKind::Function(f) = &item.kind {
                cx.check_function(f);
            }
        }

        // Pass 2.5: check the bodies of methods declared in `impl` blocks, with
        // `Self` and the impl's generics in scope (so `self`, `Self`, and `T`
        // all resolve). The methods' signatures were collected in Pass 1.
        for item in &prog.items {
            if let clotho::ast::ItemKind::Impl(imp) = &item.kind {
                let self_name = imp.for_ty.node_name();
                cx.self_type = Some(self_name);
                cx.generic_scope.push(imp.generics.clone());
                for m in &imp.items {
                    if let clotho::ast::ItemKind::Function(f) = &m.kind {
                        cx.check_function(f);
                    }
                }
                cx.generic_scope.pop();
                cx.self_type = None;
            }
        }

        if cx.errors.is_empty() {
            Ok(())
        } else {
            Err(cx.errors)
        }
    }

    fn check_function(&mut self, f: &clotho::ast::Function) {
        self.generic_scope.push(f.generics.clone());
        self.env.push(HashMap::new());
        for p in &f.params {
            let ty = self.type_to_ty(&p.ty);
            let name = pat_name(&p.pat);
            self.env.last_mut().unwrap().insert(name, ty);
        }
        self.ret = f
            .ret
            .as_ref()
            .map(|t| self.type_to_ty(t))
            .unwrap_or(Ty::Unit);
        self.ret_span = f.ret.as_ref().map(|t| t.span).unwrap_or(f.name.span);
        let body_ty = self.infer_block(&f.body);
        if !matches!(self.ret, Ty::Unit) && !self.compatible(&body_ty, &self.ret) {
            self.err(
                self.ret_span,
                format!(
                    "function `{}` expects return type {}, found {}",
                    f.name.node, self.ret, body_ty
                ),
            );
        }
        self.env.pop();
        self.generic_scope.pop();
    }

    fn infer_block(&mut self, block: &Block) -> Ty {
        self.env.push(HashMap::new());
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Local(l) => {
                    let ty = if let Some(ann) = &l.ty {
                        let ann_ty = self.type_to_ty(ann);
                        if let Some(init) = &l.init {
                            let it = self.infer_expr(init);
                            let span = init.span;
                            self.expect(&it, &ann_ty, span);
                        }
                        ann_ty
                    } else if let Some(init) = &l.init {
                        self.infer_expr(init)
                    } else {
                        Ty::Unit
                    };
                    // Bind the pattern (identifier, tuple-of-identifiers, wild,
                    // ref) into the block scope. `bind_pattern` handles tuple
                    // destructuring (`let (a, b) = t`) by binding each element to
                    // the matching `Ty::Tuple` component.
                    if let Err(msg) = self.check_pattern(&l.pat) {
                        let span = l
                            .init
                            .as_ref()
                            .map(|i| i.span)
                            .unwrap_or_else(Span::default);
                        self.err(span, msg);
                    }
                    self.bind_pattern(&l.pat, &ty);
                }
                StmtKind::Semi(e) | StmtKind::Expr(e) => {
                    self.infer_expr(e);
                }
                StmtKind::Item(_) => {
                    self.err(Span::default(), "nested items are not supported");
                }
            }
        }
        let ty = match &block.tail {
            Some(t) => self.infer_expr(t),
            None => Ty::Unit,
        };
        self.env.pop();
        ty
    }

    /// Type-check a `match` expression. The scrutinee's type is inferred, then
    /// each arm binds its pattern into the arm-body scope and type-checks the
    /// body. The match's result type is the first arm body's type; every other
    /// arm must be compatible with it (mirroring `if`/`else` aggregation).
    ///
    /// Supported patterns in the type-checked subset: identifier (`x` — binds the
    /// whole scrutinee), wildcard (`_`), literal (`42`, `"s"`, `'c'`, `true`),
    /// path/unit-variant (`None`, `Foo`), or-patterns (`A | B`), and tuple
    /// patterns of identifiers (`(a, b)` — binds each element). Unsupported
    /// patterns (enum tuple/struct variant destructuring) are rejected with a
    /// precise diagnostic.
    fn infer_match(
        &mut self,
        span: Span,
        scrutinee: &Expr,
        arms: &[(Pat, Expr)],
    ) -> Ty {
        let _scrut_ty = self.infer_expr(scrutinee);
        let mut result_ty = Ty::Unknown;
        let mut first = true;
        for (pat, body) in arms {
            if let Err(msg) = self.check_pattern(pat) {
                self.err(span, msg);
            }
            // Bind pattern names into a fresh arm scope.
            self.env.push(HashMap::new());
            self.bind_pattern(pat, &_scrut_ty);
            let arm_ty = self.infer_expr(body);
            self.env.pop();
            if first {
                result_ty = arm_ty.clone();
                first = false;
            } else if !self.compatible(&result_ty, &arm_ty) {
                self.err(
                    span,
                    format!(
                        "match arm type mismatch: expected {}, found {}",
                        result_ty, arm_ty
                    ),
                );
            }
        }
        if first {
            // No arms — degenerate; treat as unit.
            Ty::Unit
        } else {
            result_ty
        }
    }

    /// Validate that a pattern's shape is supported by the type-checked subset,
    /// returning an error message for unsupported patterns.
    fn check_pattern(&self, pat: &Pat) -> Result<(), String> {
        match pat {
            Pat::Ident(_) | Pat::Wild | Pat::Lit(_) | Pat::Path(_) | Pat::Ref(_) => Ok(()),
            Pat::Or(a, b) => {
                self.check_pattern(a)?;
                self.check_pattern(b)
            }
            Pat::Tuple(elems) => {
                for e in elems {
                    if !matches!(e, Pat::Ident(_) | Pat::Wild) {
                        return Err(
                            "tuple patterns with non-identifier elements are not yet supported"
                                .to_string(),
                        );
                    }
                }
                Ok(())
            }
            Pat::Struct(_, _) => Err(
                "struct/variant patterns are not yet supported in the type-checked subset"
                    .to_string(),
            ),
            Pat::Range(..) => Ok(()),
            Pat::Bind(_, sub) => self.check_pattern(sub),
        }
    }

    /// Bind the names introduced by a pattern into the current scope. The bound
    /// type is the scrutinee's type for identifier/wildcard patterns, the element
    /// type for each element of a tuple pattern, and `Ty::Unit` for unit-variant
    /// / literal / ref patterns (which do not introduce usable bindings).
    fn bind_pattern(&mut self, pat: &Pat, scrut_ty: &Ty) {
        match pat {
            Pat::Ident(n) => {
                self.env.last_mut().unwrap().insert(n.node.clone(), scrut_ty.clone());
            }
            Pat::Wild => {}
            Pat::Ref(inner) => self.bind_pattern(inner, scrut_ty),
            Pat::Or(a, b) => {
                self.bind_pattern(a, scrut_ty);
                self.bind_pattern(b, scrut_ty);
            }
            Pat::Tuple(elems) => {
                if let Ty::Tuple(tys) = scrut_ty {
                    for (e, t) in elems.iter().zip(tys.iter()) {
                        if let Pat::Ident(n) = e {
                            self.env.last_mut().unwrap().insert(n.node.clone(), t.clone());
                        }
                    }
                }
            }
            Pat::Lit(_) | Pat::Path(_) | Pat::Struct(_, _) | Pat::Range(..) => {}
            Pat::Bind(name, sub) => {
                self.env
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), scrut_ty.clone());
                self.bind_pattern(sub, scrut_ty);
            }
        }
    }

    /// Type-check a closure `|params| body` (or `|params| -> Ret body`).
    ///
    /// Parameters are typed from their annotations (falling back to the `i32`
    /// default for inference), the body is checked in a scope binding each
    /// parameter, and the closure's type is the function type
    /// `(param_tys) -> ret_ty`. Capture analysis and MIR/codegen of closures
    /// remain a later Phase-4 deliverable; this accepts closures at the
    /// type-check level so the compiler builds and the subset can grow toward
    /// them.
    fn check_closure(
        &mut self,
        _span: Span,
        params: &[Param],
        ret: &Option<Type>,
        body: &Expr,
    ) -> Ty {
        let mut param_tys = Vec::with_capacity(params.len());
        self.env.push(HashMap::new());
        for p in params {
            let pt = self.type_to_ty(&p.ty);
            param_tys.push(pt.clone());
            let name = pat_name(&p.pat);
            self.env.last_mut().unwrap().insert(name, pt);
        }
        let body_ty = self.infer_expr(body);
        let ret_ty: Ty = match ret {
            Some(r) => self.type_to_ty(r),
            None => body_ty.clone(),
        };
        self.env.pop();
        Ty::Fn(param_tys, Box::new(ret_ty))
    }

    /// Type-check `if let pat = init { then } else { else }` (and its `while let`
    /// analogue). The binding is scoped to the then-block (if) or loop body
    /// (while). Only identifier patterns are supported in the type-checked
    /// subset; enum destructuring (`if let Some(x) = opt`) needs full `match` /
    /// enum-variant support and is rejected with a precise diagnostic.
    fn infer_if_let(
        &mut self,
        local: &Local,
        then_b: &Block,
        else_b: Option<&Expr>,
        span: Span,
    ) -> Ty {
        if !matches!(local.pat, Pat::Ident(_)) {
            self.err(
                span,
                "`if let`/`while let` with non-identifier patterns is not yet supported (enum destructuring needs `match`)",
            );
            let tt = self.infer_block(then_b);
            let _ = else_b;
            return tt;
        }
        let init_ty = match &local.init {
            Some(e) => self.infer_expr(e),
            None => Ty::Unit,
        };
        let name = pat_name(&local.pat);
        self.env.push(HashMap::new());
        self.env.last_mut().unwrap().insert(name, init_ty);
        let tt = self.infer_block(then_b);
        self.env.pop();
        match else_b {
            Some(eb) => {
                let et = match &eb.kind {
                    ExprKind::Block(b) => self.infer_block(b),
                    _ => self.infer_expr(eb),
                };
                self.expect(&tt, &et, span);
                tt
            }
            None => Ty::Unit,
        }
    }

    /// Type-check `while let pat = init { body }`. See [`Self::infer_if_let`] for
    /// the pattern-support restriction.
    fn infer_while_let(&mut self, local: &Local, body: &Block, span: Span) -> Ty {
        if !matches!(local.pat, Pat::Ident(_)) {
            self.err(
                span,
                "`if let`/`while let` with non-identifier patterns is not yet supported (enum destructuring needs `match`)",
            );
            self.infer_block(body);
            return Ty::Unit;
        }
        let init_ty = match &local.init {
            Some(e) => self.infer_expr(e),
            None => Ty::Unit,
        };
        let name = pat_name(&local.pat);
        self.env.push(HashMap::new());
        self.env.last_mut().unwrap().insert(name, init_ty);
        self.infer_block(body);
        self.env.pop();
        Ty::Unit
    }

    fn infer_expr(&mut self, e: &Expr) -> Ty {
        let span = e.span;
        let ty = match &e.kind {
            ExprKind::Lit(l) => lit_ty(l),
            ExprKind::Range(..) => Ty::Unit,
            ExprKind::Path(p) => {
                let name = p
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                match self.lookup(&name) {
                    Some(t) => t,
                    None => {
                        // Nullary prelude values usable without a call site.
                        if let Some(t) = self.values.get(&name) {
                            t.clone()
                        } else {
                            self.err(span, format!("cannot find value `{}`", name));
                            Ty::Unknown
                        }
                    }
                }
            }
            ExprKind::Binary(l, op, r) => self.check_binary(span, l, *op, r),
            ExprKind::Unary(u, e2) => self.check_unary(span, *u, e2),
            ExprKind::Call(callee, args) => self.check_call(span, callee, args),
            ExprKind::Field(base, field) => self.check_field(span, base, field),
            ExprKind::Ref(e2, mutable) => {
                let t = self.infer_expr(e2);
                if *mutable {
                    Ty::RefMut(Box::new(t))
                } else {
                    Ty::Ref(Box::new(t))
                }
            }
            ExprKind::Deref(e2) => {
                let t = self.infer_expr(e2);
                match &t {
                    Ty::Ref(inner) | Ty::RefMut(inner) => (**inner).clone(),
                    _ => {
                        self.err(span, "cannot dereference a non-reference".to_string());
                        Ty::Unknown
                    }
                }
            }
            ExprKind::Cast(e2, ty) => {
                // Type-check the inner expression (surfacing any errors it has),
                // then the cast's type is the explicit target type. The Phase-1
                // subset does not enforce Rust's full cast-validity lattice; any
                // numeric/pointer cast is accepted, mirroring the "no-magic"
                // lowering that emits a C-style `(T)x` conversion.
                let _ = self.infer_expr(e2);
                TypeChecker::ast_to_ty(ty)
            }
            ExprKind::Struct(path, fields, base) => {
                self.check_struct(span, path, fields, base.as_deref())
            }
            ExprKind::Block(b) => self.infer_block(b),
            ExprKind::If(cond, then_b, else_b) => {
                // `if let pat = init { .. }` is parsed as `If(Block{Local(pat=init)}, ..)`.
                // Handle it as a binding scoped to the then-block.
                if let ExprKind::Block(ref cb) = cond.kind {
                    if cb.stmts.len() == 1 && cb.tail.is_none() {
                        if let StmtKind::Local(ref local) = cb.stmts[0].kind {
                            return self.infer_if_let(local, then_b, else_b.as_deref(), span);
                        }
                    }
                }
                let ct = self.infer_expr(cond);
                self.expect(&ct, &Ty::Bool, cond.span);
                let tt = self.infer_block(then_b);
                match else_b {
                    Some(eb) => {
                        let et = if matches!(eb.kind, ExprKind::Block(_)) {
                            if let ExprKind::Block(b) = &eb.kind {
                                self.infer_block(b)
                            } else {
                                unreachable!()
                            }
                        } else {
                            self.infer_expr(eb)
                        };
                        self.expect(&tt, &et, span);
                        tt
                    }
                    None => Ty::Unit,
                }
            }
            ExprKind::While(cond, body) => {
                // `while let pat = init { .. }` is parsed as `While(Block{Local}, ..)`.
                if let ExprKind::Block(ref cb) = cond.kind {
                    if cb.stmts.len() == 1 && cb.tail.is_none() {
                        if let StmtKind::Local(ref local) = cb.stmts[0].kind {
                            return self.infer_while_let(local, body, span);
                        }
                    }
                }
                let ct = self.infer_expr(cond);
                self.expect(&ct, &Ty::Bool, cond.span);
                self.infer_block(body);
                Ty::Unit
            }
            ExprKind::Loop(body) => {
                self.infer_block(body);
                Ty::Unit
            }
            ExprKind::For(pat, iter, body) => {
                let iter_ty = self.infer_expr(iter);
                // The loop variable binds to the iterator's element type: an
                // array iterator binds its element type; a range binds `i32`.
                // Anything else (e.g. a bare `i32`) is not iterable.
                let elem_ty = match &iter_ty {
                    Ty::Array(elem, _) => (**elem).clone(),
                    Ty::Range => Ty::Int,
                    other => {
                        self.err(
                            span,
                            format!(
                                "`for` iterator must be a range or array, found {}",
                                other
                            ),
                        );
                        Ty::Unknown
                    }
                };
                // Bind the pattern (`x`, or `(a, b)` for iterating an array of
                // tuples) into the loop body scope. `bind_pattern` handles the
                // tuple case by binding each element to the array's element type.
                if let Err(msg) = self.check_pattern(pat) {
                    self.err(span, msg);
                }
                self.bind_pattern(pat, &elem_ty);
                self.infer_block(body);
                Ty::Unit
            }
            ExprKind::Return(opt) => {
                let t = match opt {
                    Some(e2) => self.infer_expr(e2),
                    None => Ty::Unit,
                };
                let ret = self.ret.clone();
                self.expect(&t, &ret, span);
                Ty::Unit
            }
            ExprKind::Break | ExprKind::Continue => Ty::Unit,
            ExprKind::Match(scrutinee, arms) => {
                self.infer_match(span, scrutinee, arms)
            }
            ExprKind::MethodCall(recv, method, args) => self.check_method(span, recv, method, args),
            ExprKind::Array(elems) => {
                // Every element must share a common element type; the array's
                // type is `[elem; len]`. An empty array defaults to
                // `[i32; 0]` (the Phase-1 integer fallback).
                let mut elem_ty = Ty::Int;
                for (i, e) in elems.iter().enumerate() {
                    let t = self.infer_expr(e);
                    if i == 0 {
                        elem_ty = t.clone();
                    } else if !self.compatible(&elem_ty, &t) {
                        self.err(
                            e.span,
                            format!(
                                "array element type mismatch: expected {}, found {}",
                                elem_ty, t
                            ),
                        );
                    }
                }
                Ty::Array(Box::new(elem_ty), elems.len() as u128)
            }
            ExprKind::Tuple(elems) => {
                // Each element is inferred independently; the tuple's type is the
                // product of the element types. This is the type-checked half of
                // tuple support — MIR lowering / codegen of tuple values is the
                // documented codegen follow-up.
                let tys = elems.iter().map(|e| self.infer_expr(e)).collect();
                Ty::Tuple(tys)
            }
            ExprKind::Index(base, idx) => {
                let base_ty = self.infer_expr(base);
                let idx_ty = self.infer_expr(idx);
                if !idx_ty.is_integer() && !matches!(idx_ty, Ty::Unknown) {
                    self.err(span, "array index must be an integer".to_string());
                }
                match &base_ty {
                    Ty::Array(elem, _) => (**elem).clone(),
                    Ty::Unknown => Ty::Unknown,
                    other => {
                        self.err(span, format!("type `{}` cannot be indexed", other));
                        Ty::Unknown
                    }
                }
            }
            ExprKind::Await(inner) => {
                let t = self.infer_expr(inner);
                match &t {
                    Ty::Future(out) => (**out).clone(),
                    Ty::Pin(inner) => match &**inner {
                        Ty::Future(out) => (**out).clone(),
                        _ => {
                            self.err(span, "`await` requires a `Future`".to_string());
                            Ty::Unknown
                        }
                    },
                    _ => {
                        self.err(span, "`await` requires a `Future`".to_string());
                        Ty::Unknown
                    }
                }
            }
            ExprKind::Unsafe(b) => self.infer_block(b),
            ExprKind::Closure { params, ret, body } => {
                self.check_closure(span, params, ret, body)
            }
            ExprKind::Macro(..) => {
                self.err(
                    span,
                    "macros are not in the type-checked subset".to_string(),
                );
                Ty::Unknown
            }
            ExprKind::Try(..) => {
                self.err(
                    span,
                    "the `?` operator is not in the type-checked subset".to_string(),
                );
                Ty::Unknown
            }
        };
        ty
    }

    fn check_binary(&mut self, span: Span, l: &Expr, op: BinOp, r: &Expr) -> Ty {
        if matches!(op, BinOp::Assign) {
            let lt = self.infer_expr(l);
            let rt = self.infer_expr(r);
            self.expect(&rt, &lt, span);
            return lt;
        }
        if matches!(
            op,
            BinOp::AddAssign
                | BinOp::SubAssign
                | BinOp::MulAssign
                | BinOp::DivAssign
                | BinOp::RemAssign
                | BinOp::BitAndAssign
                | BinOp::BitOrAssign
                | BinOp::BitXorAssign
                | BinOp::ShlAssign
                | BinOp::ShrAssign
        ) {
            let lt = self.infer_expr(l);
            let rt = self.infer_expr(r);
            if !matches!(lt, Ty::Unknown) && !lt.is_numeric() {
                self.err(
                    span,
                    "compound assignment requires a numeric type".to_string(),
                );
            }
            self.expect(&rt, &lt, span);
            return lt;
        }
        if matches!(op, BinOp::Range) {
            self.infer_expr(l);
            self.infer_expr(r);
            return Ty::Range;
        }

        let lt = self.infer_expr(l);
        let rt = self.infer_expr(r);
        match op {
            BinOp::And | BinOp::Or => {
                self.expect(&lt, &Ty::Bool, span);
                self.expect(&rt, &Ty::Bool, span);
                Ty::Bool
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                if let Some(tr) = BuiltinTrait::for_binop(op) {
                    self.implements_op(&lt, tr, span);
                    self.implements_op(&rt, tr, span);
                }
                self.expect(&lt, &rt, span);
                Ty::Bool
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if let Some(tr) = BuiltinTrait::for_binop(op) {
                    self.implements_op(&lt, tr, span);
                    self.implements_op(&rt, tr, span);
                }
                self.expect(&lt, &rt, span);
                if matches!(lt, Ty::Int) {
                    rt
                } else {
                    lt
                }
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                if !matches!(lt, Ty::Unknown) && !lt.is_integer() {
                    self.err(
                        span,
                        "bitwise operator requires integer operands".to_string(),
                    );
                }
                if !matches!(rt, Ty::Unknown) && !rt.is_integer() {
                    self.err(
                        span,
                        "bitwise operator requires integer operands".to_string(),
                    );
                }
                self.expect(&lt, &rt, span);
                if matches!(lt, Ty::Int) {
                    rt
                } else {
                    lt
                }
            }
            _ => {
                self.err(
                    span,
                    "unsupported binary operator in type-checked subset".to_string(),
                );
                Ty::Unknown
            }
        }
    }

    fn check_unary(&mut self, span: Span, u: UnOp, e2: &Expr) -> Ty {
        match u {
            UnOp::Neg => {
                let t = self.infer_expr(e2);
                if !matches!(t, Ty::Unknown) && !t.is_numeric() {
                    self.err(span, "negation requires a numeric operand".to_string());
                }
                t
            }
            UnOp::Not => {
                let t = self.infer_expr(e2);
                self.expect(&t, &Ty::Bool, span);
                Ty::Bool
            }
            UnOp::Deref => {
                let t = self.infer_expr(e2);
                match &t {
                    Ty::Ref(inner) | Ty::RefMut(inner) => (**inner).clone(),
                    _ => {
                        self.err(span, "cannot dereference a non-reference".to_string());
                        Ty::Unknown
                    }
                }
            }
        }
    }

    fn check_call(&mut self, span: Span, callee: &Expr, args: &[Expr]) -> Ty {
        let path = match &callee.kind {
            ExprKind::Path(p) => p,
            _ => {
                self.err(span, "call target must be a path".to_string());
                return Ty::Unknown;
            }
        };
        let segments: Vec<String> = path.segments.iter().map(|s| s.node.clone()).collect();
        let name = segments.last().cloned().unwrap_or_default();
        let sig = match self.resolve_call(&segments, span) {
            Some(s) => s,
            None => {
                self.err(span, format!("cannot find function `{}`", name));
                return Ty::Unknown;
            }
        };
        if args.len() != sig.params.len() {
            self.err(
                span,
                format!(
                    "function `{}` expects {} argument(s), found {}",
                    name,
                    sig.params.len(),
                    args.len()
                ),
            );
        }
        // Generic functions: infer the concrete type arguments from the call
        // arguments, enforce the declared trait bounds on those types, and
        // substitute them through the return type.
        let ret_ty = if !sig.generics.params.is_empty() {
            let mut sub: HashMap<String, Ty> = HashMap::new();
            for (a, pt) in args.iter().zip(sig.params.iter()) {
                let at = self.infer_expr(a);
                if !self.unify_ty(pt, &at, &mut sub) {
                    self.err(
                        a.span,
                        format!(
                            "type mismatch: generic parameter inferred inconsistently (expected {}, found {})",
                            self.apply_sub(pt, &sub),
                            at
                        ),
                    );
                }
            }
            // Enforce declared bounds on the inferred type arguments.
            for p in &sig.generics.params {
                if let Some(concrete) = sub.get(&p.name.node) {
                    for bound in &p.bounds {
                        let bn = bound_trait_name_str(bound);
                        if let Some(bt) = BuiltinTrait::from_name(&bn) {
                            if !TraitDb::implements(bt, concrete) {
                                self.err(
                                    span,
                                    format!(
                                        "the trait bound `{}: {}` is not satisfied",
                                        concrete,
                                        bt.name()
                                    ),
                                );
                            }
                        } else if !matches!(concrete, Ty::Generic(_)) {
                            // User-defined trait bound: consult the solver's
                            // `impl` clauses (e.g. `impl Show for i32`). Integer
                            // literals infer as `Ty::Int` (the i32 default), so
                            // normalize to `Ty::I32` for the query.
                            let query_ty = if matches!(concrete, Ty::Int) {
                                Ty::I32
                            } else {
                                concrete.clone()
                            };
                            let holds = self.solver.as_ref().map_or(false, |s| {
                                s.implements_trait(
                                    crate::solver::TraitId::User(bn.clone()),
                                    &query_ty,
                                )
                            });
                            if !holds {
                                self.err(
                                    span,
                                    format!(
                                        "the trait bound `{}: {}` is not satisfied",
                                        concrete, bn
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            self.apply_sub(&sig.ret, &sub)
        } else {
            for (a, pt) in args.iter().zip(sig.params.iter()) {
                let at = self.infer_expr(a);
                self.expect(&at, pt, a.span);
            }
            sig.ret.clone()
        };
        if sig.is_async {
            Ty::Future(Box::new(ret_ty))
        } else {
            ret_ty
        }
    }

    fn check_field(&mut self, span: Span, base: &Expr, field: &clotho::ast::Spanned<String>) -> Ty {
        let bt = self.infer_expr(base);
        // Field access auto-derefs through `&`/`&mut` (so `self.x` works when
        // `self` is `&Foo`).
        let bt = self.strip_refs(&bt);
        match &bt {
            Ty::Struct(s) | Ty::Apply(s, _) => match self.structs.get(s) {
                Some(info) => match info.fields.iter().find(|(n, _)| n == &field.node) {
                    Some((_, ft)) => ft.clone(),
                    None => {
                        self.err(span, format!("no field `{}` on type `{}`", field.node, s));
                        Ty::Unknown
                    }
                },
                None => {
                    self.err(span, format!("unknown struct `{}`", s));
                    Ty::Unknown
                }
            },
            Ty::Tuple(elems) => match field.node.parse::<usize>() {
                Ok(idx) if idx < elems.len() => elems[idx].clone(),
                _ => {
                    self.err(
                        span,
                        format!("no field `{}` on tuple type `{}`", field.node, bt),
                    );
                    Ty::Unknown
                }
            },
            _ => {
                self.err(span, format!("type {} has no fields", bt));
                Ty::Unknown
            }
        }
    }

    fn check_struct(
        &mut self,
        span: Span,
        path: &Path,
        fields: &[(clotho::ast::Spanned<String>, Expr)],
        base: Option<&Expr>,
    ) -> Ty {
        let name = path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default();
        match self.structs.get(&name).cloned() {
            Some(info) => {
                // Infer the struct's generic arguments from the field
                // initializers (mirroring generic call-site inference), then
                // check each initializer against its (substituted) field type.
                let mut sub: HashMap<String, Ty> = HashMap::new();
                for (fname, fexpr) in fields {
                    if let Some((_, ft)) = info.fields.iter().find(|(n, _)| n == &fname.node) {
                        let at = self.infer_expr(fexpr);
                        if !info.generics.params.is_empty() {
                            self.unify_ty(ft, &at, &mut sub);
                        }
                    }
                }
                for (fname, fexpr) in fields {
                    match info.fields.iter().find(|(n, _)| n == &fname.node) {
                        Some((_, ft)) => {
                            let at = self.infer_expr(fexpr);
                            let expected = self.apply_sub(ft, &sub);
                            self.expect(&at, &expected, fexpr.span);
                        }
                        None => self.err(
                            span,
                            format!("struct `{}` has no field `{}`", name, fname.node),
                        ),
                    }
                }
                for (dn, _) in &info.fields {
                    if !fields.iter().any(|(fn2, _)| fn2.node == *dn) {
                        self.err(
                            span,
                            format!("missing field `{}` for struct `{}`", dn, name),
                        );
                    }
                }
                if base.is_some() {
                    self.err(
                        span,
                        "`..base` in struct literal is not supported".to_string(),
                    );
                }
                Ty::Struct(name)
            }
            None => {
                self.err(span, format!("unknown struct `{}`", name));
                Ty::Unknown
            }
        }
    }

    // --- helpers ---------------------------------------------------------

    /// Resolve a call path to a function signature. A single-segment path
    /// (`foo`) looks up a free function; a two-segment path (`Type::fn`) looks
    /// up an associated function from the prelude.
    fn resolve_call(&mut self, segments: &[String], span: Span) -> Option<FnSig> {
        match segments.len() {
            1 => self.fns.get(&segments[0]).cloned(),
            2 => self
                .assoc_fns
                .get(&(segments[0].clone(), segments[1].clone()))
                .cloned()
                .or_else(|| self.fns.get(&segments[1]).cloned()),
            _ => {
                self.err(
                    span,
                    "call path with more than two segments is not supported".to_string(),
                );
                None
            }
        }
    }

    /// Type-check a method call `receiver.method(args)`. The receiver's type is
    /// resolved and the method is looked up in the prelude's method table keyed
    /// by `(receiver_type_name, method_name)`.
    fn check_method(
        &mut self,
        span: Span,
        recv: &Expr,
        method: &clotho::ast::Spanned<String>,
        args: &[Expr],
    ) -> Ty {
        let recv_ty = self.infer_expr(recv);
        let recv_name = self.type_name(&recv_ty);
        // 1) User-defined method from an `impl` block. The receiver is the first
        //    parameter (`self`/`&self`/`&mut self`); the remaining parameters are
        //    the call arguments.
        if let Some(name) = &recv_name {
            if let Some(sig) = self
                .user_methods
                .get(&(name.clone(), method.node.clone()))
                .cloned()
            {
                return self.check_user_method(span, &recv_ty, &sig, args);
            }
        }
        // 2) Prelude method (`std` types). These carry no receiver in their
        //    signature; all parameters are the call arguments.
        let sig = match recv_name {
            Some(name) => self.methods.get(&(name, method.node.clone())).cloned(),
            None => None,
        };
        let sig = match sig {
            Some(s) => s,
            None => {
                self.err(
                    span,
                    format!(
                        "no method `{}` on type `{}`",
                        method.node,
                        self.type_name(&recv_ty)
                            .unwrap_or_else(|| "<unknown>".to_string())
                    ),
                );
                return Ty::Unknown;
            }
        };
        if args.len() != sig.params.len() {
            self.err(
                span,
                format!(
                    "method `{}` expects {} argument(s), found {}",
                    method.node,
                    sig.params.len(),
                    args.len()
                ),
            );
        }
        for (a, pt) in args.iter().zip(sig.params.iter()) {
            let at = self.infer_expr(a);
            self.expect(&at, pt, a.span);
        }
        sig.ret.clone()
    }

    /// Type-check a call to a user-defined method resolved from an `impl` block.
    /// The receiver must satisfy the method's first parameter (the receiver
    /// type, with auto-ref/auto-mut-ref, matching Rust's method resolution),
    /// and the remaining arguments must satisfy the rest of the signature.
    fn check_user_method(
        &mut self,
        span: Span,
        recv_ty: &Ty,
        sig: &FnSig,
        args: &[Expr],
    ) -> Ty {
        // Receiver check (auto-ref: `x.foo()` where `foo(&self)`).
        if !sig.params.is_empty() {
            let recv_param = &sig.params[0];
            let recv_ok = self.compatible(recv_ty, recv_param)
                || self.compatible(&Ty::Ref(Box::new(recv_ty.clone())), recv_param)
                || self.compatible(&Ty::RefMut(Box::new(recv_ty.clone())), recv_param);
            if !recv_ok {
                self.err(
                    span,
                    format!(
                        "method receiver of type `{}` does not match expected `{}`",
                        recv_ty, recv_param
                    ),
                );
            }
        }
        let rest = &sig.params[1..];
        if args.len() != rest.len() {
            self.err(
                span,
                format!(
                    "method expects {} argument(s), found {}",
                    rest.len(),
                    args.len()
                ),
            );
        }
        for (a, pt) in args.iter().zip(rest.iter()) {
            let at = self.infer_expr(a);
            self.expect(&at, pt, a.span);
        }
        sig.ret.clone()
    }

    /// The base type name of a value type, following `&`/`&mut` to its referent
    /// so methods/associated functions on the referent can be found.
    fn type_name(&self, ty: &Ty) -> Option<String> {
        match ty {
            Ty::Struct(n) | Ty::Apply(n, _) => Some(n.clone()),
            Ty::Ref(inner) | Ty::RefMut(inner) => self.type_name(inner),
            _ => None,
        }
    }

    /// Follow `&`/`&mut` to the underlying type (used for field access, which
    /// auto-derefs through references).
    fn strip_refs(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Ref(inner) | Ty::RefMut(inner) => self.strip_refs(inner),
            other => other.clone(),
        }
    }

    // --- standard-library prelude ---------------------------------------

    /// Register the standard-library prelude: recognized `std` types and their
    /// constructors, associated functions, and methods. This is the documented
    /// remaining step for "idiomatic `std`-using Rust programs compile
    /// correctly" — binding the signatures so the checker accepts them.
    ///
    /// The `std` prelude entries still model generic `std` types loosely
    /// (`Ty::Unknown` accepts and yields anything) — full generic-aware `std`
    /// entries await the struct-generics follow-up; user **function** generics
    /// are now handled natively via `Ty::Generic` (see `check_call` /
    /// `type_to_ty`).
    fn register_prelude(&mut self) {
        use Ty::*;
        let std = |name: &str| Struct(name.to_string());

        // Free constructors / functions.
        self.fns
            .insert("Some".into(), sig(vec![Unknown], std("Option")));
        self.fns.insert("None".into(), sig(vec![], std("Option")));
        // `None` is also usable as a bare nullary value (a unit variant), unlike
        // `Some` which always takes an argument.
        self.values.insert("None".into(), std("Option"));
        self.fns
            .insert("Ok".into(), sig(vec![Unknown], std("Result")));
        self.fns
            .insert("Err".into(), sig(vec![Unknown], std("Result")));
        self.fns.insert("vec_new".into(), sig(vec![], std("Vec")));
        self.fns
            .insert("string_new".into(), sig(vec![], std("String")));
        self.fns
            .insert("box_new".into(), sig(vec![Unknown], std("Box")));

        // Associated functions `Type::fn()`.
        self.assoc_fns
            .insert(("Vec".into(), "new".into()), sig(vec![], std("Vec")));
        self.assoc_fns
            .insert(("String".into(), "new".into()), sig(vec![], std("String")));
        self.assoc_fns
            .insert(("Box".into(), "new".into()), sig(vec![Unknown], std("Box")));

        // Methods on `Vec<T>`.
        self.methods
            .insert(("Vec".into(), "push".into()), sig(vec![Unknown], Unit));
        self.methods
            .insert(("Vec".into(), "len".into()), sig(vec![], Usize));
        self.methods
            .insert(("Vec".into(), "is_empty".into()), sig(vec![], Bool));
        // Methods on `Option<T>`.
        self.methods
            .insert(("Option".into(), "is_some".into()), sig(vec![], Bool));
        self.methods
            .insert(("Option".into(), "is_none".into()), sig(vec![], Bool));
        self.methods
            .insert(("Option".into(), "unwrap".into()), sig(vec![], Unknown));
        // Methods on `Result<T, E>`.
        self.methods
            .insert(("Result".into(), "is_ok".into()), sig(vec![], Bool));
        self.methods
            .insert(("Result".into(), "is_err".into()), sig(vec![], Bool));
        self.methods
            .insert(("Result".into(), "unwrap".into()), sig(vec![], Unknown));
        // Methods on `String`.
        self.methods
            .insert(("String".into(), "push_str".into()), sig(vec![Str], Unit));
        self.methods
            .insert(("String".into(), "len".into()), sig(vec![], Usize));

        // Wider primitive output helpers (mirror the C/native backend runtimes).
        self.fns.insert(
            "print_i64".to_string(),
            FnSig {
                params: vec![Ty::I64],
                ret: Ty::Unit,
                is_async: false,
                generics: Generics::default(),
            },
        );
        self.fns.insert(
            "print_usize".to_string(),
            FnSig {
                params: vec![Ty::Usize],
                ret: Ty::Unit,
                is_async: false,
                generics: Generics::default(),
            },
        );
        self.fns.insert(
            "print_f64".to_string(),
            FnSig {
                params: vec![Ty::F64],
                ret: Ty::Unit,
                is_async: false,
                generics: Generics::default(),
            },
        );
        self.fns.insert(
            "print_str".to_string(),
            FnSig {
                params: vec![Ty::Str],
                ret: Ty::Unit,
                is_async: false,
                generics: Generics::default(),
            },
        );
    }

    fn lookup(&self, name: &str) -> Option<Ty> {
        for scope in self.env.iter().rev() {
            if let Some(t) = scope.get(name) {
                return Some(t.clone());
            }
        }
        None
    }

    /// Whether `name` names a generic parameter visible in the current scope
    /// stack (a function's own params and any enclosing `impl`'s params).
    fn is_generic_param(&self, name: &str) -> bool {
        self.generic_scope
            .iter()
            .any(|g| g.params.iter().any(|p| p.name.node == name))
    }

    /// Enforce an operator's required built-in trait on an operand type.
    ///
    /// Concrete types are checked against the built-in trait database; a generic
    /// parameter `T` satisfies the operator only when it carries the
    /// corresponding bound (`T: Add`) in its enclosing function's generics.
    fn implements_op(&mut self, ty: &Ty, tr: BuiltinTrait, span: Span) {
        let ok = match ty {
            Ty::Generic(n) => {
                // `Eq`/`Ord` are written as such but surface under their
                // lang-item names (`PartialEq`/`PartialOrd`) in diagnostics.
                let names: &[&str] = match tr {
                    BuiltinTrait::Eq => &["Eq", "PartialEq"],
                    BuiltinTrait::Ord => &["Ord", "PartialOrd"],
                    other => &[other.name()],
                };
                names.iter().any(|nm| self.generic_has_bound(n, nm))
            }
            _ => TraitDb::implements(tr, ty),
        };
        if !ok {
            self.err(
                span,
                format!("the trait bound `{}: {}` is not satisfied", ty, tr.name()),
            );
        }
    }

    /// Whether the generic parameter `name` carries the trait bound `bound`
    /// (either directly `T: Bound` or via a where-clause `where T: Bound`),
    /// searching the whole generic scope stack.
    fn generic_has_bound(&self, name: &str, bound: &str) -> bool {
        for g in &self.generic_scope {
            for p in &g.params {
                if p.name.node == name
                    && p.bounds.iter().any(|b| bound_trait_name_str(b) == bound)
                {
                    return true;
                }
            }
            for pred in &g.where_clause {
                if let TypeKind::Path(p, _) = &pred.ty.kind {
                    if p.segments.last().map(|s| s.node == name).unwrap_or(false)
                        && pred.bounds.iter().any(|b| bound_trait_name_str(b) == bound)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Unify a pattern type (which may contain [`Ty::Generic`] variables) against
    /// an actual type, recording generic substitutions in `sub`. Returns false
    /// on a conflicting substitution (e.g. the same parameter inferred as both
    /// `i32` and `&str`).
    fn unify_ty(&self, pat: &Ty, actual: &Ty, sub: &mut HashMap<String, Ty>) -> bool {
        match (pat, actual) {
            (Ty::Generic(n), other) | (other, Ty::Generic(n)) => {
                if let Some(prev) = sub.get(n) {
                    prev == other
                } else {
                    sub.insert(n.clone(), other.clone());
                    true
                }
            }
            (Ty::Struct(a), Ty::Struct(b)) => a == b,
            (Ty::Apply(a, _), Ty::Apply(b, _)) => a == b,
            (Ty::Apply(a, _), Ty::Struct(b)) | (Ty::Struct(a), Ty::Apply(b, _)) => a == b,
            (Ty::Ref(a), Ty::Ref(b)) | (Ty::RefMut(a), Ty::RefMut(b)) => {
                self.unify_ty(a, b, sub)
            }
            (Ty::Array(a, _), Ty::Array(b, _)) => self.unify_ty(a, b, sub),
            (Ty::Int, x) if x.is_numeric() => true,
            (x, Ty::Int) if x.is_numeric() => true,
            (a, b) => a == b,
        }
    }

    /// Apply a generic substitution throughout `ty`, replacing each
    /// [`Ty::Generic`] parameter by its inferred concrete type.
    fn apply_sub(&self, ty: &Ty, sub: &HashMap<String, Ty>) -> Ty {
        match ty {
            Ty::Generic(n) => sub.get(n).cloned().unwrap_or_else(|| ty.clone()),
            Ty::Ref(t) => Ty::Ref(Box::new(self.apply_sub(t, sub))),
            Ty::RefMut(t) => Ty::RefMut(Box::new(self.apply_sub(t, sub))),
            Ty::Future(t) => Ty::Future(Box::new(self.apply_sub(t, sub))),
            Ty::Apply(n, args) => {
                Ty::Apply(n.clone(), args.iter().map(|a| self.apply_sub(a, sub)).collect())
            }
            other => other.clone(),
        }
    }

    fn type_to_ty(&self, t: &Type) -> Ty {
        match &t.kind {
            TypeKind::Path(p, args) => {
                let name = p
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                // `Self` resolves to the enclosing impl's concrete self type.
                if name == "Self" {
                    return match &self.self_type {
                        Some(s) => Ty::Struct(s.clone()),
                        None => Ty::Struct("Self".to_string()),
                    };
                }
                // A bare type-name that names a generic parameter of the
                // enclosing item resolves to that parameter (so `T` in
                // `fn f<T>(x: T)` or `impl<T> Foo<T>` is `Ty::Generic("T")`).
                if self.is_generic_param(&name) {
                    return Ty::Generic(name);
                }
                match prim(&name) {
                    Some(t) => t,
                    None => {
                        if args.is_empty() {
                            Ty::Struct(name)
                        } else {
                            // A generic application (e.g. `Vec<i32>`, `Option<T>`):
                            // keep the base name and carry the resolved arguments.
                            Ty::Apply(
                                name,
                                args.iter().map(|a| self.type_to_ty(a)).collect(),
                            )
                        }
                    }
                }
            }
            TypeKind::Ref(inner) => Ty::Ref(Box::new(self.type_to_ty(inner))),
            TypeKind::RefMut(inner) => Ty::RefMut(Box::new(self.type_to_ty(inner))),
            TypeKind::Future(inner) => Ty::Future(Box::new(self.type_to_ty(inner))),
            TypeKind::Array(inner, n) => {
                Ty::Array(Box::new(self.type_to_ty(inner)), *n)
            }
            TypeKind::Slice(inner) => Ty::Array(Box::new(self.type_to_ty(inner)), 0),
            TypeKind::Tuple(elems) => {
                Ty::Tuple(elems.iter().map(|e| self.type_to_ty(e)).collect())
            }
            TypeKind::Infer => Ty::Int,
        }
    }

    /// Map a Clotho type to a Lachesis type. Exposed (mirroring the method form)
    /// so other subsystems — e.g. the trait solver — can translate `impl` self
    /// types without depending on `TypeChecker` internals.
    pub fn ast_to_ty(ty: &clotho::ast::Type) -> Ty {
        match &ty.kind {
            TypeKind::Path(p, _) => {
                let name = p
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                match prim(&name) {
                    Some(t) => t,
                    None => Ty::Struct(name),
                }
            }
            TypeKind::Ref(inner) => Ty::Ref(Box::new(TypeChecker::ast_to_ty(inner))),
            TypeKind::RefMut(inner) => Ty::RefMut(Box::new(TypeChecker::ast_to_ty(inner))),
            TypeKind::Future(inner) => Ty::Future(Box::new(TypeChecker::ast_to_ty(inner))),
            TypeKind::Array(inner, n) => Ty::Array(Box::new(TypeChecker::ast_to_ty(inner)), *n),
            TypeKind::Slice(inner) => Ty::Array(Box::new(TypeChecker::ast_to_ty(inner)), 0),
            TypeKind::Infer => Ty::Int,
            _ => Ty::Unknown,
        }
    }

    /// Whether two types are mutually compatible for assignment/use.
    fn compatible(&self, a: &Ty, b: &Ty) -> bool {
        if matches!(a, Ty::Unknown) || matches!(b, Ty::Unknown) {
            return true;
        }
        if matches!(a, Ty::Generic(_)) || matches!(b, Ty::Generic(_)) {
            // Generic parameters accept/return anything within their own scope;
            // concrete obligations are enforced via the declared trait bounds.
            return true;
        }
        if matches!(a, Ty::Int) && b.is_numeric() {
            return true;
        }
        if matches!(b, Ty::Int) && a.is_numeric() {
            return true;
        }
        match (a, b) {
            (Ty::Ref(x), Ty::Ref(y))
            | (Ty::Ref(x), Ty::RefMut(y))
            | (Ty::RefMut(x), Ty::Ref(y))
            | (Ty::RefMut(x), Ty::RefMut(y)) => self.compatible(x, y),
            (Ty::Array(a, _), Ty::Array(b, _)) => self.compatible(a, b),
            (Ty::Tuple(a), Ty::Tuple(b)) => {
                a.len() == b.len()
                    && a.iter().zip(b).all(|(x, y)| self.compatible(x, y))
            }
            (Ty::Ref(_), _) | (Ty::RefMut(_), _) => false,
            (Ty::Struct(x), Ty::Struct(y)) => x == y,
            (Ty::Apply(x, _), Ty::Apply(y, _)) => x == y,
            (Ty::Apply(x, _), Ty::Struct(y))
            | (Ty::Struct(x), Ty::Apply(y, _)) => x == y,
            (Ty::Fn(pa, ra), Ty::Fn(pb, rb)) => {
                pa.len() == pb.len()
                    && pa.iter().zip(pb).all(|(x, y)| self.compatible(x, y))
                    && self.compatible(ra, rb)
            }
            (x, y) => x == y,
        }
    }

    fn expect(&mut self, a: &Ty, b: &Ty, span: Span) {
        if !self.compatible(a, b) {
            self.err(span, format!("type mismatch: expected {}, found {}", b, a));
        }
    }

    fn err(&mut self, span: Span, msg: impl Into<String>) {
        self.errors.push(Diag::error(msg.into(), Some(span)));
    }
}

/// Build a non-async [`FnSig`] from `params` and `ret` (helper for the prelude).
fn sig(params: Vec<Ty>, ret: Ty) -> FnSig {
    FnSig {
        params,
        ret,
        is_async: false,
        generics: Generics::default(),
    }
}

/// The trait name of a bound, taken from the last path segment.
fn bound_trait_name_str(b: &TypeBound) -> String {
    match b {
        TypeBound::Trait(p) => p
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default(),
    }
}

/// Map an identifier type name to its primitive [`Ty`], if it is one.
fn prim(name: &str) -> Option<Ty> {
    Some(match name {
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
        _ => return None,
    })
}

fn lit_ty(l: &Lit) -> Ty {
    match l {
        Lit::Int(_, suffix) => match suffix {
            Some(s) => prim(s).unwrap_or(Ty::Int),
            None => Ty::Int,
        },
        Lit::Float(_, suffix) => match suffix.as_deref() {
            Some("f32") => Ty::F32,
            _ => Ty::F64,
        },
        Lit::Str(_) => Ty::Str,
        Lit::Char(_) => Ty::Char,
        Lit::Bool(_) => Ty::Bool,
    }
}

fn pat_name(pat: &Pat) -> String {
    match pat {
        Pat::Ident(n) => n.node.clone(),
        Pat::Wild => "_".to_string(),
        Pat::Bind(n, _) => n.clone(),
        Pat::Path(path) => path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_else(|| "_".to_string()),
        _ => "_".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(src: &str) -> Result<(), Vec<Diag>> {
        let prog = clotho::parser::Parser::from_source(src)
            .parse_program()
            .expect("parse");
        TypeChecker::check_program(&prog)
    }

    #[test]
    fn generic_identity_is_accepted() {
        assert!(check("fn id<T>(x: T) -> T { x }").is_ok());
    }

    #[test]
    fn generic_max_with_ord_bound_is_accepted() {
        assert!(
            check("fn max<T: Ord>(a: T, b: T) -> T { if a > b { a } else { b } }").is_ok()
        );
    }

    #[test]
    fn generic_add_with_add_bound_is_accepted() {
        assert!(check("fn add<T: Add>(a: T, b: T) -> T { a + b }").is_ok());
    }

    #[test]
    fn generic_without_operator_bound_is_rejected() {
        // `a + b` requires `T: Add`, which is not declared.
        assert!(check("fn bad<T>(a: T, b: T) -> T { a + b }").is_err());
    }

    #[test]
    fn generic_call_infers_return_type() {
        assert!(check("fn id<T>(x: T) -> T { x }\nfn use_id() { let x = id(5); }").is_ok());
    }

    #[test]
    fn generic_call_with_concrete_bound_accepted() {
        assert!(check(
            "fn add<T: Add>(a: T, b: T) -> T { a + b }\nfn use_add() { let y = add(1, 2); }"
        )
        .is_ok());
    }

    #[test]
    fn generic_call_violating_bound_is_rejected() {
        // `bool` does not implement `Add`.
        assert!(check(
            "fn add<T: Add>(a: T, b: T) -> T { a + b }\nfn misuse() { add(true, false); }"
        )
        .is_err());
    }

    #[test]
    fn generic_call_ord_bound_on_i32_accepted() {
        assert!(check(
            "fn max<T: Ord>(a: T, b: T) -> T { if a > b { a } else { b } }\nfn use_max() { let m = max(3, 4); }"
        )
        .is_ok());
    }

    #[test]
    fn generic_with_where_clause_bound_is_accepted() {
        assert!(
            check("fn add<T>(a: T, b: T) -> T where T: Add { a + b }").is_ok()
        );
    }

    #[test]
    fn generic_call_inconsistent_args_rejected() {
        // The same parameter `T` is inferred as both `i32` and `bool`.
        assert!(check(
            "fn first<T>(a: T, b: T) -> T { a }\nfn bad() { first(1, true); }"
        )
        .is_err());
    }

    // --- struct generics ------------------------------------------------

    #[test]
    fn generic_struct_literal_infers_type() {
        assert!(
            check("struct Point<T> { x: T, y: T }\nfn f() { let p = Point { x: 1, y: 2 }; }")
                .is_ok()
        );
    }

    #[test]
    fn generic_struct_inconsistent_fields_rejected() {
        // `T` would have to be both `i32` (from `a`) and `bool` (from `b`).
        assert!(
            check("struct Pair<T> { a: T, b: T }\nfn f() { let p = Pair { a: 1, b: true }; }")
                .is_err()
        );
    }

    // --- user-defined method resolution --------------------------------

    #[test]
    fn user_method_call_is_accepted() {
        assert!(
            check(
                "struct Foo { x: i32 }\nimpl Foo { fn get(&self) -> i32 { self.x } }\nfn f() { let foo = Foo { x: 5 }; let y = foo.get(); }"
            )
            .is_ok()
        );
    }

    #[test]
    fn user_method_arg_mismatch_rejected() {
        assert!(
            check(
                "struct Foo { x: i32 }\nimpl Foo { fn add(&self, n: i32) -> i32 { n } }\nfn f() { let foo = Foo { x: 5 }; foo.add(true); }"
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_user_method_rejected() {
        assert!(
            check(
                "struct Foo { x: i32 }\nfn f() { let foo = Foo { x: 5 }; foo.bar(); }"
            )
            .is_err()
        );
    }

    #[test]
    fn user_method_body_is_checked() {
        // `self.y` does not exist on `Foo`, so the impl method body is rejected.
        assert!(
            check(
                "struct Foo { x: i32 }\nimpl Foo { fn bad(&self) -> i32 { self.y } }"
            )
            .is_err()
        );
    }

    #[test]
    fn generic_impl_method_uses_self_type() {
        assert!(
            check(
                "struct Foo<T> { x: T }\nimpl<T> Foo<T> { fn get(&self) -> T { self.x } }\nfn f() { let foo = Foo { x: 5 }; let y = foo.get(); }"
            )
            .is_ok()
        );
    }

    // --- generic type arguments (applied generics) ----------------------

    #[test]
    fn applied_generic_annotation_is_accepted() {
        // `Option<i32>` and the `Some` constructor (a bare `Option`) are
        // compatible by base name.
        assert!(check("fn f() { let x: Option<i32> = Some(5); }").is_ok());
    }

    #[test]
    fn applied_generic_in_struct_field_is_accepted() {
        assert!(
            check(
                "struct Pair<T> { a: T, b: Vec<T> }\nfn f() { let p = Pair { a: 1, b: vec_new() }; }"
            )
            .is_ok()
        );
    }

    #[test]
    fn applied_generic_field_enforces_type() {
        // `W { v: Vec<i32> }` rejects a `bool` initializer.
        assert!(
            check("struct W { v: Vec<i32> }\nfn f() { let w = W { v: true }; }").is_err()
        );
    }

    #[test]
    fn generic_fn_with_applied_param_is_accepted() {
        assert!(
            check(
                "fn len<T>(v: Vec<T>) -> usize { 0 }\nfn f() { let n = len(vec_new()); }"
            )
            .is_ok()
        );
    }

    // --- user-defined trait bounds on generic call sites --------------

    #[test]
    fn user_trait_bound_satisfied_is_accepted() {
        // `i32` implements `Show` via a user `impl`, so the call is accepted.
        assert!(
            check(
                "impl Show for i32 {}\nfn display<T: Show>(x: T) {}\nfn m() { display(5); }"
            )
            .is_ok()
        );
    }

    #[test]
    fn user_trait_bound_violated_is_rejected() {
        // `str` has no `impl Show`, so the call is rejected.
        assert!(
            check(
                "impl Show for i32 {}\nfn display<T: Show>(x: T) {}\nfn m() { display(\"hi\"); }"
            )
            .is_err()
        );
    }

    #[test]
    fn cast_type_checks_and_yields_target_type() {
        // `expr as Type` is accepted by the Phase-1 checker and the cast's type
        // is the explicit target type. The inner expression is still checked.
        assert!(check("fn f(x: i32) -> i64 { x as i64 }").is_ok());
        assert!(check("fn g(x: i32) { let y = x as u32; let _ = y; }").is_ok());
        // A type error inside the cast operand is still reported.
        assert!(check("fn h() { let _ = unknown_fn() as i64; }").is_err());
    }
}
