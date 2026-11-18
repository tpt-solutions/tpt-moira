//! Std methods on the C backend that the self-host sources use: `parse::<T>()` as a `Result`,
//! `rsplit`, `clear` on maps and sets, `drain` on sets, `bool::then`, and the float bit helpers.
//! Each program is lowered with the checker's types and run; skipped when no C compiler is available.

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

fn run_c(name: &str, src: &str) -> Option<String> {
    let cc = find_cc()?;
    let prog = tpt_moira_clotho::compile_source(src).expect("program should parse");
    let prog = tpt_moira_lachesis::mono::monomorphize(&prog, &[]);
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("program should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let dir: PathBuf = std::env::temp_dir();
    let c_path = dir.join(format!("moira_stdm_{name}.c"));
    let exe_path = dir.join(format!("moira_stdm_{name}"));
    std::fs::write(&c_path, c).expect("write C");
    let status = Command::new(&cc)
        .arg("-O0")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .status()
        .expect("run cc");
    assert!(status.success(), "C compilation failed for {name}");
    let out = Command::new(&exe_path).output().expect("run program");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

#[test]
fn parse_turbofish_is_a_result_and_runs() {
    let src = r#"
fn main() {
    let s = String::from("12");
    println!("{}", s.parse::<usize>().is_ok());
    println!("{}", "x1".parse::<i64>().is_err());
    match "41".parse::<i32>() {
        Ok(n) => println!("ok {}", n + 1),
        Err(_) => println!("bad"),
    }
    let v = "7".parse::<u64>().unwrap();
    println!("{}", v * 2);
    let f = "2.5".parse::<f64>().unwrap();
    println!("{}", f);
}
"#;
    let Some(out) = run_c("parse", src) else {
        return;
    };
    assert_eq!(out, "true\ntrue\nok 42\n14\n2.5\n");
}

#[test]
fn rsplit_yields_pieces_last_first() {
    let src = r#"
fn main() {
    let s = "a,b,cc";
    println!("{}", s.rsplit(',').next().unwrap());
    println!("{}", s.rsplit(',').count());
    println!("{}", s.split(',').next().unwrap());
}
"#;
    let Some(out) = run_c("rsplit", src) else {
        return;
    };
    assert_eq!(out, "cc\n3\na\n");
}

#[test]
fn map_clear_and_set_drain_run() {
    let src = r#"
use std::collections::{HashMap, HashSet};
struct H { seen: HashSet<String>, m: HashMap<String, i64> }
fn main() {
    let mut m: HashMap<String, i64> = HashMap::new();
    m.insert(String::from("a"), 1);
    m.clear();
    println!("{}", m.len());
    let mut s: HashSet<String> = HashSet::new();
    s.insert(String::from("a"));
    s.insert(String::from("b"));
    let n = s.drain().count();
    println!("{} {}", n, s.len());
    let mut h = H { seen: HashSet::new(), m: HashMap::new() };
    h.seen.insert(String::from("q"));
    h.m.insert(String::from("z"), 3);
    h.seen.clear();
    h.m.clear();
    println!("{} {}", h.seen.len(), h.m.len());
}
"#;
    let Some(out) = run_c("clear_drain", src) else {
        return;
    };
    assert_eq!(out, "0\n2 0\n0 0\n");
}

#[test]
fn bool_then_and_float_bits_run() {
    let src = r#"
fn main() {
    let b = 3 > 2;
    let r = b.then(|| 40 + 2);
    println!("{}", r.is_some());
    let c = 1 > 2;
    println!("{}", c.then(|| 5).is_none());
    let z = 0.0f64;
    println!("{}", (z / z).is_nan());
    let bits = 2.0f64.to_bits();
    println!("{}", f64::from_bits(bits));
}
"#;
    let Some(out) = run_c("then_bits", src) else {
        return;
    };
    assert_eq!(out, "true\ntrue\ntrue\n2\n");
}

#[test]
fn std_clock_env_radix_and_bool_print_run() {
    let src = r#"
use std::time::Instant;
fn main() {
    let t = Instant::now();
    let d = t.elapsed().as_secs_f64();
    println!("{}", d >= 0.0);
    let v = std::env::var_os("MOIRA_TEST_UNSET_VARIABLE_X").is_some();
    println!("{}", v);
    let w = std::env::var("MOIRA_TEST_UNSET_VARIABLE_X").is_err();
    println!("{}", w);
    let p = std::env::temp_dir();
    println!("{}", p.len() > 0);
    let n = u32::from_str_radix("1f", 16).unwrap();
    println!("{}", n);
    let a = 3 > 2;
    println!("{}", a);
}
"#;
    let Some(out) = run_c("std_clock", src) else {
        return;
    };
    assert_eq!(out, "true\nfalse\ntrue\ntrue\n31\ntrue\n");
}
