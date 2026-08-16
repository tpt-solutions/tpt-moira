//! Moira benchmark harness — Phase-1 "10x faster than `rustc`" validation.
//!
//! Compiles a fixed battery of Phase-1-subset programs (which are also valid
//! stock Rust) two ways and reports median end-to-end compile time:
//!
//! * **Moira** — the full Clotho front-end + Atropos mid-end (`--emit ssa`, which
//!   runs lex → parse → macro-expand → MIR lower → SSA build → optimize). No
//!   external assembler/linker is involved, so this isolates Moira's own work.
//! * **`rustc`** — `rustc --emit=metadata`, i.e. parse + full type inference +
//!   borrow check + codegen to metadata, the closest apples-to-apples "compile
//!   this file" invocation that does not require a system linker.
//!
//! The reported *speedup* is `rustc_ms / moira_ms`. Because Moira deliberately
//! defers type inference and borrow checking to Phase 2, this is not yet a
//! like-for-like comparison of *semantic* compilation — it measures the cost of
//! the front-end + mid-end pipeline Moira ships today. The harness exists to
//! track that number as the compiler grows and as incremental caching (Phase 2)
//! and parallelization land.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod results;
use results::{current_commit, detect_regressions, geomean_regression, load_history, now_timestamp,
              append_run, history_path, format_timestamp, Entry, RunRecord, DEFAULT_RESULTS_DIR};

/// Percent slowdown vs. the previous run that counts as a regression.
const REGRESSION_PCT: f64 = 10.0;

/// Number of timed iterations per program (median is reported).
const ITERS: usize = 30;
/// A short warm-up run excluded from measurements (JIT/cache effects).
const WARMUP: usize = 3;

const SMALL: &str = r#"
fn add(a: i32, b: i32) -> i32 { a + b }
fn main() {
    let x = 1 + 2;
    let y = add(x, 40);
    let _ = y;
}
"#;

const MEDIUM: &str = r#"
fn fib(n: i32) -> i32 {
    let mut a = 0;
    let mut b = 1;
    let mut i = 0;
    while i < n {
        let t = a + b;
        a = b;
        b = t;
        i = i + 1;
    }
    a
}
fn main() {
    let mut total = 0;
    for i in 0..100 {
        total = total + fib(i);
    }
    let _ = total;
}
"#;

const LARGE: &str = r#"
struct Point { x: i32, y: i32 }

fn dist2(p: Point, q: Point) -> i32 {
    let dx = p.x - q.x;
    let dy = p.y - q.y;
    dx * dx + dy * dy
}

fn compute() -> i32 {
    let mut s = 0;
    for i in 0..50 {
        for j in 0..50 {
            let p = Point { x: i, y: j };
            let q = Point { x: j, y: i };
            s = s + dist2(p, q);
        }
    }
    s
}

fn helper(a: i32, b: i32, c: i32, d: i32) -> i32 {
    let mut m = a;
    if b > m { m = b; }
    if c > m { m = c; }
    if d > m { m = d; }
    m
}

fn main() {
    let r = compute();
    let m = helper(3, 7, 2, 9);
    let _ = r + m;
}
"#;

