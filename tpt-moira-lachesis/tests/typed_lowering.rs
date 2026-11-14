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
    let out = String::from_utf8_lossy(&run.stdout);
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
    println!("{}", l.copied().unwrap_or(0));
    println!("{}", e.first().copied().unwrap_or(-1));
    println!("{}", b.items.len());
}
"#;
    // `println!` writes its own line breaks; blank lines are dropped.
    if let Some(out) = check_and_run("option_methods", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["4", "9", "-1", "2"]);
    }
}

#[test]
fn capturing_closure_passed_to_fn_runs() {
    let src = r#"
fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
    f(x)
}

fn main() {
    let a: i64 = 2;
    let add = |n: i64| n + a;
    println!("{}", apply(add, 1));
    let b: i64 = 5;
    println!("{}", apply(|n: i64| n * b, 4));
}
"#;
    let Some(out) = check_and_run("capturing_closure_fn_arg", src) else {
        return;
    };
    assert_eq!(
        out.trim(),
        "3
20"
    );
}

/// `println!` of a `&i64` prints the referent; `first()`/`last()`/`get()` give
/// `Option<&T>` whose `copied()` reads the referent only on the `Some` path.
#[test]
fn references_to_scalars_print_and_option_refs_run() {
    let src = r#"
fn show(r: &i64) -> i64 {
    *r + 1
}

fn main() {
    let x: i64 = 5;
    let r = &x;
    println!("{}", r);
    print_i64(show(&x));
    let mut v: Vec<i64> = Vec::new();
    v.push(4);
    v.push(9);
    let f = v.first();
    println!("{}", f.unwrap());
    println!("{}", v.last().copied().unwrap_or(0));
    let e: Vec<i64> = Vec::new();
    println!("{}", e.first().copied().unwrap_or(-1));
}
"#;
    if let Some(out) = check_and_run("refs_option", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["5", "6", "4", "9", "-1"]);
    }
}

/// `slice.first().map(|t| &t.kind)`: the closure's parameter is a pointer, so the
/// borrowed field is the element's own storage and no copy escapes the function.
#[test]
fn closure_borrowing_a_field_through_option_ref_runs() {
    let src = r#"
struct Tok {
    kind: i64,
}

fn pick(args: &[Tok]) -> i64 {
    let inner = match args.first().map(|t| &t.kind) {
        Some(k) => *k,
        None => 0,
    };
    inner + 1
}

fn main() {
    let mut v: Vec<Tok> = Vec::new();
    v.push(Tok { kind: 3 });
    print_i64(pick(&v));
}
"#;
    if let Some(out) = check_and_run("closure_field_borrow", src) {
        assert_eq!(out.trim(), "4");
    }
}

/// A method called on a closure parameter that is a reference to a struct (`|c| c.bump()`)
/// resolves to the struct's method: the parameter is a pointer, and the call passes it.
#[test]
fn method_call_on_closure_reference_parameter_runs() {
    let src = r#"
struct Counter {
    n: i64,
}

impl Counter {
    fn bump(&mut self) -> i64 {
        self.n = self.n + 1;
        self.n
    }

    fn run(&mut self, f: impl FnMut(&mut Counter) -> i64) -> i64 {
        let mut g = f;
        g(self)
    }

    fn go(&mut self) -> i64 {
        self.run(|c| c.bump())
    }
}

fn main() {
    let mut c = Counter { n: 4 };
    print_i64(c.go());
}
"#;
    if let Some(out) = check_and_run("closure_method_receiver", src) {
        assert_eq!(out.trim(), "5");
    }
}

/// `pop`, `truncate` and `is_empty` on a vector update its length in place, and `pop` on an
/// empty vector is `None`.
#[test]
fn vec_pop_truncate_and_is_empty_run() {
    let src = r#"
fn main() {
    let mut v: Vec<i64> = Vec::new();
    println!("{}", v.is_empty());
    v.push(4);
    v.push(9);
    v.push(7);
    println!("{}", v.pop().unwrap());
    println!("{}", v.len());
    v.truncate(1);
    println!("{}", v.len());
    println!("{}", v.is_empty());
    let e: Vec<i64> = Vec::new();
    let mut f = e;
    println!("{}", f.pop().unwrap_or(-1));
}
"#;
    if let Some(out) = check_and_run("vec_pop_truncate", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["true", "7", "2", "1", "false", "-1"]);
    }
}

