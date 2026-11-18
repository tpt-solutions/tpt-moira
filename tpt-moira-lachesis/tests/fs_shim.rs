//! `std::fs` on the C backend: reading and writing files, and their error results. Each program
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
fn fs_read_reports_missing_files_and_reads_present_ones() {
    let dir = std::env::temp_dir();
    let present = dir.join("moira_fs_shim_present.txt");
    std::fs::write(&present, "abcdef").expect("write fixture");
    let src = format!(
        "use std::fs;
        fn main() {{
            let r = fs::read_to_string(\"{}\");
            match r {{ Ok(s) => println!(\"read {{}}\", s.len()), Err(_) => println!(\"missing\") }}
            let m = fs::read_to_string(\"/nonexistent_moira_fs_shim\");
            match m {{ Ok(s) => println!(\"read {{}}\", s.len()), Err(e) => println!(\"{{}}\", e) }}
        }}",
        present.display().to_string().replace('\\', "/")
    );
    let out = match run_c("read", &src) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "read 6\ncannot read file\n");
}

#[test]
fn fs_write_creates_the_file_and_reports_failure() {
    let dir = std::env::temp_dir();
    let target = dir.join("moira_fs_shim_out.txt");
    let _ = std::fs::remove_file(&target);
    let path = target.display().to_string().replace('\\', "/");
    let src = format!(
        "use std::fs;
        fn main() {{
            let w = fs::write(\"{path}\", \"hello\");
            match w {{ Ok(_) => println!(\"wrote\"), Err(_) => println!(\"nope\") }}
            let v = fs::write(\"/nonexistent_moira_dir/x.txt\", \"a\");
            match v {{ Ok(_) => println!(\"wrote2\"), Err(e) => println!(\"{{}}\", e) }}
        }}"
    );
    let out = match run_c("write", &src) {
        Some(o) => o,
        None => return,
    };
    assert_eq!(out, "wrote\ncannot write file\n");
    assert_eq!(
        std::fs::read_to_string(&target).expect("written file"),
        "hello"
    );
}
