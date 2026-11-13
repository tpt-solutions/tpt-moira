//! References to scalars: a parameter, a `let` bound from it, and a closure parameter
//! typed `&i64` all hold a pointer to the value and dereference it.

use std::process::Command;

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

fn check_and_run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let prog = tpt_moira_lachesis::mono::monomorphize(&prog, &[]);
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("program should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_sref_{name}.c"));
    let exe_path = dir.join(format!("moira_sref_{name}"));
    std::fs::write(&c_path, &c).expect("write C");
    let build = Command::new(&cc)
        .arg("-w")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .output()
        .expect("run C compiler");
    assert!(
        build.status.success(),
        "C compile failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = Command::new(&exe_path).output().expect("run binary");
    assert!(run.status.success(), "binary failed");
    Some(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"))
}

#[test]
fn let_bound_from_a_reference_parameter_dereferences() {
    let src = "fn show(p: &i64) -> i64 {\n\
                   let q = p;\n\
                   *q + 1\n\
               }\n\
               fn main() {\n\
                   let x: i64 = 41;\n\
                   println!(\"{}\", show(&x));\n\
               }\n";
    if let Some(out) = check_and_run("let_ref", src) {
        assert_eq!(out, "42\n", "unexpected output");
    }
}

#[test]
fn closure_parameter_typed_as_a_reference_dereferences() {
    let src = "fn call_it<F: Fn(&i64) -> i64>(g: F, v: &i64) -> i64 {\n\
                   g(v) * 2\n\
               }\n\
               fn main() {\n\
                   let x: i64 = 4;\n\
                   let r = call_it(|p: &i64| *p + 1, &x);\n\
                   println!(\"{}\", r);\n\
               }\n";
    if let Some(out) = check_and_run("closure_ref", src) {
        assert_eq!(out, "10\n", "unexpected output");
    }
}
