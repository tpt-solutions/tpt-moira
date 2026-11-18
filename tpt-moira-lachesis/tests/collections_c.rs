//! Collection shapes the self-host sources use on the C backend: set values under `entry().or_default()`, `BTreeMap` iteration in key order, `join` on a slice, and `sort_by_key`.
//! Collection shapes the self-host sources use on the C backend: set values under `entry().or_default()`, `BTreeMap` iteration in key order, `join` on a slice, and `sort_by_key`.
//! Collection shapes the self-host sources use on the C backend: set values under `entry().or_default()`, `BTreeMap` iteration in key order, `join` on a slice, and `sort_by_key`.

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
    let c_path = dir.join(format!("moira_colc_{name}.c"));
    let exe_path = dir.join(format!("moira_colc_{name}"));
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
fn collection_shapes_run() {
    let src = r#"
use std::collections::{BTreeMap, HashMap, HashSet};
fn main() {
    let mut aliases: HashMap<String, HashSet<String>> = HashMap::new();
    aliases.entry(String::from("v")).or_default().insert(String::from("m"));
    aliases.entry(String::from("v")).or_default().insert(String::from("m"));
    aliases.entry(String::from("w")).or_default().insert(String::from("q"));
    println!("{} {}", aliases.len(), aliases.get(&String::from("v")).unwrap().len());
    let mut owners: BTreeMap<String, usize> = BTreeMap::new();
    owners.insert(String::from("zz"), 1);
    owners.insert(String::from("aa"), 2);
    match owners.iter().next() {
        Some((k, v)) => println!("{} {}", k, v),
        None => println!("none"),
    }
    let pieces = vec!["a", "b", "c"];
    println!("{}", pieces[0..2].join("_"));
    let a = String::from("Vec<x>");
    let b = String::from("ab");
    let mut ordered: Vec<&String> = vec![&a, &b];
    ordered.sort_by_key(|e| e.split("Vec<").count() - 1);
    println!("{}", ordered[0]);
}
"#;
    let Some(out) = run_c("collections", src) else {
        return;
    };
    assert_eq!(out, "2 1\naa 2\na_b\nab\n");
}
