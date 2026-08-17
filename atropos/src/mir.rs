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
    /// Generated `async fn` state machines (future struct + poll + run wrapper).
    pub async_fns: Vec<AsyncDef>,
}

/// A generated state machine backing an `async fn`.
///
/// An `async fn f(args) -> T` lowers to:
/// * `fut_name` — a struct holding `state` (an `i64` discriminator), one slot
///   per parameter/local, an `__out` slot of the output type, and an `__awt<i>`
///   slot per `.await` point (each holding the awaited future's own state
///   struct), so suspension can reconstruct the in-flight future on resume.
/// * `poll_name` — `fn(fut_name*) -> i64`, the coroutine body driven by an
///   executor. It begins with a `switch` on `state`; ordinary statements run in
///   the `state == 0` block, each `.await` sets `state` to its resume label and
///   returns `PENDING` (suspension), and the matching resume block reloads the
///   in-flight future, polls it to completion, and continues.
/// * `run_name` — `fn(args) -> T`, a sync executor that builds the future,
///   polls until `READY`, and returns `__out`. This lets async fns be called
///   transparently from synchronous `main` while the poll fn still exercises
///   real resume points.
#[derive(Debug, Clone)]
pub struct AsyncDef {
    pub fn_name: String,
    pub fut_name: String,
    /// `(field_name, field_type)` for the generated future struct.
    pub fut_fields: Vec<(String, String)>,
    pub poll_name: String,
    pub run_name: String,
    /// Output (return) type of the async fn.
    pub output_ty: String,
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
    /// Field projection `operand.field`. When `deref` is true the operand is a
    /// pointer and the access is `operand->field` (used for the future `self`).
    Field(Operand, String, bool),
    /// Address-of `&operand` (`true` for `&mut`).
    Ref(Operand, bool),
    /// Write `value` into `struct_operand.field`. When `deref` is true the
    /// operand is a pointer and the write is `operand->field = value`.
    StoreField(Operand, String, Operand, bool),
    /// Type cast `operand as TargetType` — `ty` is the target C type name
    /// (e.g. `i64`, `u32`, `i32*`). Models `expr as Type`.
    Cast(Operand, String),
    /// Array literal `[e0, e1, …]` — `elem_ty` is the C type of each element
    /// (the array temp is declared `elem_ty name[len]` and initialized inline).
    Array(String, Vec<Operand>),
    /// Array indexing `base[idx]` — reads `base[idx]` into the destination.
    Index(Operand, Operand),
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
    Assign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Deref,
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
    /// State-machine dispatch: jump to the target whose `i64` label equals the
    /// switch operand; if none match, jump to the default target. Used by the
    /// `async fn` poll coroutine to select a resume point from `state`.
    Switch(Operand, Vec<(i64, usize)>, usize),
    /// Unreachable / not-yet-terminated.
    #[default]
    Unreachable,
}

// ---------------------------------------------------------------------------
// Deterministic rendering
// ---------------------------------------------------------------------------
//
// `--emit mir` must render byte-identically regardless of which compiler
// binary produced it (stage-0 rustc-built driver vs. stage-1 Moira-built
// driver). The default `{:#?}` formatter is unsuitable because it walks the
// `temp_types` `HashMap`, whose iteration order is randomized per process.
// These `Display` impls iterate every collection in a fixed, sorted order so
// the MIR text is stable across runs and stages.

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for s in &self.structs {
            writeln!(f, "{}", s)?;
        }
        for func in &self.functions {
            writeln!(f, "{}", func)?;
        }
        Ok(())
    }
}

impl fmt::Display for StructDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "struct {} {{", self.name)?;
        for (n, t) in &self.fields {
            writeln!(f, "    {}: {}", n, t)?;
        }
        write!(f, "}}")
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ret = self.ret.clone().unwrap_or_else(|| "()".to_string());
        writeln!(
            f,
            "fn {} ({}) -> {} {{",
            self.name,
            self.params.join(", "),
            ret
        )?;
        for (n, t) in &self.locals {
            let ty = t.clone().unwrap_or_else(|| "()".to_string());
            writeln!(f, "    local {}: {}", n, ty)?;
        }
        // Sorted so the temporary block is stable across processes.
        let mut temps: Vec<(&String, &String)> = self.temp_types.iter().collect();
        temps.sort_by(|a, b| a.0.cmp(b.0));
        for (n, t) in temps {
            writeln!(f, "    temp {}: {}", n, t)?;
        }
        for (i, block) in self.blocks.iter().enumerate() {
            writeln!(f, "  b{}:", i)?;
            for s in &block.stmts {
                writeln!(f, "    {} = {}", s.place, s.rvalue)?;
            }
            writeln!(f, "    {}", block.term)?;
        }
        write!(f, "}}")
    }
}

impl fmt::Display for Rvalue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rvalue::Use(o) => write!(f, "use {}", o),
            Rvalue::Binary(b, l, r) => write!(f, "{:?} {}, {}", b, l, r),
            Rvalue::Unary(u, o) => write!(f, "{:?} {}", u, o),
            Rvalue::Call(name, args) => {
                let a: Vec<String> = args.iter().map(|x| x.to_string()).collect();
                write!(f, "call {} ({})", name, a.join(", "))
            }
            Rvalue::StructLit(name, fields) => {
                let fl: Vec<String> = fields
                    .iter()
                    .map(|(n, v)| format!("{}: {}", n, v))
                    .collect();
                write!(f, "struct {} {{ {} }}", name, fl.join(", "))
            }
            Rvalue::Field(o, field, deref) => {
                if *deref {
                    write!(f, "{}->{}", o, field)
                } else {
                    write!(f, "{}.{}", o, field)
                }
            }
            Rvalue::Ref(o, _) => write!(f, "&{}", o),
            Rvalue::StoreField(s, field, v, deref) => {
                if *deref {
                    write!(f, "{}->{} = {}", s, field, v)
                } else {
                    write!(f, "{}.{} = {}", s, field, v)
                }
            }
            Rvalue::Cast(o, ty) => write!(f, "({}) {}", ty, o),
            Rvalue::Array(ty, elems) => {
                let es: Vec<String> = elems.iter().map(|e| e.to_string()).collect();
                write!(f, "[{}; {}]", ty, es.join(", "))
            }
            Rvalue::Index(b, i) => write!(f, "{}[{}]", b, i),
        }
    }
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::Goto(n) => write!(f, "goto b{}", n),
            Terminator::If(c, t, e) => write!(f, "if {} -> b{} else b{}", c, t, e),
            Terminator::Return(Some(v)) => write!(f, "return {}", v),
            Terminator::Return(None) => write!(f, "return"),
            Terminator::Switch(disc, cases, default) => {
                write!(f, "switch {} {{ ", disc)?;
                for (label, target) in cases {
                    write!(f, "case {} => b{}, ", label, target)?;
                }
                write!(f, "default => b{} }}", default)
            }
            Terminator::Unreachable => write!(f, "unreachable"),
        }
    }
}
