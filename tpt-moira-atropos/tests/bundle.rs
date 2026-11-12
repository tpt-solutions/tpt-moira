//! Whole-program bundling: two crates whose items collide are lowered into one
//! MIR program and run as one C translation unit.

use std::path::PathBuf;
use std::process::Command;
use tpt_moira_atropos::{codegen_c, lower_bundle, TypeHints};

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

fn compile_and_run(name: &str, c: &str) -> Option<String> {
    let cc = find_cc()?;
    let dir: PathBuf = std::env::temp_dir();
    let c_path = dir.join(format!("moira_bundle_{name}.c"));
    let exe_path = dir.join(format!("moira_bundle_{name}"));
    std::fs::write(&c_path, c).unwrap();
    let status = Command::new(&cc)
        .arg("-O0")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .status()
        .expect("run cc");
    assert!(status.success(), "C compilation failed for {name}:\n{c}");
    let out = Command::new(&exe_path).output().expect("run program");
    assert!(out.status.success(), "program {name} failed");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

#[test]
fn colliding_struct_names_and_cross_crate_calls_run() {
    let root_src = r#"
struct Point {
    y: i32,
}

fn main() {
    let p = Point { y: 5 };
    println!("{}", p.y + helper(41));
}
"#;
    let lib_src = r#"
struct Point {
    x: i32,
}

fn helper(v: i32) -> i32 {
    let p = Point { x: v };
    p.x + 1
}
"#;
    let root = tpt_moira_clotho::parse_source(root_src).expect("root parses");
    let lib = tpt_moira_clotho::parse_source(lib_src).expect("lib parses");
    let externs = vec![("lib".to_string(), lib, TypeHints::new())];
    let mir = lower_bundle(&root, &TypeHints::new(), &externs).expect("bundle lowers");

    let names: Vec<&str> = mir.functions.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.contains(&"main"),
        "root main keeps its name: {names:?}"
    );
    assert!(
        names.contains(&"lib__helper"),
        "extern fn is prefixed: {names:?}"
    );
    let structs: Vec<&str> = mir.structs.iter().map(|s| s.name.as_str()).collect();
    assert!(
        structs.contains(&"Point"),
        "root struct keeps its name: {structs:?}"
    );
    assert!(
        structs.contains(&"lib__Point"),
        "extern struct is prefixed: {structs:?}"
    );

    let c = codegen_c::emit_c(&mir);
    let Some(out) = compile_and_run("collide", &c) else {
        return;
    };
    // root: 5 + helper(41) = 5 + (41 + 1)
    assert_eq!(out.trim(), "47", "output: {out:?}\n{c}");
}
