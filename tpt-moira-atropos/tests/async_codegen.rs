//! Integration tests for the async/await state-machine codegen (Phase-3).
//!
//! These exercises the full path Clotho → Atropos MIR → C for an `async fn`
//! with multiple `.await` points, and assert the generated state machine is
//! structurally sound (a real entry `switch`, no unreachable blocks, a DONE
//! epilogue) and — when a C compiler is available — that it runs and produces the
//! expected output.

use std::path::PathBuf;
use std::process::Command;

use tpt_moira_atropos::mir::{Operand, Rvalue, Terminator};

fn demo_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("demos")
        .join("async_demo.rs")
}

fn lower_async() -> tpt_moira_atropos::MirProgram {
    let src = std::fs::read_to_string(demo_path()).unwrap();
    let prog = tpt_moira_clotho::parse_source(&src).expect("async demo should parse");
    tpt_moira_atropos::lower_program(&prog).expect("async demo should lower")
}

#[test]
fn poll_entry_has_state_switch() {
    let mir = lower_async();
    let poll = mir
        .functions
        .iter()
        .find(|f| f.name == "double__poll")
        .expect("double__poll should exist");
    match &poll.blocks[0].term {
        Terminator::Switch(disc, cases, default) => {
            // `disc` must read `self__fut->__state`.
            assert!(matches!(
                disc,
                Operand::Var(n) if n.starts_with('t')
            ));
            // `double` has two `.await` points → labels 0, 1, 2.
            assert_eq!(cases.len(), 3, "expected 3 switch cases");
            assert_eq!(cases[0], (0, 1), "state 0 runs the body");
            assert_eq!(cases[1], (1, 3), "state 1 resumes first await");
            assert_eq!(cases[2], (2, 7), "state 2 resumes second await");
            assert_eq!(*default, 2, "default is the DONE block");
        }
        other => panic!("entry block must be a Switch, got {:?}", other),
    }
}

#[test]
fn no_unreachable_blocks() {
    let mir = lower_async();
    for f in &mir.functions {
        for (i, b) in f.blocks.iter().enumerate() {
            assert!(
                !matches!(b.term, Terminator::Unreachable),
                "{} block {} must not be unreachable",
                f.name,
                i
            );
        }
    }
}

#[test]
fn done_epilogue_stores_state_minus_one_and_returns_ready() {
    let mir = lower_async();
    let poll = mir
        .functions
        .iter()
        .find(|f| f.name == "double__poll")
        .expect("double__poll should exist");
    let has_done = poll.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            matches!(
                &s.rvalue,
                Rvalue::StoreField(_, field, Operand::Const(-1), _)
                    if field == "__state"
            )
        }) && matches!(b.term, Terminator::Return(Some(Operand::Const(0))))
    });
    assert!(
        has_done,
        "a block must write __state = -1 then return READY(0)"
    );
}

/// Locate a working C compiler (mirrors `tpt_moira_atropos::tests::codegen`).
fn find_cc() -> Option<String> {
    let dir = std::env::temp_dir();
    let probe = dir.join("moira_async_cc_probe.c");
    std::fs::write(&probe, "int main(void){return 0;}\n").ok()?;
    let out = dir.join("moira_async_cc_probe");
    for cc in ["gcc", "clang", "cc", "clang-cl"] {
        if Command::new(cc).arg("--version").output().is_err() {
            continue;
        }
        let status = if cc == "clang-cl" {
            Command::new(cc)
                .arg(&probe)
                .arg(format!("/Fe:{}", out.display()))
                .status()
        } else {
            Command::new(cc)
                .arg("-O2")
                .arg(&probe)
                .arg("-o")
                .arg(&out)
                .status()
        };
        if status.map(|s| s.success()).unwrap_or(false) {
            let _ = std::fs::remove_file(&out);
            let _ = std::fs::remove_file(&probe);
            return Some(cc.to_string());
        }
    }
    let _ = std::fs::remove_file(&probe);
    None
}

#[test]
fn async_demo_runs() {
    let cc = match find_cc() {
        Some(c) => c,
        None => {
            eprintln!("no C compiler available; skipping async run");
            return;
        }
    };
    let mir = lower_async();
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let dir = std::env::temp_dir();
    let c_path = dir.join("moira_async_test.c");
    let exe_path = dir.join("moira_async_test");
    std::fs::write(&c_path, c).unwrap();

    let status = if cc == "clang-cl" {
        Command::new(&cc)
            .arg("-O2")
            .arg(&c_path)
            .arg(format!("/Fe:{}", exe_path.display()))
            .status()
            .expect("run clang-cl")
    } else {
        Command::new(&cc)
            .arg("-O2")
            .arg(&c_path)
            .arg("-o")
            .arg(&exe_path)
            .status()
            .expect("run cc")
    };
    assert!(status.success(), "C compilation failed");

    let out = Command::new(&exe_path)
        .output()
        .expect("run generated binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "23", "async double(10) should print 23");
}
