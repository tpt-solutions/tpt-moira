//! Generic associated constructors called inside `vec![..]` (the macro's tokens are re-parsed, so the
//! monomorphiser must rename their owner too). Each program
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
    // The driver reads the program from a file (`compile_file_mapped`); qualified paths such as
    // `ast::W::new` resolve the same way here.
    let file = std::env::temp_dir().join(format!("moira_vec_owner_{name}.rs"));
    std::fs::write(&file, src).expect("write program");
    let mut base = 0usize;
    let mut files = Vec::new();
    let prog = tpt_moira_clotho::compile_file_mapped(&file, &mut base, &mut files)
        .expect("program should parse");
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
fn generic_constructor_inside_vec_macro() {
    let src = "mod ast {
            #[derive(Clone)]
            pub struct W<T> { pub node: T, pub k: i64 }
            impl<T> W<T> { pub fn new(node: T, k: i64) -> Self { W { node, k } } }
        }
        fn main() {
            let v = vec![ast::W::new(String::from(\"x\"), 3)];
            println!(\"{} {}\", v[0].node, v[0].k);
            let w = vec![ast::W::new(5, 3)];
            println!(\"{} {}\", w[0].node, w[0].k);
        }";
    let out = match run_c("vec_owner", src) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "x 3
5 3
", "a generic constructor inside vec! should be specialised");
}
