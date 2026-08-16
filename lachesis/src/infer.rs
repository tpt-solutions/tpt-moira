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
use clotho::ast::{BinOp, Block, Expr, ExprKind, Lit, Pat, Path, StmtKind, Type, TypeKind, UnOp};
use clotho::error::Diag;
use clotho::token::Span;
use std::collections::HashMap;

/// A struct's resolved field types.
#[derive(Debug, Clone)]
pub struct StructInfo {
    pub fields: Vec<(String, Ty)>,
}

/// A function's resolved signature.
#[derive(Debug, Clone)]
pub struct FnSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
    /// Whether the function is declared `async fn`, in which case callers that
    /// do not `.await` the result receive a `Future` wrapper.
    pub is_async: bool,
}

/// The Phase-2 type checker.
pub struct TypeChecker {
    structs: HashMap<String, StructInfo>,
    fns: HashMap<String, FnSig>,
    env: Vec<HashMap<String, Ty>>,
    ret: Ty,
    ret_span: Span,
    errors: Vec<Diag>,
}

impl Default for TypeChecker {
    fn default() -> Self {
        TypeChecker {
            structs: HashMap::new(),
            fns: HashMap::new(),
            env: Vec::new(),
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

        // Pass 1: collect struct and function signatures so callers may refer to
        // functions defined later in the file (forward references).
        for item in &prog.items {
            match &item.kind {
                clotho::ast::ItemKind::Struct(s) => {
                    let fields = s
                        .fields
                        .iter()
                        .map(|f| (f.name.node.clone(), cx.type_to_ty(&f.ty)))
                        .collect();
                    cx.structs
                        .insert(s.name.node.clone(), StructInfo { fields });
                }
                clotho::ast::ItemKind::Function(f) => {
                    let params = f.params.iter().map(|p| cx.type_to_ty(&p.ty)).collect();
                    let ret = f.ret.as_ref().map(|t| cx.type_to_ty(t)).unwrap_or(Ty::Unit);
                    cx.fns.insert(
                        f.name.node.clone(),
                        FnSig {
                            params,
                            ret,
                            is_async: f.is_async,
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
            },
        );
        cx.fns.insert(
            "print_bool".to_string(),
            FnSig {
                params: vec![Ty::Bool],
                ret: Ty::Unit,
                is_async: false,
            },
        );

        // Pass 2: check each function body.
        for item in &prog.items {
            if let clotho::ast::ItemKind::Function(f) = &item.kind {
                cx.check_function(f);
            }
        }

        if cx.errors.is_empty() {
            Ok(())
        } else {
            Err(cx.errors)
        }
    }

    fn check_function(&mut self, f: &clotho::ast::Function) {
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
                    let name = pat_name(&l.pat);
                    self.env.last_mut().unwrap().insert(name, ty);
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

    fn infer_expr(&mut self, e: &Expr) -> Ty {
        let span = e.span;
        let ty = match &e.kind {
            ExprKind::Lit(l) => lit_ty(l),
            ExprKind::Path(p) => {
                let name = p
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                match self.lookup(&name) {
                    Some(t) => t,
                    None => {
                        self.err(span, format!("cannot find value `{}`", name));
                        Ty::Unknown
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
            ExprKind::Struct(path, fields, base) => {
                self.check_struct(span, path, fields, base.as_deref())
            }
            ExprKind::Block(b) => self.infer_block(b),
            ExprKind::If(cond, then_b, else_b) => {
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
                self.infer_expr(iter);
                let name = pat_name(pat);
                self.env.last_mut().unwrap().insert(name, Ty::Int);
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
            ExprKind::Match(..) => {
                self.err(
                    span,
                    "`match` is not in the type-checked subset".to_string(),
                );
                Ty::Unknown
            }
            ExprKind::MethodCall(..) => {
                self.err(
                    span,
                    "method calls are not in the type-checked subset".to_string(),
                );
                Ty::Unknown
            }
            ExprKind::Index(..) | ExprKind::Array(..) => {
                self.err(
                    span,
                    "indexing/arrays are not in the type-checked subset".to_string(),
                );
                Ty::Unknown
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
            ExprKind::Closure { .. } => {
                self.err(
                    span,
                    "closures are not in the type-checked subset".to_string(),
                );
                Ty::Unknown
            }
            ExprKind::Cast(e2, _) => self.infer_expr(e2),
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
                    if !TraitDb::implements(tr, &lt) {
                        self.err(
                            span,
                            format!("type {} does not implement {}", lt, tr.name()),
                        );
                    }
                    if !TraitDb::implements(tr, &rt) {
                        self.err(
                            span,
                            format!("type {} does not implement {}", rt, tr.name()),
                        );
                    }
                }
                self.expect(&lt, &rt, span);
                Ty::Bool
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if let Some(tr) = BuiltinTrait::for_binop(op) {
                    if !TraitDb::implements(tr, &lt) {
                        self.err(
                            span,
                            format!("type {} does not implement {}", lt, tr.name()),
                        );
                    }
                    if !TraitDb::implements(tr, &rt) {
                        self.err(
                            span,
                            format!("type {} does not implement {}", rt, tr.name()),
                        );
                    }
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
        let name = match &callee.kind {
            ExprKind::Path(p) => p
                .segments
                .last()
                .map(|s| s.node.clone())
                .unwrap_or_default(),
            _ => {
                self.err(span, "call target must be a path".to_string());
                return Ty::Unknown;
            }
        };
        let sig = match self.fns.get(&name) {
            Some(s) => s.clone(),
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
        for (a, pt) in args.iter().zip(sig.params.iter()) {
            let at = self.infer_expr(a);
            self.expect(&at, pt, a.span);
        }
        if sig.is_async {
            Ty::Future(Box::new(sig.ret.clone()))
        } else {
            sig.ret.clone()
        }
    }

    fn check_field(&mut self, span: Span, base: &Expr, field: &clotho::ast::Spanned<String>) -> Ty {
        let bt = self.infer_expr(base);
        match &bt {
            Ty::Struct(s) => match self.structs.get(s) {
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
                for (fname, fexpr) in fields {
                    match info.fields.iter().find(|(n, _)| n == &fname.node) {
                        Some((_, ft)) => {
                            let at = self.infer_expr(fexpr);
                            self.expect(&at, ft, fexpr.span);
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

    fn lookup(&self, name: &str) -> Option<Ty> {
        for scope in self.env.iter().rev() {
            if let Some(t) = scope.get(name) {
                return Some(t.clone());
            }
        }
        None
    }

    fn type_to_ty(&self, t: &Type) -> Ty {
        match &t.kind {
            TypeKind::Path(p, args) => {
                let name = p
                    .segments
                    .last()
                    .map(|s| s.node.clone())
                    .unwrap_or_default();
                match prim(&name) {
                    Some(t) => t,
                    None => {
                        if args.is_empty() {
                            Ty::Struct(name)
                        } else {
                            // Generic applications are not yet part of the
                            // Phase-1 type system; keep the base name.
                            Ty::Struct(name)
                        }
                    }
                }
            }
            TypeKind::Ref(inner) => Ty::Ref(Box::new(self.type_to_ty(inner))),
            TypeKind::RefMut(inner) => Ty::RefMut(Box::new(self.type_to_ty(inner))),
            TypeKind::Future(inner) => Ty::Future(Box::new(self.type_to_ty(inner))),
            TypeKind::Infer => Ty::Int,
            _ => Ty::Unknown,
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
            TypeKind::Infer => Ty::Int,
            _ => Ty::Unknown,
        }
    }

    /// Whether two types are mutually compatible for assignment/use.
    fn compatible(&self, a: &Ty, b: &Ty) -> bool {
        if matches!(a, Ty::Unknown) || matches!(b, Ty::Unknown) {
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
            (Ty::Ref(_), _) | (Ty::RefMut(_), _) => false,
            (Ty::Struct(x), Ty::Struct(y)) => x == y,
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
        _ => "_".to_string(),
    }
}
