//! Moira benchmark harness — Phase-1 "10x faster than `rustc`" validation.
//!
//! Compiles a fixed battery of Phase-1-subset programs (which are also valid
//! stock Rust) several ways and reports median end-to-end compile time:
//!
//! * **Moira** — the full Clotho front-end + Atropos mid-end (`--emit ssa`, which
//!   runs lex → parse → macro-expand → MIR lower → SSA build → optimize). No
//!   external assembler/linker is involved, so this isolates Moira's own work.
//! * **C compiler** (`gcc`/`clang`) — Moira's Stage-0 C backend emits C
//!   (`--emit c`), then an external C compiler builds it (`cc -O2`). This is a
//!   genuine cross-compiler comparison: same source, Moira's front-end+mid-end
//!   vs. a production C compiler doing the final codegen.
//! * **`rustc`** — `rustc --emit=metadata`, i.e. parse + full type inference +
//!   borrow check + codegen to metadata, the closest apples-to-apples "compile
//!   this file" invocation that does not require a system linker.
//!
//! The reported *speedup* is `baseline_ms / moira_ms`. Because Moira deliberately
//! defers type inference and borrow checking to Phase 2, this is not yet a
//! like-for-like comparison of *semantic* compilation — it measures the cost of
//! the front-end + mid-end pipeline Moira ships today. The harness exists to
//! track that number as the compiler grows and as incremental caching (Phase 2)
//! and parallelization land.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod results;
use results::{
    append_run, current_commit, detect_regressions, format_timestamp, geomean_regression,
    history_path, load_history, now_timestamp, Entry, RunRecord, DEFAULT_RESULTS_DIR,
};

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

/// A larger synthetic program with a 200x200 nested loop, used to see how the
/// speedup ratio trends as the program grows.
const XLARGE: &str = r#"
struct Cell { x: i32, y: i32, z: i32 }

fn mandel(cx: f64, cy: f64) -> i32 {
    let mut x = 0.0;
    let mut y = 0.0;
    let mut i = 0;
    while i < 64 {
        let nx = x * x - y * y + cx;
        let ny = 2.0 * x * y + cy;
        x = nx;
        y = ny;
        if x * x + y * y > 4.0 { return i; }
        i = i + 1;
    }
    i
}

fn compute() -> i32 {
    let mut total = 0;
    for i in 0..200 {
        for j in 0..200 {
            let c = Cell { x: i, y: j, z: i + j };
            total = total + (c.x * c.y) % 7;
        }
    }
    total
}

fn heat() -> i32 {
    let mut s = 0;
    for i in 0..200 {
        for j in 0..200 {
            s = s + mandel((i as f64) / 100.0 - 1.5, (j as f64) / 100.0 - 1.5);
        }
    }
    s
}

fn main() {
    let r = compute();
    let h = heat();
    let _ = r + h;
}
"#;

/// A program with deep call nesting and many locals, exercising the front-end
/// + mid-end on a broader call graph.
const NESTED: &str = r#"
fn f1(a: i32) -> i32 { a + 1 }
fn f2(a: i32) -> i32 { f1(a) + 2 }
fn f3(a: i32) -> i32 { f2(a) + 3 }
fn f4(a: i32) -> i32 { f3(a) + 4 }
fn f5(a: i32) -> i32 { f4(a) + 5 }
fn f6(a: i32) -> i32 { f5(a) + 6 }
fn f7(a: i32) -> i32 { f6(a) + 7 }
fn f8(a: i32) -> i32 { f7(a) + 8 }
fn f9(a: i32) -> i32 { f8(a) + 9 }
fn f10(a: i32) -> i32 { f9(a) + 10 }

fn combine(a: i32, b: i32, c: i32, d: i32) -> i32 {
    let mut acc = 0;
    let mut i = 0;
    while i < a {
        acc = acc + f10(b) + f10(c) + f10(d);
        let mut j = 0;
        while j < a {
            acc = acc + f5(b * c) + f5(d);
            j = j + 1;
        }
        i = i + 1;
    }
    acc
}

