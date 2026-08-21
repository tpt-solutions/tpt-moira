//! Offline root-cause + minimal-fix proposer for the `moira-diff` tool.
//!
//! This is the "attach an LLM to propose root-cause + minimal fix per
//! divergence" piece of the AI-assisted diffing tool (Phase 3). It ships with a
//! fully offline heuristic proposer (no network, no dependencies) so the
//! feature works out of the box, and exposes a clean seam
//! ([`LlmProposer::with_backend`]) where a real LLM client can be plugged in —
//! the driver wires this to an OpenAI-compatible endpoint via `MOIRA_LLM_CMD`.
//!
//! The module also provides a tiny persisted triage queue ([`TriageQueue`]) that
//! records each divergence with a category *label* and the proposed fix, which
//! is the "triage queue + labels" follow-up.

use std::io::{BufRead, Write};
use std::path::Path;

use crate::diff::{classify, Divergence};

/// A pluggable root-cause backend: takes the prompt and returns the model reply.
type LlmBackend = Box<dyn Fn(&str) -> String + Send + Sync>;

/// A coarse error category used both for heuristic proposals and as a triage
/// label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    TypeMismatch,
    Borrow,
    UnknownSymbol,
    UnknownMethod,
    UnresolvedTrait,
    Overflow,
    Other,
}

impl Category {
    /// Stable machine-readable label (used as a triage label and in reports).
    pub fn label(self) -> &'static str {
        match self {
            Category::TypeMismatch => "type-mismatch",
            Category::Borrow => "borrow-check",
            Category::UnknownSymbol => "unknown-symbol",
            Category::UnknownMethod => "unknown-method",
            Category::UnresolvedTrait => "unresolved-trait",
            Category::Overflow => "overflow",
            Category::Other => "other",
        }
    }

    /// Heuristic baseline confidence that a proposal in this category is right.
    pub fn base_confidence(self) -> f64 {
        match self {
            Category::TypeMismatch => 0.7,
            Category::Borrow => 0.75,
            Category::UnknownSymbol => 0.8,
            Category::UnknownMethod => 0.6,
            Category::UnresolvedTrait => 0.6,
            Category::Overflow => 0.7,
            Category::Other => 0.4,
        }
    }
}

/// Map a single diagnostic line to a coarse [`Category`] using keyword heuristics.
pub fn classify_error(msg: &str) -> Category {
    let m = msg.to_ascii_lowercase();
    if m.contains("borrow") || m.contains("mutable") || m.contains("already borrowed") {
        Category::Borrow
    } else if m.contains("cannot find")
        || m.contains("undeclared")
        || m.contains("not found")
        || m.contains("undefined")
        || m.contains("cannot borrow")
    {
        Category::UnknownSymbol
    } else if m.contains("no method named")
        || m.contains("method")
        || m.contains("trait bound")
        || m.contains("not implemented")
    {
        Category::UnknownMethod
    } else if m.contains("trait") || m.contains("implement") {
        Category::UnresolvedTrait
    } else if m.contains("overflow") || m.contains("out of range") {
        Category::Overflow
    } else if m.contains("expected")
        || m.contains("mismatched")
        || m.contains("type")
        || m.contains("type mismatch")
    {
        Category::TypeMismatch
    } else {
        Category::Other
    }
}

/// A proposed root cause + minimal fix for a divergence.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub root_cause: String,
    pub minimal_fix: String,
    pub category: Category,
    pub confidence: f64,
}

impl Suggestion {
    /// Render the suggestion as a single human-readable line.
    pub fn describe(&self) -> String {
        format!(
            "[{}] {} → fix: {} (confidence {:.0}%)",
            self.category.label(),
            self.root_cause,
            self.minimal_fix,
            self.confidence * 100.0
        )
    }
}

/// The offline heuristic proposer. [`LlmProposer::with_backend`] lets the driver
/// attach a real LLM; when none is attached it falls back to [`offline`].
pub struct LlmProposer {
    backend: Option<LlmBackend>,
}

impl LlmProposer {
    /// A proposer that uses only the built-in heuristics.
    pub fn offline() -> LlmProposer {
        LlmProposer { backend: None }
    }

    /// Attach an LLM backend: a closure that takes the prompt and returns the
    /// model's reply. The driver wraps `MOIRA_LLM_CMD` (an OpenAI-compatible
    /// chat command) in exactly this shape.
    pub fn with_backend<F>(f: F) -> LlmProposer
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        LlmProposer {
            backend: Some(Box::new(f)),
        }
    }

    /// Produce a suggestion for `div`, given the diagnostics each compiler
    /// emitted. When an LLM backend is attached it is consulted and its reply is
    /// surfaced as the root cause; the category label and confidence still come
    /// from the offline heuristic so the result remains structured and testable.
    pub fn propose(&self, div: Divergence, moira: &[String], rustc: &[String]) -> Suggestion {
        let (category, root_cause, minimal_fix, confidence) = offline(div, moira, rustc);
        let mut root_cause = root_cause;
        if let Some(backend) = &self.backend {
            let prompt = build_prompt(div, moira, rustc);
            let reply = backend(&prompt);
            if !reply.trim().is_empty() {
                root_cause = format!("{} (LLM: {})", root_cause, reply.trim());
            }
        }
        Suggestion {
            root_cause,
            minimal_fix,
            category,
            confidence,
        }
    }
}

