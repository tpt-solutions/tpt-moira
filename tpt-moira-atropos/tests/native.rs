//! End-to-end tests for the native x86-64 backend (`codegen_native`): each test builds a
//! program to an executable, runs it, and checks its stdout. Skipped when no C toolchain (which
//! assembles and links the output) is available.

use std::path::PathBuf;
use std::process::Command;

fn toolchain_available() -> bool {
    ["gcc", "clang", "cc"]
        .iter()
        .any(|cc| Command::new(cc).arg("--version").output().is_ok())
}

/// Build `src` with the native backend, run it, and return its stdout.
fn build_and_run(name: &str, src: &str) -> Option<String> {
    if !toolchain_available() {
        eprintln!("no C toolchain; skipping native test {name}");
        return None;
    }
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let mir = tpt_moira_atropos::lower_program(&prog).expect("program should lower");
    let ssa = tpt_moira_atropos::ssa::SsaProgram::from_mir(&mir).expect("program should convert to SSA");
    let exe: PathBuf = std::env::temp_dir().join(format!("moira_native_{name}.exe"));
    tpt_moira_atropos::codegen_native::compile_to_native(&ssa, &exe).expect("native build");
    let out = Command::new(&exe).output().expect("run native program");
    assert!(out.status.success(), "native program {name} exited with failure");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

#[test]
fn native_arithmetic_branches_loops_and_strings_run() {
    let out = match build_and_run(
        "mixed",
        "fn f(n: i64) -> i64 { n + 1 }
        fn main() {
            print_i64(3 + 4 * 5);
            print_i64(17 % 7);
            print_i64(9 - 4 - 2);
            let mut a: i64 = 5;
            if a > 3 { a = a - 1; }
            print_i64(a);
            print_i64(f(41));
            let mut i: i64 = 0;
            let mut s: i64 = 0;
            while i < 10 { s = s + i; i = i + 1; }
            print_i64(s);
            println!(\"done {}\", s);
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "23\n3\n3\n4\n42\n45\ndone 45\n", "native output should match Rust");
}

