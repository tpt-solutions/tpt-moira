//! Typed lowering: Lachesis's expression types reach Atropos as hints, so a
//! `for` over a vector that the untyped lowering cannot see, and a `println!` of a
//! `bool`, lower correctly. The run test compiles the emitted C with the system C
//! compiler and is skipped when none is available.

use std::process::Command;

fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

/// Type-check `src`, lower it with the checker's hints, compile and run it.
/// Returns stdout, or `None` when no C compiler is available.
fn check_and_run(name: &str, src: &str) -> Option<String> {
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    let mir = tpt_moira_atropos::lower_program_typed(&prog, &hints)
        .expect("program should lower with hints");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);

    let cc = find_cc()?;
    let dir = std::env::temp_dir();
    let c_path = dir.join(format!("moira_typed_{name}.c"));
    let exe_path = dir.join(format!("moira_typed_{name}"));
    std::fs::write(&c_path, c).unwrap();
    let status = Command::new(&cc)
        .arg("-O0")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .status()
        .expect("run cc");
    assert!(status.success(), "C compilation failed for {name}");
    let out = Command::new(&exe_path).output().expect("run program");
    assert!(out.status.success(), "program {name} exited with failure");
    Some(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

/// The `Err(diags)` binding of a `Result` is typed by the checker, so its `for`
/// loop lowers only with the hints: without them the same program is rejected.
#[test]
fn pattern_bound_vec_lowers_only_with_hints() {
    let src = r#"
fn make() -> Result<i32, Vec<i32>> {
    let mut v: Vec<i32> = Vec::new();
    v.push(7);
    Err(v)
}

fn main() {
    if let Err(diags) = make() {
        let mut s: i32 = 0;
        for d in &diags {
            s = s + d;
        }
        println!("{}", s);
    }
}
"#;
    let prog = tpt_moira_clotho::parse_source(src).expect("program should parse");
    let (res, hints) = tpt_moira_lachesis::type_check_typed(&prog, &[]);
    assert!(res.is_ok(), "program should type-check: {:?}", res.err());
    assert!(
        tpt_moira_atropos::lower_program(&prog).is_err(),
        "without hints the pattern-bound vector cannot be iterated"
    );
    assert!(
        tpt_moira_atropos::lower_program_typed(&prog, &hints).is_ok(),
        "with the checker's hints it lowers"
    );
}

/// A `let`-bound vector from a function's return type, iterated by reference, and
/// a `bool` printed by `println!`: both rely on hints, and both run end to end.
#[test]
fn hinted_vec_loop_and_bool_print_run() {
    let src = r#"
fn make() -> Vec<i32> {
    let mut v: Vec<i32> = Vec::new();
    v.push(7);
    v.push(8);
    v
}

fn main() {
    let diags = make();
    let mut s: i32 = 0;
    for d in &diags {
        s = s + d;
    }
    println!("{}", s);
    let b: bool = true;
    println!("{}", b);
}
"#;
    let Some(out) = check_and_run("vec_bool", src) else {
        return;
    };
    assert!(out.contains("15"), "sum of the vector: {out:?}");
    assert!(out.contains("true"), "bool printed as true: {out:?}");
}

/// `?` on `Result` values: success yields the payload, an `Err` returns early
/// from the enclosing function. Runs end to end.
#[test]
fn try_operator_propagates_errors_and_runs() {
    let src = r#"
fn parse(x: i32) -> Result<i32, i32> {
    if x > 0 {
        Ok(x * 2)
    } else {
        Err(x)
    }
}

fn twice(x: i32) -> Result<i32, i32> {
    let a = parse(x)?;
    let b = parse(a)?;
    Ok(b + 1)
}

fn main() {
    if let Ok(v) = twice(3) {
        println!("{}", v);
    }
    if let Err(e) = twice(-1) {
        println!("{}", e);
    }
}
"#;
    let Some(out) = check_and_run("try_result", src) else {
        return;
    };
    assert!(out.contains("13"), "success path: {out:?}");
    assert!(out.contains("-1"), "error path: {out:?}");
}

#[test]
fn nested_loops_over_fields_of_references_run() {
    let src = r#"
struct Tok {
    n: i32,
}

struct Attr {
    args: Vec<Tok>,
}

struct Item {
    attributes: Vec<Attr>,
}

fn count(item: &Item) -> i32 {
    let mut c: i32 = 0;
    for attr in &item.attributes {
        for t in &attr.args {
            c = c + t.n;
        }
    }
    c
}

fn main() {
    let mut v: Vec<Attr> = Vec::new();
    let mut toks: Vec<Tok> = Vec::new();
    toks.push(Tok { n: 3 });
    toks.push(Tok { n: 4 });
    v.push(Attr { args: toks });
    let it = Item { attributes: v };
    println!("{}", count(&it));
}
"#;
    let Some(out) = check_and_run("nested", src) else {
        return;
    };
    // `println!` currently adds blank lines (see the atropos vec tests), so trim.
    assert_eq!(out.trim(), "7");
}

#[test]
fn iterator_chains_over_ranges_run() {
    let src = r#"
fn main() {
    let sq: Vec<i32> = (0..4).map(|i| i * i).collect();
    println!("{}", sq[3]);
    let s: i32 = (1..=3).map(|i| i).sum();
    println!("{}", s);
    if (0..5).any(|i| i == 4) {
        println!("1");
    }
}
"#;
    // The collected element type comes from the checker's hint.
    let Some(out) = check_and_run("ranges", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(values, vec!["9", "6", "1"]);
}

#[test]
fn enumerate_chains_with_pair_closures_run() {
    let src = r#"
fn main() {
    let mut v: Vec<i32> = Vec::new();
    v.push(5);
    v.push(6);
    v.push(7);
    let weighted: i32 = v.iter().enumerate().map(|(i, x)| (i as i32) * 10 + x).sum();
    println!("{}", weighted);
    let n = v.iter().enumerate().filter(|(i, x)| i > 0).count();
    println!("{}", n);
    if v.iter().enumerate().any(|(i, x)| x == 7) {
        println!("1");
    }
}
"#;
    let Some(out) = check_and_run("enumerate_chains", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    // 5 + (10 + 6) + (20 + 7) = 48; two items have a position above zero.
    assert_eq!(values, vec!["48", "2", "1"]);
}

#[test]
fn typed_enum_payloads_run() {
    let src = r#"
struct Point {
    x: i32,
    y: i32,
}

enum Shape {
    Dot(Point),
    Line(Point, i32),
    Many(Vec<i32>),
}

fn total(s: &Shape) -> i32 {
    match s {
        Shape::Dot(p) => p.x + p.y,
        Shape::Line(p, k) => p.x + k,
        Shape::Many(v) => {
            let mut t: i32 = 0;
            for x in v {
                t = t + x;
            }
            t
        }
    }
}

fn main() {
    let mut v: Vec<i32> = Vec::new();
    v.push(1);
    v.push(2);
    v.push(4);
    println!("{}", total(&Shape::Dot(Point { x: 3, y: 4 })));
    println!("{}", total(&Shape::Line(Point { x: 10, y: 0 }, 5)));
    println!("{}", total(&Shape::Many(v)));
}
"#;
    let Some(out) = check_and_run("typed_enum", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(values, vec!["7", "15", "7"]);
}

#[test]
fn typed_option_payloads_run() {
    let src = r#"
struct P {
    x: i32,
}

fn mk(n: i32) -> Option<P> {
    if n > 0 {
        Some(P { x: n })
    } else {
        None
    }
}

fn main() {
    if let Some(p) = mk(3) {
        println!("{}", p.x);
    }
    match mk(0) {
        Some(p) => println!("{}", p.x),
        None => println!("none"),
    }
}
"#;
    let Some(out) = check_and_run("typed_option", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(values, vec!["3", "none"]);
}

#[test]
fn hash_maps_run_in_insertion_order() {
    let src = r#"
fn main() {
    let mut m: HashMap<i32, i32> = HashMap::new();
    m.insert(1, 10);
    m.insert(2, 20);
    m.insert(1, 11);
    println!("{}", m.len());
    if let Some(v) = m.get(&1) {
        println!("{}", v);
    }
    if m.contains_key(&3) {
        println!("bad");
    } else {
        println!("0");
    }
    for (k, v) in &m {
        println!("{}", k * 100 + v);
    }
}
"#;
    let Some(out) = check_and_run("maps", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(values, vec!["2", "11", "0", "111", "220"]);
}

/// The root matches on an enum that another crate defines: the variant is found through
/// the owning crate's enum, and the value comes from the other crate's function. Both
/// crates are typed by the checker, as the driver does.
#[test]
fn bundle_root_matches_on_another_crates_enum() {
    let root_src = r#"
fn main() {
    match mk(3) {
        Kind::B(x) => println!("{}", x + 1),
        Kind::A => println!("0"),
    }
}
"#;
    let lib_src = r#"
enum Kind {
    A,
    B(i32),
}

fn mk(n: i32) -> Kind {
    if n > 0 {
        Kind::B(n)
    } else {
        Kind::A
    }
}
"#;
    let root = tpt_moira_clotho::parse_source(root_src).expect("root parses");
    let lib = tpt_moira_clotho::parse_source(lib_src).expect("lib parses");
    let lib_named = vec![("lib".to_string(), lib.clone())];
    let root_hints = tpt_moira_lachesis::expr_type_hints(&root, &lib_named);
    let lib_hints = tpt_moira_lachesis::expr_type_hints(&lib, &[]);
    let externs = vec![("lib".to_string(), lib, lib_hints)];
    let mir = tpt_moira_atropos::lower_bundle(&root, &root_hints, &externs).expect("bundle lowers");
    let c = tpt_moira_atropos::codegen_c::emit_c(&mir);
    let Some(cc) = find_cc() else {
        return;
    };
    let dir = std::env::temp_dir();
    let c_path = dir.join("moira_typed_cross_enum.c");
    let exe_path = dir.join("moira_typed_cross_enum");
    std::fs::write(&c_path, &c).unwrap();
    let status = std::process::Command::new(&cc)
        .arg("-O0")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .status()
        .expect("run cc");
    assert!(
        status.success(),
        "C compilation failed:
{c}"
    );
    let run = std::process::Command::new(&exe_path)
        .output()
        .expect("run program");
    let out = String::from_utf8_lossy(&run.stdout).replace(
        "
", "
",
    );
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(values, vec!["4"], "output: {out:?}\n{c}");
}

#[test]
fn hash_sets_run() {
    let src = r#"
fn main() {
    let mut s: HashSet<i32> = HashSet::new();
    s.insert(4);
    s.insert(9);
    s.insert(4);
    println!("{}", s.len());
    if s.contains(&9) {
        println!("1");
    }
    let mut total: i32 = 0;
    for x in &s {
        total = total + x;
    }
    println!("{}", total);
}
"#;
    let Some(out) = check_and_run("sets", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(values, vec!["2", "1", "13"]);
}

#[test]
fn sorted_collections_iterate_in_key_order() {
    let src = r#"
fn main() {
    let mut s: BTreeSet<i32> = BTreeSet::new();
    s.insert(5);
    s.insert(1);
    s.insert(3);
    for x in &s {
        println!("{}", x);
    }
    let mut m: BTreeMap<i32, i32> = BTreeMap::new();
    m.insert(2, 20);
    m.insert(1, 10);
    for (k, v) in &m {
        println!("{}", k * 100 + v);
    }
}
"#;
    let Some(out) = check_and_run("sorted", src) else {
        return;
    };
    let values: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(values, vec!["1", "3", "5", "110", "220"]);
}

/// A qualified unit variant in a pattern tests the variant (it is not a binding of
/// the enum's name), an array of variants is typed by its enum, and a struct-like
/// variant is built and matched by field name.
#[test]
fn enum_variant_patterns_arrays_and_struct_variants_run() {
    let src = r#"
enum Op {
    Add,
    Sub,
}

enum Shape {
    Pt,
    Rect { w: i64, h: i64 },
}

fn weight(o: Op) -> i64 {
    match o {
        Op::Add => 1,
        Op::Sub => 2,
    }
}

fn area(s: Shape) -> i64 {
    match s {
        Shape::Pt => 0,
        Shape::Rect { w, h } => w * h,
    }
}

fn main() {
    let ops = [Op::Add, Op::Sub];
    let mut total: i64 = 0;
    for o in ops {
        total += weight(o);
    }
    println!("{}", total);
    println!("{}", area(Shape::Rect { w: 3, h: 4 }));
    println!("{}", area(Shape::Pt));
}
"#;
    // `println!` emits an extra newline per call (a known gap), so blank lines are dropped.
    if let Some(out) = check_and_run("enum_variant_patterns", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["3", "12", "0"]);
    }
}

/// `first`/`last` and the `Option` methods lower to tag tests and field reads; a
/// `vec!` literal and a `Vec::new()` in a struct field take the field's element type.
#[test]
fn option_methods_first_last_and_typed_struct_fields_run() {
    let src = r#"
struct Bag {
    items: Vec<i64>,
}

fn main() {
    let mut v: Vec<i64> = Vec::new();
    v.push(4);
    v.push(9);
    let b = Bag { items: vec![1, 2] };
    let e: Vec<i64> = Vec::new();
    let f = v.first();
    let l = v.last();
    println!("{}", f.unwrap());
    println!("{}", l.unwrap_or(0));
    println!("{}", e.first().unwrap_or(-1));
    println!("{}", b.items.len());
}
"#;
    // `println!` writes its own line breaks; blank lines are dropped.
    if let Some(out) = check_and_run("option_methods", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["4", "9", "-1", "2"]);
    }
}
