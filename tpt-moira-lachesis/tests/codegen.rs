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
    // Windows text-mode C stdio emits CRLF; normalise before comparing.
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    // s = 3+4 = 7, m = max(7,10) = 10, t = sum_to(5) = 0+1+2+3+4 = 10
    assert_eq!(stdout.trim(), "7\n10\n10", "unexpected program output");
}

/// Lower an arbitrary Moira source string (rather than the demo file) so
/// feature-specific codegen can be tested in isolation.
/// Lower a source the way the compiler does: type-check first, then lower with the checker's
/// hints (the untyped `lower_program` path is not what `moira` runs).
fn lower_src(src: &str) -> tpt_moira_atropos::MirProgram {
    let prog = tpt_moira_clotho::compile_source(src).expect("source should parse");
    // The driver specializes generic functions and structs before checking; so do the tests.
    let prog = tpt_moira_lachesis::mono::monomorphize(&prog, &[]);
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "type-check: {:?}", res.err());
    tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("source should lower")
}

#[test]
fn cast_lowers_to_c_cast() {
    // `x as i64` must lower to a C-style `(i64)x` conversion (the documented
    // `as`-cast self-host gap for `atropos`).
    let mir = lower_src("fn f(x: i32) -> i64 { x as i64 }");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(
        c.contains("(long long)x"),
        "expected a C cast `(long long)x`:\n{c}"
    );
    // The cast temporary must be declared with the narrowed target type.
    assert!(!c.contains("int64_t"), "i64 is spelled long long:\n{c}");
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
        // A pointer or integer mismatch in the emitted C is a miscompile that still runs (the
        // value is wrong), so those warnings are errors here.
        Command::new(&cc)
            .arg("-O2")
            .arg("-Werror=incompatible-pointer-types")
            .arg("-Werror=int-conversion")
            .arg(&c_path)
            .arg("-o")
            .arg(&exe_path)
            .status()
            .ok()
    }?;
    // A C compiler is present, so a program that does not compile is a failure, not a skip.
    if !status.success() {
        panic!(
            "emitted C failed to compile ({}): {}",
            c_path.display(),
            "see the compiler output above"
        );
    }
    let out = Command::new(&exe_path).output().ok()?;
    let _ = std::fs::remove_file(&c_path);
    let _ = std::fs::remove_file(&exe_path);
    Some(
        String::from_utf8_lossy(&out.stdout)
            .replace("\r\n", "\n")
            .trim()
            .to_string(),
    )
}

/// A literal or nested variant inside a payload is a test, not a binding: the arm is taken
/// only when the payload matches. Previously the payload sub-guards were dropped, so
/// `Some(1)` matched `Some(9)`.
#[test]
fn variant_payload_literal_and_nested_patterns_are_tested() {
    let out = match run_src(
        "enum Shape { Circle(i64), Rect(i64, i64) }
        fn main() {
            let t: Option<i64> = Some(9);
            let a = match t {
                Some(1) => 0,
                Some(_) => 1,
                None => 2,
            };
            print_i64(a);
            let s = match Shape::Rect(3, 4) {
                Shape::Rect(3, h) => h,
                Shape::Rect(_, h) => h * 10,
                Shape::Circle(r) => r,
            };
            print_i64(s);
            let o: Option<Option<i64>> = Some(None);
            let u = match o {
                Some(Some(v)) => v,
                Some(None) => 77,
                None => 0,
            };
            print_i64(u);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping variant pattern run");
            return;
        }
    };
    assert_eq!(out, "1\n4\n77", "payload sub-patterns must be tested");
}

/// `let P = E else { B };` takes the rest of the block when `P` matches and runs `B`
/// (which diverges) otherwise. Previously the `else` block was dropped and the binding
/// read garbage.
#[test]
fn let_else_runs_the_else_block_when_the_pattern_fails() {
    let out = match run_src(
        "fn pick(v: Option<i64>) -> i64 {
            let Some(x) = v else {
                return -1;
            };
            let y = x + 1;
            y * 2
        }
        fn main() {
            print_i64(pick(Some(3)));
            print_i64(pick(None));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping let-else run");
            return;
        }
    };
    assert_eq!(
        out, "8\n-1",
        "the else block must run when the pattern does not match"
    );
}