/// `map_err` keeps the success type, so a `?` after it reads the fields of the success value,
/// and an `Err` holds the mapped error.
#[test]
fn result_map_err_then_question_mark_runs() {
    let src = r#"
struct Unit {
    n: i64,
}

fn make(ok: bool) -> Result<Unit, i64> {
    if ok {
        Ok(Unit { n: 7 })
    } else {
        Err(3)
    }
}

fn run(ok: bool) -> Result<i64, i64> {
    let u = make(ok).map_err(|e| e + 1)?;
    Ok(u.n)
}

fn main() {
    match run(true) {
        Ok(v) => print_i64(v),
        Err(e) => print_i64(e),
    }
    match run(false) {
        Ok(v) => print_i64(v),
        Err(e) => print_i64(e),
    }
}
"#;
    if let Some(out) = check_and_run("result_map_err", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["7", "4"]);
    }
}

/// `Vec::retain` keeps the elements its closure accepts, in order, and shortens the vector.
#[test]
fn vec_retain_keeps_accepted_elements_and_runs() {
    let src = r#"
fn main() {
    let mut v: Vec<i64> = Vec::new();
    v.push(1);
    v.push(4);
    v.push(9);
    v.push(2);
    v.retain(|x| *x > 1);
    println!("{}", v.len());
    println!("{}", v.pop().unwrap());
    println!("{}", v.pop().unwrap());
}
"#;
    if let Some(out) = check_and_run("vec_retain", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["3", "2", "9"]);
    }
}

/// The `wrapping_*` integer methods are the C operators.
#[test]
fn wrapping_integer_methods_run() {
    let src = r#"
fn main() {
    let a: i64 = 7;
    print_i64(a.wrapping_add(5));
    print_i64(a.wrapping_mul(3));
    print_i64(a.wrapping_sub(10));
}
"#;
    if let Some(out) = check_and_run("wrapping_methods", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["12", "21", "-3"]);
    }
}

/// `unwrap_or_default`, `Result::ok` and `Option::ok_or_else` on named values.
#[test]
fn option_result_combinators_run() {
    let src = r#"
fn find(ok: bool) -> Result<i64, i64> {
    if ok { Ok(5) } else { Err(2) }
}

fn main() {
    let a: Option<i64> = None;
    let b: Option<i64> = Some(9);
    print_i64(a.unwrap_or_default());
    print_i64(b.unwrap_or_default());
    let r: Option<i64> = find(false).ok();
    print_i64(r.unwrap_or(-1));
    let s: Option<i64> = find(true).ok();
    print_i64(s.unwrap_or(-1));
    let e: Result<i64, i64> = b.ok_or_else(|| 4i64);
    print_i64(e.unwrap_or(-2));
    let f: Result<i64, i64> = a.ok_or_else(|| 4i64);
    print_i64(f.unwrap_or(-2));
}
"#;
    if let Some(out) = check_and_run("option_combinators", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["0", "9", "-1", "5", "9", "-2"]);
    }
}

/// `Option::take` returns the value and leaves `None` in the local or field it came from;
/// `bool::then_some` is `Some` when the receiver is true.
#[test]
fn option_take_and_bool_then_some_run() {
    let src = r#"
struct Slot {
    v: Option<i64>,
}

fn main() {
    let mut x: Option<i64> = Some(4);
    let t = x.take();
    print_i64(t.unwrap_or(-1));
    print_i64(x.unwrap_or(-1));
    let mut s = Slot { v: Some(8) };
    let u = s.v.take();
    print_i64(u.unwrap_or(-1));
    print_i64(s.v.unwrap_or(-1));
    let c: bool = 3 > 2;
    let d: bool = 1 > 2;
    print_i64(c.then_some(6).unwrap_or(-1));
    print_i64(d.then_some(6).unwrap_or(-1));
}
"#;
    if let Some(out) = check_and_run("take_then_some", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["4", "-1", "8", "-1", "6", "-1"]);
    }
}

/// `Option::as_ref` gives a reference to the payload, so `unwrap_or` takes a reference too.
#[test]
fn option_as_ref_chain_runs() {
    let src = r#"
fn main() {
    let a: Option<i64> = Some(6);
    let b: Option<i64> = None;
    print_i64(a.as_ref().unwrap_or(&-1).clone());
    print_i64(b.as_ref().unwrap_or(&-1).clone());
}
"#;
    if let Some(out) = check_and_run("option_as_ref", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["6", "-1"]);
    }
}

