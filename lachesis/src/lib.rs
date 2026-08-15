//! Lachesis — Moira's type system and borrow checker.
//!
//! Named after the Fate who measured the thread. This subsystem implements type
//! inference, trait resolution, and the Non-Lexical Lifetime (NLL) borrow graph
//! solver. It is scheduled for Phase 2 (Months 9-16) of the roadmap; for now it
//! provides a placeholder module so the workspace compiles.

/// Placeholder for the Phase-2 type-inference context.
#[derive(Debug, Default)]
pub struct Lachesis;

impl Lachesis {
    /// Reports whether the type-checker is ready. Always `false` in Phase 1.
    pub fn is_ready(&self) -> bool {
        false
    }
}
