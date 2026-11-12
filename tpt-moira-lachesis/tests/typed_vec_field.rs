//! Typed lowering of vectors reached through a struct field: `field.extend(x.drain(..))`
//! moves the items and empties `x`, and `for v in field.clone()` iterates a snapshot,
//! so pushing to `field` inside the loop does not change the items visited. The run
//! test compiles the emitted C with the system C compiler and is skipped when none
//! is available.

use std::process::Command;

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

/// Type-check `src`, lower it with the checker's hints, compile and run it.
/// Returns stdout, or `None` when no C compiler is available.
fn check_and_run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .expect("program should lower with hints");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_vecfield_{name}.c"));
    let exe_path = dir.join(format!("moira_vecfield_{name}"));
    std::fs::write(&c_path, &c).expect("write C");
    let build = Command::new(&cc)
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
    Some(String::from_utf8_lossy(&run.stdout).into_owned())
}

#[test]
fn field_extend_from_drain_and_for_over_clone() {
    let src = "struct Bag { items: Vec<i64> }\n\
               fn main() {\n\
                   let mut b = Bag { items: Vec::new() };\n\
                   let mut src: Vec<i64> = Vec::new();\n\
                   src.push(1);\n\
                   src.push(2);\n\
                   b.items.extend(src.drain(..));\n\
                   println!(\"{}\", src.len());\n\
                   for x in b.items.clone() {\n\
                       b.items.push(x * 10);\n\
                   }\n\
                   println!(\"{}\", b.items.len());\n\
               }\n";
    if let Some(out) = check_and_run("extend_clone", src) {
        // Windows' C runtime writes CRLF; compare line by line.
        let out = out.replace("\r\n", "\n");
        assert_eq!(out, "0\n4\n", "unexpected output");
    }
}