/// `Option::and_then` and `Option::filter` take a closure over the payload, and `or_else`
/// takes a closure that returns an option of the same type.
#[test]
fn option_and_then_filter_or_else_run() {
    let src = r#"
fn half(x: i64) -> Option<i64> {
    if x % 2 == 0 { Some(x / 2) } else { None }
}

fn main() {
    let a: Option<i64> = Some(8);
    let b: Option<i64> = Some(7);
    let c: Option<i64> = None;
    print_i64(a.and_then(|x| half(x)).unwrap_or(-1));
    print_i64(b.and_then(|x| half(x)).unwrap_or(-1));
    print_i64(c.and_then(|x| half(x)).unwrap_or(-1));
    print_i64(a.filter(|x| *x > 5).unwrap_or(-2));
    print_i64(a.filter(|x| *x > 50).unwrap_or(-2));
    print_i64(c.or_else(|| None).unwrap_or(-3));
    print_i64(b.or_else(|| Some(1)).unwrap_or(-3));
}
"#;
    if let Some(out) = check_and_run("option_and_then", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["4", "-1", "-1", "8", "-2", "-3", "7"]);
    }
}

/// An unsuffixed literal in a closure passed to `ok_or_else` takes the type of the annotated
/// result (`i64`), not the default `i32`.
#[test]
fn unsuffixed_literal_in_closure_takes_the_annotated_type() {
    let src = r#"
fn main() {
    let none: Option<i64> = None;
    let some: Option<i64> = Some(9);
    let e: Result<i64, i64> = none.ok_or_else(|| 4);
    let f: Result<i64, i64> = some.ok_or_else(|| 4);
    print_i64(e.unwrap_or(-2));
    print_i64(f.unwrap_or(-2));
}
"#;
    if let Some(out) = check_and_run("closure_literal", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["-2", "9"]);
    }
}

/// `max`, `min` and `last` over an iterator of scalars: `None` for an empty iterator, and the
/// extreme or last element otherwise (also after a `map`).
#[test]
fn iterator_max_min_last_run() {
    let src = r#"
fn main() {
    let mut v: Vec<i64> = Vec::new();
    v.push(4);
    v.push(9);
    v.push(2);
    let e: Vec<i64> = Vec::new();
    print_i64(v.iter().max().unwrap_or(-1));
    print_i64(v.iter().min().unwrap_or(-1));
    print_i64(v.iter().last().unwrap_or(-1));
    print_i64(e.iter().max().unwrap_or(-1));
    print_i64(v.iter().map(|x| *x * 10).max().unwrap_or(-1));
}
"#;
    if let Some(out) = check_and_run("iter_max_min", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["9", "2", "2", "-1", "90"]);
    }
}

/// `map.entry(k).or_insert(v)` and `or_default()` give the value's slot, so `*slot += x`
/// updates the map; a compound assignment to a field and an element stores its result.
#[test]
fn map_entry_and_compound_assignment_to_places_run() {
    let src = r#"
use std::collections::HashMap;

struct Acc {
    total: i64,
}

fn main() {
    let mut m: HashMap<i64, i64> = HashMap::new();
    *m.entry(3).or_insert(0) += 1;
    *m.entry(3).or_insert(10) += 5;
    let d = *m.entry(4).or_default();
    print_i64(m.len() as i64);
    print_i64(m.get(3).unwrap_or(-1));
    print_i64(d);
    let mut a = Acc { total: 4 };
    a.total += 3;
    print_i64(a.total);
    let mut v: Vec<i64> = Vec::new();
    v.push(1);
    v.push(2);
    v[1] += 10;
    print_i64(v[1]);
}
"#;
    if let Some(out) = check_and_run("map_entry_compound", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["2", "6", "0", "7", "12"]);
    }
}

/// `m.entry(k).or_default().push(x)`: a vector value is created empty on first use, and the
/// push updates the vector in the map.
#[test]
fn map_entry_or_default_then_push_runs() {
    let src = r#"
use std::collections::HashMap;

fn main() {
    let mut m: HashMap<i64, Vec<i64>> = HashMap::new();
    m.entry(1).or_default().push(5);
    m.entry(1).or_default().push(6);
    m.entry(2).or_default().push(7);
    print_i64(m.len() as i64);
    print_i64(m.get(1).unwrap().len() as i64);
}
"#;
    if let Some(out) = check_and_run("entry_or_default_push", src) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["2", "2"]);
    }
}
