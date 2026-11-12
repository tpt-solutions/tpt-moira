//! A capturing closure passed to a generic function or method: its captured variables are
//! passed to a specialized instance, and its body runs there with them.

use std::process::Command;

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

fn mono_and_run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let prog = tpt_moira_lachesis::mono::monomorphize(&prog, &[]);
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "specialized program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .expect("specialized program should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_capture_{name}.c"));
    let exe_path = dir.join(format!("moira_capture_{name}"));
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
fn captures_reach_a_generic_function_and_a_generic_method() {
    let src = "struct Runner {\n\
                   base: i64,\n\
               }\n\
               impl Runner {\n\
                   fn run<F>(&self, f: F) -> i64\n\
                   where\n\
                       F: FnOnce() -> i64,\n\
                   {\n\
                       f() + self.base\n\
                   }\n\
               }\n\
               fn apply<F: Fn(i64) -> i64>(g: F, x: i64) -> i64 {\n\
                   g(x) * 2\n\
               }\n\
               fn main() {\n\
                   let k: i64 = 10;\n\
                   let m: i64 = 3;\n\
                   let a = apply(|v| v + k, 5);\n\
                   println!(\"{}\", a);\n\
                   let r = Runner { base: 100 };\n\
                   let b = r.run(|| k * m);\n\
                   println!(\"{}\", b);\n\
               }\n";
    if let Some(out) = mono_and_run("cap", src) {
        assert_eq!(out, "30\n130\n", "unexpected output");
    }
}
