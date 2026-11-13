//! Crate-qualified names for types that share a bare name across crates.
//!
//! The checker and the lowering key structs and enums by their bare name, so two
//! crates that each define `Function` would share one entry. This pass runs over a
//! program and its extern crates before anything is checked: every colliding type an
//! extern crate defines is renamed `<crate>_<Name>`, and every path that names a
//! colliding type is rewritten to the definition it refers to. The lookup uses the
//! crate's `use` declarations and module prefixes. The root crate keeps its names.
//!
//! Macro token streams are not rewritten: a colliding type named inside a macro call
//! keeps its bare name.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

/// One crate of a program: `key` is its extern name (ignored for the root), and
/// `program` its flattened items.
pub struct NamedCrate {
    pub key: String,
    pub is_root: bool,
    pub program: Program,
}

/// Rename colliding types in `crates` (see the module docs).
pub fn disambiguate(crates: &mut [NamedCrate]) {
    let resolver = Resolver::new(crates);
    if resolver.colliding.is_empty() {
        return;
    }
    for (u, krate) in crates.iter_mut().enumerate() {
        for item in krate.program.items.iter_mut() {
            resolver.item(u, item);
        }
    }
}

/// What one crate defines and imports, as the resolver sees it.
struct Scope {
    key: String,
    is_root: bool,
    defines: HashSet<String>,
    /// `use path::Name;` (or `as` alias): local name to the crate defining it.
    direct: HashMap<String, usize>,
    /// `use path::module;`: a module name to the crate it lives in.
    modules: HashMap<String, usize>,
    /// `use path::*;`: the crates glob-imported.
    globs: Vec<usize>,
    /// `use path::Name as Alias;` where `Name` is colliding: alias to (crate, name).
    aliases: HashMap<String, (usize, String)>,
}

struct Resolver {
    scopes: Vec<Scope>,
    /// Names defined by more than one crate, with the crates defining them.
    owners: HashMap<String, Vec<usize>>,
    colliding: HashSet<String>,
}

impl Resolver {
    fn new(crates: &[NamedCrate]) -> Resolver {
        let mut scopes: Vec<Scope> = crates
            .iter()
            .map(|c| {
                let mut defines: HashSet<String> = HashSet::new();
                collect_type_names(&c.program.items, &mut defines);
                Scope {
                    key: c.key.clone(),
                    is_root: c.is_root,
                    defines,
                    direct: HashMap::new(),
                    modules: HashMap::new(),
                    globs: Vec::new(),
                    aliases: HashMap::new(),
                }
            })
            .collect();

        let mut owners: HashMap<String, Vec<usize>> = HashMap::new();
        for (u, s) in scopes.iter().enumerate() {
            for n in &s.defines {
                owners.entry(n.clone()).or_default().push(u);
            }
        }
        let colliding: HashSet<String> = owners
            .iter()
            .filter(|(_, us)| us.len() > 1)
            .map(|(n, _)| n.clone())
            .collect();

        // Imports need every crate's name and definitions, so they are read second.
        let keys: Vec<(String, bool)> = scopes.iter().map(|s| (s.key.clone(), s.is_root)).collect();
        for (u, c) in crates.iter().enumerate() {
            let mut uses: Vec<(Vec<String>, Option<String>, bool)> = Vec::new();
            collect_uses(&c.program.items, &mut uses);
            for (path, alias, glob) in uses {
                let Some(first) = path.first() else { continue };
                let Some(d) = unit_of_segment(&keys, u, first) else {
                    continue;
                };
                if glob {
                    scopes[u].globs.push(d);
                    continue;
                }
                let last = path.last().cloned().unwrap_or_default();
                // `use a::b::{self}` names the module `b`.
                let last = if last == "self" && path.len() >= 2 {
                    path[path.len() - 2].clone()
                } else {
                    last
                };
                let local = alias.unwrap_or_else(|| last.clone());
                if local.starts_with(|c: char| c.is_ascii_lowercase()) {
                    scopes[u].modules.insert(local, d);
                } else if colliding.contains(&last) && scopes[d].defines.contains(&last) {
                    if local != last {
                        scopes[u].aliases.insert(local.clone(), (d, last.clone()));
                    }
                    scopes[u].direct.insert(local, d);
                }
            }
        }

        Resolver {
            scopes,
            owners,
            colliding,
        }
    }

