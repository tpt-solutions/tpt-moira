//! Abstract Syntax Tree for the Phase-1 Rust subset.

use crate::token::Span;

/// A syntax node carrying a source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Spanned { node, span }
    }
}

/// Fields of a struct literal: a list of `(name, value)` pairs plus an optional
/// `..base` expression.
pub type StructLiteral = (Vec<(Spanned<String>, Expr)>, Option<Box<Expr>>);

/// A complete parsed program: a sequence of top-level items.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub visibility: Visibility,
    pub kind: ItemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Pub,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    Function(Function),
    Struct(StructDef),
    Enum(EnumDef),
    Const(ConstDef),
    Static(StaticDef),
    Use(UseTree),
    /// A trait implementation block `impl Trait for Type { ... }`.
    Impl(ImplDef),
    /// A top-level `type Alias = Type;` definition.
    TypeAlias(TypeAliasDef),
    /// A module declaration: `mod name;` (external file) or `mod name { .. }`
    /// (inline). The frontend resolves external modules to sibling `.rs` files
    /// when self-hosting; the Phase-1 checker ignores them otherwise.
    Module(ModuleDef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: Spanned<String>,
    pub generics: Generics,
    pub is_async: bool,
    pub is_unsafe: bool,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub body: Block,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub pat: Pat,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructDef {
    pub name: Spanned<String>,
    pub generics: Generics,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: Spanned<String>,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDef {
    pub name: Spanned<String>,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub name: Spanned<String>,
    pub fields: Option<Vec<Field>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstDef {
    pub name: Spanned<String>,
    pub ty: Option<Type>,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticDef {
    pub name: Spanned<String>,
    pub ty: Type,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseTree {
    /// Root path segment (e.g. `std` in `use std::collections::HashMap;`).
    pub prefix: Vec<Spanned<String>>,
    pub kind: UseKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseKind {
    Glob,
    Leaf,
    Nested(Vec<UseTree>),
}

/// A trait implementation block `impl<T> Trait for Type<T> where T: Bound { .. }`.
///
/// `Phase 1` ignores the method bodies; the name, self-type, generic parameters
/// and where-clauses are what the Lachesis trait solver consumes to search
/// user-defined `impl`s (a Phase-3 deliverable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplDef {
    /// The implemented trait's name (last path segment, e.g. `Eq`).
    pub trait_name: Spanned<String>,
    /// The type the trait is implemented for (`for` type).
    pub for_ty: Type,
    /// Generic parameters declared on the `impl`, e.g. `<T>` in
    /// `impl<T> Trait for Wrap<T>`.
    pub generics: Generics,
    /// Whether the impl is `unsafe`.
    pub is_unsafe: bool,
    /// The associated items (methods, and `type Assoc = T;` projections).
    pub items: Vec<Item>,
}

/// A top-level `type Alias = Type;` definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeAliasDef {
    pub name: Spanned<String>,
    pub ty: Type,
}

/// A module declaration `mod name;` or `mod name { .. }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDef {
    pub name: Spanned<String>,
    pub kind: ModuleKind,
}

/// How a module's body is supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleKind {
    /// `mod name;` — resolved by loading the sibling file `name.rs`
    /// (or `name/mod.rs`) during the frontend's module-resolution pass.
    External,
    /// `mod name { .. }` — the items are written inline.
    Inline(Vec<Item>),
}

/// Generic parameters and where-clauses attached to an item.
///
/// These are the Phase-3 "trait bounds" surface: each [`TypeParam`] carries the
/// bounds written `T: Bound`, and [`Generics::where_clause`] carries the
/// `where T: Bound` predicates. The Lachesis solver lowers these into clause
/// bodies (where-clauses) and generic self-types (`Wrap<T>`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Generics {
    pub params: Vec<TypeParam>,
    pub where_clause: Vec<WherePredicate>,
}

/// A single generic parameter, e.g. `T` in `<T: Clone>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParam {
    pub name: Spanned<String>,
    pub bounds: Vec<TypeBound>,
}

/// A bound on a type, currently only a trait bound `T: Trait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeBound {
    Trait(Path),
}

