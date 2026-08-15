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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: Spanned<String>,
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
    Ref(Box<Expr>),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    Path(Path),
    Ref(Box<Type>),
    RefMut(Box<Type>),
    Array(Box<Type>, u128),
    Slice(Box<Type>),
    Tuple(Vec<Type>),
    Infer,
}
