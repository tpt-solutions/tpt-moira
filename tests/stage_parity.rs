//! Stage-1 vs Stage-2 output-parity harness.
//!
//! **Goal (Stage-2 self-host):** once Moira can compile itself, the Stage-1
//! binary (Moira-compiled Moira) must produce byte-identical MIR / SSA / C /
//! ASM output to the Stage-0 binary (rustc-built Moira) on a fixed corpus —
//! and, by induction, a Stage-2 binary compiled by Stage-1 must match too.
//!
//! **How this runs:** the Stage-1 binary does not exist until self-hosting
//! lands, so the harness is *conditional*: set `MOIRA_STAGE1_BIN` to the path
//! of a Stage-1 (or Stage-2) `moira` executable and the parity tests run for
//! real; when the variable is unset (the default today, including CI) the
//! tests skip silently. This keeps the automation wired in — the moment a
//! Stage-1 binary exists, `MOIRA_STAGE1_BIN=target/self/moira cargo test` runs
//! the full cross-stage diff with zero further changes.

use std::io::Write;
use std::process::Command;

fn stage0_bin() -> &'static str {
    env!("CARGO_BIN_EXE_moira")
}

fn stage1_bin() -> Option<String> {
    std::env::var("MOIRA_STAGE1_BIN")
        .ok()
        .filter(|p| !p.is_empty())
}

/// Write `src` to a uniquely-named file under a temp dir and return the path.
fn sample_path(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("moira_stage_parity");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(src.as_bytes()).unwrap();
    p
}

/// Run `bin --emit <mode> <file>` as a fresh process (caching disabled) and
/// return its stdout.
fn emit_once(bin: &str, mode: &str, file: &std::path::Path) -> String {
    let out = Command::new(bin)
        .env("MOIRA_NO_CACHE", "1")
        .args(["--emit", mode, &file.to_string_lossy()])
        .output()
        .unwrap_or_else(|e| panic!("failed to run {} for --emit {}: {}", bin, mode, e));
    assert!(
        out.status.success(),
        "{} --emit {} failed:\n{}",
        bin,
        mode,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Assert the Stage-0 and Stage-1 binaries emit byte-identical output.
fn assert_stage_parity(mode: &str, file_name: &str, src: &str) {
    let Some(stage1) = stage1_bin() else {
        eprintln!(
            "skipping stage-parity[{}]: MOIRA_STAGE1_BIN is not set (no Stage-1 binary yet)",
            mode
        );
        return;
    };
    let path = sample_path(file_name, src);
    let stage0_out = emit_once(stage0_bin(), mode, &path);
    let stage1_out = emit_once(&stage1, mode, &path);
    assert_eq!(
        stage0_out, stage1_out,
        "Stage-0 and Stage-1 `--emit {}` outputs differ — a non-determinism \
         (hash-map iteration, timestamps, path embedding) or a Stage-1 \
         mis-compilation broke cross-stage parity",
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

fn main() {
    print_i32(add(3, 4));
    print_i32(max(7, 10));
}
"#;

// Struct-free subset, since the from-scratch native backend rejects composite
// types and only handles the integer/control-flow restricted subset.
const ASM_SRC: &str = r#"
fn add(a: i32, b: i32) -> i32 { a + b }

fn max(a: i32, b: i32) -> i32 { if a > b { a } else { b } }

fn main() {
    print_i32(add(3, 4));
    print_i32(max(7, 10));
}
"#;

#[test]
fn ssa_stage_parity() {
    assert_stage_parity("ssa", "stage_parity_ssa.rs", MIR_SSA_C_SRC);
}

#[test]
fn mir_stage_parity() {
    assert_stage_parity("mir", "stage_parity_mir.rs", MIR_SSA_C_SRC);
}

#[test]
fn c_stage_parity() {
    assert_stage_parity("c", "stage_parity_c.rs", MIR_SSA_C_SRC);
}

#[test]
fn asm_stage_parity() {
    assert_stage_parity("asm", "stage_parity_asm.rs", ASM_SRC);
}
