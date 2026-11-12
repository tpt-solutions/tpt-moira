//! Typed lowering regressions: each program's emitted C must compile.
//! * an enum-valued `let` from a block/`match` (the local takes the enum's struct),
//! * an annotated closure parameter keeps its reference type,
//! * a vector passed where a slice is expected (a function and a method argument),
//! * the error payload of `?` keeps its type.

use std::process::Command;

fn compile_ok(name: &str, src: &str) {
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "{name}: program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .unwrap_or_else(|e| panic!("{name}: should lower: {}", e.0));
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let Some(cc) = ["gcc", "clang", "cc"]
        .into_iter()
        .find(|cc| Command::new(cc).arg("--version").output().is_ok())
    else {
        return;
    };
    let path = std::env::temp_dir().join(format!("moira_typed_{name}.c"));
    std::fs::write(&path, &c).expect("write C");
    let out = Command::new(cc)
        .arg("-w")
        .arg("-fsyntax-only")
        .arg(&path)
        .output()
        .expect("run cc");
    assert!(
        out.status.success(),
        "{name}: emitted C should compile:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn enum_valued_block_and_match_lets_compile() {
    compile_ok(
        "enum_let",
        "pub enum Kind { A(i64), B }\n\
         fn f(x: &Kind) -> i64 {\n\
             let k = match x { Kind::A(n) => Kind::A(*n), Kind::B => Kind::B };\n\
             let j = { Kind::B };\n\
             let _k2 = k;\n\
             let _j2 = j;\n\
             0\n\
         }\n\
         fn main() {}\n",
    );
}

#[test]
fn annotated_closure_parameter_keeps_its_reference_type() {
    compile_ok(
        "closure_ref",
        "struct Type { n: i64 }\n\
         struct A { n: i64 }\n\
         impl A {\n\
             fn depth(&self, t: &Type) -> i64 { t.n + self.n }\n\
             fn twice(&self, t: &Type) -> i64 {\n\
                 let f = |u: &Type| self.depth(u);\n\
                 f(t)\n\
             }\n\
         }\n\
         fn main() {}\n",
    );
}

#[test]
fn vector_passed_as_slice_compiles() {
    compile_ok(
        "vec_slice",
        "struct Item { n: i64 }\n\
         struct Prog { items: Vec<Item> }\n\
         struct T { k: i64 }\n\
         impl T {\n\
             fn collect(&mut self, items: &[Item], e: i64) -> i64 { items.len() as i64 + e }\n\
         }\n\
         fn total(items: &[Item]) -> i64 { items.len() as i64 }\n\
         fn run(prog: &Prog) -> i64 {\n\
             let mut t = T { k: 0 };\n\
             t.collect(&prog.items, 0) + total(&prog.items)\n\
         }\n\
         fn main() {}\n",
    );
}

#[test]
fn question_mark_error_payload_keeps_its_type() {
    compile_ok(
        "try_err",
        "struct Diag { m: i64 }\n\
         fn inner(n: i64) -> Result<i64, Diag> { if n > 0 { Ok(n) } else { Err(Diag { m: n }) } }\n\
         fn outer(n: i64) -> Result<i64, Diag> {\n\
             let v = inner(n)?;\n\
             Ok(v + 1)\n\
         }\n\
         fn main() {}\n",
    );
}
