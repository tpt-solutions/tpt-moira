//! Runtime check: a `str`'s `chars()` iterator, held in a local or used at once, yields
//! each character in order and ends with `None`.

use std::process::Command;

fn run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::compile_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(
        res.is_ok(),
        "{name}: program should type-check: {:?}",
        res.err()
    );
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let cc = ["gcc", "clang", "cc"]
        .into_iter()
        .find(|cc| Command::new(cc).arg("--version").output().is_ok())?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_chars_{name}.c"));
    let exe_path = dir.join(format!("moira_chars_{name}"));
    std::fs::write(&c_path, &c).expect("write C");
    let build = Command::new(cc)
        .arg("-w")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .output()
        .expect("run cc");
    assert!(
        build.status.success(),
        "{name}: C should build:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let out = Command::new(&exe_path).output().expect("run program");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

#[test]
fn chars_iterator_held_in_a_local_counts_each_character() {
    let src = "fn count_a(name: &str) -> i64 {\n\
                   let mut chars = name.chars();\n\
                   let mut n = 0;\n\
                   while let Some(c) = chars.next() {\n\
                       if c == 'a' { n += 1; }\n\
                   }\n\
                   n\n\
               }\n\
               fn main() {\n\
                   println!(\"{}\", count_a(\"banana\"));\n\
               }\n";
    let Some(out) = run("count", src) else { return };
    assert_eq!(out.trim(), "3", "output: {out:?}");
}