/// Build the prompt the LLM backend receives (also handy for tests/cli).
pub fn build_prompt(div: Divergence, moira: &[String], rustc: &[String]) -> String {
    let mut p = String::new();
    p.push_str(&format!("Moira vs rustc divergence: {}\n", div.describe()));
    p.push_str("Moira diagnostics:\n");
    for m in moira {
        p.push_str(&format!("  - {m}\n"));
    }
    p.push_str("rustc diagnostics:\n");
    for r in rustc {
        p.push_str(&format!("  - {r}\n"));
    }
    p.push_str("Propose the root cause and a minimal fix in one concise sentence.");
    p
}

/// The offline heuristic: returns (category, root_cause, minimal_fix, confidence).
fn offline(div: Divergence, moira: &[String], rustc: &[String]) -> (Category, String, String, f64) {
    match div {
        Divergence::MoiraAcceptsButRustcRejects => {
            // Moira is missing a check that rustc performs.
            (
                Category::Other,
                "Moira's checker is missing a rule that rustc enforces".to_string(),
                "extend the Lachesis type/borrow/coherence pass to cover this case".to_string(),
                0.6,
            )
        }
        Divergence::RustcAcceptsButMoiraRejects => {
            // Moira produced a (possibly spurious) diagnostic.
            let worst = moira
                .iter()
                .map(|m| classify_error(m))
                .max_by_key(|c| c.base_confidence() as i32)
                .unwrap_or(Category::Other);
            let (fix, cause) = match worst {
                Category::Borrow => (
                    "shorten the conflicting borrow's lifetime or clone the value".to_string(),
                    "a value is borrowed while a (mutable) borrow is still live".to_string(),
                ),
                Category::UnknownSymbol => (
                    "declare or import the missing item".to_string(),
                    "use of an undeclared name".to_string(),
                ),
                Category::UnknownMethod => (
                    "lower to a free/inherent function (method calls need Phase-2/3 resolution)"
                        .to_string(),
                    "a method call falls outside the type-checked subset".to_string(),
                ),
                Category::UnresolvedTrait => (
                    "implement the required trait or add the where-bound".to_string(),
                    "an operator/trait bound is not satisfied".to_string(),
                ),
                Category::Overflow => (
                    "use a wider integer type for the literal".to_string(),
                    "arithmetic/literal overflow".to_string(),
                ),
                Category::TypeMismatch => (
                    "annotate the type or adjust the literal suffix/cast".to_string(),
                    "operand/expected type disagreement".to_string(),
                ),
                Category::Other => (
                    "inspect the diagnostic and adjust the source or the checker".to_string(),
                    "Moira rejected a program rustc accepts".to_string(),
                ),
            };
            (worst, cause, fix, worst.base_confidence())
        }
        Divergence::BothReject => {
            // Both reject; check whether the categories line up.
            let moira_cat = moira
                .first()
                .map(|m| classify_error(m))
                .unwrap_or(Category::Other);
            let rustc_cat = rustc
                .first()
                .map(|m| classify_error(m))
                .unwrap_or(Category::Other);
            if moira_cat == rustc_cat && moira_cat != Category::Other {
                (
                    moira_cat,
                    format!(
                        "both compilers reject for the same reason ({})",
                        moira_cat.label()
                    ),
                    "align Moira's diagnostic wording with rustc's".to_string(),
                    moira_cat.base_confidence(),
                )
            } else {
                (
                    Category::Other,
                    "both reject, but the diagnostics disagree on the reason".to_string(),
                    "reconcile Moira's error category with rustc's".to_string(),
                    0.5,
                )
            }
        }
    }
}

/// A single persisted triage record (the "triage queue + labels" follow-up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageRecord {
    pub divergence: Divergence,
    pub label: String,
    pub root_cause: String,
    pub minimal_fix: String,
    pub source_hash: u64,
}

/// An append-only triage queue persisted as JSON-lines.
pub struct TriageQueue {
    path: std::path::PathBuf,
    records: Vec<TriageRecord>,
}

impl TriageQueue {
    /// Open (or initialize) a triage queue at `path`.
    pub fn open(path: &Path) -> std::io::Result<TriageQueue> {
        let mut records = Vec::new();
        if path.exists() {
            let file = std::fs::File::open(path)?;
            for line in std::io::BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(r) = triage_record_from_json(&line) {
                    records.push(r);
                }
            }
        }
        Ok(TriageQueue {
            path: path.to_path_buf(),
            records,
        })
    }

    /// Number of recorded divergences.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// All recorded records (read-only view).
    pub fn records(&self) -> &[TriageRecord] {
        &self.records
    }

    /// Append `record` and persist it.
    pub fn push(&mut self, record: TriageRecord) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = triage_record_to_json(&record);
        writeln!(f, "{line}")?;
        self.records.push(record);
        Ok(())
    }
}

