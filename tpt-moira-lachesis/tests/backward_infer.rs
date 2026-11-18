//! Element and key types fixed by a later use of an untyped `Vec::new()` / `HashSet::new()`, on the C backend.
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
    let c_path = dir.join(format!("moira_backinfer_{name}.c"));
    let exe_path = dir.join(format!("moira_backinfer_{name}"));
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
fn vec_fixed_by_annotated_let_runs() {
    let src = r#"
fn main() {
    let v: Vec<i64> = Vec::new();
    let w = Vec::new();
    let x: Vec<i64> = w;
    println!("{} {}", v.len(), x.len());
}
"#;
    let Some(out) = run_c("vec_let", src) else {
        return;
    };
    assert_eq!(out, "0 0\n");
}

#[test]
fn hashset_fixed_by_mut_argument_runs() {
    let src = r#"
use std::collections::HashSet;
fn fill(s: &mut HashSet<String>, k: String) {
    s.insert(k);
}
fn main() {
    let mut set = HashSet::new();
    fill(&mut set, String::from("a"));
    println!("{}", set.len());
}
"#;
    let Some(out) = run_c("set_arg", src) else {
        return;
    };
    assert_eq!(out, "1\n");
}
