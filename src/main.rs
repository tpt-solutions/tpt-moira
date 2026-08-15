//! Moira compiler driver.
//!
//! Entry point that wires the three Fates together. In Phase 1 it drives the
//! Clotho frontend (lex + parse + macro-expand), lowers through Atropos to MIR,
//! builds and optimizes the SSA mid-end, and can emit the Stage-0 C backend, the
//! from-scratch x86-64 backend, or the SSA text.

use std::io::Read;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // A simple subcommand scheme: the first `--emit <mode>` selects output and
    // `-o <file>` redirects output to a file instead of stdout.
    let mut emit = "ast".to_string();
    let mut output: Option<String> = None;
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
            other => positional.push(other.to_string()),
        }
    }

    let source = if let Some(path) = positional.first() {
        match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("moira: cannot read `{}`: {}", path, e);
                return ExitCode::FAILURE;
            }
        }
    } else {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            eprintln!("moira: failed to read stdin");
            return ExitCode::FAILURE;
        }
        buf
    };

    let program = match clotho::compile_source(&source) {
        Ok(p) => p,
        Err(diag) => {
            eprintln!("moira: parse error: {}", diag);
            return ExitCode::FAILURE;
        }
    };

    match emit.as_str() {
        "ast" => {
            println!(
                "moira: parsed {} top-level item(s)",
                program.items.len()
            );
            for item in &program.items {
                println!("  - {:?} {:?}", item.visibility, item.kind);
            }
        }
        "mir" => match atropos::lower_program(&program) {
            Ok(mir) => println!("{:#?}", mir),
            Err(e) => {
                eprintln!("moira: lowering error: {}", e);
                return ExitCode::FAILURE;
            }
        },
        "ssa" => match lower_and_ssa(&program) {
            Ok(ssa) => emit_text(&ssa.to_string(), &output),
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
        "asm" => match lower_and_ssa(&program).and_then(|ssa| atropos::codegen_native::emit_asm(&ssa)) {
            Ok(asm) => emit_text(&asm, &output),
            Err(e) => {
                eprintln!("moira: native codegen error: {}", e);
                return ExitCode::FAILURE;
            }
        },
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
            eprintln!("moira: unknown --emit mode `{}` (expected ast|mir|ssa|c|asm|native)", other);
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

fn lower_and_ssa(program: &clotho::ast::Program) -> Result<atropos::SsaProgram, String> {
    let mir = atropos::lower_program(program).map_err(|e| e.to_string())?;
    let mut ssa = atropos::build_ssa(&mir);
    ssa.optimize();
    Ok(ssa)
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
