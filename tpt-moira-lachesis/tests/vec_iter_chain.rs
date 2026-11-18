//! `chain` over sequences and `extend` on a struct field from an iterator chain on the C backend.
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
    let c_path = dir.join(format!("moira_vecchain_{name}.c"));
    let exe_path = dir.join(format!("moira_vecchain_{name}"));
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
fn chain_of_two_sequences_runs() {
    let src = r#"
fn main() {
    let v: Vec<i64> = vec![1, 2, 3];
    let w: Vec<i64> = vec![4, 5];
    let total: i64 = v.iter().chain(w.iter()).map(|x| *x).sum();
    println!("{}", total);
    println!("{}", v.iter().chain(w.iter()).count());
}
"#;
    let Some(out) = run_c("chain", src) else {
        return;
    };
    assert_eq!(out, "15\n5\n");
}

#[test]
fn extend_field_from_cloned_iterator_runs() {
    let src = r#"
struct S { items: Vec<i64> }
fn main() {
    let mut s = S { items: vec![1] };
    let other: Vec<i64> = vec![2, 3];
    s.items.push(4);
    s.items.extend(other.iter().cloned());
    println!("{}", s.items.len());
}
"#;
    let Some(out) = run_c("extend_field", src) else {
        return;
    };
    assert_eq!(out, "4\n");
}
