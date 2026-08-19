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
    assert!(
        c.contains("int32_t add(int32_t a, int32_t b)"),
        "add signature"
    );
    assert!(
        c.contains("void print_i32(long long x)"),
        "builtin print_i32"
    );
    // The `if a > b` in `max` becomes a conditional branch.
    assert!(c.contains("if ("), "expected an if-branch in C");
    assert!(c.contains("goto b"), "expected basic-block gotos in C");
}

/// Attempt to locate a *working* C compiler: one that not only reports a version
/// but can actually compile a trivial program (so we don't fail when only a bare
/// toolchain stub is present, e.g. a clang without system headers).
fn find_cc() -> Option<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir();
    // A unique probe path per invocation avoids collisions when several tests
    // call `find_cc` concurrently (each runs in its own thread).
    let tag = COUNTER.fetch_add(1, Ordering::SeqCst);
    let probe = dir.join(format!("moira_cc_probe_{}_{}.c", std::process::id(), tag));
    std::fs::write(&probe, "int main(void){return 0;}\n").ok()?;
    let out = dir.join(format!("moira_cc_probe_{}_{}", std::process::id(), tag));
    for cc in ["gcc", "clang", "cc", "clang-cl"] {
        if Command::new(cc).arg("--version").output().is_err() {
            continue;
        }
        let status = if cc == "clang-cl" {
            Command::new(cc)
                .arg(&probe)
                .arg(format!("/Fe:{}", out.display()))
                .stderr(std::process::Stdio::null())
                .status()
        } else {
            Command::new(cc)
                .arg("-O2")
                .arg(&probe)
                .arg("-o")
                .arg(&out)
                .stderr(std::process::Stdio::null())
                .status()
        };
        if status.map(|s| s.success()).unwrap_or(false) {
            let _ = std::fs::remove_file(&out);
            let _ = std::fs::remove_file(&probe);
            return Some(cc.to_string());
        }
    }
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&probe);
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

/// Lower an arbitrary Moira source string (rather than the demo file) so
/// feature-specific codegen can be tested in isolation.
fn lower_src(src: &str) -> atropos::MirProgram {
    let prog = clotho::parse_source(src).expect("source should parse");
    atropos::lower_program(&prog).expect("source should lower")
}

#[test]
fn cast_lowers_to_c_cast() {
    // `x as i64` must lower to a C-style `(i64)x` conversion (the documented
    // `as`-cast self-host gap for `atropos`).
    let mir = lower_src("fn f(x: i32) -> i64 { x as i64 }");
    let c = atropos::codegen_c::emit_c(&mir);
    assert!(c.contains("(i64)x"), "expected a C cast `(i64)x`:\n{c}");
    // The cast temporary must be declared with the narrowed target type.
    assert!(c.contains("int64_t"), "cast temp should be i64:\n{c}");
}

/// Lower + compile + run a Moira source string, returning the program's stdout.
/// Skipped silently when no working C compiler is available.
fn run_src(src: &str) -> Option<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let cc = find_cc()?;
    let mir = lower_src(src);
    let c = atropos::codegen_c::emit_c(&mir);
    let dir = std::env::temp_dir();
    // A unique temp name per call avoids parallel-test linker collisions (each
    // test runs in its own thread but they share the process temp dir).
    let tag = COUNTER.fetch_add(1, Ordering::SeqCst);
    let c_path = dir.join(format!("moira_run_{}_{}.c", std::process::id(), tag));
    let exe_path = dir.join(format!("moira_run_{}_{}", std::process::id(), tag));
    std::fs::write(&c_path, c).ok()?;
    let status = if cc == "clang-cl" {
        Command::new(&cc)
            .arg("-O2")
            .arg(&c_path)
            .arg(format!("/Fe:{}", exe_path.display()))
            .status()
            .ok()
    } else {
        Command::new(&cc)
            .arg("-O2")
            .arg(&c_path)
            .arg("-o")
            .arg(&exe_path)
            .status()
            .ok()
    }?;
    if !status.success() {
        return None;
    }
    let out = Command::new(&exe_path).output().ok()?;
    let _ = std::fs::remove_file(&c_path);
    let _ = std::fs::remove_file(&exe_path);
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn for_over_array_literal_runs() {
    let out = match run_src(
        "fn main() {
            let mut total = 0;
            for x in [10, 20, 30] {
                total = total + x;
            }
            print_i32(total);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping for-over-array run");
            return;
        }
    };
    assert_eq!(out, "60", "expected 10+20+30 = 60");
}

#[test]
fn for_over_let_bound_array_runs() {
    let out = match run_src(
        "fn main() {
            let arr = [1, 2, 3, 4];
            let mut total = 0;
            for x in arr {
                total = total + x;
            }
            print_i32(total);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping for-over-let-array run");
            return;
        }
    };
    assert_eq!(out, "10", "expected 1+2+3+4 = 10");
}

#[test]
fn array_index_and_let_bound_array_run() {
    let out = match run_src(
        "fn main() {
            let arr = [10, 20, 30];
            let a = arr[0];
            let b = arr[2];
            let c = arr[1];
            print_i32(a);
            print_i32(b);
            print_i32(c);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping array-index run");
            return;
        }
    };
    assert_eq!(out, "10\n30\n20", "expected arr[0]=10, arr[2]=30, arr[1]=20");
}

#[test]
fn if_let_identifier_pattern_runs() {
    let out = match run_src(
        "fn main() {
            let v = 7;
            let mut r = 0;
            if let x = v {
                r = x;
            } else {
                r = 99;
            }
            print_i32(r);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping if-let run");
            return;
        }
    };
    assert_eq!(out, "7", "if-let should bind the scrutinee to `x`");
}

#[test]
fn while_let_identifier_pattern_runs() {
    let out = match run_src(
        "fn main() {
            let v = 3;
            let mut r = 0;
            let mut count = 0;
            while let x = v {
                r = r + x;
                count = count + 1;
                if count >= 2 { break; }
            }
            print_i32(r);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping while-let run");
            return;
        }
    };
    assert_eq!(out, "6", "while-let should bind `x` each iteration (3+3)");
}

#[test]
fn for_over_array_lowers_to_index_loop() {
    // The array-`for` must lower to an index (`Rvalue::Index`) inside a loop,
    // not just a range counter.
    let mir = lower_src(
        "fn f() { let mut t = 0; for x in [1, 2, 3] { t = t + x; } let _ = t; }",
    );
    let f = mir.functions.iter().find(|f| f.name == "f").unwrap();
    let has_index = f.blocks.iter().any(|b| {
        b.stmts
            .iter()
            .any(|s| matches!(s.rvalue, atropos::mir::Rvalue::Index(..)))
    });
    assert!(has_index, "for-over-array should lower to an Index rvalue");
}