    /// The name a colliding type of crate `d` has in the output.
    fn mangle(&self, d: usize, name: &str) -> String {
        let s = &self.scopes[d];
        if s.is_root {
            name.to_string()
        } else {
            let key: String = s
                .key
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format!("{key}_{name}")
        }
    }

    /// The crate a path prefix names, seen from crate `u`.
    fn unit_of_prefix(&self, u: usize, segs: &[String]) -> Option<usize> {
        let first = segs.first()?;
        if let Some(&m) = self.scopes[u].modules.get(first) {
            return Some(m);
        }
        let keys: Vec<(String, bool)> = self
            .scopes
            .iter()
            .map(|s| (s.key.clone(), s.is_root))
            .collect();
        unit_of_segment(&keys, u, first)
    }

    /// The crate that defines type `name` as seen from crate `u`, given the crate the
    /// path's prefix named (if it had one).
    fn resolve_type(&self, u: usize, prefix: Option<usize>, name: &str) -> Option<usize> {
        if let Some(p) = prefix {
            return self.scopes[p].defines.contains(name).then_some(p);
        }
        let s = &self.scopes[u];
        if s.defines.contains(name) {
            return Some(u);
        }
        if let Some(&d) = s.direct.get(name) {
            return Some(d);
        }
        let globbed: Vec<usize> = s
            .globs
            .iter()
            .copied()
            .filter(|&g| self.scopes[g].defines.contains(name))
            .collect();
        // Two imports of one crate are one candidate.
        let mut globbed = globbed;
        globbed.sort();
        globbed.dedup();
        if globbed.len() == 1 {
            return Some(globbed[0]);
        }
        match self.owners.get(name) {
            Some(us) if us.len() == 1 => Some(us[0]),
            _ => None,
        }
    }

    /// Rewrite the colliding type named by a path. The type segment is the last one, or
    /// the one before it when the last names a variant (`BinOp::Add`).
    fn rewrite_path(&self, u: usize, segs: &mut [Spanned<String>]) {
        let names: Vec<String> = segs.iter().map(|s| s.node.clone()).collect();
        let len = segs.len();
        let mut candidates: Vec<usize> = Vec::new();
        if len >= 1 {
            candidates.push(len - 1);
        }
        if len >= 2 {
            candidates.push(len - 2);
        }
        for i in candidates {
            let name = &names[i];
            // A segment after an uppercase type is a variant or associated item, not a crate item.
            if i >= 1 && names[i - 1].starts_with(|c: char| c.is_ascii_uppercase()) {
                continue;
            }
            // An alias of a colliding type (`use a::Function as F;`) names its definition.
            if i == 0 {
                if let Some((d, orig)) = self.scopes[u].aliases.get(name) {
                    segs[0].node = self.mangle(*d, orig);
                    continue;
                }
            }
            if !self.colliding.contains(name) {
                continue;
            }
            let prefix = if i == 0 {
                None
            } else {
                match self.unit_of_prefix(u, &names[..i]) {
                    Some(p) => Some(p),
                    // A prefix that names no crate (`std::...`) is not ours to rename.
                    None => continue,
                }
            };
            if let Some(d) = self.resolve_type(u, prefix, name) {
                if self.owners.get(name).is_some_and(|us| us.contains(&d)) {
                    segs[i].node = self.mangle(d, name);
                }
            }
        }
    }

    fn rewrite_name(&self, u: usize, name: &mut String) {
        if !self.colliding.contains(name.as_str()) {
            return;
        }
        if let Some(d) = self.resolve_type(u, None, name) {
            if self
                .owners
                .get(name.as_str())
                .is_some_and(|us| us.contains(&d))
            {
                *name = self.mangle(d, name);
            }
        }
    }

    fn item(&self, u: usize, item: &mut Item) {
        match &mut item.kind {
            ItemKind::Function(f) => {
                self.define(u, &mut f.name.node);
                self.function(u, f);
            }
            ItemKind::Struct(s) => {
                self.define(u, &mut s.name.node);
                for f in &mut s.fields {
                    self.ty(u, &mut f.ty);
                }
            }
            ItemKind::Enum(e) => {
                self.define(u, &mut e.name.node);
                for v in &mut e.variants {
                    if let Some(fields) = &mut v.fields {
                        for f in fields.iter_mut() {
                            self.ty(u, &mut f.ty);
                        }
                    }
                }
            }
            ItemKind::Const(c) => {
                if let Some(t) = &mut c.ty {
                    self.ty(u, t);
                }
                self.expr(u, &mut c.value);
            }
            ItemKind::Static(s) => {
                self.ty(u, &mut s.ty);
                self.expr(u, &mut s.value);
            }
            ItemKind::Use(_) | ItemKind::TypeAlias(_) => {}
            ItemKind::Impl(imp) => {
                self.ty(u, &mut imp.for_ty);
                for m in &mut imp.items {
                    // Methods keep their names: they are keyed by their type.
                    match &mut m.kind {
                        ItemKind::Function(f) => self.function(u, f),
                        _ => self.item(u, m),
                    }
                }
            }
            ItemKind::Module(m) => {
                if let ModuleKind::Inline(items) = &mut m.kind {
                    for i in items {
                        self.item(u, i);
                    }
                }
            }
        }
    }

