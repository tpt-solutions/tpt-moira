//! Foundation for the Phase-3 `moira-diff` divergence harness / AI diffing tool.
//!
//! Given the diagnostics Moira and `rustc` each produce for the same source,
//! [`classify`] buckets the result into a small set of divergences. This is the
//! structured data the richer CI job and the AI root-cause layer will consume.

/// A class of disagreement between Moira and `rustc` on the same input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divergence {
    /// Moira reported no errors but `rustc` did (Moira is missing a check).
    MoiraAcceptsButRustcRejects,
    /// `rustc` reported no errors but Moira did (Moira is over-strict / spurious).
    RustcAcceptsButMoiraRejects,
    /// Both rejected, but their diagnostics may differ in wording/span.
    BothReject,
}

/// The result of comparing two compilers' diagnostics for one source file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffReport {
    pub moira_errors: Vec<String>,
    pub rustc_errors: Vec<String>,
    pub divergences: Vec<Divergence>,
}

/// Classify the relationship between Moira's and `rustc`'s diagnostics.
///
/// `moira_errors` / `rustc_errors` are the non-empty error lists each compiler
/// produced (empty means "accepted").
pub fn classify(moira_errors: &[String], rustc_errors: &[String]) -> DiffReport {
    let moira = !moira_errors.is_empty();
    let rustc = !rustc_errors.is_empty();
    let mut divergences = Vec::new();
    match (moira, rustc) {
        (false, true) => divergences.push(Divergence::MoiraAcceptsButRustcRejects),
        (true, false) => divergences.push(Divergence::RustcAcceptsButMoiraRejects),
        (true, true) => divergences.push(Divergence::BothReject),
        (false, false) => {}
    }
    DiffReport {
        moira_errors: moira_errors.to_vec(),
        rustc_errors: rustc_errors.to_vec(),
        divergences,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agreement_when_both_accept() {
        let r = classify(&[], &[]);
        assert!(r.divergences.is_empty());
    }

    #[test]
    fn moira_missing_error() {
        let r = classify(&[], &["error: mismatched types".into()]);
        assert_eq!(r.divergences, vec![Divergence::MoiraAcceptsButRustcRejects]);
    }

    #[test]
    fn moira_spurious_error() {
        let r = classify(&["moira: cannot borrow as mutable".into()], &[]);
        assert_eq!(r.divergences, vec![Divergence::RustcAcceptsButMoiraRejects]);
    }

    #[test]
    fn both_reject_is_flagged() {
        let r = classify(
            &["moira: type mismatch".into()],
            &["error: type mismatch".into()],
        );
        assert_eq!(r.divergences, vec![Divergence::BothReject]);
        assert_eq!(r.moira_errors.len(), 1);
        assert_eq!(r.rustc_errors.len(), 1);
    }
}
