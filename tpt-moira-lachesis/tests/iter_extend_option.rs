//! Iterator chains that end in `max` after a `map`, `extend` from an iterator chain, `Result::ok`, `Option::filter`, and an enum variant built in `map`.
//! Iterator chains that end in `max` after a `map`, `extend` from an iterator chain, `Result::ok`, `Option::filter`, and an enum variant built in `map`.
//! Iterator chains that end in `max` after a `map`, `extend` from an iterator chain, `Result::ok`, `Option::filter`, and an enum variant built in `map`.

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
    let c_path = dir.join(format!("moira_iterext_{name}.c"));
    let exe_path = dir.join(format!("moira_iterext_{name}"));
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
fn iterator_and_option_shapes_run() {
    let src = r#"
enum K { Hash(u64), Name(i64) }
fn parse(s: &str) -> Option<K> {
    s.parse::<u64>().ok().map(|v| K::Hash(v))
}
fn main() {
    let nums: Vec<usize> = vec![3, 7, 5];
    let m = nums.iter().map(|a| *a).max().unwrap_or(0);
    println!("{}", m);
    let src = vec![String::from("q"), String::from("rr")];
    let mut dst: Vec<String> = Vec::new();
    dst.push(String::from("z"));
    dst.extend(src.iter().map(|s| s.clone()));
    println!("{} {}", dst.len(), dst[2]);
    match parse("42") {
        Some(K::Hash(n)) => println!("hash {}", n),
        Some(K::Name(_)) => println!("name"),
        None => println!("none"),
    }
    let o: Option<usize> = Some(4);
    println!("{}", o.filter(|x| *x > 3).is_some());
}
"#;
    let Some(out) = run_c("iterext", src) else {
        return;
    };
    assert_eq!(out, "7\n3 rr\nhash 42\ntrue\n");
}
