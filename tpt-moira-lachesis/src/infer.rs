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
use tpt_moira_clotho::ast::{
    BinOp, Block, Expr, ExprKind, Generics, Lit, Local, Param, Pat, Path, StmtKind, Type,
    TypeBound, TypeKind, UnOp,
};
use tpt_moira_clotho::error::Diag;
use tpt_moira_clotho::token::Span;
use std::collections::{HashMap, HashSet};

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
    /// Methods declared inside `impl Trait for Type { .. }` blocks, keyed by
    /// `(trait_name, method_name)`. Consulted by method resolution when the
    /// receiver is a generic parameter bounded by that trait (`fn f<T: Show>(t:
    /// T) { t.show(); }`) or when the Chalk-like solver proves the concrete
    /// receiver type implements the trait (the "wire the solver into method
    /// resolution" Phase-3 deliverable).
    trait_methods: HashMap<(String, String), FnSig>,
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
    /// Associated constants `Type::CONST`, keyed by `(type_name, const_name)`
    /// (e.g. `ExitCode::FAILURE`). User top-level constants are registered
    /// under their own name at collection time.
    assoc_consts: HashMap<(String, String), Ty>,
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
            trait_methods: HashMap::new(),
            self_type: None,
            solver: None,
            values: HashMap::new(),
            assoc_consts: HashMap::new(),
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
    pub fn check_program(prog: &tpt_moira_clotho::ast::Program) -> Result<(), Vec<Diag>> {
        Self::check_program_with(prog, &[])
    }

    /// Type-check a whole program with **extern crate** signature tables:
    /// each `(crate_name, program)` pair contributes its top-level function
    /// signatures, inherent/trait impl methods, and struct shapes so that
    /// cross-crate paths (`tpt_moira_atropos::lower_program(...)`,
    /// `tpt_moira_clotho::compile_file(...)`) resolve. Only signatures are
    /// imported — bodies are not re-checked — and module prefixes are
    /// flattened: a path resolves against its last two segments, so
    /// `crate::module::item` matches key `(module, item)` and
    /// `crate::item` matches `(crate, item)`.
    pub fn check_program_with(
        prog: &tpt_moira_clotho::ast::Program,
        externs: &[(&str, &tpt_moira_clotho::ast::Program)],
    ) -> Result<(), Vec<Diag>> {
        let mut cx = TypeChecker::default();
        for (crate_name, ext) in externs {
            cx.register_extern(crate_name, ext);
        }
        // Seed the trait solver with the user `impl` clauses so user-defined
        // trait bounds (e.g. `T: Show`) can be enforced at generic call sites.
        cx.solver = Some(crate::solver::Solver::new(crate::solver::program_with_impls(
            prog,
        )));
        // Pass 1: collect struct and function signatures so callers may refer to
        // functions defined later in the file (forward references). Items
        // gated behind `#[cfg(test)]` (e.g. `mod tests`) are skipped — test
        // code is outside the type-checked subset. Inline modules are
        // *flattened* first so their items register exactly like top-level
        // ones (an enum defined in a `mod` file must still be registered).
        let mut flat_items: Vec<&tpt_moira_clotho::ast::Item> = Vec::new();
        Self::flatten_inline_modules(&prog.items, &mut flat_items);
        for item in &flat_items {
            let item: &tpt_moira_clotho::ast::Item = item;
            if item.is_cfg_test() {
                continue;
            }
            match &item.kind {
                tpt_moira_clotho::ast::ItemKind::Struct(s) => {
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
                tpt_moira_clotho::ast::ItemKind::Impl(imp) => {
                    // Collect the methods declared in user `impl` blocks so that
                    // `receiver.method(args)` calls on the user's own types
                    // resolve. `Self` and the impl's generic params are in scope.
                    let self_name = imp.for_ty.node_name();
                    cx.generic_scope.push(imp.generics.clone());
                    cx.self_type = Some(self_name.clone());
                    for m in &imp.items {
                        // Associated constants declared inside an `impl` block
                        // (`impl Allowlist { const DEFAULT_FILE: &str = ..; }`)
                        // resolve as `Type::CONST` two-segment paths.
                        if let tpt_moira_clotho::ast::ItemKind::Const(c) = &m.kind {
                            let ty = c.ty.as_ref().map(|t| cx.type_to_ty(t)).unwrap_or(Ty::Unknown);
                            cx.assoc_consts
                                .insert((self_name.clone(), c.name.node.clone()), ty);
                            continue;
                        }
                        if let tpt_moira_clotho::ast::ItemKind::Function(f) = &m.kind {
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
                            // Also index the method under its trait so that
                            // trait-bound method resolution (`T: Show` →
                            // `t.show()`) can find it via the solver.
                            let params2 = f
                                .params
                                .iter()
                                .map(|p| cx.type_to_ty(&p.ty))
                                .collect();
                            let ret2 = f
                                .ret
                                .as_ref()
                                .map(|t| cx.type_to_ty(t))
                                .unwrap_or(Ty::Unit);
                            cx.trait_methods.insert(
                                (
                                    imp.trait_name.node.clone(),
                                    f.name.node.clone(),
                                ),
                                FnSig {
                                    params: params2,
                                    ret: ret2,
                                    is_async: f.is_async,
                                    generics: imp.generics.clone(),
                                },
                            );
                        }
                    }
                    cx.self_type = None;
                    cx.generic_scope.pop();
                }
                tpt_moira_clotho::ast::ItemKind::Function(f) => {
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
                // Enums: register **nullary** variants as associated constants
                // (`TokenKind::Fn` used as a value resolves to the enum type).
                // Tuple/struct variants keep their constructor-function shape
                // and are handled at call sites.
                tpt_moira_clotho::ast::ItemKind::Enum(e) => {
                    Self::register_enum_variants(e, &mut cx);
                }
                tpt_moira_clotho::ast::ItemKind::Const(c) => {
                    // Top-level constants are usable as bare values; the
                    // declared type (when present) is their type, otherwise
                    // they absorb any use.
                    let ty = c.ty.as_ref().map(|t| cx.type_to_ty(t)).unwrap_or(Ty::Unknown);
                    cx.values.insert(c.name.node.clone(), ty);
                }
                tpt_moira_clotho::ast::ItemKind::Static(s) => {
                    let ty = cx.type_to_ty(&s.ty);
                    cx.values.insert(s.name.node.clone(), ty);
                }
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

        // Pass 2: check each function body (`#[cfg(test)]` items skipped).
        for item in &prog.items {
            if item.is_cfg_test() {
                continue;
            }
            if let tpt_moira_clotho::ast::ItemKind::Function(f) = &item.kind {
                cx.check_function(f);
            }
        }

        // Pass 2.5: check the bodies of methods declared in `impl` blocks, with
        // `Self` and the impl's generics in scope (so `self`, `Self`, and `T`
        // all resolve). The methods' signatures were collected in Pass 1.
        for item in &prog.items {
            if item.is_cfg_test() {
                continue;
            }
            if let tpt_moira_clotho::ast::ItemKind::Impl(imp) = &item.kind {
                let self_name = imp.for_ty.node_name();
                cx.self_type = Some(self_name);
                cx.generic_scope.push(imp.generics.clone());
                for m in &imp.items {
                    if let tpt_moira_clotho::ast::ItemKind::Function(f) = &m.kind {
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

    /// Import the *signatures* of an extern crate's items for cross-crate
    /// resolution. Recurses through inline modules, tracking the module path
    /// so each function is reachable as `crate::name`, `module::name`, and
    /// bare `name` (locals shadow externs because local Pass 1 runs later).
    fn register_extern(&mut self, crate_name: &str, prog: &tpt_moira_clotho::ast::Program) {
        Self::collect_extern_items(
            crate_name.to_string(),
            Vec::new(),
            &prog.items,
            self,
        );
    }

    /// Register an enum's **nullary** variants as associated constants so
    /// `EnumName::Variant` used as a *value* (not a pattern) resolves to the
    /// enum's type (`TokenKind::Fn`, `Ordering::Less`, …).
    fn register_enum_variants(
        e: &tpt_moira_clotho::ast::EnumDef,
        cx: &mut TypeChecker,
    ) {
        let enum_ty = tpt_moira_clotho::ast::Type {
            kind: tpt_moira_clotho::ast::TypeKind::Path(
                tpt_moira_clotho::ast::Path {
                    segments: vec![e.name.clone()],
                },
                Vec::new(),
            ),
            span: e.name.span,
        };
        for v in &e.variants {
            match &v.fields {
                None => {
                    let ty = cx.type_to_ty(&enum_ty);
                    cx.assoc_consts
                        .insert((e.name.node.clone(), v.name.node.clone()), ty);
                }
                Some(fields) => {
                    // Tuple variants (`Some(T)`) are constructor functions:
                    // register `Variant(..) -> Enum` (bare and qualified).
                    let ret = cx.type_to_ty(&enum_ty);
                    let sig = FnSig {
                        params: vec![Ty::Unknown; fields.len()],
                        ret,
                        is_async: false,
                        generics: Generics::default(),
                    };
                    cx.fns.insert(v.name.node.clone(), sig.clone());
                    cx.assoc_fns.insert(
                        (e.name.node.clone(), v.name.node.clone()),
                        sig,
                    );
                }
            }
        }
    }

    /// Splice inline-module items into a flat list so Pass 1 registers
    /// module-nested items (structs/enums/functions in `mod` files) exactly
    /// like top-level ones. `#[cfg(test)]` filtering stays with the caller.
    fn flatten_inline_modules<'a>(
        items: &'a [tpt_moira_clotho::ast::Item],
        out: &mut Vec<&'a tpt_moira_clotho::ast::Item>,
    ) {
        for item in items {
            if let tpt_moira_clotho::ast::ItemKind::Module(m) = &item.kind {
                if let tpt_moira_clotho::ast::ModuleKind::Inline(sub) = &m.kind {
                    Self::flatten_inline_modules(sub, out);
                }
            } else {
                out.push(item);
            }
        }
    }

    fn collect_extern_items(
        crate_name: String,
        path: Vec<String>,
        items: &[tpt_moira_clotho::ast::Item],
        cx: &mut TypeChecker,
    ) {
        for item in items {
            if item.is_cfg_test() {
                continue;
            }
            match &item.kind {
                tpt_moira_clotho::ast::ItemKind::Function(f) => {
                    cx.generic_scope.push(f.generics.clone());
                    let params = f.params.iter().map(|p| cx.type_to_ty(&p.ty)).collect();
                    let ret = f.ret.as_ref().map(|t| cx.type_to_ty(t)).unwrap_or(Ty::Unit);
                    cx.generic_scope.pop();
                    let sig = FnSig {
                        params,
                        ret,
                        is_async: f.is_async,
                        generics: f.generics.clone(),
                    };
                    // Bare-name fallback (locals registered later win).
                    cx.fns.insert(f.name.node.clone(), sig.clone());
                    // `crate::name`.
                    cx.assoc_fns.insert(
                        (crate_name.clone(), f.name.node.clone()),
                        sig.clone(),
                    );
                    // `module::name` for the innermost enclosing module.
                    if let Some(mod_name) = path.last() {
                        cx.assoc_fns.insert(
                            (mod_name.clone(), f.name.node.clone()),
                            sig,
                        );
                    }
                }
                tpt_moira_clotho::ast::ItemKind::Struct(s) => {
                    cx.generic_scope.push(s.generics.clone());
                    let fields = s
                        .fields
                        .iter()
                        .map(|f| (f.name.node.clone(), cx.type_to_ty(&f.ty)))
                        .collect();
                    cx.generic_scope.pop();
                    // Different crates can declare same-named structs (e.g.
                    // `clotho::ast::Program` vs `atropos::mir::Program`). The
                    // bare-name table is shared, so *merge* the field lists
                    // instead of overwriting — field access on either shape
                    // then resolves (check-only leniency; codegen is per-crate
                    // and never sees the merged view).
                    match cx.structs.get_mut(&s.name.node.clone()) {
                        Some(existing) => {
                            for (fname, fty) in fields {
                                if !existing.fields.iter().any(|(n, _)| n == &fname) {
                                    existing.fields.push((fname, fty));
                                }
                            }
                        }
                        None => {
                            cx.structs.insert(
                                s.name.node.clone(),
                                StructInfo {
                                    fields,
                                    generics: s.generics.clone(),
                                },
                            );
                        }
                    }
                }
                tpt_moira_clotho::ast::ItemKind::Impl(imp) => {
                    let self_name = imp.for_ty.node_name();
                    cx.generic_scope.push(imp.generics.clone());
                    cx.self_type = Some(self_name.clone());
                    for m in &imp.items {
                        // Associated constants on extern impls resolve as
                        // `Type::CONST` two-segment paths too.
                        if let tpt_moira_clotho::ast::ItemKind::Const(c) = &m.kind {
                            let ty = c.ty.as_ref().map(|t| cx.type_to_ty(t)).unwrap_or(Ty::Unknown);
                            cx.assoc_consts
                                .insert((self_name.clone(), c.name.node.clone()), ty);
                            continue;
                        }
                        if let tpt_moira_clotho::ast::ItemKind::Function(f) = &m.kind {
                            // Method-level generics (`impl Cache { fn
                            // checked<F>(&self, f: F) }`) must be in scope while
                            // resolving the signature types, and recorded on the
                            // signature so call sites can recognize `F`.
                            let mut generics = imp.generics.clone();
                            generics.params.extend(f.generics.params.clone());
                            generics.where_clause.extend(f.generics.where_clause.clone());
                            cx.generic_scope.push(f.generics.clone());
                            let params =
                                f.params.iter().map(|p| cx.type_to_ty(&p.ty)).collect();
                            let ret = f
                                .ret
                                .as_ref()
                                .map(|t| cx.type_to_ty(t))
                                .unwrap_or(Ty::Unit);
                            cx.generic_scope.pop();
                            let sig = FnSig {
                                params,
                                ret,
                                is_async: f.is_async,
                                generics,
                            };
                            cx.user_methods.insert(
                                (self_name.clone(), f.name.node.clone()),
                                sig,
                            );
                        }
                    }
                    cx.self_type = None;
                    cx.generic_scope.pop();
                }
                tpt_moira_clotho::ast::ItemKind::Enum(e) => {
                    Self::register_enum_variants(e, cx);
                }
                tpt_moira_clotho::ast::ItemKind::Module(m) => {
                    if let tpt_moira_clotho::ast::ModuleKind::Inline(sub) = &m.kind {
                        let mut sub_path = path.clone();
                        sub_path.push(m.name.node.clone());
                        Self::collect_extern_items(
                            crate_name.clone(),
                            sub_path,
                            sub,
                            cx,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn check_function(&mut self, f: &tpt_moira_clotho::ast::Function) {
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
        // The type of the final statement, used to detect a diverging block
        // (one whose last statement is `return` / `?` / `break` / `continue` /
        // `loop` and which has no tail expression).
        let mut last_stmt_ty = Ty::Unit;
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
                    last_stmt_ty = self.infer_expr(e);
                }
                StmtKind::Item(item) if matches!(item.kind, tpt_moira_clotho::ast::ItemKind::Use(_)) => {
                    // A function-local `use …` declaration is a no-op in the
                    // Phase-1 subset (imports are not resolved).
                }
                StmtKind::Item(_) => {
                    self.err(Span::default(), "nested items are not supported");
                }
            }
        }
        let ty = match &block.tail {
            Some(t) => self.infer_expr(t),
            None => {
                // No tail expression: the block diverges (type `!`) if its final
                // statement diverges, mirroring Rust. This lets a `match` arm like
                // `true => { return 1; }` keep the `!` type instead of being forced
                // to `()`, so it does not constrain the `match` result type.
                if matches!(last_stmt_ty, Ty::Never) {
                    Ty::Never
                } else {
                    Ty::Unit
                }
            }
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
            // A diverging arm (`return` / `?` / `break` / `loop` / `continue`,
            // typed `!`) does not constrain the `match` result type, mirroring
            // Rust where such arms have type `!`. Without this, `match` arms that
            // early-return are wrongly forced to `()` and clash with value arms.
            if matches!(arm_ty, Ty::Never) {
                continue;
            }
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
            Pat::Variant(..) => Ok(()),
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
            Pat::Variant(path, args) => {
                // Tuple-variant pattern `Some(x)` / `Ok(v)` / `Err(e)`: bind the
                // payload names to the variant's value type. When the scrutinee is
                // a modelled `Option<T>` / `Result<T, E>` / `*Result<T, E>` we bind
                // to `T` (or `E` for `Err`); otherwise (opaque `Unknown`) to
                // `Unknown` so usage stays lenient rather than surfacing a
                // spurious "cannot find value".
                let vname = path
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                let inner_ty = Self::variant_payload_ty(scrut_ty, &vname);
                for p in args {
                    self.bind_pattern(p, &inner_ty);
                }
            }
        }
    }

    /// The value type carried by a tuple-variant pattern's payload, given the
    /// scrutinee type. For `Option<T>` / `Result<T, E>` / a `*Result<T, E>` alias
    /// we return `T`; otherwise `Unknown` (the Phase-1 subset models `Result`/
    /// `Option` opaquely, so the element is only known when the application args
    /// are concrete).
    /// The value type carried by a tuple-variant pattern's payload, given the
    /// scrutinee type and the variant name. For `Option<T>` the payload is `T`;
    /// for `Result<T, E>` the `Ok` payload is `T` and the **`Err` payload is
    /// `E`** (binding `Err(diags)` to the `Ok` type made `diags` a `()` and
    /// cascaded "no method" diagnostics).
    fn variant_payload_ty(scrut_ty: &Ty, variant: &str) -> Ty {
        match scrut_ty {
            Ty::Apply(name, args) if name == "Option" && args.len() == 1 => args[0].clone(),
            Ty::Apply(name, args)
                if (name == "Result" || name.ends_with("Result")) && args.len() == 2 =>
            {
                if variant == "Err" {
                    args[1].clone()
                } else {
                    args[0].clone()
                }
            }
            _ => Ty::Unknown,
        }
    }

    /// Type-check a closure `|params| body` (or `|params| -> Ret body`).
    ///
    /// Parameters are typed from their annotations (falling back to the `i32`
    /// default for inference), the body is checked in a scope binding each
    /// parameter, and the closure's type is the function type
    /// `(param_tys) -> ret_ty`. Capture analysis (the prerequisite for closure
    /// codegen) is performed here: any variable the body references from the
    /// enclosing environment is reported precisely, because the C/native
    /// backends do not yet lower `ExprKind::Closure` with captures.
    fn check_closure(
        &mut self,
        span: Span,
        params: &[Param],
        ret: &Option<Type>,
        body: &Expr,
    ) -> Ty {
        let mut param_names = HashSet::new();
        for p in params {
            bind_pat_names(&p.pat, &mut param_names);
        }

        // Capture analysis: names the body references that resolve to a variable
        // in the enclosing environment (and are not the closure's own parameters
        // or a locally-bound name) are the captured variables. Captures are
        // accepted at the type level (the closure's type is its signature);
        // codegen lowers them as leading parameters on the lambda-lifted
        // function, with direct calls through a bound local rewritten to pass
        // them.
        let _ = (span, &body, &param_names);

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

    /// Type-check the `?` operator: `e?` where `e` is a `Result`/`Option` (or any
    /// applied/generic type — the Phase-1 subset cannot verify the `Try` impl, so
    /// it accepts any `Apply` such as `Result<T, E>`, `Option<T>`, or a `Result`
    /// type alias like `ParseResult<T>`, as well as a bare `Struct`/`Generic`
    /// `Try` type) is accepted. On the error path the expression returns early
    /// from the enclosing function, so it diverges there; its value type is the
    /// `Ok`/`Some` payload, modelled opaquely as `<unknown>` (matching how
    /// `Some`/`Ok` constructors and field access on `Result`/`Option` are already
    /// absorbed in the subset) to avoid cascading a wall of errors through the
    /// compiler's own (not-yet-fully-resolved) `Result`-returning code. A
    /// genuinely non-`Try` operand (a primitive, tuple, array, or reference) is
    /// still rejected so `5?` / `&x?` surface as errors.
    fn check_try(&mut self, span: Span, inner: &Expr) -> Ty {
        let t = self.infer_expr(inner);
        match &t {
            Ty::Apply(..) | Ty::Struct(_) | Ty::Generic(_) | Ty::Unknown => Ty::Unknown,
            other => {
                self.err(
                    span,
                    format!(
                        "the `?` operator can only be applied to `Result` or `Option`, found `{}`",
                        other
                    ),
                );
                Ty::Unknown
            }
        }
    }

    /// Type-check `if let pat = init { then } else { else }` (and its `while let`
    /// analogue). The binding is scoped to the then-block (if) or loop body
    /// (while). Identifier patterns and tuple-variant patterns (`if let Some(x) =
    /// opt`) are supported; other patterns (literal / tuple / struct) are rejected
    /// with a precise diagnostic. For a variant pattern the payload names are bound
    /// to the variant's value type (e.g. `T` for `Option<T>` / `Result<T, E>`).
    fn infer_if_let(
        &mut self,
        local: &Local,
        then_b: &Block,
        else_b: Option<&Expr>,
        span: Span,
    ) -> Ty {
        if !matches!(local.pat, Pat::Ident(_) | Pat::Variant(..)) {
            self.err(
                span,
                "`if let`/`while let` with unsupported pattern (enum destructuring needs `match`)",
            );
            let tt = self.infer_block(then_b);
            let _ = else_b;
            return tt;
        }
        let init_ty = match &local.init {
            Some(e) => self.infer_expr(e),
            None => Ty::Unit,
        };
        // Identifier patterns bind the whole `init` value; variant patterns bind
        // their payload to the variant's element type.
        let vname = match &local.pat {
            Pat::Variant(p, _) => p.segments.last().map(|s| s.node.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        let bind_ty = if matches!(local.pat, Pat::Variant(..)) {
            Self::variant_payload_ty(&init_ty, &vname)
        } else {
            init_ty.clone()
        };
        self.env.push(HashMap::new());
        self.bind_pattern(&local.pat, &bind_ty);
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
    /// the pattern-support notes.
    fn infer_while_let(&mut self, local: &Local, body: &Block, span: Span) -> Ty {
        if !matches!(local.pat, Pat::Ident(_) | Pat::Variant(..)) {
            self.err(
                span,
                "`if let`/`while let` with unsupported pattern (enum destructuring needs `match`)",
            );
            self.infer_block(body);
            return Ty::Unit;
        }
        let init_ty = match &local.init {
            Some(e) => self.infer_expr(e),
            None => Ty::Unit,
        };
        let vname = match &local.pat {
            Pat::Variant(p, _) => p.segments.last().map(|s| s.node.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        let bind_ty = if matches!(local.pat, Pat::Variant(..)) {
            Self::variant_payload_ty(&init_ty, &vname)
        } else {
            init_ty.clone()
        };
        self.env.push(HashMap::new());
        self.bind_pattern(&local.pat, &bind_ty);
        self.infer_block(body);
        self.env.pop();
        Ty::Unit
    }

    fn infer_expr(&mut self, e: &Expr) -> Ty {
        let span = e.span;
        let ty = match &e.kind {
            ExprKind::Lit(l) => lit_ty(l),
            ExprKind::Range(..) => Ty::Range,
            ExprKind::Path(p) => {
                let name = p
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                match self.lookup(&name) {
                    Some(t) => t,
                    None => {
                        // Associated constant `Type::CONST` (e.g.
                        // `ExitCode::FAILURE`), resolved from the path's last
                        // two segments.
                        if p.segments.len() >= 2 {
                            let mut tname = p.segments[p.segments.len() - 2].node.clone();
                            // `Self::Variant` in expression position (derive-
                            // generated `match self { .. => Self::Ident, }`)
                            // resolves against the enclosing impl's self type.
                            if tname == "Self" {
                                if let Some(sty) = &self.self_type {
                                    tname = sty.clone();
                                }
                            }
                            let key = (tname.clone(), name.clone());
                            if let Some(t) = self.assoc_consts.get(&key) {
                                return t.clone();
                            }
                            // Associated function used as a *value* (e.g.
                            // `list.map(Path::new)`): resolve to its function
                            // type from the associated-function table.
                            if let Some(s) = self.assoc_fns.get(&key) {
                                return Ty::Fn(s.params.clone(), Box::new(s.ret.clone()));
                            }
                        }
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
                // `loop` diverges (type `!`) unless broken out of.
                Ty::Never
            }
            ExprKind::For(pat, iter, body) => {
                let iter_ty = self.infer_expr(iter);
                // The iterator may sit behind a reference (`for x in &vec`) —
                // Rust auto-iterates through `&`/`&mut`.
                let stripped = self.strip_refs(&iter_ty);
                // The loop variable binds to the iterator's element type: an
                // array iterator binds its element type; a range binds `i32`;
                // a modeled std collection (`Vec`, …) yields elements whose
                // concrete type is not tracked (`Unknown`). Anything else
                // (e.g. a bare `i32`) is not iterable.
                let elem_ty = match &stripped {
                    Ty::Array(elem, _) => (**elem).clone(),
                    Ty::Range => Ty::Int,
                    Ty::Struct(name) | Ty::Apply(name, _) if name == "Vec" => Ty::Unknown,
                    // An `Unknown` iterator means an earlier error already
                    // fired; absorb it instead of cascading a diagnostic.
                    Ty::Unknown | Ty::Never => Ty::Unknown,
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
                // `return` diverges (type `!`); it does not produce a value.
                Ty::Never
            }
            ExprKind::Break | ExprKind::Continue => Ty::Never,
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
            ExprKind::Macro(m) => {
                // Builtin `println!`-family macros are type-checked with real
                // (re-parsed) arguments; their result types mirror Rust.
                let name = m
                    .path
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                match name.as_str() {
                    "println" | "eprintln" | "print" | "eprint" => {
                        for a in tpt_moira_clotho::parser::Parser::parse_macro_args(
                            m.tokens.clone(),
                        ) {
                            self.infer_expr(&a);
                        }
                        Ty::Unit
                    }
                    "format" => {
                        let args =
                            tpt_moira_clotho::parser::Parser::parse_macro_args(m.tokens.clone());
                        if args.is_empty() {
                            self.err(span, "format! requires at least a format string".to_string());
                        }
                        for a in args {
                            self.infer_expr(&a);
                        }
                        Ty::Str
                    }
                    "matches" => {
                        // `matches!(expr, pattern)` — the expression is
                        // type-checked; the pattern is not analyzed structurally
                        // (the subset's `match` handles real destructuring).
                        // The result is the boolean test.
                        let args =
                            tpt_moira_clotho::parser::Parser::parse_macro_args(m.tokens.clone());
                        if args.len() != 2 {
                            self.err(
                                span,
                                "matches! expects exactly 2 arguments".to_string(),
                            );
                        }
                        for a in &args {
                            self.infer_expr(a);
                        }
                        Ty::Bool
                    }
                    "write" | "writeln" => {
                        // `write!(f, "..", args)` / `writeln!(f, ..)`: the
                        // formatter expression and value arguments are all
                        // evaluated; the result mirrors Rust (`Result<()>`,
                        // modeled as a `Result`).
                        let args =
                            tpt_moira_clotho::parser::Parser::parse_macro_args(m.tokens.clone());
                        if args.len() < 2 {
                            self.err(span, "write! requires a formatter and format string".to_string());
                        }
                        for a in &args {
                            self.infer_expr(a);
                        }
                        Ty::Apply("Result".to_string(), vec![Ty::Unit])
                    }
                    _ => {
                        self.err(
                            span,
                            format!("macro `{}` is not in the type-checked subset", name),
                        );
                        Ty::Unknown
                    }
                }
            }
            ExprKind::Try(inner) => self.check_try(span, inner),
        };
        ty
    }

    fn check_binary(&mut self, span: Span, l: &Expr, op: BinOp, r: &Expr) -> Ty {
        if matches!(op, BinOp::Assign) {
            let lt = self.infer_expr(l);
            let rt = self.infer_expr(r);
            self.expect(&rt, &lt, span);
            // Assignment is an expression of type `()` in Rust; typing it as
            // the assigned value made `match x.as_str() { _ => flag = true }`
            // arms disagree with unit-typed arms.
            return Ty::Unit;
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
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                // `&`/`|`/`^` are valid on `bool` too (non-short-circuit
                // logical operators); integers keep the integer rules.
                if !matches!(lt, Ty::Unknown) && !lt.is_integer() && !matches!(lt, Ty::Bool) {
                    self.err(
                        span,
                        "bitwise operator requires integer operands".to_string(),
                    );
                }
                if !matches!(rt, Ty::Unknown) && !rt.is_integer() && !matches!(rt, Ty::Bool) {
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
            BinOp::Shl | BinOp::Shr => {
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
                    // Deref through `Unknown` (an earlier error already
                    // fired) is absorbed silently.
                    Ty::Unknown => Ty::Unknown,
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
            // Enforce declared bounds on the inferred type arguments — both
            // inline (`<T: Show>`) and `where`-clause (`where T: Show`)
            // predicates.
            let enforce = |cx: &mut Self, concrete: &Ty, bn: &str| {
                // A user-written or derive-generated `impl Trait for T`
                // satisfies the bound even when `Trait` is also a builtin name
                // (e.g. derived `PartialEq`): consult the solver first.
                if !matches!(concrete, Ty::Generic(_) | Ty::Unknown | Ty::Never) {
                    let query_ty = if matches!(concrete, Ty::Int) {
                        Ty::I32
                    } else {
                        concrete.clone()
                    };
                    if cx.solver.as_ref().map_or(false, |s| {
                        s.implements_trait(crate::solver::TraitId::User(bn.to_string()), &query_ty)
                    }) {
                        return;
                    }
                }
                if let Some(bt) = BuiltinTrait::from_name(bn) {
                    // Modeled std types (`String`, `Option`, …) and absorbing
                    // sentinels are lenient: derives or the opaque runtime
                    // model stand in for the real impl.
                    let std_lenient = matches!(concrete, Ty::Unknown | Ty::Never)
                        || matches!(concrete,
                            Ty::Struct(n) | Ty::Apply(n, _)
                            if matches!(
                                n.as_str(),
                                "String" | "Vec" | "Option" | "Result" | "Path"
                                    | "PathBuf" | "Cow" | "TokenKind"
                            ));
                    if !std_lenient && !TraitDb::implements(bt, concrete) {
                        cx.err(
                            span,
                            format!(
                                "the trait bound `{}: {}` is not satisfied",
                                concrete,
                                bt.name()
                            ),
                        );
                    }
                } else if !matches!(concrete, Ty::Generic(_) | Ty::Unknown | Ty::Never) {
                    // User-defined trait bound: consult the solver's `impl`
                    // clauses (e.g. `impl Show for i32`). Integer literals
                    // infer as `Ty::Int` (the i32 default), so normalize to
                    // `Ty::I32` for the query.
                    let query_ty = if matches!(concrete, Ty::Int) {
                        Ty::I32
                    } else {
                        concrete.clone()
                    };
                    let holds = cx.solver.as_ref().map_or(false, |s| {
                        s.implements_trait(crate::solver::TraitId::User(bn.to_string()), &query_ty)
                    });
                    if !holds {
                        cx.err(
                            span,
                            format!("the trait bound `{}: {}` is not satisfied", concrete, bn),
                        );
                    }
                }
            };
            for p in &sig.generics.params {
                if let Some(concrete) = sub.get(&p.name.node) {
                    for bound in &p.bounds {
                        let bn = bound_trait_name_str(bound);
                        enforce(self, concrete, &bn);
                    }
                }
            }
            // `where` predicates: `where T: Bound` (or any path type naming a
            // parameter with an inferred concrete type).
            for pred in &sig.generics.where_clause {
                if let tpt_moira_clotho::ast::TypeKind::Path(p, _) = &pred.ty.kind {
                    let pname = p.segments.last().map(|s| s.node.clone()).unwrap_or_default();
                    if let Some(concrete) = sub.get(&pname) {
                        for bound in &pred.bounds {
                            let bn = bound_trait_name_str(bound);
                            enforce(self, concrete, &bn);
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

    fn check_field(&mut self, span: Span, base: &Expr, field: &tpt_moira_clotho::ast::Spanned<String>) -> Ty {
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
                // An `Unknown` base means an earlier error already fired;
                // absorb the field access instead of cascading diagnostics.
                if matches!(bt, Ty::Unknown) {
                    return Ty::Unknown;
                }
                self.err(span, format!("type {} has no fields", bt));
                Ty::Unknown
            }
        }
    }

    fn check_struct(
        &mut self,
        span: Span,
        path: &Path,
        fields: &[(tpt_moira_clotho::ast::Spanned<String>, Expr)],
        base: Option<&Expr>,
    ) -> Ty {
        let mut name = path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default();
        // `Self { .. }` struct literals inside an `impl` block resolve against
        // the impl's self type (derive-generated `Clone` uses this shape).
        if name == "Self" {
            if let Some(sty) = &self.self_type {
                name = sty.clone();
            }
        }
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
    /// up an associated function (prelude, then inherent impls). Longer paths
    /// (`std::env::args`) resolve against their *last two* segments — module
    /// prefixes are not modeled, so `std::path::Path::new` resolves the same
    /// as `Path::new`.
    fn resolve_call(&mut self, segments: &[String], span: Span) -> Option<FnSig> {
        match segments.len() {
            1 => self.fns.get(&segments[0]).cloned().or_else(|| {
                // A local bound to a closure (`let f = |x| ...; f(2)`) is
                // callable with the closure's own signature.
                let ty = self.lookup(&segments[0])?;
                if let Ty::Fn(params, ret) = ty {
                    Some(FnSig {
                        params,
                        ret: (*ret).clone(),
                        is_async: false,
                        generics: Generics::default(),
                    })
                } else {
                    None
                }
            }),
            2 => self
                .assoc_fns
                .get(&(segments[0].clone(), segments[1].clone()))
                .cloned()
                .or_else(|| {
                    // Inherent methods on user types: `Type::method(...)` (e.g.
                    // `Arena::default()` from an `impl Arena<T>` block).
                    self.user_methods
                        .get(&(segments[0].clone(), segments[1].clone()))
                        .cloned()
                })
                .or_else(|| self.fns.get(&segments[1]).cloned()),
            n if n > 2 => {
                // Try the last two segments (`module::item`), then the
                // crate-qualified form (`crate::item`, matching how extern
                // crates register their flattened items), then the bare name.
                let t = &segments[n - 2];
                let m = &segments[n - 1];
                self.assoc_fns
                    .get(&(t.clone(), m.clone()))
                    .cloned()
                    .or_else(|| self.user_methods.get(&(t.clone(), m.clone())).cloned())
                    .or_else(|| {
                        self.assoc_fns
                            .get(&(segments[0].clone(), m.clone()))
                            .cloned()
                    })
                    .or_else(|| self.fns.get(m).cloned())
            }
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
        method: &tpt_moira_clotho::ast::Spanned<String>,
        args: &[Expr],
    ) -> Ty {
        let recv_ty = self.infer_expr(recv);
        // An `Unknown` receiver (possibly behind refs — an earlier error
        // already fired) absorbs this call silently instead of cascading a
        // wall of "no method" diagnostics.
        if matches!(recv_ty, Ty::Unknown) || matches!(self.strip_refs(&recv_ty), Ty::Unknown) {
            for a in args {
                self.infer_expr(a);
            }
            return Ty::Unknown;
        }
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
                // 3) Trait-bound method resolution through the Chalk-like
                //    solver: the receiver may be a generic parameter bounded
                //    by a user trait (`T: Show`) or a concrete type an `impl`
                //    covers (`impl Show for i32`).
                if let Some(ty) =
                    self.resolve_trait_method(span, &recv_ty, &method.node.clone(), args)
                {
                    return ty;
                }
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

    /// Trait-bound method resolution backed by the Chalk-like solver.
    ///
    /// Called when `receiver.method(args)` matched no inherent method. Two
    /// resolution paths:
    ///
    /// * **Generic receiver** (`Ty::Generic("T")`): the generic parameter's
    ///   declared bounds (`<T: Show>` or `where T: Show`) name candidate
    ///   traits; a signature from an `impl Trait for _ { fn method .. }` block
    ///   is used to check the call. No impl search is needed — the bound is
    ///   discharged where the generic is instantiated.
    /// * **Concrete receiver**: every trait that declares this method is a
    ///   candidate; the solver's impl search (`Solver::implements_trait`)
    ///   decides which ones actually apply to the receiver's type. A single
    ///   applicable trait resolves the call; several are resolved in sorted
    ///   (deterministic) order of the first match; none falls back to the
    ///   caller's "no method" diagnostic.
    ///
    /// Returns `Some(ret_ty)` and checks the arguments when resolution
    /// succeeds, `None` otherwise.
    fn resolve_trait_method(
        &mut self,
        span: Span,
        recv_ty: &Ty,
        method: &str,
        args: &[Expr],
    ) -> Option<Ty> {

        if let Ty::Generic(pname) = recv_ty {
            for tr in self.generic_bounds_of(pname) {
                if let Some(sig) =
                    self.trait_methods.get(&(tr.clone(), method.to_string())).cloned()
                {
                    return Some(self.check_user_method(span, recv_ty, &sig, args));
                }
            }
            return None;
        }
        // Concrete receiver: collect candidate traits declaring this method,
        // then let the solver pick the applicable ones. Integer literals
        // infer as `Ty::Int` (the i32 default), so normalize to `Ty::I32`
        // for the solver's impl search (mirroring the call-site bound check).
        let query_ty = if matches!(recv_ty, Ty::Int) {
            Ty::I32
        } else {
            recv_ty.clone()
        };
        let mut candidates: Vec<String> = self
            .trait_methods
            .keys()
            .filter(|(_, m)| m == method)
            .cloned()
            .map(|(t, _)| t)
            .collect();
        candidates.sort();
        candidates.dedup();
        let mut matches: Vec<(String, FnSig)> = Vec::new();
        for tr in candidates {
            let implemented = self
                .solver
                .as_ref()
                .map(|s| {
                    s.implements_trait(crate::solver::TraitId::User(tr.clone()), &query_ty)
                })
                .unwrap_or(false);
            if implemented {
                if let Some(sig) =
                    self.trait_methods.get(&(tr.clone(), method.to_string()))
                {
                    matches.push((tr, sig.clone()));
                }
            }
        }
        // Deterministic pick: sorted by trait name (the candidate list was
        // already sorted, so `matches` is in that order). The normalized
        // query type is used for the receiver check so an integer-literal
        // receiver (`Ty::Int`) matches an `&self` of `i32`.
        let (_, sig) = matches.into_iter().next()?;
        Some(self.check_user_method(span, &query_ty, &sig, args))
    }

    /// The user-trait bounds declared on generic parameter `name`, either
    /// inline (`<T: Show>`) or in a `where` clause (`where T: Show`), searched
    /// innermost-scope-first across the active generic scopes.
    fn generic_bounds_of(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push_bound = |out: &mut Vec<String>, b: &TypeBound| {
            let TypeBound::Trait(path) = b;
            let seg = path
                .segments
                .last()
                .map(|s| s.node.clone())
                .unwrap_or_default();
            if !out.contains(&seg) {
                out.push(seg);
            }
        };
        for g in self.generic_scope.iter().rev() {
            for p in &g.params {
                if p.name.node == name {
                    for b in &p.bounds {
                        push_bound(&mut out, b);
                    }
                }
            }
            for w in &g.where_clause {
                if w.ty.node_name() == name {
                    for b in &w.bounds {
                        push_bound(&mut out, b);
                    }
                }
            }
        }
        out
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
        // Names of this signature's own generic parameters. Depending on how
        // the signature was collected they appear as `Ty::Generic("F")` or as
        // an unresolved single-word struct path (`Ty::Struct("F")`); either
        // way the call site cannot verify them without full site inference,
        // so accept them (bounds were discharged where the generic was
        // instantiated).
        let generic_names: Vec<String> = sig
            .generics
            .params
            .iter()
            .map(|p| p.name.node.clone())
            .collect();
        let is_generic_ty = |pt: &Ty| match pt {
            Ty::Generic(_) => true,
            Ty::Struct(n) | Ty::Apply(n, _) => generic_names.contains(n),
            _ => false,
        };
        for (a, pt) in args.iter().zip(rest.iter()) {
            let at = self.infer_expr(a);
            if is_generic_ty(pt) {
                continue;
            }
            self.expect(&at, pt, a.span);
        }
        // A signature whose return type *is* one of its generic parameters
        // (`-> F`) yields whatever the corresponding argument produced; model
        // that opaquely rather than leaking the parameter name.
        if is_generic_ty(&sig.ret) {
            return Ty::Unknown;
        }
        sig.ret.clone()
    }

    /// The base type name of a value type, following `&`/`&mut` to its referent
    /// so methods/associated functions on the referent can be found. Primitive
    /// types map to their source-level names (`i32`, `str`, …) so prelude
    /// methods and `impl Primitive` inherent methods resolve on them too.
    fn type_name(&self, ty: &Ty) -> Option<String> {
        match ty {
            Ty::Struct(n) | Ty::Apply(n, _) => Some(n.clone()),
            Ty::Ref(inner) | Ty::RefMut(inner) => self.type_name(inner),
            // Primitives (Display prints the source-level name).
            t if matches!(
                t,
                Ty::Int
                    | Ty::I8
                    | Ty::I16
                    | Ty::I32
                    | Ty::I64
                    | Ty::I128
                    | Ty::Isize
                    | Ty::U8
                    | Ty::U16
                    | Ty::U32
                    | Ty::U64
                    | Ty::U128
                    | Ty::Usize
                    | Ty::F32
                    | Ty::F64
                    | Ty::Bool
                    | Ty::Char
                    | Ty::Str
            ) =>
            {
                Some(format!("{}", t))
            }
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
        self.assoc_fns.insert(
            ("RefCell".into(), "new".into()),
            sig(vec![Unknown], std("RefCell")),
        );

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
        // Methods on `str` (string-slice receivers, incl. literals auto-ref'd
        // to `&str`).
        self.methods
            .insert(("str".into(), "len".into()), sig(vec![], Usize));
        self.methods
            .insert(("str".into(), "is_empty".into()), sig(vec![], Bool));
        self.methods
            .insert(("str".into(), "to_string".into()), sig(vec![], std("String")));
        // Methods on `RefCell<T>` (interior mutability, as used by the
        // compiler's own arena in `moira-alloc`).
        self.methods.insert(
            ("RefCell".into(), "borrow".into()),
            sig(vec![], Ref(Box::new(Unknown))),
        );
        self.methods.insert(
            ("RefCell".into(), "borrow_mut".into()),
            sig(vec![], RefMut(Box::new(Unknown))),
        );
        self.methods
            .insert(("Vec".into(), "last".into()), sig(vec![], std("Option")));
        self.methods
            .insert(("Vec".into(), "first".into()), sig(vec![], std("Option")));
        self.methods
            .insert(("Vec".into(), "contains".into()), sig(vec![Unknown], Bool));
        self.methods
            .insert(("Vec".into(), "iter".into()), sig(vec![], Unknown));
        self.methods.insert(
            ("Vec".into(), "join".into()),
            sig(vec![Ref(Box::new(Str))], Str),
        );
        self.methods.insert(
            ("PathBuf".into(), "join".into()),
            sig(vec![Ref(Box::new(Str))], std("PathBuf")),
        );
        self.methods
            .insert(("Path".into(), "join".into()), sig(vec![Ref(Box::new(Str))], std("PathBuf")));

        // `to_string` on the numeric/primitive types (via the standard
        // `ToString` trait, modeled per receiver type name), plus the
        // `saturating_*` arithmetic helpers the compiler's own code uses.
        for prim in [
            "i32", "Int", "i64", "I64", "u8", "U8", "u32", "U32", "usize",
            "Usize", "f64", "F64", "u128", "U128", "i128", "I128",
        ] {
            self.methods.insert(
                (prim.to_string(), "to_string".into()),
                sig(vec![], std("String")),
            );
            for op in ["saturating_add", "saturating_sub", "saturating_mul"] {
                self.methods
                    .insert((prim.to_string(), op.into()), sig(vec![Unknown], Unknown));
            }
        }

        // Common `std` associated functions / constants referenced by the
        // compiler's own driver. Module prefixes are not modeled: resolution
        // uses a path's last two segments.
        self.assoc_consts.insert(
            ("ExitCode".into(), "SUCCESS".into()),
            Struct("ExitCode".to_string()),
        );
        self.assoc_consts.insert(
            ("ExitCode".into(), "FAILURE".into()),
            Struct("ExitCode".to_string()),
        );
        self.assoc_fns.insert(
            ("env".into(), "args".into()),
            sig(vec![], Apply("Args".to_string(), vec![])),
        );
        self.assoc_fns.insert(
            ("fs".into(), "read_to_string".into()),
            sig(vec![Ref(Box::new(Str))], Apply("Result".to_string(), vec![Str])),
        );
        self.assoc_fns.insert(
            // Real `Path::new` returns `&Path`.
            ("Path".into(), "new".into()),
            sig(vec![Ref(Box::new(Str))], Ref(Box::new(std("Path")))),
        );
        self.assoc_fns.insert(
            ("io".into(), "stdin".into()),
            sig(vec![], std("Stdin")),
        );
        self.assoc_fns.insert(
            ("Command".into(), "new".into()),
            sig(vec![Ref(Box::new(Str))], std("Command")),
        );
        // Builder-style `Command` methods and process/file surface used by the
        // driver (modeled loosely: builder methods return the type back,
        // terminal ones return modeled results).
        self.methods
            .insert(("Command".into(), "args".into()), sig(vec![Unknown], std("Command")));
        self.methods
            .insert(("Command".into(), "arg".into()), sig(vec![Str], std("Command")));
        self.methods
            .insert(("Command".into(), "status".into()), sig(vec![], std("ExitStatus")));
        self.methods
            .insert(("Command".into(), "output".into()), sig(vec![], std("Output")));
        self.methods.insert(
            ("Option".into(), "map".into()),
            sig(vec![Unknown], std("Option")),
        );
        self.methods.insert(
            ("Option".into(), "unwrap_or".into()),
            sig(vec![Unknown], Unknown),
        );
        self.methods.insert(
            ("Option".into(), "unwrap_or_else".into()),
            sig(vec![Unknown], Unknown),
        );
        self.methods.insert(
            ("Option".into(), "as_deref".into()),
            sig(vec![], Unknown),
        );
        self.assoc_fns
            .insert(("Stdio".into(), "piped".into()), sig(vec![], std("Stdio")));
        self.assoc_fns.insert(
            ("fs".into(), "temp_dir".into()),
            sig(vec![], std("PathBuf")),
        );
        self.assoc_fns.insert(
            ("env".into(), "temp_dir".into()),
            sig(vec![], std("PathBuf")),
        );
        self.assoc_fns.insert(
            ("env".into(), "current_dir".into()),
            sig(vec![], Apply("Result".to_string(), vec![std("PathBuf")])),
        );
        self.assoc_fns.insert(
            ("fs".into(), "remove_file".into()),
            sig(vec![Ref(Box::new(std("Path")))], Apply("Result".to_string(), vec![Unit])),
        );
        self.assoc_fns.insert(
            ("fs".into(), "open".into()),
            sig(vec![Ref(Box::new(std("Path")))], Apply("Result".to_string(), vec![std("File")])),
        );
        self.assoc_fns.insert(
            ("fs".into(), "write".into()),
            sig(
                vec![Ref(Box::new(std("Path"))), Ref(Box::new(Str))],
                Apply("Result".to_string(), vec![Unit]),
            ),
        );
        self.assoc_fns.insert(
            ("File".into(), "open".into()),
            sig(vec![Ref(Box::new(std("Path")))], Apply("Result".to_string(), vec![std("File")])),
        );
        self.assoc_fns.insert(
            ("Vec".into(), "from_utf8_lossy".into()),
            sig(vec![Ref(Box::new(std("Vec")))], std("Cow")),
        );
        self.assoc_fns.insert(
            ("String".into(), "from_utf8_lossy".into()),
            sig(vec![Ref(Box::new(std("Vec")))], std("Cow")),
        );
        // `str::split_once` yields `Option<(String, String)>`.
        self.methods.insert(
            ("str".into(), "split_once".into()),
            sig(
                vec![Str],
                Apply(
                    "Option".to_string(),
                    vec![Ty::Tuple(vec![Str.clone(), Str])],
                ),
            ),
        );
        self.methods.insert(
            ("Path".into(), "to_path_buf".into()),
            sig(vec![], std("PathBuf")),
        );
        self.methods.insert(
            ("PathBuf".into(), "to_path_buf".into()),
            sig(vec![], std("PathBuf")),
        );
        // --- Round: driver self-host gap-closing (2026-08-22) ----------------
        // `Result` combinators used by the compiler's own driver (closures are
        // passed as opaque `Unknown` arguments).
        self.methods.insert(
            ("Result".into(), "and_then".into()),
            sig(vec![Unknown], std("Result")),
        );
        self.methods.insert(
            ("Result".into(), "map_err".into()),
            sig(vec![Unknown], std("Result")),
        );
        self.methods
            .insert(("Result".into(), "map".into()), sig(vec![Unknown], Unknown));
        self.methods
            .insert(("Result".into(), "ok".into()), sig(vec![], std("Option")));
        // Iterator-adapter surface on `Args` (`env::args().skip(1)`), modeled
        // loosely: adapters return the same modeled type.
        self.methods
            .insert(("Args".into(), "skip".into()), sig(vec![Usize], std("Args")));
        self.methods
            .insert(("Args".into(), "collect".into()), sig(vec![], std("Vec")));
        self.methods
            .insert(("Vec".into(), "filter".into()), sig(vec![Unknown], std("Vec")));
        self.methods
            .insert(("Vec".into(), "map".into()), sig(vec![Unknown], std("Vec")));
        self.methods
            .insert(("Vec".into(), "collect".into()), sig(vec![], std("Vec")));
        self.methods.insert(
            ("String".into(), "contains".into()),
            sig(vec![Ref(Box::new(Str))], Bool),
        );
        self.methods.insert(
            ("str".into(), "contains".into()),
            sig(vec![Ref(Box::new(Str))], Bool),
        );
        // String / byte-slice surface.
        self.methods
            .insert(("String".into(), "as_str".into()), sig(vec![], Str));
        self.methods
            .insert(("String".into(), "is_empty".into()), sig(vec![], Bool));
        self.methods
            .insert(("str".into(), "as_bytes".into()), sig(vec![], std("Vec")));
        self.methods
            .insert(("str".into(), "chars".into()), sig(vec![], Unknown));
        self.methods
            .insert(("Cow".into(), "trim".into()), sig(vec![], Str));
        self.methods
            .insert(("Cow".into(), "lines".into()), sig(vec![], std("Vec")));
        self.methods.insert(
            ("Cow".into(), "to_string".into()),
            sig(vec![], std("String")),
        );
        // Path display / lossy conversion.
        self.methods.insert(
            ("PathBuf".into(), "to_string_lossy".into()),
            sig(vec![], std("Cow")),
        );
        self.methods.insert(
            ("Path".into(), "to_string_lossy".into()),
            sig(vec![], std("Cow")),
        );
        self.methods
            .insert(("Path".into(), "display".into()), sig(vec![], Unknown));
        self.methods
            .insert(("PathBuf".into(), "display".into()), sig(vec![], Unknown));
        // Process surface: `Command::stdin/stdout/stderr/spawn`, `Child::wait`.
        self.methods.insert(
            ("Command".into(), "stdin".into()),
            sig(vec![std("Stdio")], std("Command")),
        );
        self.methods.insert(
            ("Command".into(), "stdout".into()),
            sig(vec![std("Stdio")], std("Command")),
        );
        self.methods.insert(
            ("Command".into(), "stderr".into()),
            sig(vec![std("Stdio")], std("Command")),
        );
        self.methods.insert(
            ("Command".into(), "spawn".into()),
            sig(
                vec![],
                Apply("Result".to_string(), vec![std("Child")]),
            ),
        );
        self.methods.insert(
            ("Child".into(), "wait".into()),
            sig(
                vec![],
                Apply("Result".to_string(), vec![std("ExitStatus")]),
            ),
        );
        self.methods.insert(
            ("ExitStatus".into(), "success".into()),
            sig(vec![], Bool),
        );
        // Associated functions referenced by the driver.
        self.assoc_fns
            .insert(("Stdio".into(), "null".into()), sig(vec![], std("Stdio")));
        self.assoc_fns.insert(
            ("Stdio".into(), "inherit".into()),
            sig(vec![], std("Stdio")),
        );
        self.assoc_fns
            .insert(("PathBuf".into(), "new".into()), sig(vec![], std("PathBuf")));
        self.assoc_fns.insert(
            ("PathBuf".into(), "from".into()),
            sig(vec![Ref(Box::new(Str))], std("PathBuf")),
        );
        self.assoc_fns.insert(
            ("String".into(), "from".into()),
            sig(vec![Ref(Box::new(Str))], std("String")),
        );
        self.assoc_fns
            .insert(("Vec".into(), "from".into()), sig(vec![Unknown], std("Vec")));
        self.assoc_fns.insert(
            ("Vec".into(), "with_capacity".into()),
            sig(vec![Usize], std("Vec")),
        );
        self.assoc_fns.insert(
            ("mem".into(), "take".into()),
            sig(vec![RefMut(Box::new(Unknown))], Unknown),
        );
        self.assoc_fns.insert(
            ("env".into(), "var".into()),
            sig(
                vec![Ref(Box::new(Str))],
                Apply("Result".to_string(), vec![Str]),
            ),
        );
        self.assoc_fns.insert(
            ("process".into(), "id".into()),
            sig(vec![], Usize),
        );
        self.methods.insert(
            ("Stdin".into(), "read_to_string".into()),
            sig(
                vec![RefMut(Box::new(std("String")))],
                Apply("Result".to_string(), vec![Usize]),
            ),
        );
        self.fns.insert(
            "write".to_string(),
            FnSig {
                params: vec![Unknown, Ref(Box::new(Str))],
                ret: Ty::Unit,
                is_async: false,
                generics: Generics::default(),
            },
        );
        // Modeled map/set surface (`HashMap`/`HashSet` are opaque in the
        // subset — values flow through as `Unknown`).
        self.assoc_fns.insert(
            ("HashMap".into(), "new".into()),
            sig(vec![], std("HashMap")),
        );
        self.assoc_fns.insert(
            ("HashSet".into(), "new".into()),
            sig(vec![], std("HashSet")),
        );
        for m in ["HashMap", "HashSet"] {
            self.methods
                .insert((m.into(), "insert".into()), sig(vec![Unknown, Unknown], Unit));
            self.methods
                .insert((m.into(), "get".into()), sig(vec![Ref(Box::new(Unknown))], std("Option")));
            self.methods.insert(
                (m.into(), "contains_key".into()),
                sig(vec![Ref(Box::new(Unknown))], Bool),
            );
            self.methods
                .insert((m.into(), "remove".into()), sig(vec![Ref(Box::new(Unknown))], std("Option")));
            self.methods
                .insert((m.into(), "len".into()), sig(vec![], Usize));
            self.methods
                .insert((m.into(), "is_empty".into()), sig(vec![], Bool));
            self.methods
                .insert((m.into(), "clone".into()), sig(vec![], Unknown));
            self.methods
                .insert((m.into(), "iter".into()), sig(vec![], Unknown));
            self.methods.insert(
                (m.into(), "entry".into()),
                sig(vec![Unknown], Unknown),
            );
        }
        self.methods.insert(
            ("HashMap".into(), "values".into()),
            sig(vec![], Unknown),
        );
        self.methods.insert(
            ("HashMap".into(), "keys".into()),
            sig(vec![], Unknown),
        );
        // `clone()` on the other modeled std containers.
        for m in ["Vec", "String", "PathBuf", "Cow"] {
            self.methods
                .insert((m.into(), "clone".into()), sig(vec![], std(m)));
        }
        // `Default::default()` and per-type `default()`.
        self.assoc_fns.insert(
            ("Default".into(), "default".into()),
            sig(vec![], Unknown),
        );
        // `Vec::pop` yields `Option<T>` (modeled opaquely).
        self.methods
            .insert(("Vec".into(), "pop".into()), sig(vec![], std("Option")));

        // Register the modeled `std` types as (field-less) structs so `let v:
        // Vec<u8> = ...`, method receivers, and struct-typed expressions
        // resolve instead of erroring with "unknown struct".
        for std_type in [
            "Vec", "String", "Path", "PathBuf", "File", "Command", "ExitCode",
            "ExitStatus", "Output", "Stdio", "Stdin", "Child", "Cow", "Args",
            "HashMap", "HashSet",
        ] {
            self.structs.insert(
                std_type.to_string(),
                StructInfo {
                    fields: Vec::new(),
                    generics: Generics::default(),
                },
            );
        }

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
        // Modeled std types (`String`, `Option`, …) and absorbing sentinels
        // are lenient — derives or the opaque runtime model stand in for the
        // real impl — and a user-written / derive-generated `impl Trait for T`
        // (seeded into the solver) satisfies the operator too.
        let std_lenient = matches!(ty, Ty::Unknown | Ty::Never)
            || matches!(ty,
                Ty::Struct(n) | Ty::Apply(n, _)
                if matches!(
                    n.as_str(),
                    "String" | "Vec" | "Option" | "Result" | "Path" | "PathBuf"
                        | "Cow" | "TokenKind"
                ));
        let solver_ok = match ty {
            Ty::Generic(_) | Ty::Unknown | Ty::Never => false,
            t => {
                let query_ty = if matches!(t, Ty::Int) {
                    Ty::I32
                } else {
                    t.clone()
                };
                self.solver.as_ref().map_or(false, |s| {
                    s.implements_trait(
                        crate::solver::TraitId::User(tr.name().to_string()),
                        &query_ty,
                    )
                })
            }
        };
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
            _ => std_lenient || solver_ok || TraitDb::implements(tr, ty),
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
                // A primitive self type (`impl Show for i32`) resolves to the
                // primitive `Ty` (not a struct named "i32") so receiver checks
                // against e.g. `&self: &i32` compare equal types.
                if name == "Self" {
                    return match &self.self_type {
                        Some(s) => prim(s).unwrap_or_else(|| Ty::Struct(s.clone())),
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
                if elems.is_empty() {
                    // `()` is the unit type, not a zero-tuple.
                    Ty::Unit
                } else {
                    Ty::Tuple(elems.iter().map(|e| self.type_to_ty(e)).collect())
                }
            }
            TypeKind::Infer => Ty::Unknown,
        }
    }

    /// Map a Clotho type to a Lachesis type. Exposed (mirroring the method form)
    /// so other subsystems — e.g. the trait solver — can translate `impl` self
    /// types without depending on `TypeChecker` internals.
    pub fn ast_to_ty(ty: &tpt_moira_clotho::ast::Type) -> Ty {
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
            TypeKind::Infer => Ty::Unknown,
            _ => Ty::Unknown,
        }
    }

    /// Whether two types are mutually compatible for assignment/use.
    fn compatible(&self, a: &Ty, b: &Ty) -> bool {
        if matches!(a, Ty::Unknown | Ty::Never) || matches!(b, Ty::Unknown | Ty::Never) {
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
        // A string literal (`Ty::Str`) auto-coerces to `&str`, mirroring
        // Rust's `fn f(s: &str)` accepting `f("hi")`.
        if matches!(a, Ty::Str) && matches!(b, Ty::Ref(y) if matches!(**y, Ty::Str)) {
            return true;
        }
        // Deref coercions (Rust's `Deref`-driven argument coercions for the
        // modeled std types):
        //   `String`      -> `&str` / `&String` -> `&str`
        //   `PathBuf`     -> `&Path` / `&PathBuf` -> `&Path`
        //   `Vec<T>`      -> `&[T]` / `&Vec<T>` -> `&[T]`
        //   `Cow<str>`    -> `&str`
        if let Ty::Ref(expected) = b {
            let expected: &Ty = expected;
            let coerces = match a {
                Ty::Struct(n) => {
                    matches!(n.as_str(), "String" | "PathBuf" | "Cow")
                        && (matches!(expected, Ty::Str)
                            || matches!(expected, Ty::Struct(p) if p == "Path"))
                }
                // `&Cow<str>` coerces to `&str` like the owned forms above.
                Ty::Ref(inner) => match inner.as_ref() {
                    Ty::Struct(n) => {
                        n == "String" && matches!(expected, Ty::Str)
                            || n == "Cow" && matches!(expected, Ty::Str)
                            || n == "PathBuf"
                                && matches!(expected, Ty::Struct(p) if p == "Path")
                    }
                    _ => false,
                },
                _ => false,
            };
            if coerces {
                return true;
            }
        }
        // `&Vec<T>` / `Vec<T>` coerce to array-slice references `&[T]`.
        // A `&Cow<str>` / `&String` also coerces to a bare `str` position
        // (array elements, let annotations) — in either argument order.
        if let Ty::Ref(x) = a {
            if matches!(x.as_ref(), Ty::Struct(n) if n == "Cow" || n == "String")
                && matches!(b, Ty::Str)
            {
                return true;
            }
        }
        if let Ty::Ref(y) = b {
            if matches!(y.as_ref(), Ty::Struct(n) if n == "Cow" || n == "String")
                && matches!(a, Ty::Str)
            {
                return true;
            }
        }
        // (A modeled `Vec` may appear as `Struct("Vec")` or `Apply("Vec", ..)`.)
        let vec_a = matches!(a, Ty::Struct(n) if n == "Vec")
            || matches!(a, Ty::Apply(n, _) if n == "Vec");
        let vec_ref_a = matches!(a, Ty::Ref(x) | Ty::RefMut(x)
            if matches!(x.as_ref(), Ty::Struct(n) if n == "Vec")
                || matches!(x.as_ref(), Ty::Apply(n, _) if n == "Vec"));
        if vec_a && matches!(b, Ty::Array(..)) {
            return true;
        }
        if vec_ref_a && matches!(b, Ty::Ref(y) | Ty::RefMut(y)
            if matches!(y.as_ref(), Ty::Array(..)))
        {
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
        Pat::Path(path) | Pat::Variant(path, _) => path
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_else(|| "_".to_string()),
        _ => "_".to_string(),
    }
}

/// Add the variable names bound by `pat` to `bound`.
///
/// Only patterns that *introduce* a new binding are recorded here (identifiers,
/// tuple/reference/`@` patterns, and the inner patterns of `or`/range patterns).
/// Variant/struct patterns bind names via the matched value rather than a fresh
/// local and are intentionally ignored for capture detection.
fn bind_pat_names(pat: &Pat, bound: &mut HashSet<String>) {
    match pat {
        Pat::Ident(n) => {
            bound.insert(n.node.clone());
        }
        Pat::Wild => {}
        Pat::Lit(_) => {}
        Pat::Tuple(ps) => {
            for p in ps {
                bind_pat_names(p, bound);
            }
        }
        Pat::Path(_) => {}
        Pat::Variant(_, args) => {
            for p in args {
                bind_pat_names(p, bound);
            }
        }
        Pat::Ref(p) => bind_pat_names(p, bound),
        Pat::Or(a, b) => {
            bind_pat_names(a, bound);
            bind_pat_names(b, bound);
        }
        Pat::Struct(_, fields) => {
            for (_, p) in fields {
                bind_pat_names(p, bound);
            }
        }
        Pat::Range(a, b, _) => {
            if let Some(p) = a {
                bind_pat_names(p, bound);
            }
            if let Some(p) = b {
                bind_pat_names(p, bound);
            }
        }
        Pat::Bind(n, sub) => {
            bound.insert(n.clone());
            bind_pat_names(sub, bound);
        }
    }
}

/// Collect the variable names referenced by `expr` that are free with respect to
/// `bound` (the names bound *inside* `expr` itself, e.g. the closure's own
/// parameters and any `let`/pattern bindings in its body).
///
/// The result is deduplicated (first-use order preserved) and is the set of
/// names a closure would need to capture. It is the prerequisite for closure
/// codegen: it identifies exactly which enclosing variables escape into the
/// closure body.
#[allow(dead_code)] // capture analysis retained for future borrow-check integration
fn closure_free_vars(expr: &Expr, bound: &HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    collect_free_vars(expr, bound, &mut out, &mut seen);
    out
}

#[allow(dead_code)] // retained: general free-variable walk for future passes
fn collect_free_vars(
    expr: &Expr,
    bound: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match &expr.kind {
        ExprKind::Path(p) => {
            // Only a single-segment path can name a local variable; multi-segment
            // paths are associated functions / methods / type constructors.
            if p.segments.len() == 1 {
                let n = p.segments[0].node.clone();
                if !bound.contains(&n) && seen.insert(n.clone()) {
                    out.push(n);
                }
            }
        }
        ExprKind::Lit(_) => {}
        ExprKind::Binary(l, _, r) => {
            collect_free_vars(l, bound, out, seen);
            collect_free_vars(r, bound, out, seen);
        }
        ExprKind::Unary(_, e) => collect_free_vars(e, bound, out, seen),
        ExprKind::Call(f, args) => {
            collect_free_vars(f, bound, out, seen);
            for a in args {
                collect_free_vars(a, bound, out, seen);
            }
        }
        ExprKind::MethodCall(r, _, args) => {
            collect_free_vars(r, bound, out, seen);
            for a in args {
                collect_free_vars(a, bound, out, seen);
            }
        }
        ExprKind::Field(e, _) => collect_free_vars(e, bound, out, seen),
        ExprKind::Index(b, i) => {
            collect_free_vars(b, bound, out, seen);
            collect_free_vars(i, bound, out, seen);
        }
        ExprKind::Ref(e, _) => collect_free_vars(e, bound, out, seen),
        ExprKind::Deref(e) => collect_free_vars(e, bound, out, seen),
        ExprKind::Array(es) => {
            for e in es {
                collect_free_vars(e, bound, out, seen);
            }
        }
        ExprKind::Struct(_, fields, base) => {
            for (_, e) in fields {
                collect_free_vars(e, bound, out, seen);
            }
            if let Some(b) = base {
                collect_free_vars(b, bound, out, seen);
            }
        }
        ExprKind::Tuple(es) => {
            for e in es {
                collect_free_vars(e, bound, out, seen);
            }
        }
        ExprKind::Block(b) => collect_free_vars_block(b, bound, out, seen),
        ExprKind::If(c, tb, eb) => {
            collect_free_vars(c, bound, out, seen);
            collect_free_vars_block(tb, bound, out, seen);
            if let Some(e) = eb {
                collect_free_vars(e, bound, out, seen);
            }
        }
        ExprKind::While(c, b) => {
            collect_free_vars(c, bound, out, seen);
            collect_free_vars_block(b, bound, out, seen);
        }
        ExprKind::Loop(b) => collect_free_vars_block(b, bound, out, seen),
        ExprKind::For(p, e, b) => {
            collect_free_vars(e, bound, out, seen);
            let mut b2 = bound.clone();
            bind_pat_names(p, &mut b2);
            collect_free_vars_block(b, &b2, out, seen);
        }
        ExprKind::Return(e) => {
            if let Some(e) = e {
                collect_free_vars(e, bound, out, seen);
            }
        }
        ExprKind::Break | ExprKind::Continue => {}
        ExprKind::Match(s, arms) => {
            collect_free_vars(s, bound, out, seen);
            for (p, e) in arms {
                let mut b2 = bound.clone();
                bind_pat_names(p, &mut b2);
                collect_free_vars(e, &b2, out, seen);
            }
        }
        ExprKind::Await(e) => collect_free_vars(e, bound, out, seen),
        ExprKind::Unsafe(b) => collect_free_vars_block(b, bound, out, seen),
        ExprKind::Closure { params, body, .. } => {
            let mut b2 = bound.clone();
            for p in params {
                bind_pat_names(&p.pat, &mut b2);
            }
            collect_free_vars(body, &b2, out, seen);
        }
        ExprKind::Cast(e, _) => collect_free_vars(e, bound, out, seen),
        ExprKind::Range(a, b, _) => {
            if let Some(e) = a {
                collect_free_vars(e, bound, out, seen);
            }
            if let Some(e) = b {
                collect_free_vars(e, bound, out, seen);
            }
        }
        // Raw macro tokens are not analyzed for captures; the subset's macros
        // (`print!`, `vec!`, …) reference only globals.
        ExprKind::Macro(_) => {}
        ExprKind::Try(e) => collect_free_vars(e, bound, out, seen),
    }
}

fn collect_free_vars_block(
    b: &Block,
    bound: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let mut bset = bound.clone();
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Local(l) => {
                if let Some(init) = &l.init {
                    collect_free_vars(init, &bset, out, seen);
                }
                bind_pat_names(&l.pat, &mut bset);
            }
            StmtKind::Expr(e) | StmtKind::Semi(e) => collect_free_vars(e, &bset, out, seen),
            // Nested items introduce their own scopes; their bodies are not
            // in the captured-variable set of the enclosing closure.
            StmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &b.tail {
        collect_free_vars(tail, &bset, out, seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(src: &str) -> Result<(), Vec<Diag>> {
        let prog = tpt_moira_clotho::parser::Parser::from_source(src)
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
    fn trait_bound_method_call_resolves_via_solver() {
        // `impl Show for i32 { fn show(&self) -> i32 }` plus a generic
        // `T: Show` receiver: the method call resolves through the solver's
        // bound table (the "wire the solver into method resolution" task).
        let src = "struct Unit {}\n\
                   impl Show for i32 { fn show(&self) -> i32 { 42 } }\n\
                   fn call<T: Show>(t: T) -> i32 { t.show() }\n\
                   fn main() { let _ = call(7); }";
        assert!(check(src).is_ok(), "generic-bound method call should resolve");
    }

    #[test]
    fn concrete_receiver_trait_method_resolves_via_impl_search() {
        // A concrete receiver with no inherent method, but an `impl` of a
        // trait declaring it — resolved via Solver::implements_trait.
        let src = "struct Unit {}\n\
                   impl Show for i32 { fn show(&self) -> i32 { 1 } }\n\
                   fn main() { let x = 5; let _ = x.show(); }";
        let prog = tpt_moira_clotho::parser::Parser::from_source(src)
            .parse_program()
            .expect("parse");
        if let Err(errs) = TypeChecker::check_program(&prog) {
            for e in &errs {
                eprintln!("DIAG: {}", e);
            }
            panic!("expected success");
        }
    }

    #[test]
    fn trait_method_without_matching_impl_is_rejected_for_concrete_receiver() {
        // The trait declares `show`, but no impl covers `f64` — the solver
        // must not resolve the call.
        let src = "struct Unit {}\n\
                   impl Show for i32 { fn show(&self) -> i32 { 1 } }\n\
                   fn main() { let x = 1.0; let _ = x.show(); }";
        assert!(check(src).is_err(), "unimplemented-trait method call should fail");
    }

    #[test]
    fn generic_without_operator_bound_is_rejected() {
        // `a + b` requires `T: Add`, which is not declared.
        assert!(check("fn bad<T>(a: T, b: T) -> T { a + b }").is_err());
    }

    #[test]
    fn string_literal_coerces_to_str_ref() {
        // `fn f(s: &str)` accepts a string literal (Rust's `&str` coercion).
        assert!(check(
            "fn f(s: &str) -> usize { s.len() }\n\
             fn main() { let n = f(\"hi\"); let _ = n; }"
        )
        .is_ok());
    }

    #[test]
    fn str_len_method_resolves() {
        // `s.len()` on a string-slice receiver resolves via the prelude.
        assert!(check(
            "fn f(s: &str) -> usize { s.len() }\n\
             fn main() { let lit = \"abc\"; let _ = f(lit); }"
        )
        .is_ok());
    }

    #[test]
    fn multi_param_generics_and_struct_swap() {
        // Non-toy generics: two type parameters on a struct and a function,
        // with field access through the substituted parameter types.
        assert!(check(
            "struct Pair<A, B> { a: A, b: B }\n\
             fn swap<A, B>(p: Pair<A, B>) -> Pair<B, A> { Pair { a: p.b, b: p.a } }\n\
             fn main() {\n\
                 let p = Pair { a: 1, b: 2 };\n\
                 let q = swap(p);\n\
                 let _ = q.a;\n\
                 let _ = q.b;\n\
             }"
        )
        .is_ok());
    }

    #[test]
    fn where_clause_bound_method_resolves() {
        // A bound declared in a `where` clause drives trait-method resolution
        // exactly like an inline `<T: Show>` bound.
        assert!(check(
            "impl Show for i32 { fn show(&self) -> i32 { *self } }\n\
             fn call<T>(t: T) -> i32 where T: Show { t.show() }\n\
             fn main() { let r = call(7i32); print_i32(r); }"
        )
        .is_ok());
    }

    #[test]
    fn where_clause_bound_violation_is_rejected() {
        // `f64` has no `Show` impl, so the call site violates the bound.
        assert!(check(
            "impl Show for i32 { fn show(&self) -> i32 { *self } }\n\
             fn call<T>(t: T) -> i32 where T: Show { t.show() }\n\
             fn main() { let r = call(1.0); print_i32(r); }"
        )
        .is_err());
    }

    #[test]
    fn cfg_test_module_is_skipped_by_the_checker() {
        // `#[cfg(test)] mod tests` uses macros (`assert_eq!`) and library
        // surface outside the subset; non-test checking must ignore it.
        assert!(check(
            "fn add(a: i32, b: i32) -> i32 { a + b }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 fn check() { let _ = assert_eq_helper(1); }\n\
                 fn assert_eq_helper(x: i32) -> i32 { x.not_a_real_method() }\n\
             }"
        )
        .is_ok());
    }

    #[test]
    fn refcell_interior_mutability_resolves() {
        // `RefCell::new`, `.borrow()`, `.borrow_mut()` (as used by the
        // compiler's own arena in `moira-alloc`).
        assert!(check(
            "fn main() {\n\
                 let cell = RefCell::new(1);\n\
                 let mut r = cell.borrow_mut();\n\
                 let _ = cell.borrow();\n\
             }"
        )
        .is_ok());
    }

    #[test]
    fn type_associated_fn_from_inherent_impl_resolves() {
        // `Arena::default()` — a two-segment path resolving against an
        // inherent `impl` method (not the prelude).
        assert!(check(
            "struct Arena<T> { v: i32 }\n\
             impl<T> Arena<T> {\n\
                 fn default() -> Self { Arena { v: 0 } }\n\
             }\n\
             fn main() { let a = Arena::default(); let _ = a; }"
        )
        .is_ok());
    }

    #[test]
    fn top_level_const_resolves_as_value() {
        // `const LIMIT: i32 = 10;` used as a bare value.
        assert!(check(
            "const LIMIT: i32 = 10;\n\
             fn main() { let x = LIMIT; let _ = x; }"
        )
        .is_ok());
    }

    #[test]
    fn multi_segment_call_path_resolves_last_two_segments() {
        // `std::env::args()` resolves via its last two segments (module
        // prefixes are not modeled) against the prelude's std surface.
        assert!(check(
            "fn main() { let a = std::env::args(); let _ = a; }"
        )
        .is_ok());
    }

    #[test]
    fn assoc_const_resolves_via_path() {
        // `ExitCode::FAILURE` — an associated constant on a std type.
        assert!(check(
            "fn f() -> ExitCode { ExitCode::FAILURE }"
        )
        .is_ok());
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

    // --- closure capture analysis ----------------------------------------

    #[test]
    fn non_capturing_closure_is_accepted() {
        // The body references only the closure's own parameter.
        assert!(check("fn f() { let g = |x: i32| x + 1; }").is_ok());
    }

    #[test]
    fn closure_capturing_variable_is_accepted() {
        // `a` is an enclosing variable; captures are now accepted at the type
        // level (codegen lowers them as leading parameters on the lifted
        // function).
        assert!(check("fn f() { let a = 5; let g = |x: i32| a + x; }").is_ok());
    }

    #[test]
    fn closure_referencing_function_name_is_accepted() {
        // `add` is a top-level function (in the `fns` table, not the local
        // environment), so it is not a captured variable and the closure is
        // accepted by the type checker.
        assert!(check(
            "fn add(a: i32, b: i32) -> i32 { a + b }\nfn f() { let g = |x: i32| add(x, 1); }"
        )
        .is_ok());
    }

    #[test]
    fn closure_capturing_multiple_variables_is_accepted() {
        // Both `a` and `b` escape into the closure; accepted (lowered as two
        // leading parameters at codegen).
        assert!(check("fn f() { let a = 1; let b = 2; let g = |x: i32| a + b + x; }").is_ok());
    }

    #[test]
    fn nested_closure_capture_is_accepted() {
        // The outer closure captures `a`; the inner closure captures `a` and the
        // outer parameter `x`. Nested closures are accepted.
        assert!(
            check("fn f() { let a = 5; let g = |x: i32| { let h = |y: i32| a + x + y; }; }")
                .is_ok()
        );
    }

    // --- `?` operator + diverging arms -------------------------------------

    #[test]
    fn try_on_result_is_accepted() {
        // `get()?` yields the `Ok` payload `i32`; the function returns
        // `Result<i32, String>`.
        assert!(
            check(
                "fn get() -> Result<i32, String> { Ok(5) }\nfn f() -> Result<i32, String> { let x = get()?; Ok(x) }"
            )
            .is_ok()
        );
    }

    #[test]
    fn try_on_option_is_accepted() {
        assert!(
            check(
                "fn get() -> Option<i32> { Some(5) }\nfn f() -> Option<i32> { let x = get()?; Some(x) }"
            )
            .is_ok()
        );
    }

    #[test]
    fn try_on_non_result_is_rejected() {
        // `?` on a plain `i32` is not allowed.
        assert!(check("fn f() -> i32 { let x = 5?; x }").is_err());
    }

    #[test]
    fn return_in_match_arm_does_not_force_unit() {
        // The `true => { return 1; }` arm diverges (type `!`), so it must not
        // constrain the `match` result type to `()`; the result is `i32`.
        assert!(
            check("fn f(b: bool) -> i32 { let r = match b { true => { return 1; }, false => 2 }; r }")
                .is_ok()
        );
    }

    #[test]
    fn try_arm_aggregates_with_value_arm() {
        // A `?` arm yields the `Ok` payload type and aggregates with the other
        // value arm rather than being forced to `()`.
        assert!(
            check(
                "fn get() -> Result<i32, String> { Ok(5) }\nfn f(b: bool) -> Result<i32, String> { let r = match b { true => get()?, false => 2 }; Ok(r) }"
            )
            .is_ok()
        );
    }

    // --- enum-variant destructuring patterns ---------------------------------

    #[test]
    fn if_let_some_binds_payload() {
        // `if let Some(x) = opt` binds `x` (the `Option`'s element) into the
        // then-block; the pattern is no longer rejected as "non-identifier".
        assert!(
            check("fn f(opt: Option<i32>) -> i32 { if let Some(x) = opt { x } else { 0 } }")
                .is_ok()
        );
    }

    #[test]
    fn match_on_option_binds_payload() {
        // `match opt { Some(x) => x, None => 0 }` binds `x` in the `Some` arm.
        assert!(
            check("fn f(opt: Option<i32>) -> i32 { match opt { Some(x) => x, None => 0 } }")
                .is_ok()
        );
    }

    #[test]
    fn match_on_result_binds_err_and_ok() {
        // Both `Ok(x)` and `Err(e)` payloads bind into their arms.
        assert!(
            check(
                "fn f(r: Result<i32, String>) -> i32 { match r { Ok(x) => x, Err(_) => 0 } }"
            )
            .is_ok()
        );
    }

    #[test]
    fn let_some_binds_payload() {
        // `let Some(x) = opt;` binds `x` at the type-check level.
        assert!(check("fn f(opt: Option<i32>) { let Some(x) = opt; let _ = x; }").is_ok());
    }
}
