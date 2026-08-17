//! Foundation for the Phase-3 `moira-diff` divergence harness / AI diffing tool.
//!
//! Given the diagnostics Moira and `rustc` each produce for the same source,
//! [`classify`] buckets the result into a small set of divergences. This is the
//! structured data the richer CI job and the AI root-cause layer will consume.
//!
//! [`Allowlist`] is the persistence half of that tool: a tracked, known set of
//! divergences that the project has decided are acceptable (subset-coverage gaps
//! that are not yet worth a triage fix). `moira --diff` can consult an allowlist
//! so that only *new* divergences fail CI, and `--update-allowlist` can persist
//! the divergences seen in a run.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::Path;

/// A class of disagreement between Moira and `rustc` on the same input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Divergence {
    /// Moira reported no errors but `rustc` did (Moira is missing a check).
    MoiraAcceptsButRustcRejects,
    /// `rustc` reported no errors but Moira did (Moira is over-strict / spurious).
    RustcAcceptsButMoiraRejects,
    /// Both rejected, but their diagnostics may differ in wording/span.
    BothReject,
}

impl Divergence {
    /// Stable, human-readable tag used in allowlist files and CLI output.
    pub fn tag(self) -> &'static str {
        match self {
            Divergence::MoiraAcceptsButRustcRejects => "moira-accepts",
            Divergence::RustcAcceptsButMoiraRejects => "rustc-accepts",
            Divergence::BothReject => "both-reject",
        }
    }

    /// Parse a [`Divergence::tag`] back into a variant.
    pub fn from_tag(s: &str) -> Option<Divergence> {
        match s {
            "moira-accepts" => Some(Divergence::MoiraAcceptsButRustcRejects),
            "rustc-accepts" => Some(Divergence::RustcAcceptsButMoiraRejects),
            "both-reject" => Some(Divergence::BothReject),
            _ => None,
        }
    }

    /// Short prose form for CLI reporting.
    pub fn describe(self) -> &'static str {
        match self {
            Divergence::MoiraAcceptsButRustcRejects => "Moira accepts but rustc rejects",
            Divergence::RustcAcceptsButMoiraRejects => "rustc accepts but Moira rejects",
            Divergence::BothReject => "both reject (messages may differ)",
        }
    }
}

/// How a single allowlist entry identifies the source it applies to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum AllowKey {
    /// Match by source file path (relative or absolute).
    Path(String),
    /// Match by the content-addressed hash of the source text.
    Hash(u64),
}

impl AllowKey {
    fn render(&self) -> String {
        match self {
            AllowKey::Path(p) => format!("path:{p}"),
            AllowKey::Hash(h) => format!("hash:{h}"),
        }
    }

    fn parse(s: &str) -> Option<AllowKey> {
        let (prefix, rest) = s.split_once(':')?;
        match prefix {
            "path" => Some(AllowKey::Path(rest.to_string())),
            "hash" => rest.parse::<u64>().ok().map(AllowKey::Hash),
            _ => None,
        }
    }
}

/// A persistable set of known divergences, keyed by source identity + kind.
///
/// Entries are stored as `kind:identity` lines (e.g. `moira-accepts:path:foo.rs`
/// or `both-reject:hash:123456789`), one per line, with `#` comments. The
/// identity may be a [`AllowKey::Path`] or a [`AllowKey::Hash`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: BTreeSet<(AllowKey, Divergence)>,
}

impl Allowlist {
    /// The default file name `moira --diff` looks for in the working directory.
    pub const DEFAULT_FILE: &'static str = ".moira-divergence-allowlist";

