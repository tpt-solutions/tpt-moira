//! Validate the async/await parse + type-check path (Phase-3 deliverable).
//!
//! The full *runnable* state-machine codegen is tracked separately (see the
//! "Known Phase-3 limitations" note in TODO.md), but the front-end —
//! parsing `async fn` / `.await`, binding the wider output helpers, and
//! type-checking the suspension points — must already work end to end.

use clotho::compile_source;
use lachesis::check;

const ASYNC_DEMO: &str = r#"
async fn add_one(x: i64) -> i64 {
    let y = x + 1;
    y
}

async fn double(x: i64) -> i64 {
    let a = await add_one(x);
    let b = await add_one(a);
    a + b
}

fn main() {
    let r = await double(10);
    print_i64(r);
}
"#;

#[test]
fn async_fn_and_await_type_check() {
    let prog = compile_source(ASYNC_DEMO).expect("async demo should parse");
    assert!(check(&prog).is_ok());
}
