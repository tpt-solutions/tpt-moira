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
- [x] Validate idiomatic standard-library-using Rust programs compile correctly
  - Status: the type + borrow checker now **accepts** idiomatic `std`-using
    programs for the modelled prelude — `Option`/`Result`/`Vec`/`String`/`Box`
    constructors, associated functions (`Vec::new`, `String::new`, `Box::new`),
    and methods (`push`, `len`, `is_empty`, `is_some`, `is_none`, `unwrap`,
    `push_str`, …). The prelude is bound into the checker (`lachesis::infer`
    `register_prelude` + a nullary `None` value) and, because `check` lowers to
    MIR + borrow-check, `lower.rs` now lowers `MethodCall`s (`recv.method(args)`
    → `method(recv, args)`) and string literals so these programs also pass the
    full front-end → MIR → borrow-check pipeline. Validated by the
    `std_*` tests in `lachesis/tests/lachesis.rs` (all 26 Lachesis tests pass).
    Remaining: full **codegen** of heap-backed `std` types (`Vec` storage,
    `String` data, `Box` allocation) — that is the documented Phase-4 follow-up;
    today the MIR uses placeholder operand types for those values.

### Known Phase-2 limitations (tracked follow-ups)
- ~~The Clotho parser discards `&` vs `&mut` mutability…~~ **Resolved:** mutability
  is now threaded from `clotho` (`ExprKind::Ref(Box<Expr>, bool)` + parser capture
  of `mut`) through `lachesis` (type inference produces `Ty::RefMut` for `&mut`)
  into the `atropos` MIR (`Rvalue::Ref(Operand, bool)`), and the NLL borrow
  checker (`lachesis::borrow`) now forbids a `&mut` from coexisting with any live
  borrow of the same place (second `&mut`, or a shared `&`). See
   `lachesis/tests/lachesis.rs` (`mutable_and_shared_borrows_rejected`,
   `sequential_mutable_borrows_are_accepted`).
- ~~The Clotho expression parser misinterprets `<`/`>` as generic-argument
  delimiters in value position…~~ **Resolved:** `parse_path` now takes an
  `allow_generics` flag; in expression context it no longer consumes a following
  `<` as generic args, so `while i < n`, `if a > b`, and `a < b` parse as
  comparisons via the Pratt loop (generic args are still parsed in type/bound
  position). This unblocked the cross-process `tests/determinism.rs` harness,
   which exercises `while i < n` and `if a > b`.
- The NLL analysis is CFG-reachability + liveness based (not a full SCC
  constraint solver); it catches the canonical violations but is intentionally
  conservative.

## Phase 3 (Months 17-24) — The Full Language
Target: full language support, matching `rustc` behavior.

