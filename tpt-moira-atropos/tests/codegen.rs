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

fn lower_demo() -> tpt_moira_atropos::MirProgram {
    let src = std::fs::read_to_string(demo_path()).unwrap();
    let prog = tpt_moira_clotho::parse_source(&src).expect("demo should parse");
    tpt_moira_atropos::lower_program(&prog).expect("demo should lower")
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
        .any(|b| matches!(b.term, tpt_moira_atropos::mir::Terminator::If(..)));
    assert!(has_if, "max should contain an If terminator");

    let main = mir.functions.iter().find(|f| f.name == "main").unwrap();
    let has_struct_lit = main.blocks.iter().any(|b| {
        b.stmts
            .iter()
            .any(|s| matches!(s.rvalue, tpt_moira_atropos::mir::Rvalue::StructLit(..)))
    });
    assert!(has_struct_lit, "main should build a Point struct literal");
}

#[test]
fn c_emission_contains_expected_shapes() {
    let mir = lower_demo();
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
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
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

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
fn lower_src(src: &str) -> tpt_moira_atropos::MirProgram {
    let prog = tpt_moira_clotho::parse_source(src).expect("source should parse");
    tpt_moira_atropos::lower_program(&prog).expect("source should lower")
}

#[test]
fn cast_lowers_to_c_cast() {
    // `x as i64` must lower to a C-style `(i64)x` conversion (the documented
    // `as`-cast self-host gap for `atropos`).
    let mir = lower_src("fn f(x: i32) -> i64 { x as i64 }");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
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
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
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
fn enum_match_and_if_let_run() {
    // Enum-variant construction (`Some(7)` / `None`), `match` with
    // tag-test guards + payload binding, and `if let Some(x) = ...` all
    // lower through the tagged-struct representation and produce correct
    // runtime values.
    let out = match run_src(
        "fn describe(o: Option) -> i32 {\n\
             match o {\n\
                 Some(v) => v,\n\
                 None => 0,\n\
             }\n\
         }\n\
         fn main() {\n\
             let a = Some(7);\n\
             let b = None;\n\
             print_i32(describe(a));\n\
             print_i32(describe(b));\n\
             if let Some(x) = a {\n\
                 print_i32(x + 1);\n\
             }\n\
         }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping enum match run");
            return;
        }
    };
    assert_eq!(out, "7\n0\n8", "expected describe(Some)=7, describe(None)=0, x+1=8");
}

#[test]
fn enum_c_emission_contains_tagged_struct() {
    let mir = lower_src(
        "fn f(o: Option) -> i32 {\n\
             match o {\n\
                 Some(v) => v,\n\
                 None => 0,\n\
             }\n\
         }",
    );
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(
        c.contains("struct Option {"),
        "enum should lower to a tagged struct:\n{c}"
    );
    assert!(c.contains(".tag"), "construction should set the tag:\n{c}");
    assert!(
        c.contains(".f0 = ") || c.contains("o.f0"),
        "payload should be stored/read via the f0 field:\n{c}"
    );
}

#[test]
fn println_macro_expands_to_print_calls() {
    // `println!`/`eprintln!` expand to interleaved `print_str`/`print_i64`
    // runtime calls; literal segments, `{}` placeholders and the trailing
    // newline all appear in the emitted C.
    let mir = lower_src(
        "fn main() {\n\
             let n = 42;\n\
             println!(\"hello\");\n\
             println!(\"n = {}!\", n);\n\
         }",
    );
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(c.contains("print_str(\"hello\")"), "literal segment:\n{c}");
    assert!(c.contains("print_i64(n)"), "placeholder arg printed:\n{c}");
    assert!(c.contains("print_str(\"\\n\")"), "trailing newline:\n{c}");
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
            .any(|s| matches!(s.rvalue, tpt_moira_atropos::mir::Rvalue::Index(..)))
    });
    assert!(has_index, "for-over-array should lower to an Index rvalue");
}

#[test]
fn tuple_lowers_to_generated_struct() {
    // `(a, b)` must lower to a generated `__tuple_*` struct literal, and `t.0`
    // / `t.1` to `.f0` / `.f1` field access on that struct.
    let mir = lower_src("fn main() { let p = (3, 4); let s = p.0 + p.1; print_i64(s); }");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(c.contains("struct __tuple_"), "tuple should lower to a struct:\n{c}");
    assert!(c.contains(".f0 = 3"), "tuple element 0 should map to .f0:\n{c}");
    assert!(c.contains(".f1 = 4"), "tuple element 1 should map to .f1:\n{c}");
    assert!(c.contains("p.f0"), "tuple index 0 should lower to p.f0:\n{c}");
    assert!(c.contains("p.f1"), "tuple index 1 should lower to p.f1:\n{c}");
}

