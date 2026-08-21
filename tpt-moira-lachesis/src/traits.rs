//! Basic trait resolution (pre-specialization) for the Phase-1 subset.
//!
//! A full Prolog-style solver (Chalk-like) is a Phase-3 deliverable. For Phase 2
//! Lachesis ships a *built-in trait database*: a fixed set of fundamental
//! operator traits (`Add`, `Sub`, `Mul`, `Div`, `Rem`, `BitAnd`, …) and the
//! comparison/equality traits (`Eq`, `Ord`-family), each with the primitive
//! types that implement them. The type checker queries [`TraitDb::implements`]
//! to decide whether an operator may be applied to a given operand type — this
//! is the "basic trait resolution" called for by the roadmap, and it is the seam
//! the richer solver will later plug into.

use crate::ty::Ty;

/// A fundamental, compiler-known trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinTrait {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ord,
}

impl BuiltinTrait {
    /// Map a binary operator to the trait it requires, if any.
    pub fn for_binop(op: tpt_moira_clotho::ast::BinOp) -> Option<BuiltinTrait> {
        use tpt_moira_clotho::ast::BinOp::*;
        Some(match op {
            Add => BuiltinTrait::Add,
            Sub => BuiltinTrait::Sub,
            Mul => BuiltinTrait::Mul,
            Div => BuiltinTrait::Div,
            Rem => BuiltinTrait::Rem,
            BitAnd => BuiltinTrait::BitAnd,
            BitOr => BuiltinTrait::BitOr,
            BitXor => BuiltinTrait::BitXor,
            Shl => BuiltinTrait::Shl,
            Shr => BuiltinTrait::Shr,
            Eq | Ne => BuiltinTrait::Eq,
            Lt | Gt | Le | Ge => BuiltinTrait::Ord,
            // Assignment-family and range operators do not consult traits here.
            _ => return None,
        })
    }

    /// Map a trait's surface name (as written in a bound) to the built-in
    /// trait, if it is one. Accepts both the lang-item name (`PartialEq`) and
    /// the user-facing alias (`Eq`).
    pub fn from_name(name: &str) -> Option<BuiltinTrait> {
        use BuiltinTrait::*;
        Some(match name {
            "Add" => Add,
            "Sub" => Sub,
            "Mul" => Mul,
            "Div" => Div,
            "Rem" => Rem,
            "BitAnd" => BitAnd,
            "BitOr" => BitOr,
            "BitXor" => BitXor,
            "Shl" => Shl,
            "Shr" => Shr,
            "PartialEq" | "Eq" => Eq,
            "PartialOrd" | "Ord" => Ord,
            _ => return None,
        })
    }

    /// The trait's surface name (used in diagnostics).
    pub fn name(self) -> &'static str {
        match self {
            BuiltinTrait::Add => "Add",
            BuiltinTrait::Sub => "Sub",
            BuiltinTrait::Mul => "Mul",
            BuiltinTrait::Div => "Div",
            BuiltinTrait::Rem => "Rem",
            BuiltinTrait::BitAnd => "BitAnd",
            BuiltinTrait::BitOr => "BitOr",
            BuiltinTrait::BitXor => "BitXor",
            BuiltinTrait::Shl => "Shl",
            BuiltinTrait::Shr => "Shr",
            BuiltinTrait::Eq => "PartialEq",
            BuiltinTrait::Ord => "PartialOrd",
        }
    }
}

/// The set of built-in trait implementations known to the Phase-2 checker.
#[derive(Debug, Default)]
pub struct TraitDb;

impl TraitDb {
    /// Whether `ty` implements the built-in `trait_`.
    pub fn implements(trait_: BuiltinTrait, ty: &Ty) -> bool {
        let numeric = ty.is_numeric();
        match trait_ {
            BuiltinTrait::Add
            | BuiltinTrait::Sub
            | BuiltinTrait::Mul
            | BuiltinTrait::Div
            | BuiltinTrait::Rem => {
                // Floats and all integers implement the arithmetic traits.
                numeric
            }
            BuiltinTrait::BitAnd | BuiltinTrait::BitOr | BuiltinTrait::BitXor => ty.is_integer(),
            BuiltinTrait::Shl | BuiltinTrait::Shr => ty.is_integer(),
            BuiltinTrait::Eq => numeric || matches!(ty, Ty::Bool | Ty::Char | Ty::Str),
            BuiltinTrait::Ord => numeric || matches!(ty, Ty::Bool | Ty::Char),
        }
    }
}