- [ ] **Async/await state machine generation**
  - [x] Model `Future`/`Pin`/generator types in `lachesis::ty`
    - `lachesis::ty` defines `Ty::Future(Box<Ty>)` (models `Future<Output = T>`)
      and `Ty::Pin(Box<Ty>)`; `lachesis::infer` builds them for `async fn`
      returns (`Ty::Future(Box::new(sig.ret))`) and for `TypeKind::Future`, and
      `.await` requires a `Ty::Future` (also unwrapping a `Ty::Pin<Future<..>>`).
      Generator (coroutine) types are intentionally *not* a separate `Ty`
      variant: the async subset lowers to an explicit state machine
      (`atropos::lower::lower_async_function`), which is the concrete generator
      representation for the supported subset.
  - [x] Parse `async fn` and `.await` (parser + AST)
    - Parser/lexer already recognize `async`/`await`; the front-end parses
      `async fn` + `.await`, binds the wider output helpers (`print_i64`,
      `print_usize`, `print_f64`, `print_str` added to the `lachesis::infer`
      prelude and the Atropos C runtime), and the suspension points
      type-check. Validated by `lachesis/tests/async.rs`.
  - [x] Lower `async fn` to a suspending state-machine MIR (resume points)
    - Status: **Resolved.** `atropos::lower::lower_async_function` now emits a
      correct, runnable resume-point transform. Three bugs were fixed:
      1. the DONE epilogue (`store __out; __state = -1; return READY`) was
         being emitted while `self.current` still pointed at the *entry* block,
         which clobbered the `Switch` terminator and left the body block
         unreachable — the epilogue now lives in the body/continuation block
         and the entry block is solely the `state` dispatch `Switch`;
      2. `let` locals were double-declared (future-slot + body) — `lower_stmt`
         now skips re-declaring a slot already registered for the poll
         coroutine;
      3. the resume polling loop drove the nested future through a copy typed as
         the *outer* future struct and read its `__out` from a possibly
         uninitialized local — it now copies the stored future, polls it in a
         loop, and yields `awt._out` from a dedicated continuation block (so the
         suspension point is never clobbered by the next statement's terminator).
      Validated by `atropos/tests/async_codegen.rs` (entry switch shape, no
      unreachable blocks, DONE epilogue) and by compiling + running
      `demos/async_demo.rs`, which prints `23` (a=11, b=12, a+b).
  - [x] Codegen the state machine (C + native backends)
    - Status: the **C** backend now emits a runnable async state machine — the
      generated `double__poll`/`double__run` driver compiles (MSVC here; gcc on
      CI) and `demos/async_demo.rs` runs, printing `23`. (Two latent C-backend
      bugs were also fixed while wiring this up: void-returning calls such as
      `print_i64` are now emitted as bare statements instead of `t = print_i64()`
      assignments, and a `let x = Struct{..}` local now infers the struct type so
      `p.x` type-checks.) The native (`asm`) backend is not yet wired for async
      futures.

### Known Phase-3 limitations (tracked follow-ups)
- ~~The async state machine is fully runnable…~~ **Resolved:** `atropos::lower`
  `lower_async_function` now emits a correct, runnable resume-point transform —
  the entry block holds the `Switch` on `__state`, slots are not double-declared,
  and no block is left `unreachable`. `demos/async_demo.rs` lowers to MIR + C,
  compiles, and runs (prints `23`). The native (`asm`) backend still needs to be
  wired for the generated futures; the `.await` subset is linear (awaits only at
  the top level of an `async fn` body).
- [ ] **Complex/full macro expansion support**
  - [x] Parse `macro_rules!` definitions into a matcher/template model (`clotho::macrorules`)
  - [x] Token-tree matcher: `$x:frag`, fragments, and repetitions `$(...)*`/`+`/`?` (`clotho::macrorules`)
  - [x] Template expansion with repetition + nesting (`clotho::macrorules`)
  - [x] Integrate `macro_rules!` into the concurrent expansion pipeline (`clotho::pipeline::compile`)
  - [x] Attribute macros + `derive` macros — attribute parsing + built-in derive
        expansion is now wired end-to-end through the frontend.
    - The lexer already emits `#`, so attributes parse cleanly. `clotho::ast`
      gained an `Attribute { path, args }` type and an `attributes: Vec<Attribute>`
      field on `Item`, `StructDef`, and `EnumDef` (`ast.rs`).
    - `clotho::parser` now parses `#[...]` / `#![...]` attribute groups (capturing
      the name path and raw token args) and threads them onto items, structs, and
      enums; the old no-op `skip_attributes` placeholder is gone.
    - `clotho::derive::expand_derives` was previously dead code (the module was
      never declared in `lib.rs` and the pipeline never called it). It is now
      declared as `pub mod derive` and invoked from `clotho::pipeline::compile`,
      so `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Ord,
      PartialOrd)]` on structs **and enums** lowers to the corresponding trait
      `impl`s (enum derives now emit correct `match self { Self::Variant => ... }`
      arms; this required adding `Pat::Path` to the pattern AST and fixing a
      `parse_match` scrutinee ambiguity where `match self {` greedily consumed
      `self {` as a struct literal).
    - Validated by `clotho/tests/derive.rs` (all four tests now pass, including
      `attributes_are_retained_on_item` and `derive_on_enum_generates_impls`).
    - **Caveat:** the generated `impl` *method bodies* rely on method calls
      (`p.clone()`), which are outside the Phase-1 codegen/type-check subset
      ("method calls are not in the type-checked subset"). The frontend expansion
      is complete; lowering/calling derived methods waits on the Phase-2/3 trait
      and method-resolution work.
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
      (in `lachesis::solver`, wired into `check`) flags two `impl`s of the same
      trait whose self-types unify (concrete duplicates and overlapping generic
      blankets).
- [x] **Memoization + early-exit heuristics** — folded into the solver
  (`lachesis::solver` `SolveState::cache` goal cache + `MAX_DEPTH` depth limiter
  with early `Ambiguous` cutoff)
- [ ] **CI pipeline: full `rustc` UI/error-message test suite nightly**
  - [x] **`moira-diff` divergence harness foundation** — runs Moira `--check`
    and `rustc --emit=metadata` on a file and classifies divergences
    (Moira-accepts-but-rustc-rejects, rustc-accepts-but-Moira-rejects,
    diagnostics mismatch). See `moira --diff <file>`.
  - [x] Curate a Phase-1-subset slice of `rustc` UI tests + expected outputs
    - Curated an acceptance + rejection slice of the Phase-1 subset under
      `lachesis/tests/phase1-slice/{accept,reject}` (arithmetic, structs,
      control flow, references, `for` loops, nested calls; and type-mismatch,
      unknown-fn, mutation-while-borrowed, `bool + bool` negatives), validated
      by `lachesis/tests/phase1_slice.rs` (asserts each accept program
      type + borrow checks and each reject program is rejected).
  - [x] Nightly CI job running the slice and publishing a divergence report
    - `.github/workflows/ci.yml` `compatibility` job now runs on a nightly
      `schedule` (plus `workflow_dispatch`) and: (1) runs the slice acceptance
      test, (2) scans the accept slice through `moira --diff` against `rustc`
      and appends the divergence report to the job summary (divergences are
      tracked, not fatal).
- [ ] **AI-assisted diffing tool**
  - [x] `moira-diff` baseline (above) — the structured divergence data the
    AI layer will consume.
  - [ ] Attach an LLM to propose root-cause + minimal fix per divergence
  - [x] **Persist accepted expectations / known-divergence allowlist**
    - `lachesis::diff` gained an `Allowlist` type (line-based, human-editable
      `kind:identity` format with `path:`/`hash:` identities) plus `load`/`save`
      and `allows`/`insert` lookup by source path and content hash. `moira
      --diff` now consults `.moira-divergence-allowlist` (override with
      `--allowlist <file>`); known divergences are reported as `ALLOWED` and no
      longer fail the run, and `--update-allowlist` persists the divergences from
      a run. The default allowlist file is gitignored. Unit-tested in
      `lachesis/src/diff.rs` (path/hash match, round-trip, missing-file).
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
