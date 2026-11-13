//! A refutable `let` (`let Some(d) = o`, the `else` branch dropped by the parser) binds the
//! payload's names, and a `for` over a `zip` of two sequences runs in step.

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
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("program should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_letvar_{name}.c"));
    let exe_path = dir.join(format!("moira_letvar_{name}"));
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
fn let_else_binds_the_payload_and_mutates_through_a_loop() {
    let src = "struct Field {\n\
                   ty: i64,\n\
               }\n\
               struct Def {\n\
                   fields: Vec<Field>,\n\
               }\n\
               fn grab(o: Option<Def>) -> i64 {\n\
                   let Some(d) = o else {\n\
                       return 0;\n\
                   };\n\
                   let mut s = d;\n\
                   let mut t: i64 = 0;\n\
                   for f in &mut s.fields {\n\
                       f.ty = f.ty + 1;\n\
                       t = t + f.ty;\n\
                   }\n\
                   t\n\
               }\n\
               fn main() {\n\
                   let o = Some(Def { fields: vec![Field { ty: 1 }, Field { ty: 2 }] });\n\
                   println!(\"{}\", grab(o));\n\
               }\n";
    if let Some(out) = check_and_run("let_else", src) {
        assert_eq!(out, "5\n", "unexpected output");
    }
}

#[test]
fn zip_runs_in_step_over_two_vectors() {
    let src = "fn pairs(args: &Vec<i64>, other: Vec<i64>) -> i64 {\n\
                   let mut t: i64 = 0;\n\
                   for (a, b) in args.iter().zip(other.iter()) {\n\
                       t = t + a * b;\n\
                   }\n\
                   t\n\
               }\n\
               fn main() {\n\
                   let v: Vec<i64> = vec![1, 2, 3];\n\
                   let w: Vec<i64> = vec![4, 5];\n\
                   println!(\"{}\", pairs(&v, w));\n\
               }\n";
    if let Some(out) = check_and_run("zip", src) {
        assert_eq!(out, "14\n", "unexpected output");
    }
}
