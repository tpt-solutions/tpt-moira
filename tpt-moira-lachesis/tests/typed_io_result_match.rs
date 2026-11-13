//! Typed lowering: a one-argument `Result<T>` (`io::Result<T>`) is a `Result` with a
//! string error, and a `match` whose arm is a bare `None` takes the type of its other arm.
//! The emitted C must compile.

use std::process::Command;

const SRC: &str = "struct Q { a: i64 }\n\
    fn open(p: &str) -> std::io::Result<Q> { Ok(Q { a: 1 }) }\n\
    fn g(p: &str) -> i64 {\n\
        let mut queue = match open(p) {\n\
            Ok(q) => Some(q),\n\
            Err(e) => None,\n\
        };\n\
        match &mut queue { Some(q) => q.a, None => 0 }\n\
    }\n\
    fn main() {}\n";

#[test]
fn io_result_and_bare_none_arm_emit_compilable_c() {
    let prog = tpt_moira_clotho::parse_source(SRC).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .expect("program should lower with hints");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    assert!(
        !c.contains("struct Option t"),
        "match temp should be typed:\n{c}"
    );
    let cc = ["gcc", "clang", "cc"]
        .into_iter()
        .find(|cc| Command::new(cc).arg("--version").output().is_ok());
    let Some(cc) = cc else { return };
    let path = std::env::temp_dir().join("moira_io_result_match.c");
    std::fs::write(&path, &c).expect("write C");
    let out = Command::new(cc)
        .arg("-w")
        .arg("-fsyntax-only")
        .arg(&path)
        .output()
        .expect("run cc");
    assert!(
        out.status.success(),
        "emitted C should compile: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
