//! Runtime check: a derived `Clone` of an enum with a `Box` payload boxes a fresh copy.
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
    assert!(
        res.is_ok(),
        "{name}: program should type-check: {:?}",
        res.err()
    );
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_box_clone_{name}.c"));
    let exe_path = dir.join(format!("moira_box_clone_{name}"));
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
fn derived_clone_of_boxed_payload_allocates_a_new_box() {
    // The clone's payload is readable through its own box (a `match` on `*inner`).
    let src = "#[derive(Clone)]
enum E {
    A(Box<E>),
    B(i64),
}
fn main() {
    let w = E::A(Box::new(E::B(7)));
    let c = w.clone();
    match c {
        E::A(inner) => match *inner {
            E::B(v) => println!(\"{}\", v),
            _ => println!(\"other\"),
        },
        E::B(_) => println!(\"b\"),
    }
}
";
    let Some(out) = run("derive_box", src) else {
        return;
    };
    assert_eq!(
        out,
        "7
",
        "output: {out:?}"
    );
}
