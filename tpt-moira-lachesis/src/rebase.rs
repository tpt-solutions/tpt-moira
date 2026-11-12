//! Source-position rebasing: copies of code taken from another crate get positions that no
//! other code has. Positions key the checker's and the backend's per-expression tables, so
//! a copy keeping its original positions would share entries with the code around it.

use tpt_moira_clotho::ast::*;
use tpt_moira_clotho::token::Span;

/// Move every source position inside `item` by `base`.
pub fn rebase_item(item: &mut Item, base: usize) {
    let mut r = Rebase { base };
    r.item(item);
}

struct Rebase {
    base: usize,
}

impl Rebase {
    fn span(&self, s: &mut Span) {
        s.start += self.base;
        s.end += self.base;
    }

    fn spanned(&self, s: &mut Spanned<String>) {
        self.span(&mut s.span);
    }

    fn item(&mut self, item: &mut Item) {
        match &mut item.kind {
            ItemKind::Function(f) => self.function(f),
            ItemKind::Struct(s) => {
                self.spanned(&mut s.name);
                for f in &mut s.fields {
                    self.spanned(&mut f.name);
                    self.ty(&mut f.ty);
                }
            }
            ItemKind::Enum(e) => {
                self.spanned(&mut e.name);
                for v in &mut e.variants {
                    self.spanned(&mut v.name);
                    if let Some(fields) = &mut v.fields {
                        for f in fields.iter_mut() {
                            self.spanned(&mut f.name);
                            self.ty(&mut f.ty);
                        }
                    }
                }
            }
            ItemKind::Const(c) => {
                self.spanned(&mut c.name);
                if let Some(t) = &mut c.ty {
                    self.ty(t);
                }
                self.expr(&mut c.value);
            }
            ItemKind::Static(s) => {
                self.spanned(&mut s.name);
                self.ty(&mut s.ty);
                self.expr(&mut s.value);
            }
            ItemKind::Use(_) => {}
            ItemKind::TypeAlias(t) => {
                self.spanned(&mut t.name);
                self.ty(&mut t.ty);
            }
            ItemKind::Impl(imp) => {
                self.spanned(&mut imp.trait_name);
                self.ty(&mut imp.for_ty);
                for m in &mut imp.items {
                    self.item(m);
                }
            }
            ItemKind::Module(m) => {
                self.spanned(&mut m.name);
                if let ModuleKind::Inline(items) = &mut m.kind {
                    for i in items {
                        self.item(i);
                    }
                }
            }
        }
    }

    fn function(&mut self, f: &mut Function) {
        self.spanned(&mut f.name);
        for p in &mut f.params {
            self.pat(&mut p.pat);
            self.ty(&mut p.ty);
        }
        if let Some(r) = &mut f.ret {
            self.ty(r);
        }
        self.block(&mut f.body);
    }

    fn block(&mut self, b: &mut Block) {
        for s in &mut b.stmts {
            match &mut s.kind {
                StmtKind::Local(l) => {
                    self.pat(&mut l.pat);
                    if let Some(t) = &mut l.ty {
                        self.ty(t);
                    }
                    if let Some(e) = &mut l.init {
                        self.expr(e);
                    }
                }
                StmtKind::Expr(e) | StmtKind::Semi(e) => self.expr(e),
                StmtKind::Item(i) => self.item(i),
            }
        }
        if let Some(t) = &mut b.tail {
            self.expr(t);
        }
    }

    fn path(&mut self, p: &mut Path) {
        for s in &mut p.segments {
            self.spanned(s);
        }
    }

    fn ty(&mut self, t: &mut Type) {
        self.span(&mut t.span);
        match &mut t.kind {
            TypeKind::Path(p, args) => {
                self.path(p);
                for a in args {
                    self.ty(a);
                }
            }
            TypeKind::Ref(inner)
            | TypeKind::RefMut(inner)
            | TypeKind::Array(inner, _)
            | TypeKind::Slice(inner)
            | TypeKind::Future(inner) => self.ty(inner),
            TypeKind::Tuple(elems) => {
                for e in elems {
                    self.ty(e);
                }
            }
            TypeKind::Infer | TypeKind::Never => {}
        }
    }

