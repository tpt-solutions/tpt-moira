# TPT Moira — TODO

High-performance, from-scratch Rust compiler by TPT Solutions. See [`spec.txt`](spec.txt) for the full design doc.

## Repo & Legal Setup
- [x] Initialize git repository
- [x] Set up Cargo workspace layout (`clotho/`, `lachesis/`, `atropos/`, root driver crate)
- [x] Add dual license: `LICENSE-MIT` + `LICENSE-APACHE-2.0`, copyright "TPT Solutions (2026)"
- [x] Write README (project overview, build instructions, contributing)
- [x] Add `.gitignore` (Rust/Cargo defaults)
- [x] Set up CI skeleton (GitHub Actions: build + test matrix)

## Phase 1 (Months 1-8) — The "No-Magic" Compiler
Target: compile a strict subset of Rust (no complex trait resolution, no async/await, no advanced lifetime elision) at 10x `rustc`'s speed.

### Clotho (Frontend & Parser)
- [x] Streaming lexer
- [x] Parallelized, streaming parser
- [x] AST construction
- [x] Concurrent macro-expansion pipeline (runs alongside file I/O so CPU never waits on disk)
- [x] Define and enforce the Phase-1 language subset grammar

### Atropos (Basic Optimizer & Codegen)
- [x] MIR lowering for the Phase-1 subset
- [x] Custom lightweight SSA backend (built from scratch, not Cranelift)
- [x] Native codegen for the restricted subset
- [x] Minimal optimization passes (constant folding + dead-code elimination)

### Core Infrastructure
- [x] Region-based memory allocator for compiler-internal data structures (`moira-alloc` arena)
- [x] Stage 0 bootstrap transpiler (macro-free Moira subset → C → native binary)
- [x] Benchmark harness comparing Moira vs. `rustc` on the Phase-1 subset (`moira-bench`)
- [ ] Validate 10x speedup target for the subset
  - Status: harness in place; measured **~6.9x** geometric-mean speedup for the
    *front-end + SSA mid-end* pipeline vs `rustc --emit=metadata` on the
    Phase-1 battery (see `moira-bench`). The full 10x target is for the
    complete compiler (type-check + borrow-check + incremental caching) at scale
    and remains an open measurement once Phase 2 lands.

## Phase 2 (Months 9-16) — The Lachesis Integration
Target: compile standard, idiomatic Rust code safely.

- [x] Type inference engine (`lachesis::infer` — direct, constraint-checking inference over the Clotho AST; literals default to `i32`, variables/structs/calls resolved from the environment and signatures)
- [x] Basic trait resolution (pre-specialization) (`lachesis::traits` — a built-in trait DB; the checker queries operator-trait applicability for `+`, `-`, `*`, `/`, `%`, bitwise, `&&`/`||`, comparisons, and records the seam for the Phase-3 solver)
- [x] Full NLL borrow graph solver (graph-traversal algorithm) (`lachesis::borrow` — runs over the Atropos MIR; builds the CFG from terminators, propagates reference aliases, and detects mutations-while-borrowed and references-to-locals escaping via reachability + liveness)
- [x] Fast-path borrow checker for simple/local scopes (functions with no references skip the analysis entirely)
- [x] Fallback logic: fast-path → full NLL graph analysis for complex/cross-function lifetimes (the fast path returns early; otherwise the full MIR liveness/alias analysis runs)
- [x] Integrate Lachesis with Clotho and Atropos end-to-end (`moira --emit check` runs type-check → lower → borrow-check; `--check` gates codegen; the SSA emit is backed by the incremental cache)
- [x] Expression-level incremental cache (disk-backed, content-addressed, Salsa-like) (`lachesis::cache` + `moira --emit ssa` memoizes SSA text by source-hash under a temp cache dir)
- [ ] Validate idiomatic standard-library-using Rust programs compile correctly
  - Status: the type + borrow checker is implemented and tested for the Phase-1
    subset (structs, arithmetic, control flow, references, function calls). Programs
    that use `std` types not yet modelled (e.g. `HashMap`, `Vec`) are not yet
    accepted — binding the standard library's signatures into the checker is the
    remaining step before "idiomatic std-using Rust" validation.

