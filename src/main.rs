//! Moira compiler driver.
//!
//! Entry point that wires the three Fates together. In Phase 1 it drives the
//! Clotho frontend (lex + parse), lowers through Atropos to MIR, and can emit
//! the Stage-0 C backend.

use std::io::Read;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // A simple subcommand scheme: the first `--emit <mode>` flag selects output.
    let mut emit = "ast".to_string();
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--emit" {
            if let Some(mode) = iter.next() {
                emit = mode.clone();
            }
        } else {
            positional.push(a.clone());
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

    let program = match clotho::parse_source(&source) {
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
        "c" => match atropos::lower_program(&program) {
            Ok(mir) => {
                print!("{}", atropos::codegen_c::emit_c(&mir));
            }
            Err(e) => {
                eprintln!("moira: lowering error: {}", e);
                return ExitCode::FAILURE;
            }
        },
        other => {
            eprintln!("moira: unknown --emit mode `{}` (expected ast|mir|c)", other);
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}
