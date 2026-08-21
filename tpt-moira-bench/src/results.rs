//! Dependency-free persistence and regression analysis for benchmark results.
//!
//! Results are stored as plain text blocks (no external crates, so the harness
//! builds offline). Each recorded run is one `[run]` block; a history file is the
//! concatenation of such blocks separated by blank lines.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Default directory (relative to the current working directory) where run
/// history is appended. Override with `--results <dir>`.
pub const DEFAULT_RESULTS_DIR: &str = "moira-bench-results";

/// A single program's measured numbers for one run.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub program: String,
    pub moira_ms: Option<f64>,
    pub rustc_ms: Option<f64>,
    pub speedup: Option<f64>,
}

/// One complete benchmark run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    /// Opaque timestamp string (Unix seconds as produced by [`now_timestamp`]).
    pub timestamp: String,
    /// Short git revision, if discoverable.
    pub commit: Option<String>,
    /// Geometric-mean speedup (rustc / moira) across programs with both timings.
    pub geomean: Option<f64>,
    pub entries: Vec<Entry>,
}

/// A detected slowdown between two runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Regression {
    pub program: String,
    pub prev_ms: f64,
    pub new_ms: f64,
    /// Percent increase in Moira compile time: (new - prev) / prev * 100.
    pub pct: f64,
}

/// Current Unix time in seconds, as a string.
pub fn now_timestamp() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs().to_string(),
        Err(_) => "0".to_string(),
    }
}

/// Best-effort short git revision of the workspace, or `None`.
pub fn current_commit() -> Option<String> {
    let root = workspace_root()?;
    let out = Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Walk up from the current executable to find the workspace root (a `.git`).
fn workspace_root() -> Option<PathBuf> {
    let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

pub fn history_path(dir: &Path) -> PathBuf {
    dir.join("history.mbh")
}

/// Load all recorded runs from `dir`, oldest first. Missing file => empty.
pub fn load_history(dir: &Path) -> Vec<RunRecord> {
    let path = history_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_history(&text),
        Err(_) => Vec::new(),
    }
}

/// Append `run` to the history file in `dir`, creating the directory/file.
pub fn append_run(dir: &Path, run: &RunRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut text = String::new();
    if Path::new(&history_path(dir)).exists() {
        text = std::fs::read_to_string(history_path(dir))?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str(&serialize_run(run));
    std::fs::write(history_path(dir), text)
}

/// Serialize one run as a `[run]` block.
pub fn serialize_run(run: &RunRecord) -> String {
    let mut s = String::new();
    s.push_str("[run]\n");
    s.push_str(&format!("ts={}\n", run.timestamp));
    if let Some(c) = &run.commit {
        s.push_str(&format!("commit={}\n", c));
    }
    if let Some(g) = run.geomean {
        s.push_str(&format!("geomean={:.4}\n", g));
    }
    for e in &run.entries {
        let m = opt(e.moira_ms);
        let r = opt(e.rustc_ms);
        let sp = opt(e.speedup);
        s.push_str(&format!("e={},{},{},{}\n", e.program, m, r, sp));
    }
    s.push('\n');
    s
}

fn opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.4}"),
        None => String::new(),
    }
}

/// Parse a history file's text into a list of runs (oldest first).
pub fn parse_history(text: &str) -> Vec<RunRecord> {
    let mut runs: Vec<RunRecord> = Vec::new();
    let mut cur: Option<RunRecord> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            if let Some(r) = cur.take() {
                runs.push(r);
            }
            continue;
        }
        if line == "[run]" {
            if let Some(r) = cur.take() {
                runs.push(r);
            }
            cur = Some(RunRecord {
                timestamp: String::new(),
                commit: None,
                geomean: None,
                entries: Vec::new(),
            });
            continue;
        }
        let (key, val) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let rec = match cur.as_mut() {
            Some(r) => r,
            None => continue,
        };
        match key {
            "ts" => rec.timestamp = val.to_string(),
            "commit" => rec.commit = Some(val.to_string()),
            "geomean" => rec.geomean = val.parse().ok(),
            "e" => {
                let parts: Vec<&str> = val.split(',').collect();
                if parts.len() == 4 {
                    rec.entries.push(Entry {
                        program: parts[0].to_string(),
                        moira_ms: parse_f64(parts[1]),
                        rustc_ms: parse_f64(parts[2]),
                        speedup: parse_f64(parts[3]),
                    });
                }
            }
            _ => {}
        }
    }
    if let Some(r) = cur.take() {
        runs.push(r);
    }
    runs
}

fn parse_f64(s: &str) -> Option<f64> {
    if s.is_empty() {
        None
    } else {
        s.parse().ok()
    }
}

// ---------------------------------------------------------------------------
// Regression analysis (pure functions)
// ---------------------------------------------------------------------------

