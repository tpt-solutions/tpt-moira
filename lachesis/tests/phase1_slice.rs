//! Acceptance + rejection slice for the Phase-1 language subset.
//!
//! Curates a small, representative set of Phase-1 programs (in
//! `phase1-slice/accept` and `phase1-slice/reject`) with their expected Moira
//! behavior, backing the Phase-3 deliverable "curate a Phase-1-subset slice of
//! `rustc` UI tests + expected outputs". The nightly CI job runs the same slice
//! through `moira --diff` against `rustc` and publishes a divergence report.

use std::path::Path;

/// Whether `src` parses and passes Moira's full type + borrow check.
fn check_source(src: &str) -> bool {
    match clotho::compile_source(src) {
        Ok(prog) => lachesis::check(&prog).is_ok(),
        Err(_) => false,
    }
}

#[test]
fn phase1_accept_slice() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/phase1-slice/accept");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("accept slice dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read slice file");
        count += 1;
        assert!(
            check_source(&src),
            "Phase-1 accept-slice program `{}` should type + borrow check",
            path.display()
        );
    }
    assert!(count > 0, "no accept-slice programs found");
}

#[test]
fn phase1_reject_slice() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/phase1-slice/reject");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("reject slice dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read slice file");
        count += 1;
        assert!(
            !check_source(&src),
            "Phase-1 reject-slice program `{}` should be rejected",
            path.display()
        );
    }
    assert!(count > 0, "no reject-slice programs found");
}
