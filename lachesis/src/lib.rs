//! Lachesis — Moira's type system and borrow checker.
//!
//! Named after the Fate who measured the thread. Lachesis implements:
//! * **Type inference** ([`infer`]) for the Phase-1 subset — a direct,
//!   constraint-checking inference engine over the Clotho AST.
//! * **Trait resolution** ([`traits`]) — a built-in trait database the checker
//!   queries for operator applicability (the seam for the Phase-3 solver).
//! * **Borrow checking** ([`borrow`]) — a fast-path + NLL-style CFG liveness
//!   analysis over the Atropos MIR.
//! * **Incremental caching** ([`cache`]) — a disk-backed, content-addressed
//!   store with Salsa-like memoization.
//!
//! The top-level [`check`] function wires all three Fates together: it
//! type-checks the Clotho AST, lowers through Atropos to MIR, and runs the
//! borrow checker — exactly the end-to-end Lachesis integration the roadmap
//! calls for.

pub mod borrow;
pub mod cache;
pub mod diff;
pub mod solver;
pub mod infer;
pub mod traits;
pub mod ty;

pub use borrow::borrow_check;
pub use cache::{content_hash, Cache, IncrementalCache};
pub use infer::TypeChecker;
pub use traits::{BuiltinTrait, TraitDb};
pub use ty::Ty;

use clotho::ast::Program;
use clotho::error::Diag;

/// Type-check a whole program, returning all diagnostics on failure.
pub fn type_check(prog: &Program) -> Result<(), Vec<Diag>> {
    TypeChecker::check_program(prog)
}

/// Lower `prog` to MIR and borrow-check it.
pub fn borrow_check_program(prog: &Program) -> Result<(), Vec<Diag>> {
    let mir = atropos::lower_program(prog)
        .map_err(|e| vec![Diag::error(format!("lowering error: {e}"), None)])?;
    borrow_check(&mir)
}

/// Full end-to-end Lachesis pass: type inference + MIR lowering + borrow check
/// + trait-coherence / well-formedness validation.
///
/// Returns `Ok(())` only when the program is well-typed, borrow-safe, and its
/// user `impl`s are coherent and well-formed.
pub fn check(prog: &Program) -> Result<(), Vec<Diag>> {
    let mut errs: Vec<Diag> = Vec::new();
    match type_check(prog) {
        Ok(()) => {}
        Err(mut e) => errs.append(&mut e),
    }
    match borrow_check_program(prog) {
        Ok(()) => {}
        Err(mut e) => errs.append(&mut e),
    }
    errs.extend(coherence_report(prog));
    errs.extend(well_formedness_report(prog));
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

/// Build a trait solver over the built-in operator traits plus the user `impl`
/// clauses declared in `prog`.
pub fn build_solver(prog: &Program) -> solver::Solver {
    solver::Solver::new(solver::program_with_impls(prog))
}

/// Whether `ty` implements the user-defined trait `trait_name` in `prog`,
/// according to the trait solver's impl search.
pub fn user_impl_implements(prog: &Program, trait_name: &str, ty: &ty::Ty) -> bool {
    build_solver(prog).implements_trait(solver::TraitId::User(trait_name.to_string()), ty)
}

/// Coherence / overlap report for the user `impl`s in `prog`: any two `impl`s of
/// the same trait whose self-types overlap (unify).
pub fn coherence_report(prog: &Program) -> Vec<Diag> {
    solver::coherence_report(prog)
        .into_iter()
        .map(|(span, msg)| Diag::error(msg, Some(span)))
        .collect()
}

/// Well-formedness report for `impl` where-clauses / parameter bounds.
pub fn well_formedness_report(prog: &Program) -> Vec<Diag> {
    solver::well_formedness_report(prog)
        .into_iter()
        .map(|(span, msg)| Diag::error(msg, Some(span)))
        .collect()
}

/// Normalize the associated type `<self_ty as trait_name>::assoc` to its
/// projected type term, per the `type` declarations in `prog`'s `impl`s.
pub fn normalize_assoc_type(
    prog: &Program,
    trait_name: &str,
    assoc: &str,
    self_ty: &ty::Ty,
) -> Option<solver::Term> {
    let proj = solver::Term::Proj(
        trait_name.to_string(),
        assoc.to_string(),
        Box::new(solver::Term::Concr(self_ty.clone())),
    );
    solver::Solver::new(solver::program_with_impls(prog)).normalize(&proj)
}
