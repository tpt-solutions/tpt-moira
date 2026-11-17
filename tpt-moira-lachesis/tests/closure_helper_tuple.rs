//! Generic helpers with closure arguments over a tuple state, and references to tuples, on the C backend.
//! Each program is lowered with the checker's types and run; skipped when no C compiler is available.

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
fn reference_to_tuple_is_a_pointer_to_the_tuple() {
    let src = "fn main() { let mut u: (i64, i64) = (0, 0); let r = &mut u; r.0 = 5; println!(\"{}\", u.0); }";
    let out = match run_c("ref_tuple", src) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "5
");
}

#[test]
fn generic_helper_with_two_closures_over_a_tuple_state() {
    let src = "struct Node { v: i64, kids: Vec<Node> }
        fn visit<S>(n: &mut Node, st: &mut S, fe: &mut impl FnMut(&mut Node, &mut S), fb: &mut impl FnMut(&mut Node, &mut S)) {
            fe(n, st);
            let mut i = 0;
            while i < n.kids.len() {
                visit(&mut n.kids[i], st, fe, fb);
                i += 1;
            }
            fb(n, st);
        }
        fn main() {
            let mut root = Node { v: 1, kids: Vec::new() };
            root.kids.push(Node { v: 2, kids: Vec::new() });
            root.kids.push(Node { v: 3, kids: Vec::new() });
            let mut u: (i64, i64) = (0, 0);
            visit(&mut root, &mut u, &mut |x: &mut Node, acc: &mut (i64, i64)| { acc.0 = acc.0 + x.v; }, &mut |x: &mut Node, acc: &mut (i64, i64)| { acc.1 = acc.1 + 1; });
            println!(\"{} {}\", u.0, u.1);
        }";
    let out = match run_c("closure_tuple", src) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "6 3
");
}