#[test]
fn tuple_destructure_lowers_and_runs() {
    // `let (a, b) = (x, y)` must bind each element to a local, then the body can
    // use them. Compiles + runs via a real C compiler.
    let out = match run_src(
        "fn main() {
            let (a, b) = (3, 4);
            let s = a + b;
            print_i64(s);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping tuple-destructure run");
            return;
        }
    };
    assert_eq!(out, "7", "expected 3+4 = 7 from destructured tuple");
}

#[test]
fn tuple_destructure_from_call_lowers_and_runs() {
    // Destructure a tuple returned by a call (RHS is not a literal).
    let out = match run_src(
        "fn pair() -> (i64, i64) { (10, 20) }
         fn main() {
            let (x, y) = pair();
            print_i64(x + y);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping tuple-destructure-call run");
            return;
        }
    };
    assert_eq!(out, "30", "expected 10+20 = 30 from destructured call result");
}

#[test]
fn tuple_destructure_literal_lowers_to_direct_assignments() {
    // A tuple-literal RHS is decomposed element-wise: each binding is declared and
    // assigned its component directly (no intermediate struct value).
    let mir = lower_src("fn main() { let (a, b) = (1, 2); let _ = a + b; }");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(c.contains("a = 1"), "first binding should be assigned directly:\n{c}");
    assert!(c.contains("b = 2"), "second binding should be assigned directly:\n{c}");
}

#[test]
fn range_pattern_lowers_to_comparison_and_runs() {
    // `match x { 0..=9 => 1, _ => 0 }` must lower the inclusive range arm to a
    // bounded comparison and run correctly for in-range and out-of-range values.
    let in_range = match run_src(
        "fn main() {
            let x = 5;
            let r = match x { 0..=9 => 1, _ => 0 };
            print_i64(r);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping range-pattern run");
            return;
        }
    };
    assert_eq!(in_range, "1", "5 is within 0..=9");
    let out_of_range = match run_src(
        "fn main() {
            let x = 20;
            let r = match x { 0..=9 => 1, _ => 0 };
            print_i64(r);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping range-pattern run");
            return;
        }
    };
    assert_eq!(out_of_range, "0", "20 is outside 0..=9");
}

#[test]
fn exclusive_and_open_range_patterns_run() {
    // Exclusive upper bound (`0..3`) and open lower bound (`2..`) behave correctly.
    let out = match run_src(
        "fn classify(n: i64) -> i64 {
            match n {
                0..3 => 1,
                3.. => 2,
                _ => 0,
            }
        }
        fn main() { print_i64(classify(2)); print_i64(classify(7)); }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping open-range run");
            return;
        }
    };
    assert_eq!(out, "1\n2", "2 in [0,3) -> 1; 7 in [3,) -> 2");
}

#[test]
fn for_over_tuple_array_destructures_and_runs() {
    // `for (a, b) in [(1, 2), (3, 4)]` must bind each tuple element per iteration.
    let out = match run_src(
        "fn main() {
            let mut total = 0;
            for (a, b) in [(1, 2), (3, 4)] {
                total = total + a + b;
            }
            print_i64(total);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping for-tuple-destructure run");
            return;
        }
    };
    assert_eq!(out, "10", "expected (1+2)+(3+4) = 10");
}

#[test]
fn for_over_tuple_array_lowers_to_field_reads() {
    // The array-of-tuples element binds each component from its `.fN` field.
    let mir = lower_src(
        "fn f() { for (a, b) in [(1, 2), (3, 4)] { let _ = a + b; } }",
    );
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(c.contains("struct __tuple_"), "tuple array element needs a struct type:\n{c}");
    assert!(c.contains(".f0"), "first component must be read via .f0:\n{c}");
}

#[test]
fn match_lowers_to_conditional_chain() {
    // `match x { 1 => 10, 2 => 20, _ => 99 }` must lower to an equality-test
    // chain (`Eq x, 1` / `Eq x, 2`) driving `if`/`else` blocks, with the `_`
    // arm taken unconditionally.
    let mir = lower_src(
        "fn main() {
            let x = 2;
            let r = match x {
                1 => 10,
                2 => 20,
                _ => 99,
            };
            print_i64(r);
        }",
    );
    let main = mir.functions.iter().find(|f| f.name == "main").unwrap();
    let has_eq = main.blocks.iter().any(|b| {
        b.stmts
            .iter()
            .any(|s| matches!(s.rvalue, tpt_moira_atropos::mir::Rvalue::Binary(tpt_moira_atropos::mir::BinOp::Eq, _, _)))
    });
    assert!(has_eq, "match literal arms should lower to Eq comparisons");
    // The wildcard arm is irrefutable — at least one `goto` (not an `if`) into an
    // arm body.
    let has_if = main
        .blocks
        .iter()
        .any(|b| matches!(b.term, tpt_moira_atropos::mir::Terminator::If(..)));
    assert!(has_if, "match should contain If terminators");
}