fn main() {
    let r = combine(20, 3, 7, 11);
    let _ = r;
}
"#;

const BENCHMARKS: &[(&str, &str)] = &[
    ("small", SMALL),
    ("medium", MEDIUM),
    ("large", LARGE),
    ("xlarge", XLARGE),
    ("nested", NESTED),
];

/// Moira pipeline measured by default: just the front-end + SSA mid-end
/// (`--emit ssa`). With `--full`, the full semantic pipeline (type-check +
/// borrow-check + incremental cache via `--emit check`) is timed instead.
#[derive(Clone, Copy, PartialEq)]
enum MoiraMode {
    Ssa,
    Check,
}

impl MoiraMode {
    fn emit(&self) -> &'static str {
        match self {
            MoiraMode::Ssa => "ssa",
            MoiraMode::Check => "check",
        }
    }
    fn label(&self) -> &'static str {
        match self {
            MoiraMode::Ssa => "front-end + SSA mid-end",
            MoiraMode::Check => "front-end + type-check + borrow-check + cache",
        }
    }
}

/// `rustc` baseline measured by default: `rustc --emit=metadata` (parse +
/// full inference + borrow-check + codegen-to-metadata, no link). With `full`,
/// a complete `rustc` build (`rustc -o <tmp>`) is run so the baseline includes
/// native codegen + linking — the real "compile this file" cost.
#[derive(Clone, Copy, PartialEq)]
enum RustcMode {
    Metadata,
    Full,
}

impl RustcMode {
    fn label(&self) -> &'static str {
        match self {
            RustcMode::Metadata => "rustc --emit=metadata",
            RustcMode::Full => "rustc (full build + link)",
        }
    }
}

/// Optional C-compiler baseline: Moira's Stage-0 C backend (`--emit c`) produces
/// a C translation unit, which is then built by an external C compiler
/// (`cc -O2`). This is a real cross-compiler comparison — same source, Moira's
/// front-end+mid-end vs. a production C compiler doing the final codegen.
#[derive(Clone, Copy, PartialEq)]
enum CcMode {
    None,
    Gcc,
    Clang,
}

impl CcMode {
    fn binary(&self) -> Option<&'static str> {
        match self {
            CcMode::None => None,
            CcMode::Gcc => Some("gcc"),
            CcMode::Clang => Some("clang"),
        }
    }
    fn label(&self) -> &'static str {
        match self {
            CcMode::None => "disabled",
            CcMode::Gcc => "gcc -O2",
            CcMode::Clang => "clang -O2",
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut record = false;
    let mut dashboard = false;
    let mut moira_mode = MoiraMode::Ssa;
    let mut rustc_mode = RustcMode::Metadata;
    let mut cc_mode = CcMode::None;
    let mut results_dir = PathBuf::from(DEFAULT_RESULTS_DIR);

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--record" => record = true,
            "--dashboard" => dashboard = true,
            "--full" => moira_mode = MoiraMode::Check,
            "--cc" => match args.get(i + 1) {
                Some(m) => {
                    match m.as_str() {
                        "none" => cc_mode = CcMode::None,
                        "gcc" => cc_mode = CcMode::Gcc,
                        "clang" => cc_mode = CcMode::Clang,
                        other => {
                            eprintln!(
                                "error: --cc expects `gcc`, `clang`, or `none`, found `{other}`"
                            );
                            std::process::exit(2);
                        }
                    }
                    i += 1;
                }
                None => {
                    eprintln!("error: --cc requires an argument (`gcc`|`clang`|`none`)");
                    std::process::exit(2);
                }
            },
            "--rustc-mode" => match args.get(i + 1) {
                Some(m) => {
                    match m.as_str() {
                        "metadata" => rustc_mode = RustcMode::Metadata,
                        "full" => rustc_mode = RustcMode::Full,
                        other => {
                            eprintln!(
                                "error: --rustc-mode expects `metadata` or `full`, found `{other}`"
                            );
                            std::process::exit(2);
                        }
                    }
                    i += 1;
                }
                None => {
                    eprintln!("error: --rustc-mode requires an argument (`metadata`|`full`)");
                    std::process::exit(2);
                }
            },
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

    run_benchmark(record, &results_dir, moira_mode, rustc_mode, cc_mode);

    if moira_is_missing() {
        eprintln!("\nwarning: `moira` binary not found; run `cargo build` first.");
    }
    if rustc_is_missing() {
        eprintln!("\nwarning: `rustc` not found; skipping rustc column.");
    }
    if cc_is_missing(cc_mode) {
        eprintln!("\nwarning: C compiler not found; skipping C-compiler column.");
    }
}

