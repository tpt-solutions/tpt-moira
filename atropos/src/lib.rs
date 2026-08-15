//! Atropos — Moira's optimizer and code generator.
//!
//! Atropos lowers the Clotho AST into a lightweight, SSA-style Mid-level IR
//! ([`mir`]) and emits code through a backend. For Phase 1 the reference
//! backend is the Stage-0 C transpiler ([`codegen_c`]), which matches the
//! bootstrap path described in the design doc: a macro-free Moira subset is
//! translated to C and then compiled to a native binary. A from-scratch native
//! SSA backend is the Phase-1/Phase-4 target and slots in behind the same
//! [`lower`] + emit interface.

pub mod codegen_c;
pub mod lower;
pub mod mir;

pub use lower::{lower_program, LowerError};
pub use mir::Program as MirProgram;
