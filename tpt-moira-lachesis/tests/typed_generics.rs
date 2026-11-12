//! Monomorphization: generic functions are specialized per instantiation, then the
//! specialized program is checked, lowered, compiled and run.

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
    let c_path = dir.join(format!("moira_generic_{name}.c"));
    let exe_path = dir.join(format!("moira_generic_{name}"));
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
fn generic_function_runs_at_each_instantiation() {
    let src = "fn pick<T>(x: T, y: T) -> T {\n\
                   x\n\
               }\n\
               fn twice<T>(x: T) -> T {\n\
                   pick(x, x)\n\
               }\n\
               fn main() {\n\
                   let a: i64 = twice(40);\n\
                   println!(\"{}\", a + 2);\n\
                   println!(\"{}\", twice(true));\n\
               }\n";
    if let Some(out) = mono_and_run("two_instances", src) {
        assert_eq!(out, "42\ntrue\n", "unexpected output");
    }
}

#[test]
fn generic_function_with_one_instance_keeps_no_generic_original() {
    // Only the instance is emitted: the generic original has no C form of its own.
    let src = "fn id<T>(x: T) -> T {\n\
                   x\n\
               }\n\
               fn main() {\n\
                   println!(\"{}\", id(7));\n\
               }\n";
    let prog = tpt_moira_clotho::parse_source(src).expect("parse");
    let mono = tpt_moira_lachesis::mono::monomorphize(&prog, &[]);
    let names: Vec<String> = mono
        .items
        .iter()
        .filter_map(|i| match &i.kind {
            tpt_moira_clotho::ast::ItemKind::Function(f) => Some(f.name.node.clone()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"id__i32".to_string()), "{names:?}");
    assert!(!names.contains(&"id".to_string()), "{names:?}");
}