const BENCHMARKS: &[(&str, &str)] = &[("small", SMALL), ("medium", MEDIUM), ("large", LARGE)];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut record = false;
    let mut dashboard = false;
    let mut results_dir = PathBuf::from(DEFAULT_RESULTS_DIR);

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--record" => record = true,
            "--dashboard" => dashboard = true,
            "--results" => match args.get(i + 1) {
                Some(d) => {
                    results_dir = PathBuf::from(d);
                    i += 1;
                }
                None => {
                    eprintln!("error: --results requires a directory argument");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("error: unknown argument `{other}`");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if dashboard {
        print_dashboard(&results_dir);
        return;
    }

    run_benchmark(record, &results_dir);

    if moira_is_missing() {
        eprintln!("\nwarning: `moira` binary not found; run `cargo build` first.");
    }
    if rustc_is_missing() {
        eprintln!("\nwarning: `rustc` not found; skipping rustc column.");
    }
}

/// Run the benchmark battery, print the table, and (when `record`) persist the
/// run and report any regressions versus the previous recorded run.
fn run_benchmark(record: bool, results_dir: &Path) {
    let moira = find_moira();
    let rustc = find_rustc();

    println!("Moira vs rustc — Phase-1 subset compile-time benchmark");
    println!("({ITERS} iterations, median reported; Moira = front-end + SSA mid-end)\n");
    println!(
        "{:<8} {:>12} {:>12} {:>10}",
        "program", "moira(ms)", "rustc(ms)", "speedup"
    );
    println!("{}", "-".repeat(46));

    let mut ratios: Vec<f64> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    for (name, src) in BENCHMARKS {
        let file = write_temp("rs", src);

        let moira_ms = moira
            .as_ref()
            .and_then(|bin| time_cmd(bin, &moira_args(&file), ITERS));
        let rustc_ms = rustc
            .as_ref()
            .and_then(|bin| time_cmd(bin, &rustc_args(&file), ITERS));

        let (moira_s, rustc_s, ratio_s, speedup) = match (moira_ms, rustc_ms) {
            (Some(m), Some(r)) => {
                let ratio = r / m;
                ratios.push(ratio);
                (
                    fmt(m),
                    fmt(r),
                    format!("{ratio:>9.2}x"),
                    Some(ratio),
                )
            }
            (Some(m), None) => (fmt(m), "  n/a".to_string(), "  n/a".to_string(), None),
            (None, Some(r)) => ("  n/a".to_string(), fmt(r), "  n/a".to_string(), None),
            (None, None) => (
                "  n/a".to_string(),
                "  n/a".to_string(),
                "  n/a".to_string(),
                None,
            ),
        };
        println!("{:<8} {:>12} {:>12} {:>10}", name, moira_s, rustc_s, ratio_s);

        entries.push(Entry {
            program: (*name).to_string(),
            moira_ms,
            rustc_ms,
            speedup,
        });

        let _ = std::fs::remove_file(&file);
    }

    let geomean = if ratios.is_empty() {
        None
    } else {
        let log_sum: f64 = ratios.iter().map(|r| r.ln()).sum();
        Some((log_sum / ratios.len() as f64).exp())
    };

    println!("{}", "-".repeat(46));
    if let Some(geo) = geomean {
        println!("geometric-mean speedup (rustc / moira): {geo:.2}x");
    }

    if record {
        let run = RunRecord {
            timestamp: now_timestamp(),
            commit: current_commit(),
            geomean,
            entries,
        };
        match append_run(results_dir, &run) {
            Ok(()) => {
                println!("\nrecorded run -> {}", history_path(results_dir).display());
                let hist = load_history(results_dir);
                if hist.len() >= 2 {
                    let prev = &hist[hist.len() - 2];
                    report_regressions(prev, &run);
                } else {
                    println!("no previous run to compare against");
                }
            }
            Err(e) => eprintln!("\nerror: failed to record run: {e}"),
        }
    }
}

/// Print a textual dashboard of all recorded runs plus the latest regression
/// analysis (comparison of the two most recent runs).
fn print_dashboard(results_dir: &Path) {
    let hist = load_history(results_dir);
    println!("Moira benchmark history  (dir: {})", results_dir.display());
    println!("recorded runs: {}", hist.len());
    if hist.is_empty() {
        println!("\nno runs recorded yet — run `moira-bench --record` first.");
        return;
    }

    println!();
    for (idx, run) in hist.iter().enumerate() {
        let commit = run.commit.as_deref().unwrap_or("-");
        let geo = run
            .geomean
            .map(|g| format!("{g:.2}x"))
            .unwrap_or_else(|| "n/a".to_string());
        println!(
            "run #{:<3} {}  commit={}  geomean={}",
            idx + 1,
            format_timestamp(&run.timestamp),
            commit,
            geo
        );
        let parts: Vec<String> = run
            .entries
            .iter()
            .map(|e| {
                let m = e
                    .moira_ms
                    .map(|v| format!("{v:.2}ms"))
                    .unwrap_or_else(|| "n/a".to_string());
                format!("{}={}", e.program, m)
            })
            .collect();
        println!("        {}", parts.join("  "));
    }

    println!();
    if hist.len() < 2 {
        println!("only one run recorded; no previous run to compare against.");
        return;
    }
    let (prev, new) = (hist.len() - 2, hist.len() - 1);
    report_regressions(&hist[prev], &hist[new]);
}

/// Compare `prev` to `new` and print any regressions (or an all-clear).
fn report_regressions(prev: &RunRecord, new: &RunRecord) {
    let regs = detect_regressions(prev, new, REGRESSION_PCT);
    let geo = geomean_regression(prev, new, REGRESSION_PCT);
    if regs.is_empty() && geo.is_none() {
        println!(
            "no regressions vs previous run (threshold {:.0}%); geomean {} -> {}",
            REGRESSION_PCT,
            prev.geomean.map(|g| format!("{g:.2}x")).unwrap_or_default(),
            new.geomean.map(|g| format!("{g:.2}x")).unwrap_or_default(),
        );
        return;
    }
    println!("REGRESSIONS detected vs previous run (threshold {REGRESSION_PCT:.0}%):");
    for r in &regs {
        println!(
            "  - `{}`: Moira compile time {:.2}ms -> {:.2}ms (+{:.1}%)",
            r.program, r.prev_ms, r.new_ms, r.pct
        );
    }
    if let Some((p, n, drop)) = geo {
        println!(
            "  - geometric-mean speedup dropped {:.1}%: {:.2}x -> {:.2}x",
            drop, p, n
        );
    }
}

fn moira_is_missing() -> bool {
    find_moira().is_none()
}

fn rustc_is_missing() -> bool {
    find_rustc().is_none()
}

/// Locate the sibling `moira` binary produced by the workspace build.
fn find_moira() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for name in ["moira", "moira.exe"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn find_rustc() -> Option<PathBuf> {
    for name in ["rustc"] {
        if Command::new(name).arg("--version").output().is_ok() {
            return Some(PathBuf::from(name));
        }
    }
    None
}

fn moira_args(file: &Path) -> Vec<String> {
    let out = temp_path("ssa");
    vec![
        "--emit".into(),
        "ssa".into(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
        file.to_string_lossy().into_owned(),
    ]
}

fn rustc_args(file: &Path) -> Vec<String> {
    let out = temp_path("rmeta");
    vec![
        "--emit=metadata".into(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
        file.to_string_lossy().into_owned(),
    ]
}

/// Time `program args`, returning the median wall-clock milliseconds over
/// `iters` successful runs, or `None` if any invocation fails.
fn time_cmd(program: &Path, args: &[String], iters: usize) -> Option<f64> {
    for _ in 0..WARMUP {
        let _ = Command::new(program).args(args).status();
    }
    let mut samples: Vec<Duration> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        match Command::new(program).args(args).status() {
            Ok(status) if status.success() => samples.push(start.elapsed()),
            _ => return None,
        }
    }
    if samples.is_empty() {
        return None;
    }
    samples.sort();
    let med = samples[samples.len() / 2];
    Some(med.as_secs_f64() * 1000.0)
}

fn write_temp(ext: &str, content: &str) -> PathBuf {
    let path = temp_path(ext);
    std::fs::write(&path, content).expect("write benchmark source");
    path
}

fn temp_path(ext: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let name = format!("moira_bench_{}.{}", std::process::id(), ext);
    p.push(name);
    p
}

fn fmt(ms: f64) -> String {
    format!("{ms:>11.2} ")
}
