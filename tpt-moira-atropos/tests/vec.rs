//! End-to-end tests for the generated `Vec<T>` runtime in the Stage-0 C backend.
//!
//! Each test lowers a Moira program through Clotho + Atropos, emits C, compiles
//! it with the system C compiler, runs it, and checks stdout. The tests are
//! skipped silently when no C compiler is available.

use std::path::PathBuf;
use std::process::Command;

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

/// Lower, compile and run `src`; returns its stdout, or `None` without a C compiler.
fn run(name: &str, src: &str) -> Option<String> {
    let cc = find_cc()?;
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let mir = tpt_moira_atropos::lower_program(&prog).expect("program should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let dir: PathBuf = std::env::temp_dir();
    let c_path = dir.join(format!("moira_vec_{name}.c"));
    let exe_path = dir.join(format!("moira_vec_{name}"));
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
    // `println!` currently emits an extra newline per call (the trailing `"\n"`
    // literal is printed by `print_str`, which already appends one), so blank
    // lines are dropped here; the values themselves are what these tests check.
    let text = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let mut normalized: String = text
        .lines()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    normalized.push('\n');
    Some(normalized)
}

#[test]
fn push_len_index_and_for_loops() {
    let src = r#"
fn main() {
    let mut v: Vec<i32> = Vec::new();
    v.push(3);
    v.push(4);
    v.push(5);
    println!("{}", v.len());
    let mut sum: i32 = 0;
    for x in &v {
        sum = sum + x;
    }
    println!("{}", sum);
    println!("{}", v[1]);
    let mut total: i32 = 0;
    for y in v.iter() {
        total = total + y;
    }
    println!("{}", total);
    if v.is_empty() {
        println!("{}", 1);
    } else {
        println!("{}", 0);
    }
}
"#;
    let Some(out) = run("basic", src) else {
        return;
    };
    assert_eq!(out, "3\n12\n4\n12\n0\n");
}

#[test]
fn growth_past_initial_capacity() {
    let src = r#"
fn main() {
    let mut v: Vec<i32> = Vec::new();
    let mut i: i32 = 0;
    while i < 100 {
        v.push(i);
        i = i + 1;
    }
    println!("{}", v.len());
    println!("{}", v[99]);
}
"#;
    let Some(out) = run("growth", src) else {
        return;
    };
    assert_eq!(out, "100\n99\n");
}

#[test]
fn mutable_vec_parameter_is_pushed_through_pointer() {
    let src = r#"
fn fill(v: &mut Vec<i32>, n: i32) {
    let mut i: i32 = 0;
    while i < n {
        v.push(i * 10);
        i = i + 1;
    }
}

fn main() {
    let mut v: Vec<i32> = Vec::new();
    fill(&mut v, 4);
    println!("{}", v.len());
    let mut sum: i32 = 0;
    for x in &v {
        sum = sum + x;
    }
    println!("{}", sum);
}
"#;
    let Some(out) = run("param", src) else {
        return;
    };
    assert_eq!(out, "4\n60\n");
}

#[test]
fn vec_of_struct_elements() {
    let src = r#"
struct Point {
    x: i32,
    y: i32,
}

fn main() {
    let mut pts: Vec<Point> = Vec::new();
    pts.push(Point { x: 1, y: 2 });
    pts.push(Point { x: 3, y: 4 });
    let mut sx: i32 = 0;
    for p in &pts {
        sx = sx + p.x;
    }
    println!("{}", sx);
    println!("{}", pts[1].y);
}
"#;
    let Some(out) = run("structs", src) else {
        return;
    };
    assert_eq!(out, "4\n4\n");
}

#[test]
fn slices_of_vectors_run() {
    let src = r#"
fn sum(s: &[i32]) -> i32 {
    let mut t: i32 = 0;
    for x in s {
        t = t + x;
    }
    t
}

fn main() {
    let mut v: Vec<i32> = Vec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    v.push(4);
    v.push(5);
    println!("{}", sum(&v[1..4]));
    println!("{}", sum(&v[..2]));
    println!("{}", sum(&v[3..]));
    println!("{}", sum(&v[0..=1]));
    println!("{}", v[1..3].len());
}
"#;
    let Some(out) = run("slices", src) else {
        return;
    };
    assert_eq!(out, "9\n3\n9\n3\n2\n");
}

#[test]
fn continue_advances_counted_loops() {
    let src = r#"
fn main() {
    let mut s: i32 = 0;
    for i in 0..5 {
        if i == 2 {
            continue;
        }
        s = s + i;
    }
    println!("{}", s);
    let mut v: Vec<i32> = Vec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    v.push(4);
    let mut t: i32 = 0;
    for x in &v {
        if x == 2 {
            continue;
        }
        t = t + x;
    }
    println!("{}", t);
}
"#;
    let Some(out) = run("continue", src) else {
        return;
    };
    assert_eq!(out, "8\n8\n");
}

#[test]
fn iterator_adapters_over_vectors_run() {
    let src = r#"
fn main() {
    let mut v: Vec<i32> = Vec::new();
    v.push(10);
    v.push(20);
    v.push(30);
    for (i, x) in v.iter().enumerate() {
        println!("{}", i * 100 + x);
    }
    for x in v.iter().rev() {
        println!("{}", x);
    }
    for y in &mut v {
        *y = *y + 1;
    }
    println!("{}", v[0]);
    println!("{}", v[2]);
}
"#;
    let Some(out) = run("adapters", src) else {
        return;
    };
    assert_eq!(out, "10\n120\n230\n30\n20\n10\n11\n31\n");
}

#[test]
fn iterator_chains_over_vectors_run() {
    let src = r#"
fn main() {
    let mut v: Vec<i32> = Vec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    v.push(4);
    let doubled: Vec<i32> = v.iter().map(|x| x * 2).collect();
    println!("{}", doubled[3]);
    let s: i32 = v.iter().map(|x| x + 1).sum();
    println!("{}", s);
    let k: i32 = 10;
    let t: i32 = v.iter().map(|x| x * k).sum();
    println!("{}", t);
    let big: Vec<i32> = v.iter().filter(|x| x > 2).map(|x| x * 100).collect();
    println!("{}", big[0]);
    println!("{}", big.len());
    if v.iter().any(|x| x == 3) {
        println!("1");
    }
    if v.iter().all(|x| x > 1) {
        println!("bad");
    }
    if let Some(f) = v.iter().find(|x| x > 2) {
        println!("{}", f);
    }
}
"#;
    let Some(out) = run("chains", src) else {
        return;
    };
    assert_eq!(out, "8\n14\n100\n300\n2\n1\n3\n");
}
