//! Integration tests for the Lachesis type-checker and borrow-checker.

use clotho::compile_source;
use lachesis::solver::{Answer, Term};
use lachesis::ty::Ty;
use lachesis::{
    borrow_check, build_solver, check, coherence_report, content_hash, normalize_assoc_type,
    type_check, user_impl_implements, Cache, IncrementalCache,
};

fn parse(src: &str) -> clotho::ast::Program {
    compile_source(src).expect("source should parse")
}

fn type_ok(src: &str) -> bool {
    type_check(&parse(src)).is_ok()
}

fn type_err(src: &str) -> bool {
    type_check(&parse(src)).is_err()
}

fn full_ok(src: &str) -> bool {
    check(&parse(src)).is_ok()
}

fn full_err(src: &str) -> bool {
    check(&parse(src)).is_err()
}

#[test]
fn well_typed_programs_pass() {
    assert!(type_ok("fn add(a: i32, b: i32) -> i32 { a + b }"));
    assert!(type_ok(
        "fn main() { let x = 1 + 2; let y = add(x, 40); let _ = y; }
         fn add(a: i32, b: i32) -> i32 { a + b }"
    ));
    // Floats implement Add.
    assert!(type_ok("fn main() { let x = 1.0 + 2.0; let _ = x; }"));
    // Struct literals and field access.
    assert!(type_ok(
        "struct Point { x: i32, y: i32 }
         fn main() { let p = Point { x: 1, y: 2 }; let _ = p.x + p.y; }"
    ));
    // `if`/`else` branches must agree; here both are i32.
    assert!(type_ok(
        "fn main() { let x = 5; let r = if x > 3 { 1 } else { 2 }; let _ = r; }"
    ));
}

#[test]
fn type_mismatch_is_rejected() {
    assert!(type_err("fn main() { let x: i32 = true; }"));
    assert!(type_err("fn main() { let x: bool = 1; }"));
    assert!(type_err("fn main() { let x = 1 + true; }"));
}

#[test]
fn unknown_function_is_rejected() {
    assert!(type_err("fn main() { foo(1); }"));
}

#[test]
fn trait_resolution_rejects_inapplicable_operators() {
    // `bool` does not implement `Add`.
    assert!(type_err("fn main() { let x = true + true; }"));
    // `bool` does not implement `Sub`.
    assert!(type_err("fn main() { let x = true - 1; }"));
    // `&&` requires boolean operands.
    assert!(type_err("fn main() { let x = 1 && 2; }"));
}

#[test]
fn reference_without_mutation_is_safe() {
    assert!(full_ok(
        "fn main() {
            let x = 5;
            let r = &x;
            let _ = r;
        }"
    ));
    // Coexisting shared borrows with no mutation are fine.
    assert!(full_ok(
        "fn main() {
            let x = 5;
            let a = &x;
            let b = &x;
            let _ = a;
            let _ = b;
        }"
    ));
}

#[test]
fn mutation_while_borrowed_is_rejected() {
    // `x` is mutated after a reference to it is taken and used.
    assert!(full_err(
        "fn main() {
            let x = 5;
            let r = &x;
            x = 6;
            let _ = r;
        }"
    ));
    // The borrow escapes through a return.
    assert!(full_err(
        "fn f() -> &i32 {
            let x = 5;
            let r = &x;
            r
        }"
    ));
}

#[test]
fn mutable_and_shared_borrows_rejected() {
    // Two `&mut` borrows of the same place that coexist are rejected.
    assert!(full_err(
        "fn main() {
            let mut x = 5;
            let r = &mut x;
            let s = &mut x;
            let _ = r;
            let _ = s;
        }"
    ));
    // A `&mut` and a shared `&` to the same place may not coexist.
    assert!(full_err(
        "fn main() {
            let mut x = 5;
            let r = &mut x;
            let s = &x;
            let _ = r;
            let _ = s;
        }"
    ));
}

#[test]
fn sequential_mutable_borrows_are_accepted() {
    // The first `&mut` borrow ends before the second begins (it is used first),
    // so they do not overlap and the program is borrow-safe.
    assert!(full_ok(
        "fn main() {
            let mut x = 5;
            let r = &mut x;
            let y = *r;
            let s = &mut x;
            let _ = y;
            let _ = s;
        }"
    ));
    // Coexisting shared `&` borrows remain accepted.
    assert!(full_ok(
        "fn main() {
            let x = 5;
            let a = &x;
            let b = &x;
            let _ = a;
            let _ = b;
        }"
    ));
}

