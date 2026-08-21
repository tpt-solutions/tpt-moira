# Contributing to Moira

Thanks for your interest in improving Moira! This guide covers how to build,
test, and extend the compiler. For the high-level design and roadmap, read
[`spec.txt`](spec.txt) and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Code of conduct

Be respectful and constructive. This is a TPT Solutions project; keep
discussions focused on the engineering work.

## Getting started

Moira is a Cargo workspace. You need a stable Rust toolchain (1.85+).

```sh
# build everything
cargo build

# run the whole test suite (all crates)
cargo test --workspace

# run a single crate's tests
cargo test -p clotho
cargo test -p atropos
cargo test -p lachesis
```

The native backend (`atropos` -> x86-64 assembly) shells out to a system C
compiler (`gcc`/`clang`/`cc`) to assemble and link. If none is installed, the
native-codegen tests are skipped; the C backend (`codegen_c`) and all analysis
passes do not need it.

## Divergence checking (`moira --diff`)

`moira --diff <file>` runs Moira's own `--check` and `rustc --emit=metadata` on
the same source and classifies any divergence (`MoiraAcceptsButRustcRejects`,
`RustcAcceptsButMoiraRejects`, `BothReject`). This is the foundation for the
Phase-3 `rustc` UI-test CI job and the AI-assisted diffing tool. The pure
classification logic lives in `tpt_moira_lachesis::diff`; the orchestration is in
`src/main.rs`.

## Trait solver

`tpt_moira_lachesis::solver` is the Phase-3 Chalk-like solver foundation: a Horn-clause
(`Goal`/`Clause`) engine with inference-variable unification (occurs check) and
a memoized, depth-limited SLD resolver. The built-in operator-trait program
(`builtin_program`) mirrors `tpt_moira_lachesis::traits::TraitDb`, so the solver already
backs the existing trait checks and is the seam for user `impl`s, where-clauses,
and associated types.

User-defined trait `impl`s are parsed by `clotho` as `ast::ImplDef` (`impl Trait
for Type { .. }`; method bodies are currently ignored). `lachesis` turns them
into solver facts via `program_with_impls`, and `user_impl_implements(prog,
trait_name, ty)` queries the result. `infer::ast_to_ty` translates a Clotho type
into a `tpt_moira_lachesis::ty::Ty` for that purpose.

## Performance regression tracking

`moira-bench` measures Moira vs. `rustc` compile time on the Phase-1 battery
and reports the geometric-mean speedup:

- `cargo run -p moira-bench` — print the table for this run only.
- `cargo run -p moira-bench -- --record` — also append the run to
  `moira-bench-results/history.mbh` (with git commit + timestamp) and compare
  against the previous run, flagging any Moira compile-time increase ≥10% or
  geometric-mean speedup drop ≥10%.
- `cargo run -p moira-bench -- --dashboard` — print the full recorded history
  and the latest regression analysis.
- `--results <dir>` overrides the history directory.

History is stored as plain text (no external crates), so the harness builds and
runs offline. Wire `--record` into CI to catch compile-time regressions as the
compiler grows.

## Repository layout

| Crate | Role |
|-------|------|
| `moira` | Driver binary; wires the three Fates together (`src/main.rs`) |
| `clotho` | Frontend: streaming lexer, recursive-descent parser, AST, macro expansion |
| `lachesis` | Type inference, trait resolution, NLL-style borrow checker, incremental cache |
| `atropos` | MIR lowering, SSA backend, C and native codegen |
| `moira-alloc` | Region-based arena allocator used internally by the compiler |
| `moira-bench` | Benchmark harness comparing Moira vs. `rustc` on the Phase-1 battery |
| `examples/`, `demos/` | Sample Phase-1 sources |

## The pipeline

```
source --(clotho: lex/parse/macro)--> AST
       --(lachesis: type-check)------> typed AST
       --(atropos: lower)------------> MIR (SSA)
       --(atropos: codegen)----------> C | x86-64 asm
```

End-to-end `tpt_moira_lachesis::check` runs type inference, MIR lowering, and the borrow
checker in sequence.

## Conventions

- **Formatting & lints:** run `cargo fmt` and `cargo clippy --workspace` before
  opening a change. CI fails on warnings where configured.
- **No comments unless needed:** keep code self-documenting; add comments only
  where the *why* is non-obvious.
- **Subset-driven development:** Clotho only accepts the Phase-1 language
  subset. When extending the grammar, update `clotho/src/ast.rs`,
  `clotho/src/parser.rs`, and `clotho/src/macros.rs` together, and add a parser
  test under `clotho/tests/`.
- **Borrow checking lives on the MIR:** the NLL-style analysis in
  `lachesis/src/borrow.rs` runs over `tpt_moira_atropos::mir`. New borrow rules should be
  expressed as CFG reachability + liveness over MIR, not on the AST.
- **Tests are mandatory** for any behavior change:
  - Frontend changes -> `clotho/tests/`.
  - Type/borrow changes -> `lachesis/tests/lachesis.rs`.
  - MIR/codegen changes -> `atropos/tests/`.

## Working on a task

1. Pick an item from [`TODO.md`](TODO.md). The checkboxes mark what is done;
   status notes record measured results and known limitations.
2. Implement the change and its tests.
3. Run `cargo test --workspace`.
4. Update the relevant `TODO.md` line: check the box when complete, or add a
   status note describing partial progress / open measurement.

## Adding a language feature: the `&mut` thread as a worked example

To make the borrow checker distinguish `&` from `&mut`, the mutability flag had
to flow through every layer:

- `clotho/src/ast.rs` — `ExprKind::Ref(Box<Expr>, bool)` carries `mutable`.
- `clotho/src/parser.rs` — capture `mut` after `&` (rather than discarding it).
- `clotho/src/macros.rs` — macro expansion preserves/propagates the flag.
- `lachesis/src/infer.rs` — `&mut e` infers `Ty::RefMut`, `&e` infers `Ty::Ref`.
- `atropos/src/mir.rs` + `atropos/src/lower.rs` — `Rvalue::Ref(Operand, bool)`.
- `lachesis/src/borrow.rs` — a `&mut` may not coexist with any live borrow of
  the same place.

Use this vertical slice as a template whenever a property must be tracked from
source through to the borrow checker.

## License

By contributing, you agree that your contributions are dual-licensed under
MIT (`LICENSE-MIT`) and Apache-2.0 (`LICENSE-APACHE-2.0`), copyright TPT
Solutions (2026).
