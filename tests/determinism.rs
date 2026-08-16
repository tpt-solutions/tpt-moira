//! Cross-process determinism harness for MIR / SSA / C / ASM emit.
//!
//! **Goal (Phase 4 / Stage-2 self-host):** the same Moira source compiled by two
//! *different* binaries — e.g. the stage-0 rustc-built driver vs. the stage-1
//! Moira-built driver — must yield byte-identical MIR / SSA / C / ASM text. That
//! is how `stage-1 == stage-2 binary` is verified.
//!
//! **The trap:** every OS process seeds its hash tables (`HashMap`/`HashSet`)
//! independently, so any emit path that iterates a hash map can differ run to
//! run. A within-process test cannot catch this (the seed is fixed for the life
//! of the process), so this harness runs the real `moira` binary **twice as
//! separate processes** (with the incremental cache disabled via
//! `MOIRA_NO_CACHE=1`) and diffs the output. This is exactly the cross-stage
//! condition.

use std::io::Write;
use std::process::Command;

fn moira_bin() -> &'static str {
    env!("CARGO_BIN_EXE_moira")
}

/// Write `src` to a uniquely-named file under a temp dir and return the path.
fn sample_path(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("moira_determinism");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(src.as_bytes()).unwrap();
    p
}

/// Run `moira --emit <mode> <file>` as a fresh process (caching disabled) and
/// return its stdout.
fn emit_once(mode: &str, file: &std::path::Path) -> String {
    let out = Command::new(moira_bin())
        .env("MOIRA_NO_CACHE", "1")
        .args(["--emit", mode, &file.to_string_lossy()])
        .output()
        .unwrap_or_else(|e| panic!("failed to run moira for --emit {}: {}", mode, e));
    assert!(
        out.status.success(),
        "moira --emit {} failed:\n{}",
        mode,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Run the emit twice in two separate processes and assert the two outputs are
/// byte-identical — the cross-stage parity guarantee.
fn assert_deterministic(mode: &str, file_name: &str, src: &str) {
    let path = sample_path(file_name, src);
    let first = emit_once(mode, &path);
    let second = emit_once(mode, &path);
    assert_eq!(
        first, second,
        "moira --emit {} is NON-DETERMINISTIC across processes:\n\
         (this breaks stage-1 == stage-2 parity; a HashMap/HashSet is being \
         iterated in emit order)",
        mode
    );
}

// Exercises struct literals, field projection, control flow and several SSA
// temporaries (so `temp_types` ordering is tested) for mir/ssa/c.
const MIR_SSA_C_SRC: &str = r#"
struct Point { x: i32, y: i32 }

fn add(a: i32, b: i32) -> i32 { a + b }

fn max(a: i32, b: i32) -> i32 { if a > b { a } else { b } }

fn make() -> Point {
    let p = Point { x: 1, y: 2 };
    let z = p.x + p.y;
    Point { x: z, y: z }
}

fn sum_to(n: i32) -> i32 {
    let mut s = 0;
    let mut i = 0;
    while i < n { s = s + i; i = i + 1; }
    s
}

fn main() {
    print_i32(add(3, 4));
    print_i32(max(7, 10));
    print_i32(sum_to(5));
    let p = make();
    print_i32(p.x);
}
"#;

// Struct-free subset, since the from-scratch native backend rejects composite
// types and only handles the integer/control-flow restricted subset.
const ASM_SRC: &str = r#"
fn add(a: i32, b: i32) -> i32 { a + b }

fn max(a: i32, b: i32) -> i32 { if a > b { a } else { b } }

fn sum_to(n: i32) -> i32 {
    let mut s = 0;
    let mut i = 0;
    while i < n { s = s + i; i = i + 1; }
    s
}

fn main() {
    print_i32(add(3, 4));
    print_i32(max(7, 10));
    print_i32(sum_to(5));
}
"#;

#[test]
fn ssa_is_deterministic_across_processes() {
    assert_deterministic("ssa", "determinism_ssa.rs", MIR_SSA_C_SRC);
}

#[test]
fn mir_is_deterministic_across_processes() {
    assert_deterministic("mir", "determinism_mir.rs", MIR_SSA_C_SRC);
}

#[test]
fn c_is_deterministic_across_processes() {
    assert_deterministic("c", "determinism_c.rs", MIR_SSA_C_SRC);
}

#[test]
fn asm_is_deterministic_across_processes() {
    assert_deterministic("asm", "determinism_asm.rs", ASM_SRC);
}