#[test]
fn borrow_check_runs_on_mir() {
    // Exercises the MIR-liveness path directly: a program that type-checks and
    // has no borrow violations must pass the MIR borrow checker.
    let prog = parse("fn main() { let x = 1; let y = x + 1; let _ = y; }");
    let mir = atropos::lower_program(&prog).expect("should lower");
    assert!(borrow_check(&mir).is_ok());
}

#[test]
fn content_addressed_cache_round_trips() {
    let dir = std::env::temp_dir().join(format!("moira_cache_test_{}", std::process::id()));
    let cache = Cache::open(&dir).expect("open cache");
    let key = content_hash("fn main() {}");
    assert!(!cache.contains(key));
    cache.insert(key, b"mir-bytes").expect("insert");
    assert!(cache.contains(key));
    assert_eq!(cache.get(key).expect("get"), b"mir-bytes");
}

#[test]
fn incremental_cache_memoizes() {
    let dir = std::env::temp_dir().join(format!("moira_incr_test_{}", std::process::id()));
    let cache = IncrementalCache::open(&dir).expect("open cache");
    let key = content_hash("stable-source");
    let first = cache.compute_if_absent(key, || "computed-once".to_string());
    let second = cache.compute_if_absent(key, || "should-not-run".to_string());
    assert_eq!(first, "computed-once");
    assert_eq!(second, "computed-once");
}

#[test]
fn user_impl_is_found_by_solver() {
    // Parse an `impl` block and confirm the trait solver's impl search finds
    // it, while an unrelated trait is not implemented.
    let prog = parse(
        "struct Point { x: i32, y: i32 }
         impl Eq for Point {}
         fn main() { let _ = 1; }",
    );
    // The program must still type-check (the impl is ignored by the checker).
    assert!(check(&prog).is_ok());
    assert!(user_impl_implements(
        &prog,
        "Eq",
        &Ty::Struct("Point".into())
    ));
    assert!(!user_impl_implements(
        &prog,
        "Ord",
        &Ty::Struct("Point".into())
    ));
}

#[test]
fn user_impl_absent_for_unrelated_type() {
    let prog = parse(
        "struct A {}
         struct B {}
         impl Eq for A {}
         fn main() {}",
    );
    assert!(check(&prog).is_ok());
    assert!(user_impl_implements(&prog, "Eq", &Ty::Struct("A".into())));
    assert!(!user_impl_implements(&prog, "Eq", &Ty::Struct("B".into())));
}

#[test]
fn generic_impl_with_where_clause() {
    // `impl<T> Clone for Wrap<T> where T: Copy` — a parameterized impl whose
    // where-clause is a clause body. Concrete `Wrap<i32>` is `Clone` only
    // because `i32` implements `Copy`.
    let prog = parse(
        "struct Wrap<T> { v: T }
         impl Copy for i32 {}
         impl<T> Clone for Wrap<T> where T: Copy {}
         fn main() {}",
    );
    assert!(check(&prog).is_ok());
    let solver = build_solver(&prog);
    let wrap_i32 = Term::Apply("Wrap".to_string(), vec![Term::Concr(Ty::I32)]);
    assert_eq!(solver.impl_for_term("Clone", &wrap_i32), Answer::Yes);
    // `Wrap<str>` is not `Clone`: `str` does not implement `Copy`.
    let wrap_str = Term::Apply("Wrap".to_string(), vec![Term::Concr(Ty::Str)]);
    assert_ne!(solver.impl_for_term("Clone", &wrap_str), Answer::Yes);
}

#[test]
fn associated_type_normalization() {
    // `type Item = i32;` inside `impl Iterator for Point` normalizes
    // `<Point as Iterator>::Item` to `i32`.
    let prog = parse(
        "struct Point { x: i32, y: i32 }
         impl Iterator for Point { type Item = i32; }
         fn main() {}",
    );
    assert!(check(&prog).is_ok());
    let proj = Term::Proj(
        "Iterator".to_string(),
        "Item".to_string(),
        Box::new(Term::Concr(Ty::Struct("Point".into()))),
    );
    let normalized = build_solver(&prog).normalize(&proj);
    assert_eq!(normalized, Some(Term::Concr(Ty::I32)));

    // The public helper resolves through the parsed program directly.
    let via_helper = normalize_assoc_type(&prog, "Iterator", "Item", &Ty::Struct("Point".into()));
    assert_eq!(via_helper, Some(Term::Concr(Ty::I32)));
}

