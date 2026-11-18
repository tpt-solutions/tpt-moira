//! `Arc<T>` and `Rc<T>` are transparent on the C backend: a field typed `Arc<HashMap<..>>` takes
//! the map's methods, and `Arc::new(x)` is `x`. Skipped when no C compiler is available.

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
    let c_path = dir.join(format!("moira_shared_{name}.c"));
    let exe_path = dir.join(format!("moira_shared_{name}"));
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
fn arc_fields_take_collection_methods_and_run() {
    let src = r#"
use std::collections::HashMap;
use std::sync::Arc;
struct L { table: Arc<HashMap<String, i64>>, names: Arc<Vec<String>> }
impl L {
    fn look(&self, k: &str) -> i64 {
        match self.table.get(k) {
            Some(v) => *v,
            None => -1,
        }
    }
    fn count(&self) -> usize { self.names.len() }
}
fn main() {
    let mut m: HashMap<String, i64> = HashMap::new();
    m.insert(String::from("a"), 7);
    let l = L { table: Arc::new(m), names: Arc::new(vec![String::from("x")]) };
    println!("{} {} {}", l.look("a"), l.look("zz"), l.count());
}
"#;
    let Some(out) = run_c("arc", src) else {
        return;
    };
    assert_eq!(out, "7 -1 1\n");
}
