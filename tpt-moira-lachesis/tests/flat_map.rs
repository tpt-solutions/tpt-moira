//! `flat_map` over a vector of structs on the C backend: collected items, a capture inside `format!`, and a sum.
//! `flat_map` over a vector of structs on the C backend: collected items, a capture inside `format!`, and a sum.
//! `flat_map` over a vector of structs on the C backend: collected items, a capture inside `format!`, and a sum.

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
    let c_path = dir.join(format!("moira_flatmap_{name}.c"));
    let exe_path = dir.join(format!("moira_flatmap_{name}"));
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
fn flat_map_collects_captures_and_sums() {
    let src = r#"
struct U { items: Vec<i64>, name: String }
fn main() {
    let us = vec![U { items: vec![1, 2], name: String::from("a") }, U { items: vec![3], name: String::from("b") }];
    let all: Vec<i64> = us.iter().flat_map(|u| u.items.iter().map(|x| *x)).collect();
    println!("{}", all.len());
    let names: Vec<String> = us.iter().flat_map(|u| u.items.iter().map(|x| format!("{}{}", u.name, x))).collect();
    println!("{}", names.len());
    let total: i64 = us.iter().flat_map(|u| u.items.iter().map(|x| *x * 10)).sum();
    println!("{}", total);
}
"#;
    let Some(out) = run_c("flatmap", src) else {
        return;
    };
    assert_eq!(out, "3\n3\n60\n");
}