/// Run the benchmark battery, print the table, and (when `record`) persist the
/// run and report any regressions versus the previous recorded run.
fn run_benchmark(
    record: bool,
    results_dir: &Path,
    moira_mode: MoiraMode,
    rustc_mode: RustcMode,
    cc_mode: CcMode,
) {
    let moira = find_moira();
    let rustc = find_rustc();
    let cc = cc_mode.binary().and_then(find_cc);

    let cc_label = cc_mode.label();

    println!("╭──────────────────────────────────────────────────────────────────────────╮");
    println!("│  Moira compile-time benchmark — Phase-1 subset                             │");
    println!("╰──────────────────────────────────────────────────────────────────────────╯");
    println!(
        "  {} iterations per program (median reported). Moira = {}.",
        ITERS,
        moira_mode.label()
    );
    println!(
        "  baselines: rustc = {}   |   C compiler = {}",
        rustc_mode.label(),
        cc_label
    );
    if moira_mode == MoiraMode::Check {
        println!("  note: `--full` measures Moira's complete semantic pipeline; the rustc");
        println!(
            "        baseline may still include native codegen/link unless `--rustc-mode full`."
        );
    }
    println!();

    // Column headers
    let has_cc = cc.is_some();
    if has_cc {
        println!(
            "  {:<8} {:>11} {:>11} {:>10} {:>11} {:>10}",
            "program", "moira", cc_label, "cc×", "rustc", "rustc×"
        );
        println!("  {}", "─".repeat(73));
    } else {
        println!(
            "  {:<8} {:>11} {:>11} {:>10}",
            "program", "moira", "rustc", "rustc×"
        );
        println!("  {}", "─".repeat(45));
    }

    let mut rustc_ratios: Vec<f64> = Vec::new();
    let mut cc_ratios: Vec<f64> = Vec::new();
    let mut cc_attempted = false;
    let mut cc_succeeded = false;
    let mut entries: Vec<Entry> = Vec::new();
    for (name, src) in BENCHMARKS {
        let file = write_temp("rs", src);

        let m_args = moira_args(&file, moira_mode);
        let r_args = rustc_args(&file, rustc_mode);
        // For the C-compiler baseline we need Moira to emit C first.
        let (c_src_file, cc_args) = cc
            .as_ref()
            .map(|bin| cc_pipeline(bin, &file))
            .unwrap_or((PathBuf::new(), Vec::new()));

        let moira_ms = moira.as_ref().and_then(|bin| time_cmd(bin, &m_args, ITERS));
        let rustc_ms = rustc.as_ref().and_then(|bin| time_cmd(bin, &r_args, ITERS));
        let cc_ms = if cc_args.is_empty() {
            None
        } else {
            cc_attempted = true;
            let t = cc.as_ref().and_then(|bin| time_cmd(bin, &cc_args, ITERS));
            if t.is_some() {
                cc_succeeded = true;
            }
            t
        };

        let moira_s = fmt(moira_ms);
        let rustc_s = fmt(rustc_ms);
        let rustc_speedup = rustc_ms.and_then(|r| moira_ms.map(|m| r / m));
        let cc_s = fmt(cc_ms);
        let cc_speedup = cc_ms.and_then(|c| moira_ms.map(|m| c / m));

        if let Some(r) = rustc_speedup {
            rustc_ratios.push(r);
        }
        if let Some(c) = cc_speedup {
            cc_ratios.push(c);
        }

        if has_cc {
            println!(
                "  {:<8} {:>11} {:>11} {:>10} {:>11} {:>10}",
                name,
                moira_s,
                cc_s,
                cc_speedup
                    .map(|x| format!("{x:>8.2}x"))
                    .unwrap_or_else(|| "   n/a".into()),
                rustc_s,
                rustc_speedup
                    .map(|x| format!("{x:>8.2}x"))
                    .unwrap_or_else(|| "   n/a".into()),
            );
        } else {
            println!(
                "  {:<8} {:>11} {:>11} {:>10}",
                name,
                moira_s,
                rustc_s,
                rustc_speedup
                    .map(|x| format!("{x:>8.2}x"))
                    .unwrap_or_else(|| "   n/a".into()),
            );
        }

        entries.push(Entry {
            program: (*name).to_string(),
            moira_ms,
            cc_ms,
            rustc_ms,
            cc_speedup,
            speedup: rustc_speedup,
        });

        // Remove the temp source and the emitted artifact(s) so repeated runs
        // and different crates don't leak temp files.
        let _ = std::fs::remove_file(&file);
        for a in m_args.iter().chain(r_args.iter()).chain(cc_args.iter()) {
            let _ = std::fs::remove_file(a);
        }
        let _ = std::fs::remove_file(&c_src_file);
    }

    println!("  {}", "─".repeat(if has_cc { 73 } else { 45 }));

    let cc_geo = geomean(&cc_ratios);
    let rustc_geo = geomean(&rustc_ratios);

    if has_cc {
        println!(
            "  {:<8} {:>11} {:>11} {:>10} {:>11} {:>10}",
            "geomean",
            "",
            "",
            cc_geo
                .map(|x| format!("{x:>8.2}x"))
                .unwrap_or_else(|| "   n/a".into()),
            "",
            rustc_geo
                .map(|x| format!("{x:>8.2}x"))
                .unwrap_or_else(|| "   n/a".into()),
        );
    } else {
        println!(
            "  {:<8} {:>11} {:>11} {:>10}",
            "geomean",
            "",
            "",
            rustc_geo
                .map(|x| format!("{x:>8.2}x"))
                .unwrap_or_else(|| "   n/a".into()),
        );
    }

    if let (Some(c), Some(r)) = (cc_geo, rustc_geo) {
        println!();
        println!(
            "  summary: Moira is {:.2}x faster than {} and {:.2}x faster than {}",
            c,
            cc_label,
            r,
            rustc_mode.label()
        );
    } else if let Some(r) = rustc_geo {
        println!();
        println!(
            "  summary: Moira is {:.2}x faster than {}",
            r,
            rustc_mode.label()
        );
    }

    if cc_attempted && !cc_succeeded {
        println!();
        println!(
            "  note: `{}` is installed but failed to compile Moira's emitted C (missing",
            cc_label
        );
        println!("        system headers is common on Windows). The C-compiler column is n/a.");
    }

    if record {
        let run = RunRecord {
            timestamp: now_timestamp(),
            commit: current_commit(),
            geomean: rustc_geo,
            entries,
        };
        match append_run(results_dir, &run) {
            Ok(()) => {
                println!(
                    "\n  recorded run -> {}",
                    history_path(results_dir).display()
                );
                let hist = load_history(results_dir);
                if hist.len() >= 2 {
                    let prev = &hist[hist.len() - 2];
                    report_regressions(prev, &run);
                } else {
                    println!("  no previous run to compare against");
                }
            }
            Err(e) => eprintln!("\nerror: failed to record run: {e}"),
        }
    }
}

