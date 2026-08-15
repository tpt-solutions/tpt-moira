//! Mid-level IR for the Phase-1 subset.
//!
//! The IR is in SSA form: every value is bound exactly once to a *temporary*
//! (`t0`, `t1`, …) via a [`Stmt::Assign`], and control flow is made explicit
//! with [`Terminator`]s over basic [`Block`]s. This makes later optimization
//! passes (constant folding, dead-code elimination) and a native backend
//! straightforward to layer on.

use std::fmt;

/// A whole program in MIR.
#[derive(Debug, Clone, Default)]
pub struct Program {
    /// Struct type definitions: `(name, [(field_name, field_type)])`.
    pub structs: Vec<StructDef>,
    /// Top-level functions.
    pub functions: Vec<Function>,
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<(String, String)>,
}

/// A function: a name, parameters, an optional return type, and a list of basic
/// blocks. Block 0 is the entry point.
#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub params: Vec<String>,
    pub ret: Option<String>,
    /// Local (non-temp) variables with their declared C type, if known.
    pub locals: Vec<(String, Option<String>)>,
    /// SSA temporaries (`t0`, `t1`, …) with their inferred C type.
    pub temp_types: std::collections::HashMap<String, String>,
    pub blocks: Vec<Block>,
}

impl Function {
    /// Number of basic blocks.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

/// A basic block: a linear sequence of assignments followed by exactly one
/// terminator.
#[derive(Debug, Clone, Default)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
}

/// A single SSA binding: `place = rvalue`.
#[derive(Debug, Clone)]
pub struct Stmt {
    pub place: String,
    pub rvalue: Rvalue,
}

/// The right-hand side of an SSA assignment.
#[derive(Debug, Clone)]
pub enum Rvalue {
    /// Copy/use another operand.
    Use(Operand),
    /// Binary primitive operation.
    Binary(BinOp, Operand, Operand),
    /// Unary primitive operation.
    Unary(UnOp, Operand),
    /// Function call `name(args)`.
    Call(String, Vec<Operand>),
    /// Struct literal `{ field: value, .. }` of the named struct type.
    StructLit(String, Vec<(String, Operand)>),
    /// Field projection `operand.field`.
    Field(Operand, String),
    /// Address-of `&operand`.
    Ref(Operand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Rem,
    And, Or, BitAnd, BitOr, BitXor,
    Shl, Shr,
    Eq, Ne, Lt, Gt, Le, Ge,
    Assign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg, Not, Deref,
}

/// A value used on the right-hand side of an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// An SSA temporary or local variable (`t0`, `x`, …).
    Var(String),
    /// An integer/boolean constant.
    Const(i128),
    /// The unit value `()`.
    Unit,
}

impl Operand {
    pub fn var(name: impl Into<String>) -> Operand {
        Operand::Var(name.into())
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Var(n) => f.write_str(n),
            Operand::Const(c) => write!(f, "{}", c),
            Operand::Unit => f.write_str("()"),
        }
    }
}

/// The final instruction of a basic block, describing control flow.
#[derive(Debug, Clone, Default)]
pub enum Terminator {
    /// Unconditional jump to block `n`.
    Goto(usize),
    /// Conditional branch: if `cond` jump to `then`, else to `else_`.
    If(Operand, usize, usize),
    /// Return `value` (or `()` when `None`).
    Return(Option<Operand>),
    /// Unreachable / not-yet-terminated.
    #[default]
    Unreachable,
}