/// `s.as_bytes()` is a `&[u8]` over the string's bytes: it has the string's length, indexes
/// and iterates, and can be stored in a struct field (the shape of a lexer's byte view).
#[test]
fn as_bytes_views_the_string_bytes_and_runs() {
    let out = match run_src(
        "struct Lexer<'a> { src: &'a str, bytes: &'a [u8], pos: usize }
        impl<'a> Lexer<'a> {
            fn new(src: &'a str) -> Self {
                Lexer { src, bytes: src.as_bytes(), pos: 0 }
            }
        }
        fn main() {
            let s = String::from(\"AB\");
            let b = s.as_bytes();
            print_i64(b.len() as i64);
            print_i64(b[1] as i64);
            let mut n: u32 = 0;
            for x in b.iter() {
                n = n + (*x as u32);
            }
            print_i64(n as i64);
            let l = Lexer::new(\"hey\");
            print_i64(l.pos as i64);
            print_i64(l.bytes.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping as_bytes run");
            return;
        }
    };
    assert_eq!(
        out, "2\n66\n131\n0\n3",
        "as_bytes should view the string's bytes"
    );
}

/// `v.contains(&x)` compares the element with `x`: the reference is not passed to the
/// helper. Scalar elements (`usize`, `i64`) on a vector local, a `&Vec` parameter and a
/// slice all failed to compile before, since the `&` reached the C helper.
#[test]
fn contains_with_a_reference_compares_scalar_elements() {
    let out = match run_src(
        "fn has(v: &Vec<usize>, x: usize) -> bool { v.contains(&x) }
        fn in_slice(v: &[usize], x: usize) -> bool { v.contains(&x) }
        fn main() {
            let v: Vec<i64> = vec![2, 3];
            let x: i64 = 3;
            print_bool(v.contains(&x));
            let u: Vec<usize> = vec![2, 3];
            print_bool(has(&u, 3));
            print_bool(in_slice(&u, 9));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping contains run");
            return;
        }
    };
    assert_eq!(
        out, "true\ntrue\nfalse",
        "contains(&x) should compare values"
    );
}

/// A `vec![..]` literal passed to a map's `insert` takes the map's value type: the element
/// type comes from the context, not only from the checker's hint.
#[test]
fn vec_literal_as_map_value_takes_the_value_type() {
    let out = match run_src(
        "use std::collections::HashMap;
        fn main() {
            let mut m: HashMap<String, Vec<usize>> = HashMap::new();
            m.insert(String::from(\"x\"), vec![2, 3]);
            print_i64(m.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping map value run");
            return;
        }
    };
    assert_eq!(
        out, "1",
        "the inserted vector should have the map's value type"
    );
}

/// `Option::is_none_or(f)`: an absent option is `true`, a present one applies `f`.
#[test]
fn option_is_none_or_runs_for_both_variants() {
    let out = match run_src(
        "fn main() {
            let a: Option<i64> = None;
            let b: Option<i64> = Some(-1);
            let c: Option<i64> = Some(5);
            print_bool(a.is_none_or(|x| x > 0));
            print_bool(b.is_none_or(|x| x > 0));
            print_bool(c.is_none_or(|x| x > 0));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping is_none_or run");
            return;
        }
    };
    assert_eq!(
        out, "true\nfalse\ntrue",
        "is_none_or should test only a present payload"
    );
}

/// `filter_map` keeps the payloads of the `Some` results and drops the `None`s, and the
/// chain's terminal sees those payloads (`sum`, `collect`).
#[test]
fn filter_map_keeps_some_payloads_for_sum_and_collect() {
    let out = match run_src(
        "fn main() {
            let v: Vec<i64> = vec![1, 2, 3, 4, 5, 6];
            let s: i64 = v.iter().filter_map(|x| if x % 2 == 0 { Some(x * 10) } else { None }).sum();
            print_i64(s);
            let w: Vec<i64> = v.iter().filter_map(|x| if *x > 4 { Some(*x + 100) } else { None }).collect();
            print_i64(w.len() as i64);
            print_i64(w[0]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping filter_map run");
            return;
        }
    };
    assert_eq!(
        out, "120\n2\n105",
        "filter_map should keep only the Some payloads"
    );
}

/// `Result::map` maps the `Ok` payload and passes an `Err` through; `Result::map_err` maps
/// the `Err` payload and passes an `Ok` through. Both chain into `unwrap_or`.
#[test]
fn result_map_and_map_err_map_one_arm_and_pass_the_other() {
    let out = match run_src(
        "fn main() {
            let e: Result<i64, i64> = Err(7);
            match e.map(|x| x * 2) {
                Ok(v) => print_i64(v),
                Err(x) => print_i64(x),
            }
            let o: Result<i64, i64> = Ok(7);
            match o.map_err(|x| x * 100) {
                Ok(v) => print_i64(v),
                Err(x) => print_i64(x),
            }
            let r: Result<i64, i64> = Ok(3);
            print_i64(r.map(|x| x + 1).unwrap_or(0));
            let q: Result<i64, i64> = Err(9);
            print_i64(q.map(|x| x + 1).unwrap_or(-5));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping Result::map run");
            return;
        }
    };
    assert_eq!(
        out, "7\n7\n4\n-5",
        "map maps Ok and passes Err; map_err maps Err and passes Ok"
    );
}

/// `r.map(Some)` wraps an `Ok` payload in an `Option`: the mapping function is the bare
/// constructor, so no closure is lowered.
#[test]
fn result_map_with_some_constructor_wraps_the_payload() {
    let out = match run_src(
        "fn parse(n: i64) -> Result<i64, String> {
            if n > 0 { Ok(n) } else { Err(String::from(\"neg\")) }
        }
        fn main() {
            let r = parse(4).map(Some);
            match r {
                Ok(o) => match o {
                    Some(v) => print_i64(v),
                    None => print_i64(-1),
                },
                Err(_) => print_i64(-2),
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping map(Some) run");
            return;
        }
    };
    assert_eq!(out, "4", "map(Some) should wrap the Ok payload");
}

/// `Vec::reverse`, `Vec::remove(i)` (returns the element, shifts the tail) and
/// `Vec::append(&mut w)` (moves `w`'s elements and leaves `w` empty) on a named vector.
#[test]
fn vec_reverse_remove_and_append_run() {
    let out = match run_src(
        "fn main() {
            let mut v: Vec<i64> = vec![1, 2, 3, 4];
            v.reverse();
            print_i64(v[0]);
            print_i64(v[3]);
            let r = v.remove(1);
            print_i64(r);
            print_i64(v.len() as i64);
            print_i64(v[1]);
            let mut a: Vec<i64> = vec![1];
            let mut b: Vec<i64> = vec![2, 3];
            a.append(&mut b);
            print_i64(a.len() as i64);
            print_i64(b.len() as i64);
            print_i64(a[2]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping Vec operations run");
            return;
        }
    };
    assert_eq!(
        out, "4\n1\n3\n3\n2\n3\n0\n3",
        "reverse, remove and append should match Rust"
    );
}

/// `Option::or(other)` is the receiver when it holds a value, else `other`.
#[test]
fn option_or_takes_the_receiver_or_the_argument() {
    let out = match run_src(
        "fn main() {
            let o: Option<i64> = None;
            print_i64(o.or(Some(4)).unwrap());
            let p: Option<i64> = Some(2);
            print_i64(p.or(Some(9)).unwrap());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping Option::or run");
            return;
        }
    };
    assert_eq!(out, "4\n2", "or should keep a present value");
}

/// `str::split_at(i)` gives the two halves as strings.
#[test]
fn str_split_at_gives_both_halves() {
    let out = match run_src(
        "fn main() {
            let s = String::from(\"abcd\");
            let (a, b) = s.split_at(1);
            println!(\"{} {}\", a, b);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping split_at run");
            return;
        }
    };
    assert_eq!(out, "a bcd", "split_at should split the string in two");
}

/// A user function named after a C library function (`exit`) is renamed consistently at its
/// definition and its calls, while library calls the lowering emits (`strlen`) keep their names.
#[test]
fn user_function_named_like_a_c_library_call_runs() {
    let out = match run_src(
        "fn exit(x: i64) -> i64 { x + 1 }
        fn main() {
            print_i64(exit(4));
            let s = String::from(\"abc\");
            print_i64(s.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping reserved-name run");
            return;
        }
    };
    assert_eq!(
        out, "5\n3",
        "a user `exit` must not collide with the C `exit`"
    );
}

/// `for part in s.split(sep)` yields the pieces between separators, empty ones included, as
/// Rust does: `"a,,b,"` gives `a`, ``, `b`, ``. A multi-character separator works too.
#[test]
fn for_over_str_split_yields_rust_pieces() {
    let out = match run_src(
        "fn main() {
            let s = String::from(\"a,,b,\");
            for p in s.split(',') {
                println!(\"[{}]\", p);
            }
            let t = String::from(\"x::y\");
            let mut c = 0;
            for p in t.split(\"::\") {
                c = c + p.len();
            }
            print_i64(c as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping str split run");
            return;
        }
    };
    assert_eq!(
        out, "[a]\n[]\n[b]\n[]\n2",
        "split should yield every piece, empty ones included"
    );
}

/// `s.split(sep)` as an iterator chain source: `collect` into a `Vec<&str>`, a `map` and
/// `sum` over the pieces, and `count` (empty pieces included).
#[test]
fn split_chain_collects_maps_and_counts_pieces() {
    let out = match run_src(
        "fn main() {
            let s = String::from(\"a,bb,,ccc\");
            let v: Vec<&str> = s.split(',').collect();
            print_i64(v.len() as i64);
            print_i64(v[2].len() as i64);
            let n: usize = s.split(',').map(|p| p.len()).sum();
            print_i64(n as i64);
            print_i64(s.split(',').count() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping split chain run");
            return;
        }
    };
    assert_eq!(
        out, "4\n0\n6\n4",
        "split should be a chain source of its pieces"
    );
}

/// `let x: T = s.parse().unwrap()` and `.unwrap_or(d)` for a numeric `T`: a signed or unsigned
/// integer parses only when it fits its width, and a float parses as `f64`. A value that
/// does not parse takes the default, or panics for `unwrap`.
#[test]
fn let_parse_unwrap_takes_the_annotated_numeric_type() {
    let out = match run_src(
        "fn main() {
            let s = String::from(\"42\");
            let n: i64 = s.parse().unwrap();
            print_i64(n + 1);
            let neg = String::from(\"-7\");
            let m: i64 = neg.parse().unwrap_or(0);
            print_i64(m);
            let bad = String::from(\"x1\");
            let k: i64 = bad.parse().unwrap_or(-3);
            print_i64(k);
            let big = String::from(\"300\");
            let b: u8 = big.parse().unwrap_or(9);
            print_i64(b as i64);
            let f = String::from(\"2.5\");
            let x: f64 = f.parse().unwrap();
            print_f64(x * 2.0);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping let parse run");
            return;
        }
    };
    assert_eq!(
        out, "43\n-7\n-3\n9\n5",
        "parse should take the annotated type"
    );
}

/// `to_string()` on a reference to a string (a `HashMap::get` payload, which is a pointer)
/// copies the string the reference points to.
#[test]
fn to_string_through_a_string_reference_copies_the_string() {
    let out = match run_src(
        "use std::collections::HashMap;
        struct Table { types: HashMap<String, String> }
        impl Table {
            fn ty(&self, name: &str) -> String {
                match self.types.get(name) {
                    Some(t) => t.to_string(),
                    None => String::from(\"none\"),
                }
            }
        }
        fn main() {
            let mut m: HashMap<String, String> = HashMap::new();
            m.insert(String::from(\"a\"), String::from(\"int\"));
            let t = Table { types: m };
            println!(\"{}\", t.ty(\"a\"));
            println!(\"{}\", t.ty(\"b\"));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping to_string run");
            return;
        }
    };
    assert_eq!(
        out, "int\nnone",
        "to_string should read through the reference"
    );
}

/// `len()` and `is_empty()` on a fixed-size array (a local or a constant) are its length, and
/// `max`/`min` on an integer compare its operands.
#[test]
fn fixed_array_len_and_integer_max_min_run() {
    let out = match run_src(
        "const REGS: [&str; 3] = [\"a\", \"bb\", \"ccc\"];
        fn main() {
            let a: [i64; 2] = [1, 2];
            print_i64(a.len() as i64);
            print_bool(a.is_empty());
            print_i64(REGS.len() as i64);
            let x: i64 = 3;
            let y: i64 = 9;
            print_i64(x.max(y));
            print_i64(x.min(y));
            let c: u8 = 7;
            print_i64(c.min(4) as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping array len run");
            return;
        }
    };
    assert_eq!(
        out, "2\nfalse\n3\n9\n3\n4",
        "array length and integer max/min should match Rust"
    );
}

/// `HashMap::get_mut` gives a mutable view of the value (a write through it persists), and
/// `Iterator::nth(i)` is the item at position `i`, or `None` when there is none.
#[test]
fn map_get_mut_writes_through_and_iterator_nth_indexes() {
    let out = match run_src(
        "use std::collections::HashMap;
        fn main() {
            let mut m: HashMap<String, i64> = HashMap::new();
            m.insert(String::from(\"a\"), 1);
            if let Some(v) = m.get_mut(\"a\") {
                *v = *v + 5;
            }
            print_i64(*m.get(\"a\").unwrap());
            match m.get_mut(\"zz\") {
                Some(_) => print_i64(1),
                None => print_i64(0),
            }
            let s = String::from(\"a9\");
            let c = s.chars().nth(1).unwrap();
            print_bool(c.is_ascii_digit());
            match s.chars().nth(5) {
                Some(_) => print_i64(1),
                None => print_i64(0),
            }
            let v: Vec<i64> = vec![4, 5, 6];
            print_i64(v.iter().nth(2).copied().unwrap());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping get_mut/nth run");
            return;
        }
    };
    assert_eq!(
        out, "6\n0\ntrue\n0\n6",
        "get_mut should write through and nth should index"
    );
}

/// `Option::as_ref()` on an `Option<String>` and on an `Option<i64>`: the view has the
/// checker's type, so the value reads through it without a C type mismatch.
#[test]
fn option_as_ref_over_string_and_integer_runs() {
    let out = match run_src(
        "fn main() {
            let o: Option<String> = Some(String::from(\"hi\"));
            let r: Option<&String> = o.as_ref();
            print_bool(r.is_some());
            match r {
                Some(s) => print_i64(s.len() as i64),
                None => print_i64(0),
            }
            let n: Option<i64> = Some(4);
            let m: Option<&i64> = n.as_ref();
            match m {
                Some(v) => print_i64(*v + 1),
                None => print_i64(0),
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping as_ref run");
            return;
        }
    };
    assert_eq!(
        out, "true\n2\n5",
        "as_ref should view the payload with its checked type"
    );
}

/// `HashSet::intersection(&b)` keeps the keys of `a` that `b` holds, `union(&b)` keeps every key
/// of either set once. Both count, and iterate in a `for` loop.
#[test]
fn hashset_intersection_and_union_count_and_iterate() {
    let out = match run_src(
        "use std::collections::HashSet;
        fn main() {
            let mut a: HashSet<i64> = HashSet::new();
            a.insert(1);
            a.insert(2);
            a.insert(3);
            let mut b: HashSet<i64> = HashSet::new();
            b.insert(3);
            b.insert(9);
            print_i64(a.intersection(&b).count() as i64);
            print_i64(a.union(&b).count() as i64);
            let mut s: HashSet<String> = HashSet::new();
            s.insert(String::from(\"x\"));
            s.insert(String::from(\"y\"));
            let mut t: HashSet<String> = HashSet::new();
            t.insert(String::from(\"x\"));
            t.insert(String::from(\"z\"));
            let mut n = 0;
            for k in s.union(&t) {
                n = n + k.len();
            }
            print_i64(n as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping set operation run");
            return;
        }
    };
    assert_eq!(out, "1\n4\n3", "set operations should match Rust");
}

/// A set reached through a reference parameter (`&HashSet<String>`) takes part in
/// `intersection`/`union` like an owned one.
#[test]
fn set_operation_through_a_reference_parameter() {
    let out = match run_src(
        "use std::collections::HashSet;
        fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> i64 {
            a.intersection(b).count() as i64
        }
        fn main() {
            let mut a: HashSet<String> = HashSet::new();
            a.insert(String::from(\"x\"));
            a.insert(String::from(\"y\"));
            let mut b: HashSet<String> = HashSet::new();
            b.insert(String::from(\"x\"));
            print_i64(jaccard(&a, &b));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping set reference run");
            return;
        }
    };
    assert_eq!(
        out, "1",
        "a set behind a reference should intersect like an owned set"
    );
}

/// `Vec::clear` empties a vector of any element type; `Vec::dedup` drops consecutive equal
/// items of a string or scalar vector.
#[test]
fn vec_clear_and_dedup_run() {
    let out = match run_src(
        "fn main() {
            let mut v: Vec<String> = Vec::new();
            v.push(String::from(\"a\"));
            v.push(String::from(\"a\"));
            v.push(String::from(\"b\"));
            v.dedup();
            print_i64(v.len() as i64);
            let mut w: Vec<i64> = vec![1, 1, 2, 2, 2, 3, 1];
            w.dedup();
            print_i64(w.len() as i64);
            print_i64(w[3] as i64);
            w.clear();
            print_i64(w.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping clear/dedup run");
            return;
        }
    };
    assert_eq!(out, "2\n4\n1\n0", "clear and dedup should match Rust");
}

/// `m.values()` and `m.keys()` are iterator chains over a map's values and keys: `sum`,
/// `count`, and a `for` loop over the keys.
#[test]
fn map_values_and_keys_are_iterator_sources() {
    let out = match run_src(
        "use std::collections::HashMap;
        fn main() {
            let mut m: HashMap<String, i64> = HashMap::new();
            m.insert(String::from(\"a\"), 2);
            m.insert(String::from(\"bb\"), 3);
            let t: i64 = m.values().sum();
            print_i64(t);
            print_i64(m.keys().count() as i64);
            let mut n = 0;
            for k in m.keys() {
                n = n + k.len();
            }
            print_i64(n as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping map view run");
            return;
        }
    };
    assert_eq!(
        out, "5\n2\n3",
        "map keys and values should iterate as Rust does"
    );
}

/// `Option::as_mut()` views the payload mutably: a write through it persists in the option.
#[test]
fn option_as_mut_writes_through() {
    let out = match run_src(
        "fn main() {
            let mut p: Option<i64> = Some(3);
            if let Some(x) = p.as_mut() {
                *x = *x + 1;
            }
            print_i64(p.unwrap());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping as_mut run");
            return;
        }
    };
    assert_eq!(out, "4", "as_mut should write through the payload");
}

/// `str::split_once(pat)` splits at the first match (a character or a string), and is `None`
/// without one. `Vec::split_last()` gives the last element and a slice of the rest, or `None`.
#[test]
fn split_once_and_vec_split_last_run() {
    let out = match run_src(
        "fn main() {
            let s = String::from(\"key=val=x\");
            match s.split_once('=') {
                Some((k, v)) => println!(\"{} {}\", k, v),
                None => println!(\"none\"),
            }
            let t = String::from(\"a::b\");
            match t.split_once(\"::\") {
                Some((a, b)) => print_i64(b.len() as i64 + a.len() as i64),
                None => print_i64(0),
            }
            let v: Vec<i64> = vec![1, 2, 3];
            match v.split_last() {
                Some((l, rest)) => {
                    print_i64(*l);
                    print_i64(rest.len() as i64);
                }
                None => print_i64(-1),
            }
            let e: Vec<i64> = Vec::new();
            match e.split_last() {
                Some(_) => print_i64(1),
                None => print_i64(0),
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping split_once run");
            return;
        }
    };
    assert_eq!(
        out, "key val=x\n2\n3\n2\n0",
        "split_once and split_last should match Rust"
    );
}

/// Constructors and `None` take the type the context expects: a returned tuple's components
/// (`Vec::new()`, `None`), a `return` of `Vec::new()` (its code after the return is dead and
/// keeps no second return), and a `push` of `HashMap::new()` into a field.
#[test]
fn constructors_take_the_expected_type_in_returns_and_pushes() {
    let out = match run_src(
        "use std::collections::HashMap;
        struct E { env: Vec<HashMap<String, i64>> }
        fn pair(n: usize) -> (Vec<usize>, Option<usize>, usize) {
            if n == 0 {
                return (Vec::new(), None, 0);
            }
            (Vec::new(), Some(n), n)
        }
        fn fresh() -> Vec<usize> {
            return Vec::new();
        }
        fn main() {
            let (v, o, k) = pair(3);
            print_i64(v.len() as i64);
            print_bool(o.is_some());
            print_i64(k as i64);
            print_i64(fresh().len() as i64);
            let mut e = E { env: Vec::new() };
            e.env.push(HashMap::new());
            print_i64(e.env.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping constructor run");
            return;
        }
    };
    assert_eq!(
        out, "0\ntrue\n3\n0\n1",
        "constructors should take the expected type"
    );
}

/// A closure passed for an `impl Fn(A) -> R` parameter takes `A` and `R` from it: a struct
/// argument (`None`, typed by the parameter) and an `i64` result. A closure that captures a
/// variable and is passed as a value is reported, not emitted.
#[test]
fn closure_parameters_take_the_declared_struct_and_result_types() {
    let out = match run_src(
        "fn each(f: impl Fn(Option<i64>) -> i64) -> i64 { f(None) }
        fn main() {
            print_i64(each(|b| if b.is_none() { 7 } else { 0 }));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping closure ABI run");
            return;
        }
    };
    assert_eq!(
        out, "7",
        "the closure should receive an Option and return an i64"
    );
}

/// A helper that takes a closure is expanded at its call site: the closure body runs where the
/// helper calls it, so a write to a captured variable reaches the caller's variable, and a
/// `match` in the closure body lowers as written.
#[test]
fn helper_taking_a_closure_is_expanded_at_its_call_site() {
    let out = match run_src(
        "fn each(items: &[i64], f: &mut impl FnMut(i64, Option<i64>)) {
            for x in items {
                let y = *x + 1;
                f(y, if *x > 1 { Some(*x) } else { None });
            }
        }
        fn apply(v: i64, f: impl Fn(Option<i64>) -> i64) -> i64 {
            let t = v * 2;
            f(Some(t)) + 1
        }
        fn main() {
            let mut n = 0;
            each(&[1, 2, 3], &mut |a, b| {
                if b.is_none() {
                    n = n + a;
                }
            });
            let k = 10;
            let r = apply(4, |o| match o {
                Some(x) => x + k,
                None => 0,
            });
            print_i64(n);
            print_i64(r);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping closure expansion run");
            return;
        }
    };
    assert_eq!(
        out, "2\n19",
        "the expanded helper should write the captured variable"
    );
}

/// A let-bound closure that writes a captured variable writes the caller's variable: the
/// variable is passed by pointer, and the closure body reads and writes it through that
/// pointer. A closure that only reads a capture still passes it by value.
#[test]
fn let_bound_closure_writes_reach_the_captured_variable() {
    let out = match run_src(
        "fn main() {
            let mut n = 0;
            let k = 10;
            let mut f = |a: i64| {
                n = n + a;
                n += k;
            };
            f(2);
            f(3);
            print_i64(n);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping by-reference capture run");
            return;
        }
    };
    assert_eq!(out, "25", "each call should add to the captured variable");
}

/// A generic helper that takes its closure by `&mut F` (or `impl FnMut`) is specialised for the
/// closure: a write to a captured variable reaches the caller, the closure's `Option` parameter
/// has its declared type, and a recursive helper that passes the closure on works.
#[test]
fn generic_helper_with_a_borrowed_closure_writes_the_capture() {
    let out = match run_src(
        "fn each<F: FnMut(i64, Option<i64>)>(items: &[i64], f: &mut F) {
            for x in items {
                let y = *x + 1;
                f(y, if *x > 1 { Some(*x) } else { None });
            }
        }
        fn walk<F: FnMut(i64)>(n: i64, f: &mut F) {
            if n > 0 {
                f(n);
                walk(n - 1, f);
            }
        }
        fn main() {
            let mut n = 0;
            each(&[1, 2, 3], &mut |a, b| {
                if b.is_none() {
                    n = n + a;
                }
            });
            print_i64(n);
            let mut s: i64 = 0;
            walk(3, &mut |x| {
                s = s + x;
            });
            print_i64(s);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping generic closure run");
            return;
        }
    };
    assert_eq!(
        out, "2\n6",
        "the specialised helpers should write the captured variables"
    );
}

/// A recursive helper that passes two closures on to itself, as `visit_expr` does: each closure
/// is specialised into the copies, a write to each captured variable reaches the caller, and a
/// capture read by one closure and written by the other is shared by both.
#[test]
fn recursive_helper_passing_two_closures_on_runs() {
    let out = match run_src(
        "struct Node { v: i64, kids: Vec<Node> }
        fn visit(n: &mut Node, fe: &mut impl FnMut(&mut Node), fb: &mut impl FnMut(&mut Node)) {
            fe(n);
            for k in n.kids.iter_mut() {
                visit(k, fe, fb);
            }
            fb(n);
        }
        fn main() {
            let mut total: i64 = 0;
            let mut count: i64 = 0;
            let mut t = Node {
                v: 1,
                kids: vec![Node { v: 2, kids: vec![] }, Node { v: 3, kids: vec![] }],
            };
            visit(
                &mut t,
                &mut |n| {
                    total = total + n.v;
                },
                &mut |_n| {
                    count = count + 1;
                },
            );
            print_i64(total);
            print_i64(count);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping two-closure pass-on run");
            return;
        }
    };
    assert_eq!(out, "6\n3", "each closure should run on every node");
}

/// A path is its text: `parent()` is the text before the last separator (`None` for a root),
/// `to_string_lossy()` is the text, and `exists()` / `is_file()` ask the file system.
#[test]
fn path_text_methods_run() {
    let out = match run_src(
        "fn main() {
            let p = String::from(\"dir/sub/file.txt\");
            match p.parent() {
                Some(d) => println!(\"{}\", d),
                None => println!(\"none\"),
            }
            let q = String::from(\".\");
            print_bool(q.exists());
            let r = String::from(\"definitely_missing_xyz\");
            print_bool(r.is_file());
            let s = String::from(\"a/b\");
            println!(\"{}\", s.to_string_lossy());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping path run");
            return;
        }
    };
    assert_eq!(
        out, "dir/sub\ntrue\nfalse\na/b",
        "path methods should match Rust"
    );
}

/// `v.iter().rev()` walks the sequence from its last element; the source is left unchanged.
#[test]
fn iter_rev_chain_reverses_the_source_run() {
    let out = match run_src(
        "fn main() {
            let v: Vec<i32> = vec![1, 2, 3];
            let w: Vec<i32> = v.iter().rev().cloned().collect();
            println!(\"{} {} {} {}\", w[0], w[1], w[2], v[0]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping rev chain run");
            return;
        }
    };
    assert_eq!(out, "3 2 1 1", "rev should walk from the last element");
}

/// A local `let v = Vec::new();` moved into a struct field takes the field's element type.
#[test]
fn pending_vec_local_takes_struct_field_type_run() {
    let out = match run_src(
        "struct P { v: Vec<i32> }
        fn mk() -> P {
            let v = Vec::new();
            P { v }
        }
        fn main() {
            let p = mk();
            println!(\"{}\", p.v.len());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping struct field vec run");
            return;
        }
    };
    assert_eq!(out, "0", "the pending vector should take the field's type");
}

/// Binary operators of the same precedence group to the left: `9 - 4 - 2` is `(9 - 4) - 2`.
#[test]
fn left_associative_operators_group_left_run() {
    let out = match run_src(
        "fn main() {
            print_i64(9 - 4 - 2);
            print_i64(8 / 4 / 2);
            print_i64(2 + 3 * 4);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping associativity run");
            return;
        }
    };
    assert_eq!(
        out, "3\n1\n14",
        "subtraction and division should group to the left"
    );
}

/// An untyped `Vec::new()` that only receives unsuffixed literals takes Rust's default `i32`.
#[test]
fn untyped_vec_of_literals_defaults_to_i32_run() {
    let out = match run_src(
        "fn main() {
            let mut v = Vec::new();
            v.push(4);
            v.push(5);
            println!(\"{} {}\", v.len(), v[1]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping literal vec run");
            return;
        }
    };
    assert_eq!(out, "2 5", "literal elements should default to i32");
}

/// An untyped `HashMap::new()` that only receives unsuffixed literal keys and values defaults
/// to `i32` for both. (Uses inside the body are typed before the default applies, so this test
/// reads only the length.)
#[test]
fn untyped_map_of_literals_defaults_to_i32_run() {
    let out = match run_src(
        "use std::collections::HashMap;
        fn main() {
            let mut m = HashMap::new();
            m.insert(1, 20);
            m.insert(2, 30);
            println!(\"{}\", m.len());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping literal map run");
            return;
        }
    };
    assert_eq!(out, "2", "literal keys and values should default to i32");
}

/// `for x in v.iter_mut()` yields mutable references, so the loop body can write through them.
#[test]
fn iter_mut_loop_writes_through_run() {
    let out = match run_src(
        "fn main() {
            let mut v: Vec<i32> = vec![1, 2, 3];
            for x in v.iter_mut() {
                *x += 10;
            }
            println!(\"{} {} {}\", v[0], v[1], v[2]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping iter_mut run");
            return;
        }
    };
    assert_eq!(
        out, "11 12 13",
        "iter_mut items should be mutable references"
    );
}

/// `insert(i, x)` puts the element at index `i` (front, middle and end).
#[test]
fn vec_insert_at_each_position_run() {
    let out = match run_src(
        "fn main() {
            let mut v: Vec<i32> = vec![1, 3];
            v.insert(1, 2);
            v.insert(0, 0);
            v.insert(4, 4);
            println!(\"{} {} {} {} {} {}\", v.len(), v[0], v[1], v[2], v[3], v[4]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping insert run");
            return;
        }
    };
    assert_eq!(
        out, "5 0 1 2 3 4",
        "insert should place each element at its index"
    );
}

/// `remove`, `reverse` and `clear` on a vector field write the change back into the struct.
#[test]
fn vec_field_remove_reverse_clear_run() {
    let out = match run_src(
        "struct Store { items: Vec<i32> }
        impl Store {
            fn drop_first(&mut self) -> i32 { self.items.remove(0) }
            fn flip(&mut self) { self.items.reverse(); }
            fn clear_all(&mut self) { self.items.clear(); }
        }
        fn main() {
            let mut s = Store { items: vec![1, 2, 3, 4] };
            println!(\"{}\", s.drop_first());
            s.flip();
            println!(\"{}\", s.items.len());
            s.clear_all();
            println!(\"{}\", s.items.len());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping vec field run");
            return;
        }
    };
    assert_eq!(out, "1\n3\n0", "field vector helpers should write back");
}

/// `insert(i, x)` on a vector field writes the grown vector back into the struct.
#[test]
fn vec_field_insert_run() {
    let out = match run_src(
        "struct Store { items: Vec<i32> }
        impl Store {
            fn put(&mut self, i: usize, x: i32) { self.items.insert(i, x); }
        }
        fn main() {
            let mut s = Store { items: vec![1, 3] };
            s.put(1, 2);
            println!(\"{} {} {} {}\", s.items.len(), s.items[0], s.items[1], s.items[2]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping vec field insert run");
            return;
        }
    };
    assert_eq!(out, "3 1 2 3", "field insert should write back");
}

/// `retain` on a vector field keeps the items the closure accepts and writes the result back.
#[test]
fn vec_field_retain_run() {
    let out = match run_src(
        "struct Store { items: Vec<i32> }
        impl Store {
            fn keep_positive(&mut self) { self.items.retain(|x| *x > 0); }
        }
        fn main() {
            let mut s = Store { items: vec![1, -2, 3, -4, 5] };
            s.keep_positive();
            println!(\"{} {} {}\", s.items.len(), s.items[0], s.items[2]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping vec field retain run");
            return;
        }
    };
    assert_eq!(out, "3 1 5", "field retain should keep the accepted items");
}

/// `find_map` returns the first `Some` the closure produces, and `None` when there is none.
#[test]
fn find_map_returns_first_some_run() {
    let out = match run_src(
        "fn pick(v: &Vec<i32>, limit: i32) -> Option<i32> {
            v.iter().find_map(|x| if *x > limit { Some(*x * 10) } else { None })
        }
        fn main() {
            let v: Vec<i32> = vec![1, 2, 3, 4];
            match pick(&v, 2) {
                Some(n) => println!(\"{}\", n),
                None => println!(\"none\"),
            }
            match pick(&v, 9) {
                Some(n) => println!(\"{}\", n),
                None => println!(\"none\"),
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping find_map run");
            return;
        }
    };
    assert_eq!(out, "30\nnone", "find_map should yield the first Some");
}

/// `std::mem::take(&mut s)` on a string local returns the old text and leaves `s` empty.
#[test]
fn mem_take_string_local_run() {
    let out = match run_src(
        "fn main() {
            let mut s = String::from(\"abc\");
            let t = std::mem::take(&mut s);
            println!(\"{}|{}|{}\", t, s, s.len());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping mem::take run");
            return;
        }
    };
    assert_eq!(
        out, "abc||0",
        "mem::take should move the string out and leave it empty"
    );
}

/// `PathBuf::from(s)` is the same text as the path it was given.
#[test]
fn pathbuf_from_run() {
    let out = match run_src(
        "fn main() {
            let p = PathBuf::from(\"dir/file.txt\");
            match p.parent() {
                Some(d) => println!(\"{}\", d),
                None => println!(\"none\"),
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping pathbuf run");
            return;
        }
    };
    assert_eq!(out, "dir", "PathBuf::from should keep the path text");
}

/// `String::with_capacity(n)` in an untyped `let` position is an empty string; the capacity
/// argument is evaluated but does not change the value.
#[test]
fn string_with_capacity_run() {
    let out = match run_src(
        "fn main() {
            let mut out = String::with_capacity(16);
            println!(\"{}\", out.len());
            out.push_str(\"ab\");
            println!(\"{}\", out);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping string run");
            return;
        }
    };
    assert_eq!(out, "0\nab", "String::with_capacity should start empty");
}

#[test]
fn enum_match_and_if_let_run() {
    // Enum-variant construction (`Some(7)` / `None`), `match` with
    // tag-test guards + payload binding, and `if let Some(x) = ...` all
    // lower through the tagged-struct representation and produce correct
    // runtime values.
    let out = match run_src(
        "fn describe(o: Option<i32>) -> i32 {\n\
             match o {\n\
                 Some(v) => v,\n\
                 None => 0,\n\
             }\n\
         }\n\
         fn main() {\n\
             let a: Option<i32> = Some(7);\n\
             let b: Option<i32> = None;\n\
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
    assert_eq!(
        out, "7\n0\n8",
        "expected describe(Some)=7, describe(None)=0, x+1=8"
    );
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
    // `println!`/`eprintln!` expand to interleaved raw writer calls
    // (`__moira_w_str`/`__moira_w_i64`, which do not end the line); literal segments,
    // `{}` placeholders and the trailing newline all appear in the emitted C.
    let mir = lower_src(
        "fn main() {\n\
             let n = 42;\n\
             println!(\"hello\");\n\
             println!(\"n = {}!\", n);\n\
         }",
    );
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(
        c.contains("__moira_w_str(\"hello\")"),
        "literal segment:\n{c}"
    );
    assert!(
        c.contains("__moira_w_i64(n)"),
        "placeholder arg printed:\n{c}"
    );
    assert!(
        c.contains("__moira_w_str(\"\\n\")"),
        "trailing newline:\n{c}"
    );
}

#[test]
fn capturing_closure_direct_call_runs() {
    // A capturing closure bound to a local and invoked directly: captures are
    // lowered as leading parameters on the lifted function and the call sites
    // pass them.
    let out = match run_src(
        "fn main() {\n\
             let base = 100;\n\
             let add = |x: i32| x + base;\n\
             print_i32(add(5));\n\
             print_i32(add(20));\n\
         }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping capturing-closure run");
            return;
        }
    };
    assert_eq!(out, "105\n120", "expected 100+5 and 100+20");
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
    assert_eq!(
        out, "10\n30\n20",
        "expected arr[0]=10, arr[2]=30, arr[1]=20"
    );
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
    let mir = lower_src("fn f() { let mut t = 0; for x in [1, 2, 3] { t = t + x; } let _ = t; }");
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
    assert!(
        // The checker names the tuple struct `Tuple_i32_i32`; without hints it is `__tuple_*`.
        c.contains("struct __tuple_") || c.contains("struct Tuple_"),
        "tuple should lower to a struct:\n{c}"
    );
    assert!(
        c.contains(".f0 = 3"),
        "tuple element 0 should map to .f0:\n{c}"
    );
    assert!(
        c.contains(".f1 = 4"),
        "tuple element 1 should map to .f1:\n{c}"
    );
    assert!(
        c.contains("p.f0"),
        "tuple index 0 should lower to p.f0:\n{c}"
    );
    assert!(
        c.contains("p.f1"),
        "tuple index 1 should lower to p.f1:\n{c}"
    );
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
    assert_eq!(
        out, "30",
        "expected 10+20 = 30 from destructured call result"
    );
}

#[test]
fn tuple_destructure_literal_lowers_to_direct_assignments() {
    // A tuple-literal RHS is decomposed element-wise: each binding is declared and
    // assigned its component directly (no intermediate struct value).
    let mir = lower_src("fn main() { let (a, b) = (1, 2); let _ = a + b; }");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(
        c.contains("a = 1"),
        "first binding should be assigned directly:\n{c}"
    );
    assert!(
        c.contains("b = 2"),
        "second binding should be assigned directly:\n{c}"
    );
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
    let mir = lower_src("fn f() { for (a, b) in [(1, 2), (3, 4)] { let _ = a + b; } }");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(
        // The checker names the tuple struct `Tuple_i32_i32`; without hints it is `__tuple_*`.
        c.contains("struct __tuple_") || c.contains("struct Tuple_"),
        "tuple array element needs a struct type:\n{c}"
    );
    assert!(
        c.contains(".f0"),
        "first component must be read via .f0:\n{c}"
    );
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
        b.stmts.iter().any(|s| {
            matches!(
                s.rvalue,
                tpt_moira_atropos::mir::Rvalue::Binary(tpt_moira_atropos::mir::MirBinOp::Eq, _, _)
            )
        })
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

#[test]
fn multi_payload_variant_constructs_and_runs() {
    // A variant with two payload values needs one payload slot per field of
    // the widest variant (`f0`, `f1`); a single-slot layout rejected it.
    let out = match run_src(
        "enum Shape {\n\
             Pair(i32, i32),\n\
             Solo(i32),\n\
             Empty,\n\
         }\n\
         fn main() {\n\
             let p = Shape::Pair(3, 4);\n\
             if let Shape::Pair(a, b) = p {\n\
                 print_i32(a + b);\n\
             }\n\
         }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping multi-payload variant run");
            return;
        }
    };
    assert_eq!(out.trim(), "7");
}

#[test]
fn function_sharing_a_variant_name_is_called_normally() {
    // `Num` is both a unit variant and a two-argument function. The call
    // `Num(2, 5)` must reach the function, not the variant constructor (which
    // takes no payload and previously failed with an arity error).
    let out = match run_src(
        "enum Tok {\n\
             Num,\n\
         }\n\
         fn Num(a: i32, b: i32) -> i32 {\n\
             a * b\n\
         }\n\
         fn main() {\n\
             print_i32(Num(2, 5));\n\
         }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping variant-name collision run");
            return;
        }
    };
    assert_eq!(out.trim(), "10");
}

#[test]
fn expression_macro_keeps_operator_grouping() {
    // An expression-position macro keeps its template's braces, so `2 * m!()`
    // multiplies the whole `1 + 2` (6), not just the `1` (which would give 4).
    let out = match run_src(
        "macro_rules! m {\n\
             () => { 1 + 2 };\n\
         }\n\
         fn main() {\n\
             let a = 2 * m!();\n\
             print_i32(a);\n\
         }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping macro precedence run");
            return;
        }
    };
    assert_eq!(out.trim(), "6");
}

#[test]
fn item_position_macro_generates_a_function() {
    // An item-position macro expands to items (no enclosing braces) and the
    // generated function is callable.
    let out = match run_src(
        "macro_rules! make_answer {\n\
             ($name:ident) => { fn $name() -> i32 { 42 } };\n\
         }\n\
         make_answer!(answer);\n\
         fn main() {\n\
             print_i32(answer());\n\
         }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping item macro run");
            return;
        }
    };
    assert_eq!(out.trim(), "42");
}

#[test]
fn assert_passes_and_runs_through() {
    // A passing `assert!`/`assert_eq!` falls through to the code after it.
    let out = match run_src(
        "fn f(x: i32) -> i32 {\n\
             assert!(x > 0);\n\
             assert_eq!(x, 3);\n\
             x\n\
         }\n\
         fn main() {\n\
             print_i32(f(3));\n\
         }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping assert run");
            return;
        }
    };
    assert_eq!(out.trim(), "3");
}

#[test]
fn nested_item_and_forward_call_run() {
    // A nested helper is hoisted and called from its function; `main` also calls a
    // function defined after it (forward reference through the C prototypes).
    let out = match run_src(
        "fn outer() -> i32 {\n\
             fn helper(x: i32) -> i32 { x + 1 }\n\
             helper(2)\n\
         }\n\
         fn main() {\n\
             print_i32(outer());\n\
             print_i32(later(4));\n\
         }\n\
         fn later(x: i32) -> i32 { x * 2 }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping nested item run");
            return;
        }
    };
    assert_eq!(
        out.trim(),
        "3\n8".trim().lines().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn glob_imported_unit_variant_pattern_runs() {
    // `Str` after `use Ty::*;` is a tag test: the second arm runs for `Ty::Str`.
    let out = match run_src(
        "enum Ty { Unknown, Str }\n\
         fn code(t: Ty) -> i32 {\n\
             use Ty::*;\n\
             match t {\n\
                 Unknown => 1,\n\
                 Str => 2,\n\
             }\n\
         }\n\
         fn main() {\n\
             print_i32(code(Ty::Str));\n\
             print_i32(code(Ty::Unknown));\n\
         }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping glob variant run");
            return;
        }
    };
    assert_eq!(out.trim(), "2\n1");
}

#[test]
fn unsafe_block_lowers_as_plain_block() {
    // `unsafe` only gates what the block may do; it must not block codegen.
    let src = "fn main() { let x: i64 = unsafe { 1 + 2 }; println!(\"{}\", x); }";
    let prog = tpt_moira_clotho::parse_source(src).expect("should parse");
    let mir = tpt_moira_atropos::lower_program(&prog).expect("unsafe block should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(c.contains("main"), "main should be emitted");
}

#[test]
fn returned_non_capturing_closure_runs() {
    // A function returning a closure has a function-pointer return type, whose C
    // declarator must wrap the name and parameters (`R (*f(params))(P)`); the call
    // result bound by `let` must keep that function-pointer type.
    let out = match run_src(
        "fn make() -> fn(i64) -> i64 {
            |x: i64| x + 1
        }
        fn main() {
            let f = make();
            print_i64(f(41));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping returned-closure run");
            return;
        }
    };
    assert_eq!(out, "42", "the returned closure should be callable");
}

#[test]
fn returned_capturing_closure_runs_when_bound_by_let() {
    // `let f = make(4);` inlines the returned `move |x| x + k` at the call site, so the
    // capture is an ordinary let-bound local and the lifted closure gets it as an argument.
    let out = match run_src(
        "fn make(k: i64, m: i64) -> impl Fn(i64) -> i64 {
            move |x: i64| x * k + m
        }
        fn main() {
            let k: i64 = 2;
            let f = make(k + 1, 10);
            print_i64(f(3));
            let mut i: i64 = 0;
            while i < 2 {
                let h = make(i, 100);
                print_i64(h(1));
                i = i + 1;
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping returned-capturing-closure run");
            return;
        }
    };
    assert_eq!(
        out, "19\n100\n101",
        "each `let f = make(..)` should bind its own captures"
    );
}

#[test]
fn returned_capturing_closure_with_pure_lets_runs_when_bound_by_let() {
    // The body's leading `let` is pure, so it is inlined at the call site with the
    // parameters, and the closure captures the let-bound name.
    let out = match run_src(
        "fn make(k: i64) -> impl Fn(i64) -> i64 {
            let j = k * 2;
            move |x: i64| x + j
        }
        fn main() {
            let f = make(4);
            print_i64(f(1));
            let mut i: i64 = 0;
            while i < 2 {
                let h = make(i);
                print_i64(h(10));
                i = i + 1;
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping returned-capturing-closure run");
            return;
        }
    };
    assert_eq!(
        out,
        "9
10
12",
        "the let-bound value must be recomputed per call and captured by the closure"
    );
}

#[test]
fn returned_capturing_closure_from_method_runs_when_bound_by_let() {
    // `let f = a.make(..)` inlines the method's closure: `&self`, `self` by value, and
    // `&mut self` receivers, each with the receiver bound to `self` at the call site.
    let out = match run_src(
        "struct Adder { k: i64 }
        impl Adder {
            fn make(&self) -> impl Fn(i64) -> i64 {
                move |x| x + self.k
            }
        }
        struct Scale { k: i64 }
        impl Scale {
            fn times(self, m: i64) -> impl Fn(i64) -> i64 {
                let t = self.k * m;
                move |x| x * t
            }
        }
        struct Counter { base: i64 }
        impl Counter {
            fn adder(&mut self, d: i64) -> impl Fn(i64) -> i64 {
                move |x| x + d + 100
            }
        }
        fn main() {
            let a = Adder { k: 5 };
            let f = a.make();
            print_i64(f(1));
            let s = Scale { k: 3 };
            let g = s.times(2);
            print_i64(g(7));
            let mut c = Counter { base: 1 };
            let h = c.adder(2);
            print_i64(h(1));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping returned-capturing-closure run");
            return;
        }
    };
    assert_eq!(
        out,
        "6
42
103",
        "each method's closure should see its receiver"
    );
}

#[test]
fn returned_capturing_closure_passed_as_argument_runs() {
    // `apply(make(4), 1)`: the pure-argument call is hoisted into a `let` before the
    // statement, then inlined like any other `let f = make(..)`.
    let out = match run_src(
        "fn make(k: i64) -> impl Fn(i64) -> i64 {
            move |x: i64| x + k
        }
        fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
            f(x)
        }
        fn main() {
            let r = apply(make(4), 1);
            print_i64(r);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping argument-position closure run");
            return;
        }
    };
    assert_eq!(
        out, "5",
        "the hoisted closure should be passed as the fn argument"
    );
}

#[test]
fn returned_capturing_closure_inside_assert_runs() {
    // `assert_eq!(apply(make(4), 1), 5)`: the call sits in an eagerly evaluated macro
    // argument, so it is hoisted out of the tokens into a `let` before the statement.
    let out = match run_src(
        "fn make(k: i64) -> impl Fn(i64) -> i64 {
            move |x: i64| x + k
        }
        fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
            f(x)
        }
        fn main() {
            assert_eq!(apply(make(4), 1), 5);
            print_i64(9);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping assert closure run");
            return;
        }
    };
    assert_eq!(
        out, "9",
        "the hoisted closure call should satisfy the assertion"
    );
}

#[test]
fn c_reserved_names_are_escaped_and_run() {
    // Functions and locals named like C keywords and libc names must still compile.
    let out = match run_src(
        "fn double(x: i64) -> i64 { x * 2 }
        fn exit(x: i64) -> i64 { x + 1 }
        fn main() {
            let int = double(4);
            print_i64(int);
            print_i64(exit(int));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping reserved-name run");
            return;
        }
    };
    assert_eq!(out, "8\n9");
}

#[test]
fn returned_capturing_closure_with_impure_argument_runs() {
    // `apply(make(side()), 1)`: the impure argument of the candidate call is evaluated
    // by the hoisted `let`, which runs before the statement. `side` prints first, so
    // the output also checks that the impure argument is evaluated exactly once.
    let out = match run_src(
        "fn make(k: i64) -> impl Fn(i64) -> i64 {
            move |x: i64| x + k
        }
        fn side() -> i64 {
            print_i64(7);
            4
        }
        fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
            f(x)
        }
        fn main() {
            let r = apply(make(side()), 1);
            print_i64(r);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping impure-argument closure run");
            return;
        }
    };
    assert_eq!(
        out,
        "7
5",
        "the impure argument should run once, before the statement"
    );
}

#[test]
fn returned_capturing_closure_inside_println_runs() {
    // The call sits in a `println!` argument, which is a token stream until lowering;
    // the pass hoists it out of the tokens into a `let` before the statement.
    let out = match run_src(
        "fn make(k: i64) -> impl Fn(i64) -> i64 {
            move |x: i64| x + k
        }
        fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
            f(x)
        }
        fn main() {
            println!(\"{}\", apply(make(4), 1));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping println-argument closure run");
            return;
        }
    };
    assert_eq!(
        out, "5",
        "the closure inside a println argument should be hoisted and run"
    );
}

#[test]
fn compound_assignment_as_match_arm_runs() {
    // `1 => depth += 1,` is a unit-typed arm, so it agrees with the `_ => {}` arm.
    let out = match run_src(
        "fn main() {
            let x: i64 = 1;
            let mut depth: i64 = 10;
            match x {
                1 => depth += 5,
                _ => {}
            }
            print_i64(depth);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping compound-assignment arm run");
            return;
        }
    };
    assert_eq!(out, "15");
}

#[test]
fn generic_struct_literal_inside_vec_macro_runs() {
    // `vec![Spanned { .. }]`: the struct literal lives in macro tokens, so monomorphization
    // must rename it to its instance as well.
    let out = match run_src(
        "struct Spanned<T> {
            node: T,
            span: i64,
        }
        struct Path {
            segments: Vec<Spanned<i64>>,
        }
        fn main() {
            let p = Path { segments: vec![Spanned { node: 4i64, span: 1 }] };
            print_i64(p.segments[0].node);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping generic-struct-in-vec run");
            return;
        }
    };
    assert_eq!(out, "4");
}

#[test]
fn boxed_recursive_option_runs() {
    // `Option<Box<Node>>` fields, `Box::new` construction, `if let Some(b) = &..`,
    // `*box` deref and a method on a struct reached through a plain field.
    let out = match run_src(
        "struct Node {
            val: i64,
            next: Option<Box<Node>>,
        }
        impl Node {
            fn get(&self) -> i64 {
                self.val
            }
        }
        fn sum(n: &Node) -> i64 {
            let mut total: i64 = n.val;
            if let Some(b) = &n.next {
                total = total + sum(b);
            }
            total
        }
        fn main() {
            let tail = Node { val: 3, next: None };
            let head = Node { val: 1, next: Some(Box::new(tail)) };
            print_i64(sum(&head));
            let bx: Box<i64> = Box::new(41);
            print_i64(*bx + 1);
            let n2 = Node { val: 8, next: None };
            print_i64(n2.get());
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping boxed-list run");
            return;
        }
    };
    assert_eq!(out, "4\n42\n8");
}

#[test]
fn option_of_hashmap_assigned_from_constructor_runs() {
    // `acc = Some(HashMap::new())`: the assignment's target type reaches the call, and the
    // constructor lowers to a map instance even though it is not a `let` initializer.
    let out = match run_src(
        "use std::collections::HashMap;
        fn main() {
            let mut acc: Option<HashMap<String, i64>> = None;
            if acc.is_none() {
                acc = Some(HashMap::new());
            }
            print_i64(acc.is_some() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping Option<HashMap> run");
            return;
        }
    };
    assert_eq!(out, "1");
}

#[test]
fn byte_ascii_methods_and_char_from_u32_run() {
    // `u8` takes the ASCII methods (as C integers), and `char::from_u32` returns an
    // `Option<char>` that is `None` for a surrogate.
    let out = match run_src(
        "fn main() {
            let b: u8 = 55;
            let c: u8 = 65;
            print_i64(b.is_ascii_digit() as i64);
            print_i64(c.is_ascii_alphabetic() as i64);
            print_i64(c.to_ascii_lowercase() as i64);
            print_i64(char::from_u32(65).is_some() as i64);
            print_i64(char::from_u32(55296).is_some() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping byte-method run");
            return;
        }
    };
    assert_eq!(out, "1\n1\n97\n1\n0");
}

#[test]
fn char_to_digit_runs() {
    // `c.to_digit(radix)` is `Some(digit)` for a digit in the radix, else `None`.
    let out = match run_src(
        "fn main() {
            let b: char = '7';
            let a: char = 'z';
            print_i64(b.to_digit(10).is_some() as i64);
            print_i64(a.to_digit(10).is_some() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping to_digit run");
            return;
        }
    };
    assert_eq!(out, "1\n0");
}

#[test]
fn glob_imported_variant_resolves_to_its_enum() {
    // Two enums share the variant name `Match`; the function-local `use B::*;` names `B`.
    let out = match run_src(
        "#[derive(Clone, Copy)]
        enum A { Match, Other }
        #[derive(Clone, Copy)]
        enum B { Match, Plain }
        fn pick_b(n: i64) -> B {
            use B::*;
            match n {
                1 => Match,
                _ => Plain,
            }
        }
        fn main() {
            let v = pick_b(1);
            let code = match v {
                B::Match => 7,
                B::Plain => 3,
            };
            print_i64(code);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping glob-variant run");
            return;
        }
    };
    assert_eq!(out, "7");
}

#[test]
fn if_let_enum_pattern_and_while_let_run() {
    // `if let Variant(r) = s { .. } else { .. }` and `while let Some(x) = f(i) { .. }` are
    // rewritten to `match` (and a `loop` with a `break` arm), so any pattern works.
    let out = match run_src(
        "enum Shape {
            Circle(i64),
            Square(i64),
            Dot,
        }
        fn area(s: Shape) -> i64 {
            let mut out: i64 = 0;
            if let Shape::Circle(r) = s {
                out = 3 * r * r;
            } else {
                out = 1;
            }
            out
        }
        fn step(i: i64) -> Option<i64> {
            if i < 3 {
                Some(i)
            } else {
                None
            }
        }
        fn main() {
            print_i64(area(Shape::Circle(2)));
            print_i64(area(Shape::Dot));
            let mut n: i64 = 0;
            let mut i: i64 = 0;
            while let Some(x) = step(i) {
                n = n + x;
                i = i + 1;
            }
            print_i64(n);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping let-condition run");
            return;
        }
    };
    assert_eq!(out, "12\n1\n3");
}

#[test]
fn option_is_some_and_runs() {
    let out = match run_src(
        "fn main() {
            let a: Option<i64> = Some(4);
            let b: Option<i64> = None;
            let big = |x: i64| x > 3;
            if a.is_some_and(big) {
                print_i32(1);
            } else {
                print_i32(0);
            }
            if b.is_some_and(big) {
                print_i32(1);
            } else {
                print_i32(0);
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping is_some_and run");
            return;
        }
    };
    assert_eq!(
        out, "1\n0",
        "`is_some_and` applies the predicate only to `Some`"
    );
}

#[test]
fn vec_of_strings_contains_compares_content_and_runs() {
    let out = match run_src(
        "fn main() {
            let mut names: Vec<String> = Vec::new();
            names.push(\"alpha\".to_string());
            names.push(\"beta\".to_string());
            let probe = \"beta\".to_string();
            let missing = \"gamma\".to_string();
            if names.contains(&probe) {
                print_i32(1);
            } else {
                print_i32(0);
            }
            if names.contains(&missing) {
                print_i32(1);
            } else {
                print_i32(0);
            }
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping Vec<String>::contains run");
            return;
        }
    };
    assert_eq!(out, "1\n0", "`contains` on Vec<String> compares by content");
}

#[test]
fn format_of_enum_without_display_lowering_is_rejected() {
    let src = "enum K { A(i64), B }
        fn show(k: &K) -> String {
            match k {
                K::A(_) => String::new(),
                other => format!(\"found {}\", other),
            }
        }
        fn main() { let _ = show(&K::B); }";
    let prog = tpt_moira_clotho::compile_source(src).expect("source should parse");
    let err = tpt_moira_atropos::lower_program(&prog)
        .err()
        .expect("formatting an enum must be a lowering error, not integer output");
    assert!(
        format!("{err}").contains("has no lowering yet"),
        "unexpected error: {err}"
    );
}

#[test]
fn user_display_impl_formats_through_format_and_write_runs() {
    let out = match run_src(
        "use std::fmt;
        struct P { x: i64 }
        impl fmt::Display for P {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, \"P({})\", self.x)
            }
        }
        enum K { A(i64), B }
        impl fmt::Display for K {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    K::A(n) => write!(f, \"a{}\", n),
                    K::B => write!(f, \"b\"),
                }
            }
        }
        fn main() {
            let p = P { x: 3 };
            let s = format!(\"[{}]\", p);
            let t = format!(\"{} {}\", K::A(5), K::B);
            print_i64(s.len() as i64);
            print_i64(t.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping Display run");
            return;
        }
    };
    // "[P(3)]" is 6 bytes and "a5 b" is 4.
    assert_eq!(
        out, "6\n4",
        "`{{}}` of a user `impl Display` must use its `fmt`"
    );
}

#[test]
fn unclosed_impl_is_a_parse_error_not_a_hang() {
    let r =
        tpt_moira_clotho::compile_source("struct K {}\nimpl K {\nfn main() {\n    let p = 1;\n}\n");
    assert!(
        r.is_err(),
        "an impl missing its closing brace must be rejected"
    );
}

#[test]
fn array_argument_coerces_to_slice_and_runs() {
    let out = match run_src(
        "fn total(xs: &[i64]) -> i64 {
            let mut t = 0;
            for x in xs {
                t += *x;
            }
            t
        }
        fn main() {
            let a = [1i64, 2, 3];
            print_i64(total(&a));
            print_i64(total(&[4i64, 5]));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping array-to-slice run");
            return;
        }
    };
    assert_eq!(out, "6\n9", "`&arr` and `&[..]` pass as slices");
}

#[test]
fn unannotated_nested_vec_and_mem_take_run() {
    let out = match run_src(
        "fn split(xs: &[i64]) -> Vec<Vec<i64>> {
            let mut res = Vec::new();
            let mut cur = Vec::new();
            for x in xs {
                if *x == 0 {
                    res.push(std::mem::take(&mut cur));
                } else {
                    cur.push(*x);
                }
            }
            res.push(cur);
            res
        }
        fn main() {
            let v = split(&[1i64, 0, 2]);
            print_i64(v.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping nested Vec run");
            return;
        }
    };
    assert_eq!(
        out, "2",
        "`Vec::new()` takes its element type from later pushes"
    );
}

#[test]
fn entry_or_insert_with_struct_value_keeps_first_insert_and_runs() {
    let out = match run_src(
        "use std::collections::HashMap;
        struct Pt { x: i64, y: i64 }
        fn main() {
            let mut m: HashMap<i64, Pt> = HashMap::new();
            m.entry(1).or_insert_with(|| Pt { x: 4, y: 5 });
            m.entry(1).or_insert_with(|| Pt { x: 9, y: 9 });
            let p = m.get(&1).unwrap();
            print_i64(p.x + p.y);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping entry run");
            return;
        }
    };
    assert_eq!(out, "9", "a present key keeps its first value");
}

#[test]
fn hashmap_new_in_struct_field_runs() {
    let out = match run_src(
        "use std::collections::HashMap;
        struct T { defs: HashMap<String, i64> }
        fn mk() -> T {
            T { defs: HashMap::new() }
        }
        fn main() {
            let t = mk();
            print_i64(t.defs.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping HashMap field run");
            return;
        }
    };
    assert_eq!(
        out, "0",
        "`HashMap::new()` in a struct field takes its types from the field"
    );
}

#[test]
fn to_vec_of_vec_and_slice_runs() {
    let out = match run_src(
        "fn main() {
            let v: Vec<i64> = vec![4, 5, 6];
            let c = v.to_vec();
            let s = &v[1..];
            let d = s.to_vec();
            print_i64(c.len() as i64 + d[0]);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping to_vec run");
            return;
        }
    };
    assert_eq!(out, "8", "`to_vec` copies a vector and a slice");
}

#[test]
fn local_closure_shadows_same_named_function_and_runs() {
    let out = match run_src(
        "fn visit(a: i64, b: i64) -> i64 {
            a + b
        }
        fn main() {
            let visit = |x: i64| x * 10;
            print_i64(visit(4));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping shadowing run");
            return;
        }
    };
    assert_eq!(
        out, "40",
        "a local binding shadows a function of the same name"
    );
}

#[test]
fn ref_bindings_bind_the_place_and_run() {
    let out = match run_src(
        "struct P { x: i64 }
        fn main() {
            let o: Option<i64> = Some(7);
            if let Some(ref v) = o {
                print_i64(*v + 1);
            }
            let p = P { x: 4 };
            let P { ref x } = p;
            print_i64(*x);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping ref-binding run");
            return;
        }
    };
    assert_eq!(out, "8\n4", "`ref` binds a reference to the matched place");
}

#[test]
fn extern_generic_struct_literal_in_generic_impl_runs() {
    // A struct literal of a generic struct inside an instance of a generic impl method
    // (`Spanned { node, span }` for `T = i64`) must name that instance.
    let out = match run_src(
        "struct Spanned<T> {
            node: T,
            span: i64,
        }
        impl<T> Spanned<T> {
            fn new(node: T, span: i64) -> Spanned<T> {
                Spanned { node, span }
            }
        }
        fn main() {
            let s = Spanned::new(5i64, 2);
            print_i64(s.node + s.span);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping generic impl run");
            return;
        }
    };
    assert_eq!(
        out, "7",
        "the generic struct literal takes the instance's arguments"
    );
}

#[test]
fn tuple_struct_constructor_and_positional_fields_run() {
    let out = match run_src(
        "struct Pair(i64, i64);
        fn sum(p: &Pair) -> i64 { p.0 + p.1 }
        fn main() {
            let p = Pair(3, 4);
            print_i64(sum(&p));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping tuple struct run");
            return;
        }
    };
    assert_eq!(
        out, "7",
        "`Pair(3, 4)` builds the struct; `p.0` reads its first field"
    );
}

#[test]
fn unannotated_hashset_takes_its_element_type_from_insert() {
    let out = match run_src(
        "use std::collections::HashSet;
        fn reach(n: usize) -> usize {
            let mut seen = HashSet::new();
            let mut i = 0;
            while i < n {
                seen.insert(i % 3);
                i += 1;
            }
            seen.len()
        }
        fn main() { print_i64(reach(10) as i64); }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping HashSet run");
            return;
        }
    };
    assert_eq!(out, "3");
}

#[test]
fn trim_matches_with_char_and_string_patterns_run() {
    let out = match run_src(
        "fn main() {
            let s = String::from(\"ab__\");
            let t = s.trim_end_matches('_');
            let u = String::from(\"xxab\").trim_start_matches(\"x\");
            print_i64((t.len() * 10 + u.len()) as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping trim_matches run");
            return;
        }
    };
    assert_eq!(out, "22");
}

#[test]
fn read_only_vec_methods_on_a_field_receiver_run() {
    let out = match run_src(
        "struct Scan { names: Vec<String>, items: Vec<i64> }
        fn has(sc: &Scan, n: &String) -> bool {
            sc.names.contains(n)
        }
        fn main() {
            let sc = Scan { names: vec![String::from(\"a\"), String::from(\"b\")], items: vec![1, 2, 3] };
            let k = String::from(\"b\");
            print_i64(has(&sc, &k) as i64 * 100 + sc.items.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping field vec method run");
            return;
        }
    };
    assert_eq!(out, "103");
}

#[test]
fn iterator_next_on_a_map_lookup_runs() {
    let out = match run_src(
        "use std::collections::HashMap;
        fn target(m: &HashMap<i64, Vec<i64>>, k: i64) -> i64 {
            let owners = match m.get(&k) {
                Some(o) => o,
                None => return 0,
            };
            match owners.iter().next() {
                Some(x) => *x,
                None => 0,
            }
        }
        fn main() {
            let mut m: HashMap<i64, Vec<i64>> = HashMap::new();
            let mut v: Vec<i64> = Vec::new();
            v.push(7);
            v.push(8);
            m.insert(1, v);
            print_i64(target(&m, 1));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping iterator next run");
            return;
        }
    };
    assert_eq!(out, "7");
}

#[test]
fn hashset_iterator_chains_read_its_keys_and_run() {
    // The order of a set's items is not specified, so the test only counts them.
    let out = match run_src(
        "use std::collections::HashSet;
        struct Holder { seen: HashSet<i64> }
        fn has_item(h: &Holder) -> i64 {
            match h.seen.iter().next() {
                Some(_) => 1,
                None => 0,
            }
        }
        fn main() {
            let mut s: HashSet<i64> = HashSet::new();
            s.insert(4);
            s.insert(9);
            let big = s.iter().filter(|x| **x > 5).count() as i64;
            let h = Holder { seen: s };
            print_i64(has_item(&h) * 10 + big);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping HashSet chain run");
            return;
        }
    };
    assert_eq!(out, "11");
}

#[test]
fn btreeset_iterator_chains_read_in_key_order_and_run() {
    let out = match run_src(
        "use std::collections::BTreeSet;
        fn main() {
            let mut s: BTreeSet<i64> = BTreeSet::new();
            s.insert(9);
            s.insert(4);
            s.insert(6);
            let mut first = 0;
            if let Some(x) = s.iter().next() {
                first = *x;
            }
            let big = s.iter().filter(|x| **x > 5).count() as i64;
            print_i64(first * 10 + big);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping BTreeSet chain run");
            return;
        }
    };
    assert_eq!(
        out, "42",
        "the first item of a sorted set is its smallest key"
    );
}

#[test]
fn saturating_integer_methods_run() {
    let out = match run_src(
        "fn main() {
            let a: i64 = 5;
            let b: usize = 2;
            let s = a.saturating_sub(9);
            let u = b.saturating_sub(7);
            let m = (a * 4).saturating_mul(3);
            print_i64(s * 100 + u * 10 + m / 10);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping saturating run");
            return;
        }
    };
    assert_eq!(
        out, "-394",
        "signed saturation goes negative; unsigned `saturating_sub` stops at 0"
    );
}

#[test]
fn map_constructed_through_a_type_alias_runs() {
    let out = match run_src(
        "use std::collections::HashMap;
        type Table = HashMap<i64, i64>;
        fn build() -> Table {
            let mut t: Table = Table::new();
            t.insert(1, 5);
            t
        }
        fn main() {
            let t = build();
            print_i64(t.len() as i64 * 10 + t.get(&1).copied().unwrap_or(0));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping alias map run");
            return;
        }
    };
    assert_eq!(out, "15");
}

#[test]
fn map_get_forms_run() {
    let out = match run_src(
        "use std::collections::HashMap;
        fn main() {
            let mut t: HashMap<i64, i64> = HashMap::new();
            t.insert(1, 5);
            let a = *t.get(&1).unwrap();
            let b = t.get(&1).copied().unwrap_or(0);
            let c = match t.get(&2) {
                Some(v) => *v,
                None => 7,
            };
            let mut d = 0;
            if let Some(v) = t.get(&1) {
                d = *v;
            }
            print_i64(a * 1000 + b * 100 + c * 10 + d);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping map get run");
            return;
        }
    };
    assert_eq!(
        out, "5575",
        "`get` yields a reference: unwrap, copied, match and if-let all read it"
    );
}

#[test]
fn literal_elements_take_the_returned_collections_types() {
    let out = match run_src(
        "use std::collections::HashMap;
        type Table = HashMap<i64, i64>;
        fn build() -> Table {
            let mut t = Table::new();
            t.insert(1, 5);
            t
        }
        fn nums() -> Vec<i64> {
            let mut v = Vec::new();
            v.push(1);
            v.push(2);
            v
        }
        fn main() {
            let t = build();
            let v = nums();
            print_i64(t.len() as i64 * 10 + *t.get(&1).unwrap() + v.len() as i64 * 100);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping return-type inference run");
            return;
        }
    };
    assert_eq!(
        out, "215",
        "unsuffixed literals take the element types of the returned collection"
    );
}

#[test]
fn push_through_last_mut_reaches_the_inner_vector() {
    let out = match run_src(
        "fn main() {
            let mut stack: Vec<Vec<i64>> = vec![Vec::new()];
            stack.last_mut().unwrap().push(4);
            stack.last_mut().unwrap().push(5);
            let n = stack.last().unwrap().len();
            print_i64(n as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping last_mut push run");
            return;
        }
    };
    assert_eq!(
        out, "2",
        "a push on `last_mut().unwrap()` goes into the last inner vector"
    );
}

#[test]
fn hashset_in_a_struct_field_takes_insert_and_contains() {
    let out = match run_src(
        "use std::collections::HashSet;
        struct Scope { defines: HashSet<String> }
        fn has(s: &Scope, name: &str) -> bool {
            s.defines.contains(name)
        }
        fn main() {
            let mut s = Scope { defines: HashSet::new() };
            s.defines.insert(\"a\".to_string());
            let n = if has(&s, \"a\") { 1 } else { 0 };
            let mut set: HashSet<i64> = HashSet::new();
            set.insert(3);
            let m = if set.contains(&3) { 10 } else { 0 };
            print_i64(n + m);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping set field run");
            return;
        }
    };
    assert_eq!(
        out, "11",
        "a set in a struct field takes insert and contains through its pointer"
    );
}

#[test]
fn fresh_vec_passed_by_mut_reference_takes_the_parameter_type() {
    let out = match run_src(
        "fn walk(acc: &mut Vec<String>, n: i64) -> i64 {
            acc.push(n.to_string());
            acc.len() as i64
        }
        fn main() {
            print_i64(walk(&mut Vec::new(), 4));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping fresh vec run");
            return;
        }
    };
    assert_eq!(
        out, "1",
        "`&mut Vec::new()` takes the element type of the `&mut Vec<T>` parameter"
    );
}

#[test]
fn suffixed_literal_arguments_type_an_unannotated_map() {
    let out = match run_src(
        "use std::collections::HashMap;
        fn main() {
            let mut m = HashMap::new();
            m.insert(1i64, 2i64);
            print_i64(m.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping suffixed literal run");
            return;
        }
    };
    assert_eq!(
        out, "1",
        "a suffixed literal has its own type, so the map's element types are known"
    );
}

#[test]
fn append_and_map_remove_work_through_struct_fields() {
    let out = match run_src(
        "use std::collections::HashMap;
        struct Store { items: Vec<i64>, index: HashMap<i64, i64> }
        fn drain_into(s: &mut Store, other: &mut Vec<i64>) -> i64 {
            s.items.append(other);
            let removed = s.index.remove(&1).unwrap_or(0);
            removed + s.items.len() as i64
        }
        fn main() {
            let mut s = Store { items: vec![1, 2], index: HashMap::new() };
            s.index.insert(1, 7);
            let mut extra: Vec<i64> = vec![3, 4];
            print_i64(drain_into(&mut s, &mut extra));
            print_i64(extra.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping append/remove run");
            return;
        }
    };
    assert_eq!(
        out,
        "11
0",
        "append moves the items and empties the source; remove takes the entry out"
    );
}

#[test]
fn vec_literal_passed_by_mut_reference_takes_the_parameter_element_type() {
    let out = match run_src(
        "fn take(v: &mut Vec<i64>) -> i64 {
            v.push(9);
            v.len() as i64
        }
        fn main() {
            let mut extra = vec![3, 4];
            print_i64(take(&mut extra));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping vec literal run");
            return;
        }
    };
    assert_eq!(
        out, "3",
        "an unannotated `vec![..]` takes the element type of the `&mut Vec<T>` parameter"
    );
}

#[test]
fn position_counts_the_items_that_reach_it() {
    let out = match run_src(
        "fn find(keys: &[i64], want: i64) -> i64 {
            keys.iter().position(|k| *k == want).unwrap_or(9) as i64
        }
        fn main() {
            let v = vec![1i64, 2, 3, 4];
            let hit = find(&v, 2);
            let filtered = v.iter().filter(|x| **x > 1).position(|x| *x == 3).unwrap_or(9) as i64;
            let miss = v.iter().position(|x| *x == 7).unwrap_or(9) as i64;
            print_i64(hit * 1000 + filtered * 100 + miss);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping position run");
            return;
        }
    };
    assert_eq!(
        out, "1109",
        "position is the index among the items the chain yields; a miss is None"
    );
}

#[test]
fn untyped_extend_and_btree_constructors_take_their_element_types() {
    let out = match run_src(
        "use std::collections::BTreeMap;
        fn main() {
            let mut v = Vec::new();
            v.extend(vec![1i64, 2]);
            let mut m = BTreeMap::new();
            m.insert(2i64, 3i64);
            print_i64(v.len() as i64 * 10 + m.len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping extend/BTreeMap run");
            return;
        }
    };
    assert_eq!(
        out, "21",
        "extend takes the element type of its argument; BTreeMap::new takes it from insert"
    );
}

#[test]
fn clone_of_a_vector_reference_types_an_if_with_a_new_else() {
    let out = match run_src(
        "fn inherit(gated: bool, attrs: &Vec<i64>) -> Vec<i64> {
            if gated { attrs.clone() } else { Vec::new() }
        }
        fn main() {
            let a = vec![1i64];
            print_i64(inherit(true, &a).len() as i64 * 10 + inherit(false, &a).len() as i64);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping clone/if run");
            return;
        }
    };
    assert_eq!(
        out, "10",
        "clone of `&Vec<T>` is a Vec<T>, so the else branch's Vec::new takes that type"
    );
}

#[test]
fn as_deref_reads_an_option_string_through_a_reference() {
    let out = match run_src(
        "fn len_of(name: &Option<String>) -> i64 {
            name.as_deref().unwrap_or(\"\").len() as i64
        }
        fn owned(name: Option<String>) -> i64 {
            name.as_deref().unwrap_or(\"\").len() as i64
        }
        fn main() {
            let n: Option<String> = Some(\"abc\".to_string());
            print_i64(len_of(&n) * 10 + owned(n.clone()));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping as_deref run");
            return;
        }
    };
    assert_eq!(
        out, "33",
        "as_deref of an Option<String> is the string, through a reference too"
    );
}

#[test]
fn and_then_on_an_option_reference_runs_the_closure() {
    let out = match run_src(
        "fn half(x: i64) -> Option<i64> {
            if x % 2 == 0 { Some(x / 2) } else { None }
        }
        fn scaled(a: &Option<i64>) -> i64 {
            a.and_then(|v| Some(v * 3)).unwrap_or(0)
        }
        fn main() {
            let a = Some(2i64);
            let b: Option<i64> = Some(8);
            print_i64(scaled(&a) * 10 + b.and_then(|v| half(v)).unwrap_or(-1));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping and_then run");
            return;
        }
    };
    assert_eq!(
        out, "64",
        "and_then through a reference applies the closure to the payload"
    );
}

#[test]
fn and_then_takes_a_named_function() {
    let out = match run_src(
        "fn double(v: i64) -> Option<i64> {
            Some(v * 2)
        }
        fn main() {
            let a: Option<i64> = Some(4);
            print_i64(a.and_then(double).unwrap_or(0));
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping and_then-by-name run");
            return;
        }
    };
    assert_eq!(
        out, "8",
        "a named function given to and_then is called with the payload"
    );
}

#[test]
fn contains_on_const_and_local_slices() {
    let out = match run_src(
        "const N: &[i64] = &[1, 2];
        const RESERVED: &[&str] = &[\"fn\", \"let\"];
        fn has(x: i64) -> i64 {
            if N.contains(&x) { 1 } else { 0 }
        }
        fn is_reserved(name: &str) -> i64 {
            if RESERVED.contains(&name) { 1 } else { 0 }
        }
        fn local(name: &str) -> i64 {
            let r: &[&str] = &[\"fn\", \"let\"];
            if r.contains(&name) { 1 } else { 0 }
        }
        fn main() {
            print_i64(has(2) * 100 + is_reserved(\"let\") * 10 + local(\"let\") + has(5) * 1000);
        }",
    ) {
        Some(o) => o,
        None => {
            eprintln!("no C compiler available; skipping slice contains run");
            return;
        }
    };
    assert_eq!(
        out, "111",
        "contains on a const slice of integers or strings, and on a local slice"
    );
}

/// `if let V(n) = &mut e` binds `n` to the payload's address: writes through it reach `e`.
#[test]
fn mut_ref_enum_payload_binding_writes_through() {
    let out = match run_src(
        "enum Kind { Inline(i64), Other }
        struct M { kind: Kind }
        fn bump(m: &mut M) {
            if let Kind::Inline(n) = &mut m.kind {
                *n = *n + 10;
            }
        }
        fn main() {
            let mut m = M { kind: Kind::Inline(1) };
            bump(&mut m);
            if let Kind::Inline(n) = m.kind {
                print_i64(n);
            }
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "11");
}

/// `std::mem::take` through a `&mut` payload binding empties the enum's own vector.
#[test]
fn mem_take_through_mut_enum_payload_binding_runs() {
    let out = match run_src(
        "enum Kind { Inline(Vec<i64>), Other }
        struct M { kind: Kind }
        fn take_it(m: &mut M) -> Vec<i64> {
            let mut out: Vec<i64> = Vec::new();
            if let Kind::Inline(sub) = &mut m.kind {
                let taken = std::mem::take(sub);
                out = taken;
            }
            out
        }
        fn main() {
            let mut m = M { kind: Kind::Inline(vec![1, 2, 3]) };
            let v = take_it(&mut m);
            print_i64(v.len() as i64);
            if let Kind::Inline(rest) = &m.kind {
                print_i64(rest.len() as i64);
            }
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "3\n0");
}

/// A string literal pattern (`match`, `matches!`) matches the string's contents, not its
/// address: a runtime `String` equal to the literal is taken.
#[test]
fn string_literal_patterns_match_by_contents() {
    let out = match run_src(
        r#"fn kind(s: String) -> i64 {
            match s.as_str() {
                "Fn" => 1,
                _ => 0,
            }
        }
        fn is_fn(s: String) -> bool {
            matches!(s.as_str(), "Fn" | "FnMut")
        }
        fn main() {
            print_i64(kind(String::from("Fn")));
            print_i64(kind(String::from("Other")));
            if is_fn(String::from("FnMut")) { print_i64(7); }
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "1\n0\n7");
}

/// `Option::is_some_and` binds its closure parameter to the payload (typed from the receiver).
#[test]
fn option_is_some_and_binds_payload_type() {
    let out = match run_src(
        "struct Seg { n: i64 }
        fn big(segs: Vec<Seg>) -> bool {
            segs.last().is_some_and(|s| s.n > 5)
        }
        fn main() {
            let a = vec![Seg { n: 9 }];
            let b = vec![Seg { n: 2 }];
            if big(a) { print_i64(1); }
            if big(b) { print_i64(2); }
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "1");
}

/// A `Vec<(T, U)>` annotation types the elements of `vec![(a, 1)]`: the literal takes `i64`,
/// not the default `i32`, so the element matches the vector's C type.
#[test]
fn vec_macro_elements_take_the_annotated_tuple_type() {
    let out = match run_src(
        r#"fn main() {
            let v: Vec<(String, i64)> = vec![(String::from("b"), 1)];
            let w: Vec<i64> = vec![2, 3];
            print_i64(v[0].1 + w[1]);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "4");
}

/// `first(&BTreeSet<T>)`: a reference to a sorted set keeps its hint, so `iter().next()` works.
#[test]
fn reference_to_btree_set_iterates_through_its_hint() {
    let out = match run_src(
        "use std::collections::BTreeSet;
        fn first(owners: &BTreeSet<usize>) -> usize {
            match owners.iter().next() {
                Some(&o) => o,
                None => 0,
            }
        }
        fn main() {
            let mut s: BTreeSet<usize> = BTreeSet::new();
            s.insert(4);
            print_i64(first(&s) as i64);
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "4");
}

/// `take(n)` and `skip(n)` in iterator chains over a vector and over `chars()`: the count is
/// read once, and the items are the first `n` (or those after the first `n`).
#[test]
fn iterator_take_and_skip_stages_run() {
    let out = match run_src(
        r#"fn main() {
            let v: Vec<i64> = vec![1, 2, 3, 4, 5];
            let a: Vec<i64> = v.iter().take(2).cloned().collect();
            let b: Vec<i64> = v.iter().skip(3).cloned().collect();
            let c: i64 = v.iter().skip(1).take(2).sum();
            let s = String::from("abc");
            let t: String = s.chars().take(2).collect();
            print_i64(a.len() as i64);
            print_i64(b.len() as i64);
            print_i64(c);
            println!("{}", t);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "2\n2\n5\nab");
}

/// `to_string` on a reference to an integer reads the number through the pointer.
#[test]
fn to_string_of_integer_reference_runs() {
    let out = match run_src(
        "fn show(c: &i128) -> String { c.to_string() }
        fn main() { let n: i128 = 42; println!(\"{}\", show(&n)); }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "42");
}

/// `for (name, n) in xs.iter_mut()` over tuples: `n` is the address of the tuple's second
/// field, so writes through it change the vector's elements.
#[test]
fn iter_mut_tuple_pattern_binds_field_addresses() {
    let out = match run_src(
        r#"fn bump(xs: &mut Vec<(String, i64)>) {
            for (_, n) in xs.iter_mut() {
                *n = *n + 1;
            }
        }
        fn main() {
            let mut v: Vec<(String, i64)> = vec![(String::from("a"), 1), (String::from("b"), 5)];
            bump(&mut v);
            print_i64(v[0].1);
            print_i64(v[1].1);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "2\n6");
}

/// `push` and `insert` arguments take the collection's declared element types: a tuple
/// literal pushed into a `Vec<(String, i64)>` or inserted into a `HashMap<String, (i64, usize)>`
/// builds the declared C type.
#[test]
fn collection_push_and_insert_arguments_take_declared_types() {
    let out = match run_src(
        r#"use std::collections::HashMap;
        fn main() {
            let mut v: Vec<(String, i64)> = Vec::new();
            v.push((String::from("a"), 1));
            let mut m: HashMap<String, (i64, usize)> = HashMap::new();
            m.insert(String::from("k"), (7, 2));
            print_i64(v[0].1);
            print_i64(m.len() as i64);
            print_i64(m.get("k").unwrap().0);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "1\n1\n7");
}

/// `for x in <adapter chain>` over pure closures collects the chain and iterates it; a
/// `let`-bound chain that the next statement's `for` consumes is fused into that loop.
#[test]
fn for_over_pure_adapter_chain_runs() {
    let out = match run_src(
        r#"struct F { locals: Vec<(String, Option<i64>)>, values: Vec<i64> }
        fn total(f: &F) -> i64 {
            let mut t: i64 = 0;
            let vals = f.locals.iter().filter_map(|(_, o)| *o);
            for v in vals {
                t = t + v;
            }
            for v in f.values.iter().map(|v| *v + 100) {
                t = t + v;
            }
            t
        }
        fn main() {
            let f = F { locals: vec![(String::from("a"), Some(1)), (String::from("b"), None)], values: vec![2] };
            print_i64(total(&f));
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "103");
}

/// A struct field's declared type decides the literal parts of its initializer: `Some(1)`
/// inside a `vec![(s, Some(1))]` field of `Vec<(String, Option<i64>)>` is an `i64` payload.
#[test]
fn struct_field_initializer_takes_declared_nested_type() {
    let out = match run_src(
        r#"struct F { v: Vec<(String, Option<i64>)> }
        fn main() {
            let f = F { v: vec![(String::from("a"), Some(1))] };
            print_i64(f.v.len() as i64);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "1");
}

/// A `let` annotation reaches a `collect()` inside `opt.map(|x| ..)` through `unwrap_or_default`:
/// the collected vector takes the annotated element type.
#[test]
fn let_annotation_reaches_collect_inside_option_map() {
    let out = match run_src(
        "struct Field { name: i64 }
        struct Variant { fields: Option<Vec<Field>> }
        fn count(v: &Variant) -> usize {
            let tys: Vec<i64> = v.fields.as_ref().map(|fs| fs.iter().map(|f| f.name).collect()).unwrap_or_default();
            tys.len()
        }
        fn main() {
            let v = Variant { fields: Some(vec![Field { name: 4 }, Field { name: 5 }]) };
            print_i64(count(&v) as i64);
            let none = Variant { fields: None };
            print_i64(count(&none) as i64);
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "2\n0");
}

/// `join` on a vector reached through a reference (`&Vec<String>`).
#[test]
fn join_through_vector_reference_runs() {
    let out = match run_src(
        "fn names(params: &Vec<String>) -> String { params.join(\", \") }
        fn main() {
            let v = vec![String::from(\"a\"), String::from(\"b\")];
            println!(\"{}\", names(&v));
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "a, b");
}

/// `m.entry(k).or_default().insert(x)` on a map of sets, and `len()` of a set read back through
/// `get(..).unwrap()`.
#[test]
fn entry_or_default_set_insert_and_sorted_len_run() {
    let out = match run_src(
        r#"use std::collections::HashMap;
        use std::collections::BTreeSet;
        fn main() {
            let mut defined_by: HashMap<String, BTreeSet<usize>> = HashMap::new();
            let names = vec![String::from("x"), String::from("x"), String::from("y")];
            for idx in 0..3 {
                defined_by.entry(names[idx].clone()).or_default().insert(idx % 2);
            }
            print_i64(defined_by.len() as i64);
            print_i64(defined_by.get("x").unwrap().len() as i64);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "2\n2");
}

/// `take_while` stops at the first item that fails the predicate; `skip_while` drops the items
/// up to it and keeps the rest.
#[test]
fn iterator_take_while_and_skip_while_run() {
    let out = match run_src(
        r#"fn main() {
            let rest = String::from("123ab4");
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let tail: String = rest.chars().skip_while(|c| c.is_ascii_digit()).collect();
            let v: Vec<i64> = vec![1, 2, 5, 1, 9];
            let a: Vec<i64> = v.iter().take_while(|x| **x < 5).cloned().collect();
            let b: Vec<i64> = v.iter().skip_while(|x| **x < 5).cloned().collect();
            println!("{} {} {} {}", digits, tail, a.len(), b.len());
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "123 ab4 2 3");
}

/// `HashMap::retain` removes the entries whose predicate fails, with the predicate's `&mut V`
/// writes kept; the entry moved into a freed slot is tested too.
#[test]
fn map_retain_removes_failing_entries_and_keeps_writes() {
    let out = match run_src(
        r#"use std::collections::HashMap;
        fn main() {
            let mut h: HashMap<i64, i64> = HashMap::new();
            for i in 0..10 { h.insert(i, i * 10); }
            h.retain(|k, v| { *v = *v + 1; *k % 3 == 0 });
            let mut s: i64 = 0;
            let mut n: i64 = 0;
            for (k, v) in h.iter() { s = s + *k + *v; n = n + 1; }
            print_i64(n);
            print_i64(s);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "4\n202");
}

/// A `std::collections::HashMap::new()` written with its module path, inside a call argument
/// and inside a macro's arguments.
#[test]
fn qualified_std_constructor_paths_run() {
    let out = match run_src(
        r#"use std::collections::HashMap;
        fn size(m: &HashMap<String, i64>) -> usize { m.len() }
        fn main() {
            let n = size(&std::collections::HashMap::new());
            println!("{} {}", n, size(&std::collections::HashMap::new()));
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "0 0");
}

/// `extend(x.clone())` takes the items of a vector field read through a reference.
#[test]
fn extend_with_cloned_vector_field_runs() {
    let out = match run_src(
        "#[derive(Clone)]
        struct P { n: i64 }
        struct G { params: Vec<P> }
        fn merged(g: &mut G, extra: &G) { g.params.extend(extra.params.clone()); }
        fn main() {
            let mut g = G { params: vec![P { n: 1 }] };
            let e = G { params: vec![P { n: 2 }] };
            merged(&mut g, &e);
            print_i64(g.params.len() as i64);
            print_i64(g.params[1].n);
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "2\n2");
}

/// `set.extend(<adapter chain>)` inserts the chain's items; a set's `iter()` items are typed, so a
/// `filter_map` over them can call a string method on each.
#[test]
fn set_extend_from_chain_and_typed_set_iter_run() {
    let out = match run_src(
        r#"use std::collections::BTreeSet;
        use std::collections::HashSet;
        struct F { params: Vec<String>, locals: Vec<(String, i64)> }
        fn names(func: &F) -> usize {
            let mut bound: BTreeSet<String> = func.params.iter().cloned().collect();
            bound.extend(func.locals.iter().map(|(n, _)| n.clone()));
            bound.len()
        }
        fn fill(seen: &mut HashSet<String>, n: i64) { seen.insert(format!("x{}", n)); }
        fn prefixed(n: i64) -> Vec<String> {
            let mut seen = std::collections::HashSet::new();
            fill(&mut seen, n);
            let out: Vec<String> = seen.iter().filter_map(|s| s.strip_prefix("x").map(|x| x.to_string())).collect();
            out
        }
        fn main() {
            let f = F { params: vec![String::from("a")], locals: vec![(String::from("b"), 1), (String::from("a"), 2)] };
            print_i64(names(&f) as i64);
            let r = prefixed(7);
            println!("{}", r[0]);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "2\n7");
}

/// `Path::file_stem` drops the last extension; a path without a file name has none. `for x in
/// arr.iter()` over a let-bound array reads the array's items.
#[test]
fn file_stem_and_array_iter_run() {
    let out = match run_src(
        r#"use std::path::Path;
        fn stem(p: &str) -> String {
            let x = Path::new(p);
            x.file_stem().map(|s| s.to_string()).unwrap_or_default()
        }
        fn main() {
            println!("{} {} [{}]", stem("dir/file.rs"), stem("a.tar.gz"), stem("dir/"));
            let names = ["a", "bb"];
            let mut n = 0;
            for x in names.iter() { n = n + x.len(); }
            print_i64(n as i64);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "file a.tar [dir]\n3");
}

/// `let &(a, b) = r;` binds the components of a tuple read through a reference.
#[test]
fn let_destructures_a_tuple_reference_run() {
    let out = match run_src(
        "fn main() {
            let t: (i64, i64) = (3, 4);
            let r: &(i64, i64) = &t;
            let &(a, b) = r;
            let pair: (i64, i64) = (5, 6);
            let &(c, d) = &pair;
            print_i64(a + b + c + d);
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "18");
}

/// `chain` takes an adapter chain as its other side (collected first when its closures are pure),
/// and `parse::<i128>` / `parse::<u128>` parse as the backend's 64-bit integers.
#[test]
fn chain_with_adapter_other_side_and_wide_parse_run() {
    let out = match run_src(
        r#"fn main() {
            let a: Vec<i64> = vec![1, 2];
            let b: Vec<Vec<i64>> = vec![vec![10], vec![20, 30]];
            let s: i64 = a.iter().chain(b.iter().flat_map(|r| r.iter())).sum();
            let t: Vec<i64> = a.iter().chain(a.iter().map(|x| *x * 10)).cloned().collect();
            print_i64(s);
            print_i64(t.len() as i64);
            let n: i128 = "42".parse::<i128>().unwrap();
            let m = "7".parse::<u128>().unwrap_or(0);
            print_i64(n as i64 + m as i64);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "63\n4\n49");
}

/// `for line in s.split_inclusive('\n')` yields each line with its newline (the last piece has
/// none when the text does not end in one).
#[test]
fn split_inclusive_newline_loop_keeps_newlines() {
    let out = match run_src(
        r#"fn main() {
            let c = String::from("ab\ncd\n\nef");
            let mut n: i64 = 0;
            let mut pieces: i64 = 0;
            for line in c.split_inclusive('\n') {
                n = n + line.len() as i64;
                pieces = pieces + 1;
            }
            print_i64(n);
            print_i64(pieces);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "9\n4");
}

/// `m.into_iter()` over a map reads its `(key, value)` pairs, in a `for` loop and in a chain.
#[test]
fn map_into_iter_runs_in_loop_and_chain() {
    let out = match run_src(
        r#"use std::collections::HashMap;
        fn main() {
            let mut m: HashMap<String, i64> = HashMap::new();
            m.insert(String::from("ab"), 5);
            let mut n: i64 = 0;
            for (k, v) in m.into_iter() { n = n + k.len() as i64 + v; }
            print_i64(n);
            let mut m2: HashMap<String, i64> = HashMap::new();
            m2.insert(String::from("ab"), 5);
            let total: i64 = m2.into_iter().map(|(k, v)| k.len() as i64 + v).sum();
            print_i64(total);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "7\n7");
}

/// An array used as an iterator chain's source (`KEYWORDS.iter().any(..)`) is walked as its items.
#[test]
fn array_as_iterator_chain_source_runs() {
    let out = match run_src(
        r#"const KEYWORDS: [&str; 3] = ["fn", "let", "if"];
        fn is_kw(s: &str) -> bool { KEYWORDS.iter().any(|k| *k == s) }
        fn main() {
            let names = ["a", "bb"];
            println!("{} {}", is_kw("let"), is_kw("x"));
            let total: usize = names.iter().map(|n| n.len()).sum();
            print_i64(total as i64);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "true false\n3");
}

/// `max()` / `min()` after a `map` compare the mapped items, not the source tuples.
#[test]
fn max_after_map_compares_mapped_items() {
    let out = match run_src(
        r#"fn main() {
            let v: Vec<(String, i64)> = vec![(String::from("a"), 4), (String::from("b"), 9)];
            let best: i64 = v.iter().map(|p| p.1).max().unwrap_or(0);
            let low: i64 = v.iter().map(|(_, t)| *t).min().unwrap_or(0);
            print_i64(best);
            print_i64(low);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "9\n4");
}

/// Indexing a vector-valued call (`mk()[1]`) and a method result that returns a reference to a
/// vector (`s.get()[0]`).
#[test]
fn index_of_call_and_method_result_runs() {
    let out = match run_src(
        "fn mk() -> Vec<i64> { vec![4, 5] }
        struct S { v: Vec<i64> }
        impl S { fn get(&self) -> &Vec<i64> { &self.v } }
        fn main() {
            let a = mk()[1];
            let s = S { v: vec![7, 8] };
            let b = s.get()[0];
            print_i64(a + b);
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "12");
}

/// `v.iter_mut().find(|x| ..)` gives `Option<&mut T>`: a write through the found element reaches
/// the vector, for a vector of integers and for a vector of `(name, value)` tuples matched with
/// `Some((_, t))`.
#[test]
fn iter_mut_find_writes_through_the_found_element() {
    let out = match run_src(
        r#"struct S { fields: Vec<(String, i64)> }
        fn bump(v: &mut Vec<i64>, want: i64) -> bool {
            match v.iter_mut().find(|x| **x == want) {
                Some(t) => { *t = *t + 100; true }
                None => false,
            }
        }
        fn set(s: &mut S, name: &str, v: i64) -> bool {
            match s.fields.iter_mut().find(|(n, _)| n == name) {
                Some((_, t)) => { if *t != v { *t = v; } true }
                None => false,
            }
        }
        fn main() {
            let mut v = vec![1, 2];
            let ok = bump(&mut v, 2);
            print_i64(v[1]);
            print_i64(if ok { 1 } else { 0 });
            let mut s = S { fields: vec![(String::from("a"), 1), (String::from("b"), 2)] };
            let r = set(&mut s, "b", 9);
            print_i64(s.fields[1].1);
            print_i64(if r { 1 } else { 0 });
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "102\n1\n9\n1");
}

/// `fold(init, |acc, x| ..)` over a chain: the closure takes the accumulator and each item, and
/// its result is the next accumulator.
#[test]
fn fold_accumulates_through_the_closure() {
    let out = match run_src(
        r#"fn main() {
            let v: Vec<f64> = vec![1.5, 4.0, 2.0];
            let a: f64 = v.iter().map(|x| *x * 2.0).fold(0.0f64, |acc, x| if x > acc { x } else { acc });
            let n: i64 = vec![1i64, 5, 3].iter().fold(0, |acc, x| acc + *x);
            print_f64(a);
            print_i64(n);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "8\n9");
}

/// `fold(init, f64::max)` / `fold(init, i64::min)`: the numeric function is compared inline.
#[test]
fn fold_with_numeric_max_and_min_paths_runs() {
    let out = match run_src(
        r#"fn main() {
            let v: Vec<f64> = vec![1.5, 4.0, 2.0];
            let b: f64 = v.iter().map(|x| *x).fold(0.0f64, f64::max);
            let d: i64 = vec![7i64, 3, 9].iter().map(|x| *x).fold(100, i64::min);
            print_f64(b);
            print_i64(d);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "4\n3");
}

/// `zip(params.iter().cloned())` after a `map` over tuples, collected into a map: the zip's other
/// side is its sequence, with the adapters stripped.
#[test]
fn zip_with_cloned_sequence_after_map_collects_into_map() {
    let out = match run_src(
        r#"use std::collections::HashMap;
        #[derive(Clone)]
        enum Ty { I(i64), B }
        struct Call { params: Vec<(String, Ty)> }
        fn solved(call: &Call, params: &[Ty]) -> i64 {
            let m: HashMap<String, Ty> = call.params.iter().map(|(n, _)| n.clone()).zip(params.iter().cloned()).collect();
            let mut t: i64 = 0;
            for (_, v) in m.iter() { if let Ty::I(x) = v { t = t + *x; } }
            t
        }
        fn main() {
            let c = Call { params: vec![(String::from("a"), Ty::I(4))] };
            print_i64(solved(&c, &[Ty::I(9)]));
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "9");
}

/// `max_by_key` / `min_by_key` return the item whose key is largest / smallest (the first of equal
/// keys), or `None` for an empty chain. A `&vec![..]` argument takes the parameter's element type.
#[test]
fn by_key_terminals_and_argument_element_types_run() {
    let out = match run_src(
        r#"#[derive(Clone, Copy)]
        enum Cat { Low, High }
        impl Cat { fn conf(&self) -> i64 { match self { Cat::Low => 1, Cat::High => 5 } } }
        fn classify(n: &i64) -> Cat { if *n > 3 { Cat::High } else { Cat::Low } }
        fn best(v: &Vec<i64>) -> i64 {
            let c = v.iter().map(|m| classify(m)).max_by_key(|c| c.conf()).unwrap_or(Cat::Low);
            match c { Cat::High => 1, Cat::Low => 0 }
        }
        fn total(v: &Vec<i64>) -> i64 { let mut t: i64 = 0; for x in v.iter() { t = t + *x; } t }
        fn main() {
            let v: Vec<i64> = vec![4, -2, 9, 9, 1];
            let hi = v.iter().max_by_key(|x| **x).unwrap_or(0);
            let lo = v.iter().min_by_key(|x| **x - 3).unwrap_or(0);
            let e: Vec<i64> = Vec::new();
            let none = e.iter().max_by_key(|x| **x).unwrap_or(-1);
            print_i64(hi);
            print_i64(lo);
            print_i64(none);
            print_i64(best(&vec![1, 7, 2]));
            print_i64(total(&vec![1, 2]));
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "9\n-2\n-1\n1\n3");
}

/// A `&dyn Fn(&str) -> String` parameter keeps its signature: calling it gives a `String` whose
/// methods (`trim`) resolve, and the value is passed through the function pointer.
#[test]
fn dyn_fn_parameter_keeps_its_signature_and_runs() {
    let out = match run_src(
        r#"fn run(backend: &dyn Fn(&str) -> String, p: &str) -> String {
            let reply = backend(p);
            if !reply.trim().is_empty() { reply.trim().to_string() } else { String::new() }
        }
        fn main() {
            let f = |s: &str| format!("  {} ", s);
            println!("[{}]", run(&f, "q"));
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "[q]");
}

/// `.collect::<Vec<_>>()` takes its element type from the chain; a `vec!` literal passed as
/// `&v` to a slice parameter takes the slice's element type; `to_string` of a copied item works.
#[test]
fn turbofish_collect_and_slice_argument_inference_run() {
    let out = match run_src(
        r#"fn show(args: &[i64]) -> String {
            args.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
        }
        fn total(v: &[i64]) -> i64 { let mut t: i64 = 0; for x in v.iter() { t = t + *x; } t }
        fn main() {
            let v = vec![1, 2];
            let s = String::from("abc");
            let t = s.chars().collect::<String>();
            println!("{} {}", show(&v), t);
            print_i64(total(&v));
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "1, 2 abc\n3");
}

/// A chain whose left side is itself a pure adapter chain (`a.iter().filter_map(..).chain(..)`),
/// used by a `for` loop and collected.
#[test]
fn chain_with_adapter_left_side_runs() {
    let out = match run_src(
        r#"struct F { locals: Vec<(String, Option<i64>)>, values: Vec<i64> }
        fn main() {
            let f = F { locals: vec![(String::from("a"), Some(1)), (String::from("b"), None)], values: vec![2] };
            let mut n: i64 = 0;
            for ty in f.locals.iter().filter_map(|(_, t)| *t).chain(f.values.iter().map(|v| *v + 100)) { n = n + ty; }
            let all: Vec<i64> = f.values.iter().map(|v| *v).chain(f.values.iter().map(|v| *v * 2)).collect();
            print_i64(n);
            print_i64(all.len() as i64);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "103\n2");
}

/// `v.iter_mut().for_each(f)` calls `f` with each element's address, for a function and for a
/// closure.
#[test]
fn iter_mut_for_each_writes_each_element() {
    let out = match run_src(
        r#"fn dbl(x: &mut i64) { *x = *x * 2; }
        fn main() {
            let mut v: Vec<i64> = vec![1, 2, 3];
            v.iter_mut().for_each(dbl);
            v.iter_mut().for_each(|x| *x = *x + 1);
            print_i64(v[0] + v[1] + v[2]);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "15");
}

/// `clone()` of a set parameter is a set of the same type: inserts into the copy are typed.
#[test]
fn clone_of_set_reference_keeps_its_type() {
    let out = match run_src(
        r#"use std::collections::HashSet;
        fn grow(bound: &HashSet<String>, names: &Vec<String>) -> usize {
            let mut bound_set = bound.clone();
            for n in names.iter() { bound_set.insert(n.clone()); }
            bound_set.len()
        }
        fn main() {
            let mut s: HashSet<String> = HashSet::new();
            s.insert(String::from("x"));
            let v = vec![String::from("a")];
            print_i64(grow(&s, &v) as i64);
        }"#,
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "2");
}