/// Geometric mean of a slice of ratios (`baseline / moira`), or `None` if empty.
fn geomean(ratios: &[f64]) -> Option<f64> {
    if ratios.is_empty() {
        return None;
    }
    let log_sum: f64 = ratios.iter().map(|r| r.ln()).sum();
    Some((log_sum / ratios.len() as f64).exp())
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
                let sp = e
                    .speedup
                    .map(|v| format!("{v:.2}x"))
                    .unwrap_or_else(|| "n/a".to_string());
                let cc = e
                    .cc_speedup
                    .map(|v| format!("cc={v:.2}x"))
                    .unwrap_or_else(|| "cc=n/a".to_string());
                format!("{}={}({}; {})", e.program, m, sp, cc)
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

fn cc_is_missing(mode: CcMode) -> bool {
    match mode.binary() {
        Some(bin) => find_cc(bin).is_none(),
        None => false,
    }
}

/// Locate a C compiler binary by name (e.g. `gcc`, `clang`), verifying it runs.
fn find_cc(binary: &str) -> Option<PathBuf> {
    if Command::new(binary).arg("--version").output().is_ok() {
        return Some(PathBuf::from(binary));
    }
    None
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

fn moira_args(file: &Path, mode: MoiraMode) -> Vec<String> {
    let out = temp_path(mode.emit());
    vec![
        "--emit".into(),
        mode.emit().into(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
        file.to_string_lossy().into_owned(),
    ]
}

fn rustc_args(file: &Path, mode: RustcMode) -> Vec<String> {
    match mode {
        RustcMode::Metadata => {
            let out = temp_path("rmeta");
            vec![
                "--emit=metadata".into(),
                "-o".into(),
                out.to_string_lossy().into_owned(),
                file.to_string_lossy().into_owned(),
            ]
        }
        RustcMode::Full => {
            let dir = temp_path("rustc_full");
            vec![
                "-o".into(),
                dir.to_string_lossy().into_owned(),
                file.to_string_lossy().into_owned(),
            ]
        }
    }
}

/// Build the C-compiler baseline. First, Moira's Stage-0 C backend emits a C
/// translation unit from the Rust source (`moira --emit c -o <tmp>.c`); this
/// emission is *not* timed — the returned `cc_args` are what we measure, so the
/// comparison is "Moira front-end+mid-end" vs. "external C compiler codegen".
/// Returns `(c_source_path, cc_invocation_args)`.
fn cc_pipeline(cc_binary: &Path, rust_file: &Path) -> (PathBuf, Vec<String>) {
    let c_src = temp_path("c");
    let emit = vec![
        "moira".to_string(),
        "--emit".to_string(),
        "c".to_string(),
        "-o".to_string(),
        c_src.to_string_lossy().into_owned(),
        rust_file.to_string_lossy().into_owned(),
    ];
    // Run the emit step up-front (outside the timed region).
    let _ = Command::new(find_moira().unwrap_or_else(|| PathBuf::from("moira")))
        .args(&emit)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let out = temp_path("cc_out");
    let cc_args = vec![
        cc_binary.to_string_lossy().into_owned(),
        "-O2".to_string(),
        c_src.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
    ];
    (c_src, cc_args)
}

/// Time `program args`, returning the median wall-clock milliseconds over
/// `iters` successful runs, or `None` if any invocation fails. Subprocess
/// stdout/stderr are discarded so compiler diagnostics don't pollute the table.
fn time_cmd(program: &Path, args: &[String], iters: usize) -> Option<f64> {
    use std::process::Stdio;
    for _ in 0..WARMUP {
        let _ = Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let mut samples: Vec<Duration> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        match Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
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

fn fmt(ms: Option<f64>) -> String {
    match ms {
        Some(m) => format!("{m:>10.2}ms"),
        None => "      n/a".to_string(),
    }
}