/// Detect per-program compile-time regressions: any program present in both runs
/// whose Moira time increased by at least `threshold_pct` percent.
pub fn detect_regressions(
    prev: &RunRecord,
    new: &RunRecord,
    threshold_pct: f64,
) -> Vec<Regression> {
    let mut out = Vec::new();
    for pe in &prev.entries {
        let pm = match pe.moira_ms {
            Some(x) => x,
            None => continue,
        };
        if let Some(ne) = new.entries.iter().find(|e| e.program == pe.program) {
            if let Some(nm) = ne.moira_ms {
                let pct = (nm - pm) / pm * 100.0;
                if pct >= threshold_pct {
                    out.push(Regression {
                        program: pe.program.clone(),
                        prev_ms: pm,
                        new_ms: nm,
                        pct,
                    });
                }
            }
        }
    }
    out
}

/// Detect a geometric-mean speedup regression: a drop of at least
/// `threshold_pct` percent versus the previous run. Returns
/// `(prev_geomean, new_geomean, pct_drop)`.
pub fn geomean_regression(
    prev: &RunRecord,
    new: &RunRecord,
    threshold_pct: f64,
) -> Option<(f64, f64, f64)> {
    let (p, n) = match (prev.geomean, new.geomean) {
        (Some(p), Some(n)) => (p, n),
        _ => return None,
    };
    if p <= 0.0 {
        return None;
    }
    let pct_drop = (p - n) / p * 100.0;
    if pct_drop >= threshold_pct {
        Some((p, n, pct_drop))
    } else {
        None
    }
}

/// Render a Unix-seconds timestamp as `YYYY-MM-DD HH:MM:SS UTC`.
pub fn format_timestamp(ts: &str) -> String {
    let secs: i64 = match ts.parse() {
        Ok(s) => s,
        Err(_) => return ts.to_string(),
    };
    const DAY: i64 = 86400;
    let days = secs / DAY;
    let mut sod = secs % DAY;
    if sod < 0 {
        sod += DAY;
    }
    // civil_from_days (Hinnant), valid for post-1970 dates.
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let h = sod / 3600;
    let mn = (sod % 3600) / 60;
    let s = sod % 60;
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", y, m, d, h, mn, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(program: &str, moira: f64) -> Entry {
        Entry {
            program: program.to_string(),
            moira_ms: Some(moira),
            rustc_ms: Some(moira * 2.0),
            speedup: Some(2.0),
        }
    }

    fn run(ts: &str, geomean: f64, entries: Vec<Entry>) -> RunRecord {
        RunRecord {
            timestamp: ts.to_string(),
            commit: None,
            geomean: Some(geomean),
            entries,
        }
    }

    #[test]
    fn detects_per_program_regression() {
        let prev = run("1", 2.0, vec![entry("small", 100.0)]);
        let new = run("2", 2.0, vec![entry("small", 150.0)]);
        let r = detect_regressions(&prev, &new, 10.0);
        assert_eq!(r.len(), 1);
        assert!((r[0].pct - 50.0).abs() < 1e-6);
    }

    #[test]
    fn ignores_improvement_and_small_change() {
        let prev = run("1", 2.0, vec![entry("small", 100.0)]);
        let faster = run("2", 2.0, vec![entry("small", 90.0)]);
        let slight = run("3", 2.0, vec![entry("small", 105.0)]);
        assert!(detect_regressions(&prev, &faster, 10.0).is_empty());
        assert!(detect_regressions(&prev, &slight, 10.0).is_empty());
    }

    #[test]
    fn detects_geomean_drop() {
        let prev = run("1", 7.0, vec![entry("small", 100.0)]);
        let new = run("2", 5.0, vec![entry("small", 100.0)]);
        let r = geomean_regression(&prev, &new, 10.0);
        assert!(r.is_some());
        let (p, n, drop) = r.unwrap();
        assert!((p - 7.0).abs() < 1e-9);
        assert!((n - 5.0).abs() < 1e-9);
        assert!((drop - (2.0 / 7.0 * 100.0)).abs() < 1e-6);
    }

    #[test]
    fn serialize_round_trips() {
        let rec = run("1700000000", 6.9, vec![entry("small", 1.23)]);
        let text = serialize_run(&rec);
        let back = parse_history(&text);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].geomean, Some(6.9));
        assert_eq!(back[0].entries[0].moira_ms, Some(1.23));
        assert_eq!(back[0].timestamp, "1700000000");
    }

    #[test]
    fn parse_multiple_runs() {
        let a = serialize_run(&run("1", 6.9, vec![entry("small", 1.0)]));
        let b = serialize_run(&run("2", 7.1, vec![entry("small", 0.9)]));
        let text = format!("{a}\n{b}");
        let runs = parse_history(&text);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].geomean, Some(6.9));
        assert_eq!(runs[1].geomean, Some(7.1));
    }
}