    /// Load an allowlist from `path`. A missing file is treated as an empty
    /// allowlist (so the tool works before any divergences have been recorded).
    pub fn load(path: &Path) -> std::io::Result<Allowlist> {
        if !path.exists() {
            return Ok(Allowlist::default());
        }
        let file = std::fs::File::open(path)?;
        let mut entries = BTreeSet::new();
        for line in std::io::BufReader::new(file).lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((tag, key)) = line.split_once(':') {
                let Some(kind) = Divergence::from_tag(tag) else {
                    continue;
                };
                if let Some(k) = AllowKey::parse(key) {
                    entries.insert((k, kind));
                }
            }
        }
        Ok(Allowlist { entries })
    }

    /// Write the allowlist back to `path` in stable (sorted) line order.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut buf = String::from(
            "# moira-diff known-divergence allowlist\n\
             # format: <kind>:<identity>  (kind = moira-accepts | rustc-accepts | both-reject)\n\
             #         identity = path:<file> | hash:<content-hash>\n",
        );
        for (key, kind) in &self.entries {
            buf.push_str(&format!("{}:{}\n", kind.tag(), key.render()));
        }
        let mut f = std::fs::File::create(path)?;
        f.write_all(buf.as_bytes())?;
        Ok(())
    }

    /// Does the allowlist cover `kind` for a source identified by `path` (if any)
    /// and `hash` (always)?
    pub fn allows(&self, path: Option<&Path>, hash: u64, kind: Divergence) -> bool {
        if let Some(p) = path {
            let key = p.to_string_lossy().into_owned();
            if self.entries.contains(&(AllowKey::Path(key), kind)) {
                return true;
            }
        }
        self.entries.contains(&(AllowKey::Hash(hash), kind))
    }

    /// Record `kind` for a source identified by `path` (if any) and `hash`.
    pub fn insert(&mut self, path: Option<&Path>, hash: u64, kind: Divergence) {
        if let Some(p) = path {
            let key = p.to_string_lossy().into_owned();
            self.entries.insert((AllowKey::Path(key), kind));
        }
        self.entries.insert((AllowKey::Hash(hash), kind));
    }

    /// Number of tracked entries (for reporting).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the allowlist has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
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

    #[test]
    fn divergence_tags_roundtrip() {
        for d in [
            Divergence::MoiraAcceptsButRustcRejects,
            Divergence::RustcAcceptsButMoiraRejects,
            Divergence::BothReject,
        ] {
            assert_eq!(Divergence::from_tag(d.tag()), Some(d));
        }
        assert_eq!(Divergence::from_tag("nope"), None);
    }

    #[test]
    fn allowlist_matches_by_path_and_hash() {
        let tmp = std::env::temp_dir().join("moira_allowlist_test.toml");
        let _ = std::fs::remove_file(&tmp);
        let mut a = Allowlist::default();
        a.insert(
            Some(std::path::Path::new("foo.rs")),
            42,
            Divergence::BothReject,
        );
        a.save(&tmp).unwrap();
        let loaded = Allowlist::load(&tmp).unwrap();
        assert_eq!(loaded, a);
        assert!(loaded.allows(
            Some(std::path::Path::new("foo.rs")),
            999,
            Divergence::BothReject
        ));
        // Hash match works even without a path (e.g. stdin).
        assert!(loaded.allows(None, 42, Divergence::BothReject));
        // Different kind on the same file is not allowed.
        assert!(!loaded.allows(
            Some(std::path::Path::new("foo.rs")),
            42,
            Divergence::MoiraAcceptsButRustcRejects
        ));
        // Unknown file + hash is not allowed.
        assert!(!loaded.allows(
            Some(std::path::Path::new("bar.rs")),
            7,
            Divergence::BothReject
        ));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn allowlist_missing_file_is_empty() {
        let a =
            Allowlist::load(std::path::Path::new("/no/such/.moira-divergence-allowlist")).unwrap();
        assert!(a.is_empty());
    }
}

/// Error-message parity audit against `rustc` (Phase-4 "error message parity
/// audit").
///
/// Two compilers "agree" on a diagnostic when, for a `rustc` error, Moira emits
/// an error in the *same coarse category* (e.g. both are borrow errors) and the
/// wording overlaps. The [`ParityReport`] quantifies this so a CI job can track
/// Moira's diagnostic fidelity over time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParityReport {
    /// Number of `rustc` error lines considered.
    pub rustc_count: usize,
    /// Number of Moira error lines considered.
    pub moira_count: usize,
    /// How many `rustc` errors had a Moira error in the same category.
    pub category_matches: usize,
    /// Average best token-overlap (Jaccard) between each `rustc` error and the
    /// most similar Moira error, in `[0, 1]`.
    pub wording_similarity: f64,
}

impl ParityReport {
    /// A single 0..1 score blending category coverage and wording similarity.
    pub fn score(&self) -> f64 {
        let coverage = if self.rustc_count == 0 {
            1.0
        } else {
            self.category_matches as f64 / self.rustc_count as f64
        };
        0.6 * coverage + 0.4 * self.wording_similarity
    }

    /// Human-readable summary line (also consumed by the triage/parity report).
    pub fn describe(&self) -> String {
        format!(
            "parity: rustc_errors={} moira_errors={} category_matches={} wording_similarity={:.2} score={:.2}",
            self.rustc_count,
            self.moira_count,
            self.category_matches,
            self.wording_similarity,
            self.score(),
        )
    }
}

fn word_set(msg: &str) -> std::collections::HashSet<String> {
    msg.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() >= 3)
        .collect()
}

fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        1.0
    } else {
        inter / union
    }
}

/// Score diagnostic parity between Moira's and `rustc`'s errors.
pub fn parity(moira: &[String], rustc: &[String]) -> ParityReport {
    let moira_cats: Vec<_> = moira
        .iter()
        .map(|m| crate::propose::classify_error(m))
        .collect();
    let moira_sets: Vec<_> = moira.iter().map(|m| word_set(m)).collect();

    let mut category_matches = 0usize;
    let mut sim_sum = 0.0f64;
    for r in rustc {
        let rcat = crate::propose::classify_error(r);
        if moira_cats.contains(&rcat) {
            category_matches += 1;
        }
        let rset = word_set(r);
        let best = moira_sets
            .iter()
            .map(|mset| jaccard(&rset, mset))
            .fold(0.0f64, f64::max);
        sim_sum += best;
    }
    let wording_similarity = if rustc.is_empty() {
        1.0
    } else {
        sim_sum / rustc.len() as f64
    };

    ParityReport {
        rustc_count: rustc.len(),
        moira_count: moira.len(),
        category_matches,
        wording_similarity,
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;

    #[test]
    fn full_parity_when_no_errors() {
        let r = parity(&[], &[]);
        assert_eq!(r.score(), 1.0);
    }

    #[test]
    fn category_match_boosts_score() {
        let r = parity(
            &["moira: cannot borrow as mutable".into()],
            &["error[E0502]: cannot borrow `x` as mutable".into()],
        );
        assert_eq!(r.category_matches, 1);
        assert!(r.wording_similarity > 0.0);
        assert!(r.score() > 0.5);
    }

    #[test]
    fn mismatch_scores_low() {
        let r = parity(
            &["moira: overflow".into()],
            &["error: mismatched types".into()],
        );
        assert_eq!(r.category_matches, 0);
        assert!(r.score() < 0.6);
    }
}
