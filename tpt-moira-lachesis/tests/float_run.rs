//! Runtime checks for floating-point values: literals, arithmetic, and `Display`.
//! Expected output is what `rustc` prints for the same program.

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
    let c_path = dir.join(format!("moira_float_{name}.c"));
    let exe_path = dir.join(format!("moira_float_{name}"));
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
fn floats_print_like_rustc() {
    // Literals, arithmetic, negation, and `Display` (shortest round-trip, no exponent)
    // must match rustc exactly. Before floats were lowered, each of these printed as an
    // integer.
    let src = r#"fn half(x: f64) -> f64 {
    x / 2.0
}
fn main() {
    let a = 2.5f64;
    println!("{}", a);
    println!("{}", half(a) * 3.0);
    println!("{}", 0.1f64 + 0.2f64);
    println!("{}", -1.25f64);
    let s = format!("{} {}", 1e21f64, 0.0000001f64);
    println!("{}", s);
    let third = 1.0f64 / 3.0f64;
    println!("{}", third.to_string());
    println!("{}", 2.0f64);
}
"#;
    let Some(out) = run("floats", src) else {
        return;
    };
    assert_eq!(
        out,
        "2.5\n3.75\n0.30000000000000004\n-1.25\n1000000000000000000000 0.0000001\n0.3333333333333333\n2\n"
    );
}

#[test]
fn formatting_a_struct_is_a_lowering_error_not_a_wrong_value() {
    // `format!` of a struct has no `Display` lowering; it must say so rather than format
    // the struct's pointer as an integer.
    let src = "struct P { x: i64 }\nfn main() { let p = P { x: 3 }; let s = format!(\"{}\", p); println!(\"{}\", s); }\n";
    let prog = tpt_moira_clotho::compile_source(src).expect("program should parse");
    let (_res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    let err = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .err()
        .expect("lowering should reject a struct in format!");
    assert!(err.0.contains("no lowering yet"), "error: {}", err.0);
}