    /// A type definition: renamed when this crate owns a colliding name.
    fn define(&self, u: usize, name: &mut String) {
        if self.colliding.contains(name.as_str())
            && self
                .owners
                .get(name.as_str())
                .is_some_and(|us| us.contains(&u))
        {
            *name = self.mangle(u, name);
        }
    }

    fn function(&self, u: usize, f: &mut Function) {
        for p in &mut f.params {
            self.pat(u, &mut p.pat);
            self.ty(u, &mut p.ty);
        }
        if let Some(r) = &mut f.ret {
            self.ty(u, r);
        }
        self.block(u, &mut f.body);
    }

    fn block(&self, u: usize, b: &mut Block) {
        for s in &mut b.stmts {
            match &mut s.kind {
                StmtKind::Local(l) => {
                    self.pat(u, &mut l.pat);
                    if let Some(t) = &mut l.ty {
                        self.ty(u, t);
                    }
                    if let Some(e) = &mut l.init {
                        self.expr(u, e);
                    }
                }
                StmtKind::Expr(e) | StmtKind::Semi(e) => self.expr(u, e),
                StmtKind::Item(i) => self.item(u, i),
            }
        }
        if let Some(t) = &mut b.tail {
            self.expr(u, t);
        }
    }

    fn ty(&self, u: usize, t: &mut Type) {
        match &mut t.kind {
            TypeKind::Path(p, args) => {
                self.path(u, p);
                for a in args {
                    self.ty(u, a);
                }
            }
            TypeKind::Ref(inner)
            | TypeKind::RefMut(inner)
            | TypeKind::Array(inner, _)
            | TypeKind::Slice(inner)
            | TypeKind::Future(inner) => self.ty(u, inner),
            TypeKind::Tuple(elems) => {
                for e in elems {
                    self.ty(u, e);
                }
            }
            TypeKind::Infer | TypeKind::Never => {}
        }
    }

    fn path(&self, u: usize, p: &mut Path) {
        self.rewrite_path(u, &mut p.segments);
    }

    fn pat(&self, u: usize, p: &mut Pat) {
        match p {
            Pat::Ident(_) | Pat::Wild | Pat::Lit(_) => {}
            Pat::Tuple(ps) => {
                for q in ps {
                    self.pat(u, q);
                }
            }
            Pat::Path(path) => self.path(u, path),
            Pat::Variant(path, ps) => {
                self.path(u, path);
                for q in ps {
                    self.pat(u, q);
                }
            }
            Pat::Ref(q) | Pat::Bind(_, q) => self.pat(u, q),
            Pat::Or(a, b) => {
                self.pat(u, a);
                self.pat(u, b);
            }
            Pat::Struct(name, fields) => {
                self.rewrite_name(u, name);
                for (_, q) in fields {
                    self.pat(u, q);
                }
            }
            Pat::Range(a, b, _) => {
                if let Some(a) = a {
                    self.pat(u, a);
                }
                if let Some(b) = b {
                    self.pat(u, b);
                }
            }
        }
    }

