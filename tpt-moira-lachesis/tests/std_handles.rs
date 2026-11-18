//! std handles on the C backend: `File::create` with `write_all`, `fs::read`, `create_dir_all`, and `DefaultHasher` with `hash` and `finish`.
//! std handles on the C backend: `File::create` with `write_all`, `fs::read`, `create_dir_all`, and `DefaultHasher` with `hash` and `finish`.
//! std handles on the C backend: `File::create` with `write_all`, `fs::read`, `create_dir_all`, and `DefaultHasher` with `hash` and `finish`.

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
    let c_path = dir.join(format!("moira_stdh_{name}.c"));
    let exe_path = dir.join(format!("moira_stdh_{name}"));
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
fn file_handles_and_hasher_run() {
    let src = r#"
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
fn write_out(path: &str, text: &str) -> std::io::Result<()> {
    std::fs::create_dir_all("moira_std_handles_test_dir")?;
    let mut f = std::fs::File::create(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}
fn main() {
    let p = "moira_std_handles_test_dir/out.txt";
    match write_out(p, "hello\n") {
        Ok(()) => println!("wrote"),
        Err(_) => println!("failed"),
    }
    let bytes = std::fs::read(p).ok();
    println!("{}", bytes.map(|b| b.len()).unwrap_or(0));
    let mut h = DefaultHasher::new();
    "abc".hash(&mut h);
    5usize.hash(&mut h);
    let a = h.finish();
    let mut h2 = DefaultHasher::new();
    "abc".hash(&mut h2);
    5usize.hash(&mut h2);
    println!("{}", a == h2.finish());
}
"#;
    let Some(out) = run_c("handles", src) else {
        return;
    };
    let _ = std::fs::remove_dir_all("moira_std_handles_test_dir");
    assert_eq!(out, "wrote\n6\ntrue\n");
}

#[test]
fn env_args_collect_run() {
    // The program runs without arguments, so only its own name is there.
    let src = r#"
fn main() {
    let all: Vec<String> = std::env::args().collect();
    let rest: Vec<String> = all[1..].to_vec();
    println!("{} {}", all.len(), rest.len());
}
"#;
    let Some(out) = run_c("envargs", src) else {
        return;
    };
    assert_eq!(out, "1 0\n");
}