    fn pat(&mut self, p: &mut Pat) {
        match p {
            Pat::Ident(n) => self.spanned(n),
            Pat::Wild | Pat::Lit(_) => {}
            Pat::Tuple(ps) => {
                for q in ps {
                    self.pat(q);
                }
            }
            Pat::Path(path) => self.path(path),
            Pat::Variant(path, ps) => {
                self.path(path);
                for q in ps {
                    self.pat(q);
                }
            }
            Pat::Ref(q) | Pat::Bind(_, q) => self.pat(q),
            Pat::Or(a, b) => {
                self.pat(a);
                self.pat(b);
            }
            Pat::Struct(_, fields) => {
                for (_, q) in fields {
                    self.pat(q);
                }
            }
            Pat::Range(a, b, _) => {
                if let Some(a) = a {
                    self.pat(a);
                }
                if let Some(b) = b {
                    self.pat(b);
                }
            }
        }
    }

    fn expr(&mut self, e: &mut Expr) {
        self.span(&mut e.span);
        match &mut e.kind {
            ExprKind::Lit(_) | ExprKind::Break | ExprKind::Continue => {}
            ExprKind::Macro(mc) => {
                self.path(&mut mc.path);
                for t in &mut mc.tokens {
                    self.span(&mut t.span);
                }
            }
            ExprKind::Path(p) => self.path(p),
            ExprKind::Binary(a, _, b) | ExprKind::Index(a, b) | ExprKind::ArrayRepeat(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            ExprKind::Range(a, b, _) => {
                if let Some(a) = a {
                    self.expr(a);
                }
                if let Some(b) = b {
                    self.expr(b);
                }
            }
            ExprKind::Unary(_, a)
            | ExprKind::Ref(a, _)
            | ExprKind::Deref(a)
            | ExprKind::Await(a)
            | ExprKind::Try(a) => self.expr(a),
            ExprKind::Field(a, name) => {
                self.expr(a);
                self.spanned(name);
            }
            ExprKind::Cast(a, t) => {
                self.expr(a);
                self.ty(t);
            }
            ExprKind::Call(f, args) => {
                self.expr(f);
                for a in args {
                    self.expr(a);
                }
            }
            ExprKind::MethodCall(recv, name, args) => {
                self.expr(recv);
                self.spanned(name);
                for a in args {
                    self.expr(a);
                }
            }
            ExprKind::Array(xs) | ExprKind::Tuple(xs) => {
                for x in xs {
                    self.expr(x);
                }
            }
            ExprKind::Struct(path, fields, base) => {
                self.path(path);
                for (name, x) in fields {
                    self.spanned(name);
                    self.expr(x);
                }
                if let Some(b) = base {
                    self.expr(b);
                }
            }
            ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => self.block(b),
            ExprKind::If(c, then, els) => {
                self.expr(c);
                self.block(then);
                if let Some(e2) = els {
                    self.expr(e2);
                }
            }
            ExprKind::While(c, b) => {
                self.expr(c);
                self.block(b);
            }
            ExprKind::For(p, iter, b) => {
                self.pat(p);
                self.expr(iter);
                self.block(b);
            }
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
            }
            ExprKind::Match(scrut, arms) => {
                self.expr(scrut);
                for (p, body) in arms {
                    self.pat(p);
                    self.expr(body);
                }
            }
            ExprKind::Closure { params, ret, body } => {
                for p in params {
                    self.pat(&mut p.pat);
                    self.ty(&mut p.ty);
                }
                if let Some(r) = ret {
                    self.ty(r);
                }
                self.expr(body);
            }
        }
    }
}
