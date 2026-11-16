//! End-to-end tests for the native backend with struct values, lowered with the checker's type
//! hints (method calls on struct locals need them). Skipped when no C toolchain is available.

use std::path::PathBuf;
use std::process::Command;

fn toolchain_available() -> bool {
    ["gcc", "clang", "cc"]
        .iter()
        .any(|cc| Command::new(cc).arg("--version").output().is_ok())
}

fn build_and_run(name: &str, src: &str) -> Option<String> {
    if !toolchain_available() {
        eprintln!("no C toolchain; skipping native test {name}");
        return None;
    }
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let hints = tpt_moira_lachesis::expr_type_hints(&prog, &[]);
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("program should lower");
    let ssa = tpt_moira_atropos::ssa::SsaProgram::from_mir(&mir).expect("program should convert to SSA");
    let exe: PathBuf = std::env::temp_dir().join(format!("moira_native_{name}.exe"));
    tpt_moira_atropos::codegen_native::compile_to_native(&ssa, &exe).expect("native build");
    let out = Command::new(&exe).output().expect("run native program");
    assert!(out.status.success(), "native program {name} exited with failure");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

#[test]
fn native_structs_copy_mutate_and_return_run() {
    let out = match build_and_run(
        "structs",
        "#[derive(Clone, Copy)]
        struct P { x: i64, y: i64 }
        fn bump(mut p: P) -> i64 { p.x = p.x + 100; p.x }
        fn make(x: i64, y: i64) -> P { P { x: x, y: y } }
        impl P {
            fn sum(&self) -> i64 { self.x + self.y }
            fn scale(&mut self, k: i64) { self.x = self.x * k; self.y = self.y * k; }
        }
        fn main() {
            let a = P { x: 1, y: 2 };
            let mut b = a;
            b.x = 50;
            print_i64(a.x);
            print_i64(b.x);
            print_i64(bump(a));
            print_i64(a.x);
            let mut c = make(3, 4);
            print_i64(c.sum());
            c.scale(10);
            print_i64(c.sum());
        }",
    ) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "1\n50\n101\n1\n7\n70\n", "struct values should copy, mutate and return as in Rust");
}
