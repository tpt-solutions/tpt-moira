//! Moira compiler driver.
//!
//! Entry point that wires the three Fates together. In Phase 1 it drives the
//! Clotho frontend (lex + parse + macro-expand), lowers through Atropos to MIR,
//! builds and optimizes the SSA mid-end, and can emit the Stage-0 C backend, the
//! from-scratch x86-64 backend, or the SSA text.

use std::io::Read;
use std::process::{Command, ExitCode};

use tpt_moira_lachesis::diff::{classify, parity, Allowlist};
use tpt_moira_lachesis::propose::{make_record, LlmProposer, TriageQueue};
use tpt_moira_lachesis::PipelineCacheExt;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Subcommand scheme:
    //   --emit <mode>  selects output (ast|mir|ssa|c|asm|native|check)
    //   -o <file>      redirects output to a file instead of stdout
    //   --check        run the Lachesis type + borrow checker before codegen
    //   --diff         run the moira-diff divergence harness against rustc
    //   --parity       in --diff, also print the error-message parity audit
    //   --propose      in --diff, attach the AI root-cause proposer + triage queue
    //   --allowlist    override the divergence allowlist file
    //   --update-allowlist  persist divergences seen in a run into the allowlist
    //   --extern name=path  register another crate's source for cross-crate
    //                  signature resolution during --check / --emit check
    let mut emit = "ast".to_string();
    let mut output: Option<String> = None;
    let mut check = false;
    let mut diff = false;
    let mut parity_flag = false;
    let mut propose_flag = false;
    let mut allowlist_path: Option<String> = None;
    let mut update_allowlist = false;
    let mut externs: Vec<String> = Vec::new();
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
            "--extern" => {
                if let Some(spec) = iter.next() {
                    // `name=path` — the name may itself carry a hyphen that
                    // Cargo normalizes to underscore; keep it verbatim.
                    externs.push(spec.to_string());
                }
            }
            "--diff" => diff = true,
            "--parity" => parity_flag = true,
            "--propose" => propose_flag = true,
            "--allowlist" => {
                if let Some(path) = iter.next() {
                    allowlist_path = Some(path.clone());
                }
            }
            "--update-allowlist" => update_allowlist = true,
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
        let prog = match tpt_moira_clotho::compile_file(std::path::Path::new(path)) {
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
        let prog = match tpt_moira_clotho::compile_source(&buf) {
            Ok(p) => p,
            Err(diag) => {
                eprintln!("moira: parse error: {}", diag);
                return ExitCode::FAILURE;
            }
        };
        (buf, prog)
    };

    let input_path = positional
        .first()
        .map(|p| std::path::Path::new(p).to_path_buf());

    // Parse `--extern name=path` specs into (name, path) pairs for cross-crate
    // signature resolution. Written within the self-host subset (no
    // iterator adapters, no `?` on `Option`, no capturing closures).
    let mut extern_pairs: Vec<(String, String)> = Vec::new();
    for spec in &externs {
        match spec.split_once('=') {
            Some(pair) => {
                extern_pairs.push((pair.0.to_string(), pair.1.to_string()));
            }
            None => {}
        }
    }
    let mut extern_programs: Vec<(String, tpt_moira_clotho::ast::Program)> = Vec::new();
    for pair in &extern_pairs {
        let name = &pair.0;
        let path = &pair.1;
        let compiled = tpt_moira_clotho::compile_file(std::path::Path::new(path));
        match compiled {
            Ok(p) => {
                extern_programs.push((name.clone(), p));
            }
            Err(diag) => {
                eprintln!(
                    "moira: extern crate `{}` ({}): parse error: {}",
                    name, path, diag
                );
                return ExitCode::FAILURE;
            }
        }
    }
    let have_externs = !extern_programs.is_empty();
    let check_call = |program: &tpt_moira_clotho::ast::Program| {
        if have_externs {
            tpt_moira_lachesis::check_with_externs(program, &extern_programs)
        } else {
            tpt_moira_lachesis::check(program)
        }
    };

    if diff {
        return run_diff(
            &source,
            &program,
            input_path.as_deref(),
            allowlist_path.as_deref(),
            update_allowlist,
            parity_flag,
            propose_flag,
        );
    }

    // End-to-end integration: optionally (or for the `check` emit) run the
    // Lachesis type + borrow checker before lowering/codegen. The result is read
    // through the whole-pipeline content-addressed cache when caching is on.
    if check {
        if let Err(diags) = pipeline().checked(&source, || check_call(&program)) {
            for d in &diags {
                eprintln!("moira: {}", d);
            }
            eprintln!("moira: aborting: {} type/borrow error(s)", diags.len());
            return ExitCode::FAILURE;
        }
    }

    match emit.as_str() {
        "check" => match pipeline().checked(&source, || check_call(&program)) {
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
        "mir" => match tpt_moira_atropos::lower_program(&program) {
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
        "c" => match tpt_moira_atropos::lower_program(&program) {
            Ok(mir) => emit_text(&tpt_moira_atropos::codegen_c::emit_c(&mir), &output),
            Err(e) => {
                eprintln!("moira: lowering error: {}", e);
                return ExitCode::FAILURE;
            }
        },
        "asm" => {
            match lower_and_ssa(&program)
                .and_then(|ssa| tpt_moira_atropos::codegen_native::emit_asm(&ssa))
            {
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
                match tpt_moira_atropos::codegen_native::compile_to_native(
                    &ssa,
                    std::path::Path::new(&out),
                ) {
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

/// The whole-pipeline content-addressed cache, or a no-op wrapper when
/// `MOIRA_NO_CACHE` is set (used by cross-process determinism checks).
fn pipeline() -> PipelineCacheExt {
    PipelineCacheExt::new()
}

fn lower_and_ssa(
    program: &tpt_moira_clotho::ast::Program,
) -> Result<tpt_moira_atropos::SsaProgram, String> {
    let mir = tpt_moira_atropos::lower_program(program).map_err(|e| e.to_string())?;
    let mut ssa = tpt_moira_atropos::build_ssa(&mir)?;
    ssa.optimize();
    Ok(ssa)
}

/// Lower to SSA text, consulting the disk-backed content-addressed cache so an
/// unchanged source string reuses its previously computed SSA instead of
/// recomputing it (the Phase-4 whole-pipeline cache: this is one of the
/// artifacts keyed by source hash alongside the check result and MIR).
fn ssa_text(source: &str, program: &tpt_moira_clotho::ast::Program) -> Result<String, String> {
    let s = pipeline().text(source, "ssa", || {
        lower_and_ssa(program)
            .map(|ssa| ssa.to_string())
            .unwrap_or_default()
    });
    Ok(s)
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
///
/// `allowlist_path` (defaulting to [`Allowlist::DEFAULT_FILE`]) is consulted so
/// that tracked, known divergences don't fail the run; `--update-allowlist`
/// persists the divergences seen here back into that file. A run "succeeds"
/// (exit 0) when every divergence is covered by the allowlist.
///
/// With `--parity` the error-message parity audit is printed; with `--propose`
/// the offline AI root-cause proposer runs and appends to the triage queue.
fn run_diff(
    source: &str,
    program: &tpt_moira_clotho::ast::Program,
    input_path: Option<&std::path::Path>,
    allowlist_path: Option<&str>,
    update_allowlist: bool,
    parity_flag: bool,
    propose_flag: bool,
) -> ExitCode {
    let moira_errors = match tpt_moira_lachesis::check(program) {
        Ok(()) => Vec::new(),
        Err(diags) => diags.iter().map(|d| d.to_string()).collect(),
    };
    let rustc_errors = run_rustc_check(source);
    let report = classify(&moira_errors, &rustc_errors);

    let allow_file = allowlist_path
        .map(std::path::Path::new)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(Allowlist::DEFAULT_FILE));
    let mut allowlist = match Allowlist::load(&allow_file) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "moira-diff: could not read allowlist `{}`: {}",
                allow_file.display(),
                e
            );
            Allowlist::default()
        }
    };
    let hash = tpt_moira_lachesis::content_hash(source);

    println!(
        "moira-diff: Moira errors={}, rustc errors={}",
        moira_errors.len(),
        rustc_errors.len()
    );

    if parity_flag {
        let pr = parity(&moira_errors, &rustc_errors);
        println!("  {}", pr.describe());
    }

    let mut unallowed = Vec::new();
    for &d in &report.divergences {
        if allowlist.allows(input_path, hash, d) {
            println!("  ALLOWED (known divergence): {}", d.describe());
        } else {
            unallowed.push(d);
            println!("  DIVERGENCE: {}", d.describe());
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

    // AI-assisted diffing: propose root cause + minimal fix per divergence and
    // record it in the triage queue.
    if propose_flag && !report.divergences.is_empty() {
        let proposer = llm_proposer_from_env();
        let queue_path = std::path::Path::new(".moira-triage.jsonl");
        let mut queue = match TriageQueue::open(queue_path) {
            Ok(q) => Some(q),
            Err(e) => {
                eprintln!(
                    "moira-diff: could not open triage queue `{}`: {}",
                    queue_path.display(),
                    e
                );
                TriageQueue::open(&std::env::temp_dir().join("moira_triage.jsonl")).ok()
            }
        };
        for &d in &report.divergences {
            let rec = make_record(d, &moira_errors, &rustc_errors, &proposer, hash);
            println!("  PROPOSAL [{}]: {}", rec.label, rec.root_cause);
            println!("           fix: {}", rec.minimal_fix);
            if let Some(q) = &mut queue {
                let _ = q.push(rec);
            }
        }
        if let Some(q) = &queue {
            println!("moira-diff: triage queue has {} record(s)", q.len());
        }
    }

    if update_allowlist && !report.divergences.is_empty() {
        for &d in &report.divergences {
            allowlist.insert(input_path, hash, d);
        }
        if let Err(e) = allowlist.save(&allow_file) {
            eprintln!(
                "moira-diff: could not write allowlist `{}`: {}",
                allow_file.display(),
                e
            );
        } else {
            println!(
                "moira-diff: recorded {} divergence(s) into `{}`",
                report.divergences.len(),
                allow_file.display()
            );
        }
    }

    if unallowed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Build the root-cause proposer, attaching an LLM backend if `MOIRA_LLM_CMD` is
/// set (an OpenAI-compatible chat command reading the prompt on stdin and
/// printing the reply to stdout, e.g. `ollama run <model>` or a `curl` call).
fn llm_proposer_from_env() -> LlmProposer {
    match std::env::var("MOIRA_LLM_CMD") {
        Ok(cmd) if !cmd.trim().is_empty() => {
            let cmd = cmd.clone();
            LlmProposer::with_backend(move |prompt: &str| -> String {
                run_llm_command(&cmd, prompt)
            })
        }
        _ => LlmProposer::offline(),
    }
}

#[cfg(windows)]
fn run_llm_command(cmd: &str, prompt: &str) -> String {
    run_llm_command_shell("cmd", &["/C", cmd], prompt)
}

#[cfg(not(windows))]
fn run_llm_command(cmd: &str, prompt: &str) -> String {
    run_llm_command_shell("sh", &["-c", cmd], prompt)
}

fn run_llm_command_shell(shell: &str, args: &[&str], prompt: &str) -> String {
    use std::io::Write;
    let child = Command::new(shell)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
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
        .args([
            "--emit=metadata",
            "-o",
            &out.to_string_lossy(),
            &src.to_string_lossy(),
        ])
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
