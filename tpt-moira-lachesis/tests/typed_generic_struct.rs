//! Monomorphization of generic structs: each instantiation gets its own struct and impl
//! block, and the program runs with the instances it uses.

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
    let c_path = dir.join(format!("moira_genstruct_{name}.c"));
    let exe_path = dir.join(format!("moira_genstruct_{name}"));
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
fn generic_struct_runs_at_two_instantiations() {
    let src = "struct Pair<T> { first: T, count: i64 }\n\
               impl<T> Pair<T> {\n\
                   fn new(first: T) -> Pair<T> {\n\
                       Pair { first: first, count: 1 }\n\
                   }\n\
                   fn bump(&mut self) {\n\
                       self.count = self.count + 1;\n\
                   }\n\
               }\n\
               fn main() {\n\
                   let mut p: Pair<i64> = Pair::new(40);\n\
                   p.bump();\n\
                   println!(\"{}\", p.first + p.count);\n\
                   let mut q: Pair<bool> = Pair::new(true);\n\
                   q.bump();\n\
                   println!(\"{}\", q.count);\n\
               }\n";
    if let Some(out) = mono_and_run("pair", src) {
        assert_eq!(out, "42\n2\n", "unexpected output");
    }
}

#[test]
fn generic_struct_original_is_not_emitted() {
    let src = "struct Pair<T> { first: T }\n\
               fn main() {\n\
                   let p: Pair<i64> = Pair { first: 3 };\n\
                   println!(\"{}\", p.first);\n\
               }\n";
    let prog = tpt_moira_clotho::parse_source(src).expect("parse");
    let mono = tpt_moira_lachesis::mono::monomorphize(&prog, &[]);
    let structs: Vec<String> = mono
        .items
        .iter()
        .filter_map(|i| match &i.kind {
            tpt_moira_clotho::ast::ItemKind::Struct(s) => Some(s.name.node.clone()),
            _ => None,
        })
        .collect();
    assert!(structs.contains(&"Pair__i64".to_string()), "{structs:?}");
    // The generic original stays for the checker, but the backend emits only the instance.
    let stripped = tpt_moira_atropos::codegen_c::emit_c(
        &tpt_moira_atropos::lower_program_typed(&mono, &Default::default())
            .expect("instance program should lower"),
    );
    assert!(!stripped.contains("struct Pair {"), "generic original was emitted");
    assert!(stripped.contains("struct Pair__i64"), "instance missing");
}
