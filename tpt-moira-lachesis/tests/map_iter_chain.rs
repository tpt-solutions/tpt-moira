//! `m.iter()` on a `HashMap` as an iterator-chain source on the C backend: `count`, `filter`,
//! `map`, `sum` and `next` over the `(key, value)` pairs. Each program is lowered with the
//! checker's types and run; skipped when no C compiler is available.

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
    let c_path = dir.join(format!("moira_mapiter_{name}.c"));
    let exe_path = dir.join(format!("moira_mapiter_{name}"));
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
fn map_iter_chain_terminals_run() {
    let src = r#"
use std::collections::HashMap;
fn main() {
    let mut m: HashMap<String, usize> = HashMap::new();
    m.insert(String::from("a"), 1);
    m.insert(String::from("bb"), 3);
    let n = m.iter().count();
    println!("{}", n);
    let big = m.iter().filter(|p| p.1 > 1).count();
    println!("{}", big);
    let total: usize = m.iter().map(|p| p.1).sum();
    println!("{}", total);
    let first = m.iter().next();
    println!("{}", first.is_some());
}
"#;
    let Some(out) = run_c("terminals", src) else {
        return;
    };
    assert_eq!(out, "2\n1\n4\ntrue\n");
}

#[test]
fn map_iter_chain_over_vector_values_runs() {
    let src = r#"
use std::collections::HashMap;
fn main() {
    let mut m: HashMap<String, Vec<usize>> = HashMap::new();
    m.insert(String::from("a"), vec![1, 2]);
    m.insert(String::from("bb"), vec![3]);
    let big = m.iter().filter(|p| p.1.len() > 1).count();
    println!("{}", big);
    println!("{}", m.iter().next().is_some());
}
"#;
    let Some(out) = run_c("vec_values", src) else {
        return;
    };
    assert_eq!(out, "1\ntrue\n");
}
