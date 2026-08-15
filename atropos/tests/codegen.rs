//! End-to-end codegen tests for the Atropos Stage-0 C backend.
//!
//! These tests lower a Moira source through Clotho + Atropos and inspect the
//! resulting MIR / C. The `end_to_end_runs` test additionally compiles and runs
//! the emitted C when a C compiler is available (common on CI runners); it is
//! skipped silently when none is found.

use std::path::PathBuf;
use std::process::Command;

fn demo_path() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.join("..").join("demos").join("codegen_demo.rs")
}

fn lower_demo() -> atropos::MirProgram {
    let src = std::fs::read_to_string(demo_path()).unwrap();
    let prog = clotho::parse_source(&src).expect("demo should parse");
    atropos::lower_program(&prog).expect("demo should lower")
}

#[test]
fn mir_has_expected_functions() {
    let mir = lower_demo();
    let names: Vec<&str> = mir.functions.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"add"), "missing add: {:?}", names);
    assert!(names.contains(&"max"), "missing max");
    assert!(names.contains(&"sum_to"), "missing sum_to");
    assert!(names.contains(&"main"), "missing main");
    // The demo declares a `Point` struct.
    assert!(mir.structs.iter().any(|s| s.name == "Point"));
}

#[test]
fn mir_lowers_struct_literal_and_if() {
    let mir = lower_demo();
    let max = mir.functions.iter().find(|f| f.name == "max").unwrap();
    // `max` should have multiple basic blocks (then/else/merge) and an If terminator.
    assert!(max.blocks.len() >= 3, "max should have >=3 blocks");
    let has_if = max
        .blocks
        .iter()
        .any(|b| matches!(b.term, atropos::mir::Terminator::If(..)));
    assert!(has_if, "max should contain an If terminator");

    let main = mir.functions.iter().find(|f| f.name == "main").unwrap();
    let has_struct_lit = main.blocks.iter().any(|b| {
        b.stmts
            .iter()
            .any(|s| matches!(s.rvalue, atropos::mir::Rvalue::StructLit(..)))
    });
    assert!(has_struct_lit, "main should build a Point struct literal");
}

#[test]
fn c_emission_contains_expected_shapes() {
    let mir = lower_demo();
    let c = atropos::codegen_c::emit_c(&mir);
    assert!(c.contains("struct Point"), "struct def missing");
    assert!(c.contains("int32_t add(int32_t a, int32_t b)"), "add signature");
    assert!(c.contains("void print_i32(long long x)"), "builtin print_i32");
    // The `if a > b` in `max` becomes a conditional branch.
    assert!(c.contains("if ("), "expected an if-branch in C");
    assert!(c.contains("goto b"), "expected basic-block gotos in C");
}

/// Attempt to locate a C compiler. Returns the command to invoke for
/// `compiler -O2 input -o output`.
fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc", "clang-cl"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

#[test]
fn end_to_end_runs() {
    let cc = match find_cc() {
        Some(c) => c,
        None => {
            eprintln!("no C compiler available; skipping end-to-end run");
            return;
        }
    };

    let mir = lower_demo();
    let c = atropos::codegen_c::emit_c(&mir);

    let dir = std::env::temp_dir();
    let c_path = dir.join("moira_codegen_test.c");
    let exe_path = dir.join("moira_codegen_test");
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
    // s = 3+4 = 7, m = max(7,10) = 10, t = sum_to(5) = 0+1+2+3+4 = 10
    assert_eq!(stdout.trim(), "7\n10\n10", "unexpected program output");
}
