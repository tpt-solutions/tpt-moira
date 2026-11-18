//! `extend_from_slice`, `unwrap_or_default` on a vector payload, and `PathBuf::from` on the C backend.
//! `extend_from_slice`, `unwrap_or_default` on a vector payload, and `PathBuf::from` on the C backend.
//! `extend_from_slice`, `unwrap_or_default` on a vector payload, and `PathBuf::from` on the C backend.

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
    let c_path = dir.join(format!("moira_misc_{name}.c"));
    let exe_path = dir.join(format!("moira_misc_{name}"));
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
fn extend_from_slice_default_and_pathbuf_run() {
    let src = r#"
use std::collections::HashMap;
use std::path::PathBuf;
fn main() {
    let g = vec![1i64, 2, 3];
    let mut n: Vec<i64> = vec![0];
    n.extend_from_slice(&g[1..]);
    println!("{}", n.len());
    let mut env: HashMap<String, Vec<i64>> = HashMap::new();
    env.insert(String::from("k"), vec![1, 2]);
    let t: Vec<i64> = env.get(&String::from("k")).cloned().unwrap_or_default();
    println!("{}", t.len());
    let p = PathBuf::from("a/b");
    println!("{}", p.len());
}
"#;
    let Some(out) = run_c("misc", src) else {
        return;
    };
    assert_eq!(out, "3\n2\n3\n");
}
