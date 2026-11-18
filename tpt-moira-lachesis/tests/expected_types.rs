//! Expected types pushed into `Ok`/`Some` payloads, tuple elements and returned tuples on the C backend.
//! Expected types pushed into `Ok`/`Some` payloads, tuple elements and returned tuples on the C backend.
//! Expected types pushed into `Ok`/`Some` payloads, tuple elements and returned tuples on the C backend.

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
    let c_path = dir.join(format!("moira_exptype_{name}.c"));
    let exe_path = dir.join(format!("moira_exptype_{name}"));
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
fn expected_types_reach_payloads_tuples_and_unwraps() {
    let src = r#"
fn f(n: usize) -> Result<(Option<i64>, Vec<i64>), String> {
    if n == 0 {
        return Ok((None, Vec::new()));
    }
    Ok((Some(1), vec![2]))
}
fn g(n: usize) -> Result<(i64, i64), String> {
    Ok((n as i64, 2))
}
fn main() {
    let r = f(0).unwrap();
    println!("{} {}", r.0.is_none(), r.1.len());
    let q = f(1).unwrap();
    println!("{}", q.0.is_some());
    let t = g(3).unwrap();
    println!("{}", t.0);
    let u = usize::from(true) + 1;
    println!("{}", u);
}
"#;
    let Some(out) = run_c("exptype", src) else {
        return;
    };
    assert_eq!(out, "true 0\ntrue\n3\n2\n");
}
