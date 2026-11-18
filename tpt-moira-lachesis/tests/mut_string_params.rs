//! `&mut String` parameters on the C backend: a callee's writes reach the caller's string, also through nested calls and struct fields.
//! `&mut String` parameters on the C backend: a callee's writes reach the caller's string, also through nested calls and struct fields.
//! `&mut String` parameters on the C backend: a callee's writes reach the caller's string, also through nested calls and struct fields.

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
    let c_path = dir.join(format!("moira_mutstr_{name}.c"));
    let exe_path = dir.join(format!("moira_mutstr_{name}"));
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
fn mut_string_parameters_write_back_to_the_caller() {
    let src = r#"
fn add_many(out: &mut String, n: usize) {
    for i in 0..n {
        out.push_str("ab");
        if i == 0 { out.push('-'); }
    }
    let l = out.len();
    out.push_str(&l.to_string());
}
fn reset(s: &mut String) -> usize {
    s.clear();
    s.push_str("fresh");
    s.len()
}
fn outer(s: &mut String) {
    add_many(s, 2);
    reset(s);
    s.push('!');
}
fn add_bang(s: &mut String) {
    s.push_str("!!");
}
struct W { out: String, n: i64 }
impl W {
    fn emit(&mut self, text: &str) {
        add_bang(&mut self.out);
        self.out.push_str(text);
    }
}
fn main() {
    let mut buf = String::from("x");
    add_many(&mut buf, 3);
    println!("{}", buf);
    let n = reset(&mut buf);
    println!("{} {}", n, buf);
    let mut z = String::new();
    outer(&mut z);
    println!("[{}]", z);
    let mut w = W { out: String::from("a"), n: 1 };
    w.emit("b");
    println!("{} {}", w.out, w.n);
}
"#;
    let Some(out) = run_c("mutstr", src) else {
        return;
    };
    assert_eq!(out, "xab-abab8\n5 fresh\n[fresh!]\na!!b 1\n");
}
