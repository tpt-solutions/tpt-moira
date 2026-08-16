//! Moira compiler driver.
//!
//! Entry point that wires the three Fates together. In Phase 1 it drives the
//! Clotho frontend (lex + parse + macro-expand), lowers through Atropos to MIR,
//! builds and optimizes the SSA mid-end, and can emit the Stage-0 C backend, the
//! from-scratch x86-64 backend, or the SSA text.

use std::io::Read;
use std::process::{Command, ExitCode};

use lachesis::diff::{classify, Divergence};
use lachesis::Cache;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Subcommand scheme:
    //   --emit <mode>  selects output (ast|mir|ssa|c|asm|native|check)
    //   -o <file>      redirects output to a file instead of stdout
    //   --check        run the Lachesis type + borrow checker before codegen
    let mut emit = "ast".to_string();
    let mut output: Option<String> = None;
    let mut check = false;
    let mut diff = false;
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--emit" => {
                if let Some(mode) = iter.next() {
                    emit = mode.clone();
                }
            }
            "-o" => {
                if let Some(path) = iter.next() {
                    output = Some(path.clone());
                }
            }
            "--check" => check = true,
            "--diff" => diff = true,
            other => positional.push(other.to_string()),
        }
    }

    let (source, program) = if let Some(path) = positional.first() {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("moira: cannot read `{}`: {}", path, e);
                return ExitCode::FAILURE;
            }
        };
        let prog = match clotho::compile_file(std::path::Path::new(path)) {
            Ok(p) => p,
            Err(diag) => {
                eprintln!("moira: parse error: {}", diag);
                return ExitCode::FAILURE;
            }
        };
        (src, prog)
    } else {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            eprintln!("moira: failed to read stdin");
            return ExitCode::FAILURE;
        }
        let prog = match clotho::compile_source(&buf) {
            Ok(p) => p,
            Err(diag) => {
                eprintln!("moira: parse error: {}", diag);
                return ExitCode::FAILURE;
            }
        };
        (buf, prog)
    };

    if diff {
        return run_diff(&source, &program);
    }

    // End-to-end integration: optionally (or for the `check` emit) run the
    // Lachesis type + borrow checker before lowering/codegen.
    if check {
        if let Err(diags) = lachesis::check(&program) {
            for d in &diags {
                eprintln!("moira: {}", d);
            }
            eprintln!("moira: aborting: {} type/borrow error(s)", diags.len());
            return ExitCode::FAILURE;
        }
    }

    match emit.as_str() {
        "check" => match lachesis::check(&program) {
            Ok(()) => println!("moira: type-check and borrow-check passed"),
            Err(diags) => {
                for d in &diags {
                    eprintln!("moira: {}", d);
                }
                return ExitCode::FAILURE;
            }
        },
        "ast" => {
            println!("moira: parsed {} top-level item(s)", program.items.len());
            for item in &program.items {
                println!("  - {:?} {:?}", item.visibility, item.kind);
            }
        }
        "mir" => match atropos::lower_program(&program) {
            Ok(mir) => println!("{mir}"),
            Err(e) => {
                eprintln!("moira: lowering error: {}", e);
                return ExitCode::FAILURE;
            }
        },
        "ssa" => match ssa_text(&source, &program) {
            Ok(ssa) => emit_text(&ssa, &output),
            Err(e) => {
                eprintln!("moira: ssa error: {}", e);
                return ExitCode::FAILURE;
            }
        },
        "c" => match atropos::lower_program(&program) {
            Ok(mir) => emit_text(&atropos::codegen_c::emit_c(&mir), &output),
            Err(e) => {
                eprintln!("moira: lowering error: {}", e);
                return ExitCode::FAILURE;
            }
        },
        "asm" => {
            match lower_and_ssa(&program).and_then(|ssa| atropos::codegen_native::emit_asm(&ssa)) {
                Ok(asm) => emit_text(&asm, &output),
                Err(e) => {
                    eprintln!("moira: native codegen error: {}", e);
                    return ExitCode::FAILURE;
                }
            }
        }
        "native" => match lower_and_ssa(&program) {
            Ok(ssa) => {
                let out = output.unwrap_or_else(|| "moira_out".to_string());
                match atropos::codegen_native::compile_to_native(&ssa, std::path::Path::new(&out)) {
                    Ok(()) => println!("moira: wrote native executable to `{}`", out),
                    Err(e) => {
                        eprintln!("moira: native codegen error: {}", e);
                        return ExitCode::FAILURE;
                    }
                }
            }
            Err(e) => {
                eprintln!("moira: ssa error: {}", e);
                return ExitCode::FAILURE;
            }
        },
        other => {
            eprintln!(
                "moira: unknown --emit mode `{}` (expected ast|mir|ssa|c|asm|native|check)",
                other
            );
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

fn lower_and_ssa(program: &clotho::ast::Program) -> Result<atropos::SsaProgram, String> {
    let mir = atropos::lower_program(program).map_err(|e| e.to_string())?;
    let mut ssa = atropos::build_ssa(&mir)?;
    ssa.optimize();
    Ok(ssa)
}

/// Lower to SSA text, consulting the disk-backed content-addressed cache so an
/// unchanged source string reuses its previously computed SSA instead of
/// recomputing it (the Phase-2 incremental cache, demonstrated on `--emit ssa`).
fn ssa_text(source: &str, program: &clotho::ast::Program) -> Result<String, String> {
    let key = lachesis::content_hash(source);
    let cache_dir = std::env::temp_dir().join("moira_cache");
    // `MOIRA_NO_CACHE=1` bypasses the incremental cache so the SSA is recomputed
    // from scratch every invocation. This is what makes cross-process
    // determinism checks (and reproducible self-host builds) meaningful: without
    // it, a cached first run would mask any nondeterminism in the emit path.
    if std::env::var("MOIRA_NO_CACHE").is_err() {
        if let Ok(cache) = Cache::open(&cache_dir) {
            if let Some(bytes) = cache.get(key) {
                if let Ok(s) = String::from_utf8(bytes) {
                    return Ok(s);
                }
            }
            let s = lower_and_ssa(program).map(|ssa| ssa.to_string())?;
            let _ = cache.insert(key, s.as_bytes());
            return Ok(s);
        }
    }
    lower_and_ssa(program).map(|ssa| ssa.to_string())
}

fn emit_text(text: &str, output: &Option<String>) {
    match output {
        Some(path) => {
            if std::fs::write(path, text).is_err() {
                eprintln!("moira: failed to write `{}`", path);
            }
        }
        None => print!("{}", text),
    }
}

/// `moira-diff` baseline: run Moira's checker and `rustc --emit=metadata` on the
/// same source and classify any divergence between the two compilers.
fn run_diff(source: &str, program: &clotho::ast::Program) -> ExitCode {
    let moira_errors = match lachesis::check(program) {
        Ok(()) => Vec::new(),
        Err(diags) => diags.iter().map(|d| d.to_string()).collect(),
    };
    let rustc_errors = run_rustc_check(source);
    let report = classify(&moira_errors, &rustc_errors);

    println!(
        "moira-diff: Moira errors={}, rustc errors={}",
        moira_errors.len(),
        rustc_errors.len()
    );
    for d in &report.divergences {
        match d {
            Divergence::MoiraAcceptsButRustcRejects => {
                println!("  DIVERGENCE: Moira accepts but rustc rejects")
            }
            Divergence::RustcAcceptsButMoiraRejects => {
                println!("  DIVERGENCE: rustc accepts but Moira rejects")
            }
            Divergence::BothReject => println!("  DIVERGENCE: both reject (messages may differ)"),
        }
    }
    if report.divergences.is_empty() {
        println!("  in agreement");
    }
    if !report.rustc_errors.is_empty() {
        println!("rustc diagnostics:");
        for e in &report.rustc_errors {
            println!("  {}", e);
        }
    }

    if report.divergences.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run `rustc --emit=metadata` on a temp copy of `source` and return the
/// diagnostic lines (errors/warnings). Returns empty if `rustc` is unavailable.
fn run_rustc_check(source: &str) -> Vec<String> {
    let pid = std::process::id();
    let src = std::env::temp_dir().join(format!("moira_diff_{pid}.rs"));
    let out = std::env::temp_dir().join(format!("moira_diff_{pid}.rmeta"));
    if std::fs::write(&src, source).is_err() {
        return Vec::new();
    }
    let output = Command::new("rustc")
        .args(["--emit=metadata", "-o", &out.to_string_lossy(), &src.to_string_lossy()])
        .output();
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
    match output {
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            stderr
                .lines()
                .filter(|l| l.contains("error") || l.contains("warning"))
                .map(|l| l.to_string())
                .collect()
        }
        Err(_) => Vec::new(),
    }
}