    fn expr(&self, u: usize, e: &mut Expr) {
        match &mut e.kind {
            ExprKind::Lit(_) | ExprKind::Break | ExprKind::Continue | ExprKind::Macro(_) => {}
            ExprKind::Path(p) => self.path(u, p),
            ExprKind::Binary(a, _, b) | ExprKind::Index(a, b) | ExprKind::ArrayRepeat(a, b) => {
                self.expr(u, a);
                self.expr(u, b);
            }
            ExprKind::Range(a, b, _) => {
                if let Some(a) = a {
                    self.expr(u, a);
                }
                if let Some(b) = b {
                    self.expr(u, b);
                }
            }
            ExprKind::Unary(_, a)
            | ExprKind::Field(a, _)
            | ExprKind::Ref(a, _)
            | ExprKind::Deref(a)
            | ExprKind::Await(a)
            | ExprKind::Try(a) => self.expr(u, a),
            ExprKind::Call(f, args) => {
                self.expr(u, f);
                for a in args {
                    self.expr(u, a);
                }
            }
            ExprKind::MethodCall(recv, _, args) => {
                self.expr(u, recv);
                for a in args {
                    self.expr(u, a);
                }
            }
            ExprKind::Array(xs) | ExprKind::Tuple(xs) => {
                for x in xs {
                    self.expr(u, x);
                }
            }
            ExprKind::Struct(path, fields, base) => {
                self.path(u, path);
                for (_, x) in fields {
                    self.expr(u, x);
                }
                if let Some(b) = base {
                    self.expr(u, b);
                }
            }
            ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => self.block(u, b),
            ExprKind::If(c, then, els) => {
                self.expr(u, c);
                self.block(u, then);
                if let Some(e2) = els {
                    self.expr(u, e2);
                }
            }
            ExprKind::While(c, b) => {
                self.expr(u, c);
                self.block(u, b);
            }
            ExprKind::For(p, iter, b) => {
                self.pat(u, p);
                self.expr(u, iter);
                self.block(u, b);
            }
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(u, v);
                }
            }
            ExprKind::Match(scrut, arms) => {
                self.expr(u, scrut);
                for (p, body) in arms {
                    self.pat(u, p);
                    self.expr(u, body);
                }
            }
            ExprKind::Closure { params, ret, body } => {
                for p in params {
                    self.pat(u, &mut p.pat);
                    self.ty(u, &mut p.ty);
                }
                if let Some(r) = ret {
                    self.ty(u, r);
                }
                self.expr(u, body);
            }
            ExprKind::Cast(a, t) => {
                self.expr(u, a);
                self.ty(u, t);
            }
        }
    }
}

/// The crate a path's first segment names, seen from crate `u`.
fn unit_of_segment(keys: &[(String, bool)], u: usize, seg: &str) -> Option<usize> {
    if matches!(seg, "crate" | "self" | "super") {
        return Some(u);
    }
    // Extern crates are named like `tpt_moira_clotho` for `clotho`.
    let wanted = seg.strip_prefix("tpt_moira_").unwrap_or(seg);
    // The key may be given either way (`clotho` or `tpt_moira_clotho`).
    keys.iter().position(|(k, is_root)| {
        let k = k.strip_prefix("tpt_moira_").unwrap_or(k);
        !is_root
            && !k.is_empty()
            && k.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>()
                == wanted
    })
}

/// The struct, enum and free-function names a list of items defines, through inline
/// modules. Methods (functions inside `impl` blocks) are not listed: they belong to
/// their type.
fn collect_type_names(items: &[Item], out: &mut HashSet<String>) {
    for item in items {
        match &item.kind {
            ItemKind::Struct(s) => {
                out.insert(s.name.node.clone());
            }
            ItemKind::Enum(e) => {
                out.insert(e.name.node.clone());
            }
            ItemKind::Function(f) => {
                out.insert(f.name.node.clone());
            }
            ItemKind::Module(m) => {
                if let ModuleKind::Inline(sub) = &m.kind {
                    collect_type_names(sub, out);
                }
            }
            _ => {}
        }
    }
}

/// Every import in a list of items, as `(full path, alias, is glob)`.
fn collect_uses(items: &[Item], out: &mut Vec<(Vec<String>, Option<String>, bool)>) {
    for item in items {
        match &item.kind {
            ItemKind::Use(tree) => flatten_use(tree, &[], out),
            ItemKind::Module(m) => {
                if let ModuleKind::Inline(sub) = &m.kind {
                    collect_uses(sub, out);
                }
            }
            _ => {}
        }
    }
}

fn flatten_use(
    tree: &UseTree,
    prefix: &[String],
    out: &mut Vec<(Vec<String>, Option<String>, bool)>,
) {
    let mut path: Vec<String> = prefix.to_vec();
    for s in &tree.prefix {
        path.push(s.node.clone());
    }
    match &tree.kind {
        UseKind::Glob => out.push((path, None, true)),
        UseKind::Leaf => {
            let alias = tree.alias.as_ref().map(|a| a.node.clone());
            out.push((path, alias, false));
        }
        UseKind::Nested(children) => {
            for c in children {
                flatten_use(c, &path, out);
            }
        }
    }
}
