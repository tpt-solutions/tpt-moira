//! Runtime checks of typed lowering: each program is type-checked, lowered with the
//! checker's hints, compiled with the system C compiler, and run. The tests are
//! skipped when no C compiler is available.

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
/// Returns stdout (line endings normalized), or `None` when no C compiler is available.
fn check_and_run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .expect("program should lower with hints");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_runtime_{name}.c"));
    let exe_path = dir.join(format!("moira_runtime_{name}"));
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
fn option_map_keeps_some_and_none() {
    let src = "fn main() {\n\
                   let a: Option<i64> = Some(5);\n\
                   let b: Option<i64> = None;\n\
                   let x = a.map(|v| v + 1);\n\
                   let y = b.map(|v| v + 1);\n\
                   match x {\n\
                       Some(v) => println!(\"{}\", v),\n\
                       None => println!(\"none\"),\n\
                   }\n\
                   match y {\n\
                       Some(v) => println!(\"{}\", v),\n\
                       None => println!(\"none\"),\n\
                   }\n\
               }\n";
    if let Some(out) = check_and_run("option_map", src) {
        assert_eq!(out, "6\nnone\n", "unexpected output");
    }
}

#[test]
fn seq_get_points_into_the_sequence() {
    // `get` yields a pointer to the element (no copy), and `None` past the end.
    let src = "struct Tok { kind: i64 }\n\
               fn kind_at(toks: &Vec<Tok>, i: usize) -> i64 {\n\
                   match toks.get(i) {\n\
                       Some(t) => t.kind,\n\
                       None => -1,\n\
                   }\n\
               }\n\
               fn main() {\n\
                   let mut v: Vec<Tok> = Vec::new();\n\
                   v.push(Tok { kind: 7 });\n\
                   v.push(Tok { kind: 9 });\n\
                   println!(\"{}\", kind_at(&v, 1));\n\
                   println!(\"{}\", kind_at(&v, 5));\n\
                   let k = v.get(0).map(|t| t.kind);\n\
                   match k {\n\
                       Some(x) => println!(\"{}\", x),\n\
                       None => println!(\"none\"),\n\
                   }\n\
               }\n";
    if let Some(out) = check_and_run("seq_get", src) {
        assert_eq!(out, "9\n-1\n7\n", "unexpected output");
    }
}

#[test]
fn lines_skips_terminators_like_rust() {
    // Rust's `lines()`: `\n` and `\r\n` end a line; a trailing newline adds no empty line.
    let src = "fn main() {\n\
                   let text: String = String::from(\"alpha\\r\\n# skip\\n\\nbeta\\n\");\n\
                   let mut n: i64 = 0;\n\
                   for line in text.lines() {\n\
                       if line.starts_with(\"#\") {\n\
                           continue;\n\
                       }\n\
                       println!(\"{}\", line);\n\
                       n = n + 1;\n\
                   }\n\
                   println!(\"{}\", n);\n\
               }\n";
    if let Some(out) = check_and_run("lines", src) {
        assert_eq!(out, "alpha\n\nbeta\n3\n", "unexpected output");
    }
}
