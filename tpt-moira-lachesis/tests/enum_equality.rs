//! `==` on an enum with a derived `PartialEq` calls its `eq` (the C backend cannot compare two
//! struct values). Each test lowers a program with the checker's types, emits C, compiles and
//! runs it. Skipped when no C compiler is available.

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
    // As the driver does: derives are expanded, generics specialised, and the checker's types given.
    let prog = tpt_moira_clotho::compile_source(src).expect("program should parse");
    let prog = tpt_moira_lachesis::mono::monomorphize(&prog, &[]);
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("program should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let dir: PathBuf = std::env::temp_dir();
    let c_path = dir.join(format!("moira_enum_eq_{name}.c"));
    let exe_path = dir.join(format!("moira_enum_eq_{name}"));
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
fn derived_enum_equality_calls_eq_run() {
    let out = match run_c(
        "enum_eq",
        "#[derive(Clone, Copy, PartialEq)]
        enum Kind { A, B }
        fn main() {
            let k = Kind::B;
            print_i64((k == Kind::B) as i64);
            print_i64((k == Kind::A) as i64);
            print_i64((k != Kind::A) as i64);
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "1\n0\n1\n", "derived enum equality should match Rust");
}