### Known Phase-2 limitations (tracked follow-ups)
- ~~The Clotho parser discards `&` vs `&mut` mutability…~~ **Resolved:** mutability
  is now threaded from `clotho` (`ExprKind::Ref(Box<Expr>, bool)` + parser capture
  of `mut`) through `lachesis` (type inference produces `Ty::RefMut` for `&mut`)
  into the `atropos` MIR (`Rvalue::Ref(Operand, bool)`), and the NLL borrow
  checker (`lachesis::borrow`) now forbids a `&mut` from coexisting with any live
  borrow of the same place (second `&mut`, or a shared `&`). See
  `lachesis/tests/lachesis.rs` (`mutable_and_shared_borrows_rejected`,
  `sequential_mutable_borrows_are_accepted`).
- The NLL analysis is CFG-reachability + liveness based (not a full SCC
  constraint solver); it catches the canonical violations but is intentionally
  conservative.

## Phase 3 (Months 17-24) — The Full Language
Target: full language support, matching `rustc` behavior.

- [ ] **Async/await state machine generation**
  - [ ] Model `Future`/`Pin`/generator types in `lachesis::ty`
  - [ ] Parse `async fn` and `.await` (parser + AST)
  - [ ] Lower `async fn` to a suspending state-machine MIR (resume points)
  - [ ] Codegen the state machine (C + native backends)
- [ ] **Complex/full macro expansion support**
  - [x] Parse `macro_rules!` definitions into a matcher/template model (`clotho::macrorules`)
  - [x] Token-tree matcher: `$x:frag`, fragments, and repetitions `$(...)*`/`+`/`?` (`clotho::macrorules`)
  - [x] Template expansion with repetition + nesting (`clotho::macrorules`)
  - [x] Integrate `macro_rules!` into the concurrent expansion pipeline (`clotho::pipeline::compile`)
  - [ ] Attribute macros + `derive` macros (not yet supported)
- [ ] **Next-generation Chalk-like Prolog-style trait solver**
  - [x] **Solver foundation: `Goal`/`Clause` model + SLD-style resolution with
    inference-variable unification + memoization** (`lachesis::solver`); backs
    the existing operator-trait checks via `TraitDb` and is the seam for the
    full solver.
  - [x] **Where-clauses / trait bounds on functions, structs, and impls** —
     `clotho` now parses generic param lists `<T: Bound>`, `where` clauses, and
     associated `type X = Y;` items (`ast::Generics`/`TypeParam`/`WherePredicate`
     /`TypeAliasDef`; `Type` carries generic args `Path(Path, Vec<Type>)`). The
     solver lowers `impl<T> Trait for Type<T> where T: Bound` into a clause whose
     body is the where-clause/bounds, and `lachesis::check` runs a
     well-formedness pass (concrete `T: Bound` predicates must be satisfiable).
     Bounds on generic params of functions/structs are parsed and represented;
     full inference-context enforcement awaits generic `Ty` support.
  - [x] **User-defined `impl Trait for Type` clauses (impl search)** — `clotho`
     parses `impl Trait for Type { .. }` (`ast::ImplDef`); `lower` ignores it and
     the checker ignores it, so existing programs are unaffected. `lachesis`
     builds a `Solver` (`builtin_program` + user `impl` facts via
     `program_with_impls`) and `user_impl_implements(prog, trait, ty)` answers
     whether a type implements a user trait. Backed by unit + integration tests.
  - [x] **Associated-type normalization (simple `type` projections)** —
     `lachesis::solver` adds `Term::Proj` + a `projections` table;
     `impl Trait for T { type Assoc = V; }` becomes
     `<T as Trait>::Assoc -> V`, resolved by `Solver::normalize` /
     `lachesis::normalize_assoc_type` with generic self-type instantiation.
  - [x] **Coherence / overlap checking among impls** — `lachesis::coherence_report`
     (wired into `check`) flags two `impl`s of the same trait whose self-types
     unify (concrete duplicates and overlapping generic blankets).
- [x] **Memoization + early-exit heuristics** — folded into the solver
  (`lachesis::solver` `SolveState::cache` goal cache + `MAX_DEPTH` depth limiter
  with early `Ambiguous` cutoff)