/// Build a [`TriageRecord`] from a divergence, its diagnostics, and an [`LlmProposer`]
/// suggestion.
pub fn make_record(
    div: Divergence,
    moira: &[String],
    rustc: &[String],
    proposer: &LlmProposer,
    source_hash: u64,
) -> TriageRecord {
    let s = proposer.propose(div, moira, rustc);
    TriageRecord {
        divergence: div,
        label: s.category.label().to_string(),
        root_cause: s.root_cause,
        minimal_fix: s.minimal_fix,
        source_hash,
    }
}

/// Convenience: classify two compilers' diagnostics and, for each divergence,
/// build a triage record via `proposer`.
pub fn triage(
    moira: &[String],
    rustc: &[String],
    proposer: &LlmProposer,
    source_hash: u64,
) -> Vec<TriageRecord> {
    classify(moira, rustc)
        .divergences
        .into_iter()
        .map(|d| make_record(d, moira, rustc, proposer, source_hash))
        .collect()
}

/// Minimal, dependency-free JSON serialization for [`TriageRecord`] (a flat
/// object with one string enum, two strings, and a u64). This keeps the triage
/// queue inspectable without pulling in `serde`.
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn triage_record_to_json(r: &TriageRecord) -> String {
    format!(
        "{{\"divergence\":\"{}\",\"label\":\"{}\",\"root_cause\":\"{}\",\"minimal_fix\":\"{}\",\"source_hash\":{}}}",
        r.divergence.tag(),
        escape_json(&r.label),
        escape_json(&r.root_cause),
        escape_json(&r.minimal_fix),
        r.source_hash,
    )
}

/// Tolerant parser for the flat JSON object emitted by [`triage_record_to_json`].
/// Extracts each known field by its `"key":` prefix; tolerates field order.
fn triage_record_from_json(line: &str) -> Option<TriageRecord> {
    let get_str = |key: &str| -> Option<String> {
        let marker = format!("\"{key}\":");
        let idx = line.find(&marker)? + marker.len();
        let rest = &line[idx..];
        let start = rest.find('"')? + 1;
        let mut s = String::new();
        let mut chars = rest[start..].chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => {
                    if let Some(e) = chars.next() {
                        match e {
                            '"' => s.push('"'),
                            '\\' => s.push('\\'),
                            'n' => s.push('\n'),
                            'r' => s.push('\r'),
                            't' => s.push('\t'),
                            'u' => {
                                let hex: String = chars.by_ref().take(4).collect();
                                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                                    if let Some(ch) = char::from_u32(cp) {
                                        s.push(ch);
                                    }
                                }
                            }
                            other => s.push(other),
                        }
                    }
                }
                other => s.push(other),
            }
        }
        Some(s)
    };

    let tag = get_str("divergence")?;
    let divergence = Divergence::from_tag(&tag)?;
    let label = get_str("label")?;
    let root_cause = get_str("root_cause")?;
    let minimal_fix = get_str("minimal_fix")?;
    let hash_marker = "\"source_hash\":";
    let idx = line.find(hash_marker)? + hash_marker.len();
    let rest = line[idx..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let source_hash = digits.parse::<u64>().ok()?;
    Some(TriageRecord {
        divergence,
        label,
        root_cause,
        minimal_fix,
        source_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_handles_each_divergence() {
        let p = LlmProposer::offline();
        let m = p.propose(
            Divergence::RustcAcceptsButMoiraRejects,
            &["moira: cannot borrow as mutable".into()],
            &[],
        );
        assert_eq!(m.category, Category::Borrow);
        assert!(m.confidence > 0.5);

        let a = p.propose(
            Divergence::MoiraAcceptsButRustcRejects,
            &[],
            &["error: mismatched types".into()],
        );
        assert_eq!(a.category, Category::Other);
    }

    #[test]
    fn both_reject_same_category() {
        let p = LlmProposer::offline();
        let s = p.propose(
            Divergence::BothReject,
            &["moira: type mismatch".into()],
            &["error: expected integer, found &str".into()],
        );
        assert_eq!(s.category, Category::TypeMismatch);
    }

    #[test]
    fn triage_queue_persists_and_reloads() {
        let tmp = std::env::temp_dir().join("moira_triage_test.jsonl");
        let _ = std::fs::remove_file(&tmp);
        {
            let mut q = TriageQueue::open(&tmp).unwrap();
            assert!(q.is_empty());
            let rec = triage(
                &["moira: cannot borrow as mutable".into()],
                &[],
                &LlmProposer::offline(),
                12345,
            )
            .into_iter()
            .next()
            .unwrap();
            q.push(rec).unwrap();
            assert_eq!(q.len(), 1);
        }
        let reloaded = TriageQueue::open(&tmp).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.records()[0].label, "borrow-check");
        assert_eq!(reloaded.records()[0].source_hash, 12345);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn llm_backend_is_consulted() {
        let p = LlmProposer::with_backend(|_prompt| "use a wider type".to_string());
        let s = p.propose(
            Divergence::RustcAcceptsButMoiraRejects,
            &["moira: overflow".into()],
            &[],
        );
        assert!(s.root_cause.contains("LLM: use a wider type"));
    }
}
