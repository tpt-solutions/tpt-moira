//! `std::mem::take` and `std::mem::replace` on the C backend, through pointers and on fields. Each program
//! is lowered with the checker's types and run; skipped when no C compiler is available.

use std::path::PathBuf;
use std::process::Command;

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

fn run_c(name: &str, src: &str) -> Option<String> {
    let cc = find_cc()?;
    let prog = tpt_moira_clotho::compile_source(src).expect("program should parse");
    let prog = tpt_moira_lachesis::mono::monomorphize(&prog, &[]);
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints).expect("program should lower");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let dir: PathBuf = std::env::temp_dir();
    let c_path = dir.join(format!("moira_fs_{name}.c"));
    let exe_path = dir.join(format!("moira_fs_{name}"));
    std::fs::write(&c_path, c).expect("write C");
    let status = Command::new(&cc)
        .arg("-O0")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .status()
        .expect("run cc");
    assert!(status.success(), "C compilation failed for {name}");
    let out = Command::new(&exe_path).output().expect("run program");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

#[test]
fn mem_take_through_a_pointer_and_mem_replace_on_a_field() {
    let src = "struct Box2 { items: Vec<i64>, tag: i64 }
        fn drain(sub: &mut Vec<i64>) -> Vec<i64> {
            std::mem::take(sub)
        }
        fn main() {
            let mut v: Vec<i64> = vec![1, 2, 3];
            let w = drain(&mut v);
            println!(\"{} {}\", v.len(), w.len());
            let mut b = Box2 { items: vec![4, 5], tag: 9 };
            let old = std::mem::replace(&mut b.tag, 11);
            println!(\"{} {}\", old, b.tag);
            let mut z: Vec<i64> = vec![7];
            let zz = std::mem::take(&mut z);
            println!(\"{} {}\", z.len(), zz[0]);
            println!(\"{}\", b.items.len());
        }";
    let out = match run_c("mem", src) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(
        out,
        "0 3
9 11
0 7
2
",
        "mem::take and mem::replace should match Rust"
    );
}