- [ ] **CI pipeline: full `rustc` UI/error-message test suite nightly**
  - [x] **`moira-diff` divergence harness foundation** — runs Moira `--check`
    and `rustc --emit=metadata` on a file and classifies divergences
    (Moira-accepts-but-rustc-rejects, rustc-accepts-but-Moira-rejects,
    diagnostics mismatch). See `moira --diff <file>`.
  - [ ] Curate a Phase-1-subset slice of `rustc` UI tests + expected outputs
  - [ ] Nightly CI job running the slice and publishing a divergence report
- [ ] **AI-assisted diffing tool**
  - [x] `moira-diff` baseline (above) — the structured divergence data the
    AI layer will consume.
  - [ ] Attach an LLM to propose root-cause + minimal fix per divergence
  - [ ] Persist accepted expectations / known-divergence allowlist
- [ ] **Triage and fix flagged divergences continuously**
  - [ ] Triage queue + labels; close subset-coverage gaps first

## Phase 4 (Months 25-30) — The Ouroboros & Production
Target: self-hosting, production-grade, ≥5x faster than `rustc` at scale.

- [ ] **Stage 1 self-host: Moira compiles its own full Rust source**
  - [ ] Reach full-subset coverage so Moira can compile `clotho`/`lachesis`/`atropos`
  - [x] Bootstrap build script wiring the three crates (like `x.py`) — `x.py`
        orchestrates Stage-0 host build + Stage-1 self-host readiness probe
        (see `docs/SELF_HOSTING.md`); probe currently reports 0/5 crates
        self-hostable, blocked by generics (`<`) and `mod` parsing gaps.
  - [ ] Verify behavior parity of the self-compiled binary
- [ ] **Stage 2 self-host: Moira-compiled-Moira compiles itself again**
  - [x] **Deterministic MIR/SSA output across stages**
    - Status: the MIR/SSA/C/native-ASM emit paths no longer iterate hash
      maps in emit order, so identical source yields byte-identical output
      regardless of which binary (stage-0 rustc-built vs. stage-1
      Moira-built) produced it. Concretely: `mir` now renders via a
      deterministic `Display` (the old `{:#?}` walked the `temp_types`
      `HashMap`, whose order is randomized per process); the C backend sorts
      `temp_types` before emitting declarations; the native backend assigns
      cross-block merge slots in sorted name order; and `lower` now publishes
      the inferred temporary types into `mir::Function::temp_types` (previously
      always empty). A new integration test (`tests/determinism.rs`) runs
      `moira --emit {mir,ssa,c,asm}` twice as *separate processes* (caching
      disabled via `MOIRA_NO_CACHE=1`) and asserts the two outputs are
      identical — the exact cross-stage condition.
  - [ ] Verify output parity (stage-1 == stage-2 binary)
- [ ] **Final cross-subsystem optimization pass**
  - [ ] Parallelize type-check + borrow-check passes
  - [ ] Incremental caching across the whole pipeline (not just expressions)
- [ ] Validate against a full Linux kernel build
- [ ] Validate against large crates.io dependency graphs
- [ ] Achieve ≥5x speedup over `rustc` on production-scale codebases
- [ ] Error message parity audit against `rustc`
- [ ] Stability guarantees / versioning policy
- [ ] Release packaging & distribution

## Cross-Cutting / Ongoing Tracks
- [x] Architecture documentation (`docs/ARCHITECTURE.md`)
- [x] Contributor guide (`CONTRIBUTING.md`)
- [x] Test suite growth alongside `rustc` compatibility work (added `lachesis` integration tests: type inference, trait resolution, borrow checking, incremental cache)
- [x] Performance regression tracking / dashboards
  - Status: `moira-bench` now supports `--record` (appends a run to
    `moira-bench-results/history.mbh` with commit + timestamp) and `--dashboard`
    (prints the run history and a per-run geomean). After each recorded run it
    compares against the previous run and flags any Moira compile-time increase
    ≥10% or geometric-mean speedup drop ≥10% (`results::detect_regressions` /
    `geomean_regression`). Pure comparison logic is unit-tested.
