//! Runtime check: an `Option<Vec<T>>` payload matched through a reference is bound
//! by reference, so mutating it through `&mut` changes the value the owner sees.

use std::process::Command;

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

fn check_and_run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .expect("program should lower with hints");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_payload_{name}.c"));
    let exe_path = dir.join(format!("moira_payload_{name}"));
    std::fs::write(&c_path, &c).expect("write C");
    let build = Command::new(&cc)
        .arg("-w")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .output()
        .expect("run C compiler");
    assert!(
        build.status.success(),
        "C compile failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = Command::new(&exe_path).output().expect("run binary");
    assert!(run.status.success(), "binary failed");
    Some(String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"))
}

#[test]
fn option_vec_payload_mutates_through_mut_reference() {
    let src = "struct Field { ty: i64 }\n\
               struct Variant { fields: Option<Vec<Field>> }\n\
               fn bump(v: &mut Variant) {\n\
                   if let Some(fields) = &mut v.fields {\n\
                       for f in fields.iter_mut() {\n\
                           f.ty = f.ty + 1;\n\
                       }\n\
                   }\n\
               }\n\
               fn total(v: &Variant) -> i64 {\n\
                   let mut t: i64 = 0;\n\
                   if let Some(fields) = &v.fields {\n\
                       for f in fields.iter() {\n\
                           t = t + f.ty;\n\
                       }\n\
                   }\n\
                   t\n\
               }\n\
               fn main() {\n\
                   let mut items: Vec<Field> = Vec::new();\n\
                   items.push(Field { ty: 1 });\n\
                   items.push(Field { ty: 5 });\n\
                   let mut v = Variant { fields: Some(items) };\n\
                   bump(&mut v);\n\
                   println!(\"{}\", total(&v));\n\
               }\n";
    if let Some(out) = check_and_run("option_vec", src) {
        assert_eq!(out, "8\n", "unexpected output");
    }
}