/// A `where` predicate, e.g. `where Self: Clone` or `where T: Bound`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WherePredicate {
    pub ty: Type,
    pub bounds: Vec<TypeBound>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// Optional trailing expression that becomes the block's value.
    pub tail: Option<Box<Expr>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    pub kind: StmtKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StmtKind {
    Local(Local),
    Expr(Expr),
    Item(Item),
    Semi(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub pat: Pat,
    pub ty: Option<Type>,
    pub init: Option<Expr>,
    /// Whether the binding was declared with `mut`.
    pub mutable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pat {
    Ident(Spanned<String>),
    Wild,
    Tuple(Vec<Pat>),
    Struct(String, Vec<(String, Pat)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    Lit(Lit),
    Path(Path),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    MethodCall(Box<Expr>, Spanned<String>, Vec<Expr>),
    Field(Box<Expr>, Spanned<String>),
    Index(Box<Expr>, Box<Expr>),
    /// `&expr` or `&mut expr`. The boolean is `true` for `&mut`.
    Ref(Box<Expr>, bool),
    Deref(Box<Expr>),
    Array(Vec<Expr>),
    /// A struct literal: `Path { field: value, ..base }`.
    Struct(Path, Vec<(Spanned<String>, Expr)>, Option<Box<Expr>>),
    Block(Block),
    If(Box<Expr>, Block, Option<Box<Expr>>),
    While(Box<Expr>, Block),
    Loop(Block),
    For(Pat, Box<Expr>, Block),
    Return(Option<Box<Expr>>),
    Break,
    Continue,
    Match(Box<Expr>, Vec<(Pat, Expr)>),
    /// `expr.await` — suspends the enclosing `async` context until `expr`
    /// (a value implementing `Future`) is ready, yielding its output.
    Await(Box<Expr>),
    /// An `unsafe { .. }` block.
    Unsafe(Block),
    /// A closure `|params| body` (or `|params| -> Ret body`).
    Closure {
        params: Vec<Param>,
        ret: Option<Type>,
        body: Box<Expr>,
    },
    /// A `expr as Type` cast.
    Cast(Box<Expr>, Type),
    /// A macro invocation `path!(tokens)` / `path![tokens]` / `path!{tokens}`.
    Macro(MacroCall),
    /// The `?` operator: `expr?`.
    Try(Box<Expr>),
}

/// A macro invocation captured during parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroCall {
    pub path: Path,
    pub delimiter: MacroDelim,
    /// The raw token stream inside the delimiter, including nested delimiters.
    pub tokens: Vec<crate::token::Token>,
}

/// The delimiter kind used by a macro invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroDelim {
    Paren,
    Bracket,
    Brace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lit {
    Int(u128, Option<String>),
    Float(String, Option<String>),
    Str(String),
    Char(char),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    pub segments: Vec<Spanned<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    /// Range operator `..`.
    Range,
    /// Plain assignment `=`.
    Assign,
    /// Compound assignment operators: `+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `|=`,
    /// `^=`, `<<=`, `>>=`.
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    ShlAssign,
    ShrAssign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Deref,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

impl Type {
    /// A short, human-readable name for diagnostics (the last path segment, or a
    /// placeholder for non-path types).
    pub fn node_name(&self) -> String {
        match &self.kind {
            TypeKind::Path(p, _) => p
                .segments
                .last()
                .map(|s| s.node.clone())
                .unwrap_or_else(|| "<type>".to_string()),
            TypeKind::Ref(inner) => format!("&{}", inner.node_name()),
            TypeKind::RefMut(inner) => format!("&mut {}", inner.node_name()),
            TypeKind::Array(inner, _) => format!("[{}]", inner.node_name()),
            TypeKind::Slice(inner) => format!("[{}]", inner.node_name()),
            TypeKind::Tuple(_) => "<tuple>".to_string(),
            TypeKind::Infer => "_".to_string(),
            TypeKind::Future(inner) => format!("impl Future<Output = {}>", inner.node_name()),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    /// A named type, optionally with generic arguments: `Foo`, `Wrap<T>`.
    Path(Path, Vec<Type>),
    Ref(Box<Type>),
    RefMut(Box<Type>),
    Array(Box<Type>, u128),
    Slice(Box<Type>),
    Tuple(Vec<Type>),
    Infer,
    /// `impl Future<Output = T>` — the anonymous return type of an `async fn`
    /// (and the type of an `.await`-able expression). The inner type is the
    /// future's resolved output.
    Future(Box<Type>),
}
