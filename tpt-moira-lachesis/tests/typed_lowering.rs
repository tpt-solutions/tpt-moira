//! Typed lowering: Lachesis's expression types reach Atropos as hints, so a
//! `for` over a vector that the untyped lowering cannot see, and a `println!` of a
//! `bool`, lower correctly. The run test compiles the emitted C with the system C
//! compiler and is skipped when none is available.

use std::process::Command;

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

/// Type-check `src`, lower it with the checker's hints, compile and run it.
/// Returns stdout, or `None` when no C compiler is available.
fn check_and_run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .expect("program should lower with hints");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_typed_{name}.c"));
    let exe_path = dir.join(format!("moira_typed_{name}"));
    std::fs::write(&c_path, c).unwrap();
    let status = Command::new(&cc)
        .arg("-O0")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .status()
        .expect("run cc");
    assert!(status.success(), "C compilation failed for {name}");
    let out = Command::new(&exe_path).output().expect("run program");
    assert!(out.status.success(), "program {name} exited with failure");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

/// The `Err(diags)` binding of a `Result` is typed by the checker, so its `for`
/// loop lowers only with the hints: without them the same program is rejected.
#[test]
fn pattern_bound_vec_lowers_only_with_hints() {
    let src = r#"
fn make() -> Result<i32, Vec<i32>> {
    let mut v: Vec<i32> = Vec::new();
    v.push(7);
    Err(v)
}

fn main() {
    if let Err(diags) = make() {
        let mut s: i32 = 0;
        for d in &diags {
            s = s + d;
        }
        println!("{}", s);
    }
}
"#;
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    assert!(
        tpt_moira_atropos::lower_program(&prog).is_err(),
        "without hints the pattern-bound vector cannot be iterated"
    );
    assert!(
        tpt_moira_atropos::lower_program_typed(&prog, &hints).is_ok(),
        "with the checker's hints it lowers"
    );
}

/// A `let`-bound vector from a function's return type, iterated by reference, and
/// a `bool` printed by `println!`: both rely on hints, and both run end to end.
#[test]
fn hinted_vec_loop_and_bool_print_run() {
    let src = r#"
fn make() -> Vec<i32> {
    let mut v: Vec<i32> = Vec::new();
    v.push(7);
    v.push(8);
    v
}

fn main() {
    let diags = make();
    let mut s: i32 = 0;
    for d in &diags {
        s = s + d;
    }
    println!("{}", s);
    let b: bool = true;
    println!("{}", b);
}
"#;
    let Some(out) = check_and_run("vec_bool", src) else {
        return;
    };
    assert!(out.contains("15"), "sum of the vector: {out:?}");
    assert!(out.contains("true"), "bool printed as true: {out:?}");
}

/// `?` on `Result` values: success yields the payload, an `Err` returns early
/// from the enclosing function. Runs end to end.
#[test]
fn try_operator_propagates_errors_and_runs() {
    let src = r#"
fn parse(x: i32) -> Result<i32, i32> {
    if x > 0 {
        Ok(x * 2)
    } else {
        Err(x)
    }
}

fn twice(x: i32) -> Result<i32, i32> {
    let a = parse(x)?;
    let b = parse(a)?;
    Ok(b + 1)
}

fn main() {
    if let Ok(v) = twice(3) {
        println!("{}", v);
    }
    if let Err(e) = twice(-1) {
        println!("{}", e);
    }
}
"#;
    let Some(out) = check_and_run("try_result", src) else {
        return;
    };
    assert!(out.contains("13"), "success path: {out:?}");
    assert!(out.contains("-1"), "error path: {out:?}");
}

#[test]
fn nested_loops_over_fields_of_references_run() {
    let src = r#"
struct Tok {
    n: i32,
}

struct Attr {
    args: Vec<Tok>,
}

struct Item {
    attributes: Vec<Attr>,
}

fn count(item: &Item) -> i32 {
    let mut c: i32 = 0;
    for attr in &item.attributes {
        for t in &attr.args {
            c = c + t.n;
        }
    }
    c
}

fn main() {
    let mut v: Vec<Attr> = Vec::new();
    let mut toks: Vec<Tok> = Vec::new();
    toks.push(Tok { n: 3 });
    toks.push(Tok { n: 4 });
    v.push(Attr { args: toks });
    let it = Item { attributes: v };
    println!("{}", count(&it));
}
"#;
    let Some(out) = check_and_run("nested", src) else {
        return;
    };
    // `println!` currently adds blank lines (see the atropos vec tests), so trim.
    assert_eq!(out.trim(), "7");
}

#[test]
fn iterator_chains_over_ranges_run() {
    let src = r#"
fn main() {
    let sq: Vec<i32> = (0..4).map(|i| i * i).collect();
    println!("{}", sq[3]);
    let s: i32 = (1..=3).map(|i| i).sum();
    println!("{}", s);
    if (0..5).any(|i| i == 4) {
        println!("1");
    }
}
"#;
    // The collected element type comes from the checker's hint.
    let Some(out) = check_and_run("ranges", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(values, vec!["9", "6", "1"]);
}

#[test]
fn enumerate_chains_with_pair_closures_run() {
    let src = r#"
fn main() {
    let mut v: Vec<i32> = Vec::new();
    v.push(5);
    v.push(6);
    v.push(7);
    let weighted: i32 = v.iter().enumerate().map(|(i, x)| (i as i32) * 10 + x).sum();
    println!("{}", weighted);
    let n = v.iter().enumerate().filter(|(i, x)| i > 0).count();
    println!("{}", n);
    if v.iter().enumerate().any(|(i, x)| x == 7) {
        println!("1");
    }
}
"#;
    let Some(out) = check_and_run("enumerate_chains", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    // 5 + (10 + 6) + (20 + 7) = 48; two items have a position above zero.
    assert_eq!(values, vec!["48", "2", "1"]);
}