#[test]
fn generic_associated_type_normalization() {
    // `impl<T> It for Wrap<T> { type Item = T; }` normalizes the projection by
    // instantiating the generic self type.
    let prog = parse(
        "struct Wrap<T> { v: T }
         impl<T> It for Wrap<T> { type Item = T; }
         fn main() {}",
    );
    assert!(check(&prog).is_ok());
    let proj = Term::Proj(
        "It".to_string(),
        "Item".to_string(),
        Box::new(Term::Apply("Wrap".to_string(), vec![Term::Concr(Ty::I32)])),
    );
    let normalized = build_solver(&prog).normalize(&proj);
    assert_eq!(normalized, Some(Term::Concr(Ty::I32)));
}

#[test]
fn overlapping_concrete_impls_rejected() {
    let prog = parse(
        "struct Point { x: i32, y: i32 }
         impl Eq for Point {}
         impl Eq for Point {}
         fn main() {}",
    );
    assert!(check(&prog).is_err());
    assert!(!coherence_report(&prog).is_empty());
}

#[test]
fn overlapping_generic_impls_rejected() {
    // Two blanket `impl`s for the same trait over the same generic self type
    // overlap for every instantiation.
    let prog = parse(
        "struct Wrap<T> { v: T }
         impl<T> Eq for Wrap<T> {}
         impl<U> Eq for Wrap<U> {}
         fn main() {}",
    );
    assert!(check(&prog).is_err());
    assert!(!coherence_report(&prog).is_empty());
}

#[test]
fn distinct_impls_are_coherent() {
    // Two `impl`s of the same trait for *different* types do not overlap.
    let prog = parse(
        "struct A {}
         struct B {}
         impl Eq for A {}
         impl Eq for B {}
         fn main() {}",
    );
    assert!(check(&prog).is_ok());
    assert!(coherence_report(&prog).is_empty());
}

// --- standard-library prelude (Phase-2 "idiomatic std" validation) --------

#[test]
fn std_option_is_accepted() {
    assert!(check(&parse(
        "fn main() {
            let x = Some(5);
            let _ = x.is_some();
            let _ = x.is_none();
            let _ = x.unwrap();
            let y = None;
            let _ = y;
        }"
    )).is_ok());
}

#[test]
fn std_result_is_accepted() {
    assert!(check(&parse(
        "fn main() {
            let r = Ok(5);
            let e = Err(0);
            let _ = r.is_ok();
            let _ = r.is_err();
            let _ = e.is_err();
            let _ = r.unwrap();
        }"
    )).is_ok());
}

#[test]
fn std_vec_methods_are_accepted() {
    assert!(check(&parse(
        "fn main() {
            let v = Vec::new();
            v.push(1);
            v.push(2);
            let _ = v.len();
            let _ = v.is_empty();
        }"
    )).is_ok());
}

#[test]
fn std_string_methods_are_accepted() {
    assert!(check(&parse(
        "fn main() {
            let s = String::new();
            s.push_str(\"hi\");
            let _ = s.len();
        }"
    )).is_ok());
}

#[test]
fn std_constructor_free_functions_are_accepted() {
    assert!(check(&parse(
        "fn main() {
            let v = vec_new();
            v.push(1);
            let s = string_new();
            let b = box_new(7);
            let _ = v;
            let _ = s;
            let _ = b;
        }"
    )).is_ok());
}

#[test]
fn unknown_std_method_is_rejected() {
    // `Vec` has no `frobnicate` method; the checker must still reject it.
    assert!(check(&parse(
        "fn main() {
            let v = Vec::new();
            v.frobnicate();
        }"
    )).is_err());
}

#[test]
fn std_method_argument_count_is_checked() {
    assert!(check(&parse(
        "fn main() {
            let v = Vec::new();
            v.push();
        }"
    )).is_err());
}
