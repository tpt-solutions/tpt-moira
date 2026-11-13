//! Monomorphization of methods with their own type parameters: each call instantiates the
//! method for its argument types, in the impl block of the receiver's type.

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
    assert!(
        res.is_ok(),
        "specialized program should type-check: {:?}",
        res.err()
    );
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .expect("specialized program should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_genmethod_{name}.c"));
    let exe_path = dir.join(format!("moira_genmethod_{name}"));
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
fn generic_method_runs_at_each_argument_type() {
    let src = "struct Counter {\n\
                   base: i64,\n\
               }\n\
               impl Counter {\n\
                   fn add<T>(&self, x: T, extra: i64) -> i64 {\n\
                       self.base + extra\n\
                   }\n\
                   fn tag<U>(&self, u: U) -> i64 {\n\
                       self.base * 10\n\
                   }\n\
               }\n\
               fn main() {\n\
                   let c = Counter { base: 4 };\n\
                   println!(\"{}\", c.add(true, 5));\n\
                   println!(\"{}\", c.add(7, 1));\n\
                   println!(\"{}\", c.tag(\"x\"));\n\
               }\n";
    if let Some(out) = mono_and_run("counter", src) {
        assert_eq!(out, "9\n5\n40\n", "unexpected output");
    }
}
