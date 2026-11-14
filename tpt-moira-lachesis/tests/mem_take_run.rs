//! Runtime checks for `std::mem::take(&mut place)` on vector places.
//!
//! These lower with the checker's type hints (the path the driver takes), so the
//! place's type is known. Unlike the skip-on-failure helper in the other run tests,
//! a C compile or run failure here is a test failure.

use std::process::Command;

/// The first C compiler on the PATH, if any.
fn find_cc() -> Option<&'static str> {
    ["gcc", "clang", "cc"]
        .into_iter()
        .find(|cc| Command::new(cc).arg("--version").output().is_ok())
}

/// Type-check and lower `src` with hints, compile the C, run it, and return stdout.
/// `None` only when no C compiler is installed.
fn run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::compile_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "{name}: program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_mem_take_{name}.c"));
    let exe_path = dir.join(format!("moira_mem_take_{name}"));
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
    assert!(out.status.success(), "{name}: program should exit cleanly");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

#[test]
fn mem_take_moves_a_field_out_and_leaves_it_empty() {
    // The caller's copy keeps the items; the field the `&mut` points at is left empty.
    let src = "struct Bag { items: Vec<i64> }
fn drain(b: &mut Bag) -> Vec<i64> {
    std::mem::take(&mut b.items)
}
fn main() {
    let mut b = Bag { items: Vec::new() };
    b.items.push(3);
    b.items.push(4);
    let taken = drain(&mut b);
    println!(\"{}\", taken.len());
    println!(\"{}\", b.items.len());
}
";
    let Some(out) = run("field", src) else { return };
    assert_eq!(out, "2\n0\n", "output: {out:?}");
}

#[test]
fn mem_take_on_a_local_vec_empties_the_local() {
    let src = "fn main() {
    let mut v: Vec<i64> = Vec::new();
    v.push(7);
    let old = std::mem::take(&mut v);
    println!(\"{} {}\", old.len(), v.len());
}
";
    let Some(out) = run("local", src) else { return };
    assert_eq!(out, "1 0\n", "output: {out:?}");
}
