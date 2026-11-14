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
- [x] Benchmark harness comparing Moira vs. `rustc` on the Phase-1 subset (`tpt-moira-bench`)
- [ ] Validate 10x speedup target for the subset
  - Status: harness extended and re-run; **10x is not yet met for the canonical
    front-end + SSA mid-end vs `rustc --emit=metadata` measurement (~7.0x
    geomean), but a full `rustc` build+link baseline shows ~17.9x geomean**, so
    the gap is in Moira's own mid-end/codegen rather than end-to-end wall time.
    Three measurements now exist, all recorded via `tpt-moira-bench --record`:
    * **front-end + SSA mid-end** vs `rustc --emit=metadata` → **~7.0x** geomean
      (5 programs: small/medium/large/xlarge/nested; battery grown with two
      larger synthetic programs to check the ratio vs. size — it holds ~6.7–7.2x
      across the size range, i.e. it does *not* degrade as programs grow).
    * **full Moira pipeline** (type-check + borrow-check + incremental cache, via
      `moira --emit check`) vs `rustc --emit=metadata` → **~7.36x** geomean.
    * **full Moira pipeline** vs a **complete `rustc` build+link** (new
      `--rustc-mode full`) → **~17.9x** geomean.
    Run `tpt-moira-bench` (default = first row), `tpt-moira-bench --full` (second), and
    `tpt-moira-bench --full --rustc-mode full` (third). The 10x target remains open
    for the canonical front-end+mid-end number; closing it depends on the
    Phase-4 final optimization pass (parallel codegen, arena tuning).
    - **Host floor (2026-11-15): 10x is not reachable by wall-clock on this host.**
      Every timed run is a process launch. Measured here: an empty C executable
      7.2 ms, `moira --emit ssa` on an empty file 8.8-9.5 ms, `rustc --emit=metadata`
      on a trivial file ~59 ms. With zero compile work the ceiling is therefore
      about 6.5-8x, and the bench now prints it (`wall-clock ceiling` line).
      The battery's own compile work is small. Per-stage profile of the xlarge program
      (`MOIRA_TIMINGS=1`, median, cache off): read+parse ~0.5 ms, lower+SSA ~0.9 ms,
      C emission ~2.7 ms (this includes a full type check to produce typed hints, ~1.4 ms).
      Halving all compile work would move the xlarge ratio from ~4.5x to ~5.5x, not 10x.
      **Decision (2026-11-15, per the recommendation):** keep wall-clock as the headline
      and report the floor-bounded ceiling beside it. The in-process compile-time metric
      is not adopted. The 10x target stays open and is blocked by the host floor.
  - [x] Extend `tpt-moira-bench` to time the *full* pipeline (type-check +
        borrow-check + incremental cache), not just front-end + SSA mid-end
        — `--full` now times `moira --emit check`; the table prints which
        pipeline is measured and `tpt-moira-bench --dashboard` tracks it.
  - [x] Add a `rustc` comparison mode that runs a full `cargo build`/`rustc`
        (not just `--emit=metadata`) so the baseline matches what Moira measures
        — `--rustc-mode {metadata,full}`; `full` runs `rustc -o <tmp> <file>`
        (native codegen + link), the real "compile this file" cost.
  - [x] Grow the Phase-1 benchmark battery with larger synthetic programs to
        see how the speedup ratio trends with size — added `xlarge` (200×200
        nested loops + float `mandel`) and `nested` (deep call nesting), so the
        battery spans small→xlarge. The ratio stays ~7x across the range.
  - [x] Re-run and record the geomean via `tpt-moira-bench --record`; update this
        entry with the final measured ratio against the 10x target — recorded
        runs live in `moira-bench-results/history.mbh`; see above for ratios.
  - Note: while wiring this, found and fixed a cache-poisoning bug in
    `tpt_moira_lachesis::pipeline_cache` — `checked` cached *failed* check results, so a
    single transient failure permanently rejected valid code. It now caches only
    successful checks (`pipeline_cache.rs`); covered by an updated unit test.


## Phase 2 (Months 9-16) — The Lachesis Integration
Target: compile standard, idiomatic Rust code safely.

- [x] Type inference engine (`tpt_moira_lachesis::infer` — direct, constraint-checking inference over the Clotho AST; literals default to `i32`, variables/structs/calls resolved from the environment and signatures)
- [x] Basic trait resolution (pre-specialization) (`tpt_moira_lachesis::traits` — a built-in trait DB; the checker queries operator-trait applicability for `+`, `-`, `*`, `/`, `%`, bitwise, `&&`/`||`, comparisons, and records the seam for the Phase-3 solver)
- [x] Full NLL borrow graph solver (graph-traversal algorithm) (`tpt_moira_lachesis::borrow` — runs over the Atropos MIR; builds the CFG from terminators, propagates reference aliases, and detects mutations-while-borrowed and references-to-locals escaping via reachability + liveness)
- [x] Fast-path borrow checker for simple/local scopes (functions with no references skip the analysis entirely)
- [x] Fallback logic: fast-path → full NLL graph analysis for complex/cross-function lifetimes (the fast path returns early; otherwise the full MIR liveness/alias analysis runs)
- [x] Integrate Lachesis with Clotho and Atropos end-to-end (`moira --emit check` runs type-check → lower → borrow-check; `--check` gates codegen; the SSA emit is backed by the incremental cache)
- [x] Expression-level incremental cache (disk-backed, content-addressed, Salsa-like) (`tpt_moira_lachesis::cache` + `moira --emit ssa` memoizes SSA text by source-hash under a temp cache dir)
- [x] Validate idiomatic standard-library-using Rust programs compile correctly
  - Status: the type + borrow checker now **accepts** idiomatic `std`-using
    programs for the modelled prelude — `Option`/`Result`/`Vec`/`String`/`Box`
    constructors, associated functions (`Vec::new`, `String::new`, `Box::new`),
    and methods (`push`, `len`, `is_empty`, `is_some`, `is_none`, `unwrap`,
    `push_str`, …). The prelude is bound into the checker (`tpt_moira_lachesis::infer`
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
  checker (`tpt_moira_lachesis::borrow`) now forbids a `&mut` from coexisting with any live
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

- [x] **Async/await state machine generation**
  - [x] Model `Future`/`Pin`/generator types in `tpt_moira_lachesis::ty`
    - `tpt_moira_lachesis::ty` defines `Ty::Future(Box<Ty>)` (models `Future<Output = T>`)
      and `Ty::Pin(Box<Ty>)`; `tpt_moira_lachesis::infer` builds them for `async fn`
      returns (`Ty::Future(Box::new(sig.ret))`) and for `TypeKind::Future`, and
      `.await` requires a `Ty::Future` (also unwrapping a `Ty::Pin<Future<..>>`).
      Generator (coroutine) types are intentionally *not* a separate `Ty`
      variant: the async subset lowers to an explicit state machine
      (`tpt_moira_atropos::lower::lower_async_function`), which is the concrete generator
      representation for the supported subset.
  - [x] Parse `async fn` and `.await` (parser + AST)
    - Parser/lexer already recognize `async`/`await`; the front-end parses
      `async fn` + `.await`, binds the wider output helpers (`print_i64`,
      `print_usize`, `print_f64`, `print_str` added to the `tpt_moira_lachesis::infer`
      prelude and the Atropos C runtime), and the suspension points
      type-check. Validated by `lachesis/tests/async.rs`.
  - [x] Lower `async fn` to a suspending state-machine MIR (resume points)
    - Status: **Resolved.** `tpt_moira_atropos::lower::lower_async_function` now emits a
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
- ~~The async state machine is fully runnable…~~ **Resolved:** `tpt_moira_atropos::lower`
  `lower_async_function` now emits a correct, runnable resume-point transform —
  the entry block holds the `Switch` on `__state`, slots are not double-declared,
  and no block is left `unreachable`. `demos/async_demo.rs` lowers to MIR + C,
  compiles, and runs (prints `23`). The native (`asm`) backend still needs to be
  wired for the generated futures; the `.await` subset is linear (awaits only at
  the top level of an `async fn` body).
- **Lexer: `break` keyword was swallowed by the `br"…"` raw-byte-string rule.**
  **Resolved:** `tpt_moira_clotho::lexer` only routes `b` + `r` to the raw-byte-string path
  when the third character is a quote (`"` or `#`); an identifier merely starting
  with `br` (e.g. `break`, `brotli`) now falls through to normal keyword
  identification. Previously `break` was mis-lexed as a malformed `br"…"` byte
  string, so `break;` failed to parse (and `while`/`loop` bodies containing it
  errored with "expected }, found end of file"). Fixed by adding the quote guard
  in `lex_ident_or_keyword`; regression-tested by `clotho/tests/frontend.rs`
  (`lex_keywords_and_idents` now asserts `break`/`continue`/`return` lex as
  keywords) and by `atropos/tests/codegen.rs` (`while_let_identifier_pattern_runs`
  exercises `break` inside a loop, compiling + running).
- [x] **Complex/full macro expansion support**
  - [x] Parse `macro_rules!` definitions into a matcher/template model (`tpt_moira_clotho::macrorules`)
  - [x] Token-tree matcher: `$x:frag`, fragments, and repetitions `$(...)*`/`+`/`?` (`tpt_moira_clotho::macrorules`)
  - [x] Template expansion with repetition + nesting (`tpt_moira_clotho::macrorules`)
  - [x] Integrate `macro_rules!` into the concurrent expansion pipeline (`tpt_moira_clotho::pipeline::compile`)
  - [x] Attribute macros + `derive` macros — attribute parsing + built-in derive
        expansion is now wired end-to-end through the frontend.
    - The lexer already emits `#`, so attributes parse cleanly. `tpt_moira_clotho::ast`
      gained an `Attribute { path, args }` type and an `attributes: Vec<Attribute>`
      field on `Item`, `StructDef`, and `EnumDef` (`ast.rs`).
    - `tpt_moira_clotho::parser` now parses `#[...]` / `#![...]` attribute groups (capturing
      the name path and raw token args) and threads them onto items, structs, and
      enums; the old no-op `skip_attributes` placeholder is gone.
    - `tpt_moira_clotho::derive::expand_derives` was previously dead code (the module was
      never declared in `lib.rs` and the pipeline never called it). It is now
      declared as `pub mod derive` and invoked from `tpt_moira_clotho::pipeline::compile`,
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
- [x] **Next-generation Chalk-like Prolog-style trait solver**
  - [x] **Solver foundation: `Goal`/`Clause` model + SLD-style resolution with
    inference-variable unification + memoization** (`tpt_moira_lachesis::solver`); backs
    the existing operator-trait checks via `TraitDb` and is the seam for the
    full solver.
  - [x] **Where-clauses / trait bounds on functions, structs, and impls** —
     `clotho` now parses generic param lists `<T: Bound>`, `where` clauses, and
     associated `type X = Y;` items (`ast::Generics`/`TypeParam`/`WherePredicate`
     /`TypeAliasDef`; `Type` carries generic args `Path(Path, Vec<Type>)`). The
     solver lowers `impl<T> Trait for Type<T> where T: Bound` into a clause whose
     body is the where-clause/bounds, and `tpt_moira_lachesis::check` runs a
      well-formedness pass (concrete `T: Bound` predicates must be satisfiable).
      Bounds on generic params of functions/structs are parsed and represented;
      full inference-context enforcement now lands for **functions** via the new
      `Ty::Generic` variant: a bare `T` in a signature/body resolves to
       `Ty::Generic("T")`, operator sites enforce the declared bound (`T: Add`
      for `+`), and generic call sites infer the concrete type arguments and
      check the bounds on the inferred types (see `tpt_moira_lachesis::infer`).
   - [x] **Struct generics (field types referencing the struct's own params)**
      — `StructInfo` now carries its `Generics`; field types resolve to
      `Ty::Generic` within the struct's scope, and a generic struct literal
      (`Foo { x: 1, y: 2 }`) infers the concrete type arguments from its field
      initializers and checks each field against the substituted type.
   - [x] **User-defined trait bounds enforced on generic call sites** — generic
      call sites now consult the Chalk-like `Solver` (seeded with the program's
      `impl` clauses) to enforce *user* trait bounds (e.g. `T: Show`), not just
      built-in bounds (`T: Ord`/`Add`). `TypeChecker` keeps a `Solver` built once
      per run; `check_call` queries `Solver::implements_trait(TraitId::User(..),
      concrete)` for non-builtin bounds (with `Int` normalized to `I32`, since
      integer literals infer as `Ty::Int`). Validated by the new `infer` unit
      tests `user_trait_bound_satisfied_is_accepted` /
      `user_trait_bound_violated_is_rejected`.
   - [x] **Applied generics in type annotations (`Ty::Apply`)** — a generic
      application like `Vec<i32>` or `Option<T>` in a type annotation or struct
      field now lowers to `Ty::Apply(name, args)`; `type_name`/`check_field`/
      `compatible`/`unify_ty`/`apply_sub` all handle it (matched by base name,
      arguments carried through). So `let x: Option<i32> = Some(5);`, a
      `Vec<T>` struct field, and `fn len<T>(v: Vec<T>) -> usize` all type-check.
      Validated by the new `infer` unit tests (`applied_generic_annotation_is_accepted`,
      `applied_generic_in_struct_field_is_accepted`, `applied_generic_field_enforces_type`,
      `generic_fn_with_applied_param_is_accepted`).
   - [x] **User-defined method resolution** — `impl` blocks are scanned in Pass
      1 and their methods collected into a `user_methods` table keyed by
      `(self_type, method_name)`. `receiver.method(args)` resolves against this
      table (with auto-ref/auto-mut-ref on the receiver, matching Rust), checks
      the receiver against the method's `self` parameter and the arguments
      against the rest of the signature, and the impl method bodies are checked
      in Pass 2.5 with `Self` and the impl's generics in scope (so `self`,
      `Self`, and `T` all resolve). Validated by the new `infer` unit tests
      (`user_method_call_is_accepted`, `generic_impl_method_uses_self_type`,
      `user_method_body_is_checked`, …).
  - [x] **User-defined `impl Trait for Type` clauses (impl search)** — `clotho`
     parses `impl Trait for Type { .. }` (`ast::ImplDef`); `lower` ignores it and
     the checker ignores it, so existing programs are unaffected. `lachesis`
     builds a `Solver` (`builtin_program` + user `impl` facts via
     `program_with_impls`) and `user_impl_implements(prog, trait, ty)` answers
     whether a type implements a user trait. Backed by unit + integration tests.
  - [x] **Associated-type normalization (simple `type` projections)** —
     `tpt_moira_lachesis::solver` adds `Term::Proj` + a `projections` table;
     `impl Trait for T { type Assoc = V; }` becomes
     `<T as Trait>::Assoc -> V`, resolved by `Solver::normalize` /
     `tpt_moira_lachesis::normalize_assoc_type` with generic self-type instantiation.
- [x] **Coherence / overlap checking among impls** — `tpt_moira_lachesis::coherence_report`
      (in `tpt_moira_lachesis::solver`, wired into `check`) flags two `impl`s of the same
      trait whose self-types unify (concrete duplicates and overlapping generic
      blankets).
- [x] **Memoization + early-exit heuristics** — folded into the solver
  (`tpt_moira_lachesis::solver` `SolveState::cache` goal cache + `MAX_DEPTH` depth limiter
  with early `Ambiguous` cutoff)
- [x] **CI pipeline: full `rustc` UI/error-message test suite nightly**
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
- [x] **AI-assisted diffing tool**
  - [x] `moira-diff` baseline (above) — the structured divergence data the
    AI layer will consume.
  - [x] Attach an LLM to propose root-cause + minimal fix per divergence
    - **Status: Resolved (offline-first).** `tpt_moira_lachesis::propose` adds an
      `LlmProposer` with a built-in, dependency-free heuristic that classifies
      each divergence into a coarse category (`type-mismatch`, `borrow-check`,
      `unknown-symbol`, `unknown-method`, `unresolved-trait`, `overflow`,
      `other`) and emits a root cause + minimal fix from keyword rules over the
      Moira/`rustc` diagnostics. A real LLM plugs in through
      `LlmProposer::with_backend` — the driver wires this to an OpenAI-compatible
      command via the `MOIRA_LLM_CMD` env var (reads the prompt on stdin, prints
      the reply to stdout). `moira --diff --propose` runs the proposer and prints
      the suggestions. Unit-tested in `lachesis/src/propose.rs`
      (`offline_handles_each_divergence`, `llm_backend_is_consulted`, …).
  - [x] **Persist accepted expectations / known-divergence allowlist**
    - `tpt_moira_lachesis::diff` gained an `Allowlist` type (line-based, human-editable
      `kind:identity` format with `path:`/`hash:` identities) plus `load`/`save`
      and `allows`/`insert` lookup by source path and content hash. `moira
      --diff` now consults `.moira-divergence-allowlist` (override with
      `--allowlist <file>`); known divergences are reported as `ALLOWED` and no
      longer fail the run, and `--update-allowlist` persists the divergences from
      a run. The default allowlist file is gitignored. Unit-tested in
      `lachesis/src/diff.rs` (path/hash match, round-trip, missing-file).
- [x] **Triage and fix flagged divergences continuously**
  - [x] Triage queue + labels; close subset-coverage gaps first
    - **Status: Resolved.** `tpt_moira_lachesis::propose` adds a `TriageQueue`
      (JSON-lines, no external deps — manually serialized/parsed) that records
      each divergence with a category *label*, the proposed root cause, the
      minimal fix, and the source hash. `moira --diff --propose` appends to
      `.moira-triage.jsonl` (gitignored). Validated by
      `propose::tests::triage_queue_persists_and_reloads`. Closing the
      subset-coverage gaps themselves remains the continuous follow-up below.

## Phase 4 (Months 25-30) — The Ouroboros & Production
Target: self-hosting, production-grade, ≥5x faster than `rustc` at scale.

- [ ] **Stage 1 self-host: Moira compiles its own full Rust source**
  - [ ] Reach full-subset coverage so Moira can compile `clotho`/`lachesis`/`atropos`
    - Status: Parser coverage was broadened further this session (see
      `docs/SELF_HOSTING.md` "Resolved this session"). Concretely fixed this
      session: tuple-literal parsing (a double-`expect` of the closing `)` in
      `parse_atom`), trailing-comma tuple patterns `(a,)`, method-call turbofish
      `::<T>`, enum **struct** variants `Variant { field: Type }`, `if let`/`while
      let` with any RHS (the expression-position `let` now parses its initializer
      with struct-literal disallowed so `if let P = path { .. }` does not swallow
      the then-block), `match` guards `Pat if cond => body`, **`impl Trait` in type
      position** (`impl` is now a keyword; `impl Into<String>` / `&mut impl
      FnMut(&str)` parse), tuple field access `expr.0` and tuple *types* `(A, B)`
      (the tuple-type branch also had the double-`expect` bug), and generic type
      aliases `type Pair<T> = (T, T);`. The **driver crate (`src/main.rs`) now
      parses fully** and reaches type-check (132 borrow/type errors). `clotho` and
      `atropos` each advanced past their `impl Trait` gate (and `codegen_c.rs`
      past tuple-index) and now hit the next construct in their own source:
`clotho`'s **byte literal** (`b'\n'`) blocker is **resolved** — the lexer's
     `lex_byte_char` handles `b'…'` and it parses + type-checks as `Char` (verified
     via `moira --emit check` on a `b'\n'`/`b'a'` program); `atropos`'s
     `format!("...", name = expr)` named-argument comma in `codegen_native.rs`
     remains an open parser/macro gap; `lachesis`'s `for` loop in `borrow.rs` is
     **resolved** — a `for x in arr` over an array now type- and borrow-checks
     cleanly (verified via `moira --emit check` on a `for`-over-array program and
     the existing `for_over_array_is_type_checked` / `for_over_array_literal_runs`
     tests). `moira-alloc` parses (25 type/borrow errors). The real remaining wall
     is the **type checker + code generator**,
     which still rejects `match` / closures / generics / tuples from the
     compiler's own code; closing that is a large, multi-session effort (tuple
     values end-to-end, `match`/`if let`/closures in `tpt_moira_lachesis::infer` + MIR,
     generics + `where`-clause solving, trait solving wired into `infer`).
     Validated by `clotho/tests/frontend.rs`, `lachesis/src/infer.rs`, and the
     `x.py self-host` probe.
  - [x] **Parser: close remaining gating constructs** (blocks `clotho`/`lachesis`/`atropos` from parsing at all)
        - Status: `x.py self-host` reports every crate's parse column as `ok` (all five crates parse, including the new `cfg`/alias/macro code). Closed in this pass: `const fn`, `-> !`, `dyn`/`impl`/`fn(..)` and qualified types, `where`/`for<..>` binders, `?Sized`/defaults/const generics, turbofish and `<<` openers, array-repeat and `[]`, struct/enum/match/literal attributes and `#[cfg]`, `#[path]`, `&&T`, `&raw`, leading `::`, block-like statements.
    - [x] Tuple expressions (`(a, b)`) and tuple types — **parser resolved**:
      the tuple branch in `tpt_moira_clotho::parser::parse_atom` double-consumed the
      closing `)` (surfaced as `expected ), found ;`); fixed so the `)` is
      consumed exactly once, and single-element / trailing-comma tuple patterns
      `(a,)` now parse. Method-call turbofish `.collect::<Vec<_>>()` also parses
      now. Tuple *values* end-to-end (type-check + MIR + C/native codegen) remain
      under the Type-checker task below.
    - [x] Array indexing (`arr[i]`) and `for`-over-array/slice
          - **Status: Resolved.** `arr[i]` indexing was already end-to-end
            (parser → `ExprKind::Index` → `Ty::Array` inference → `Rvalue::Index`
            MIR → C `base[idx]`). `for x in [..]` and `for x in let`-bound arrays
            now lower to an index loop (`tpt_moira_atropos::lower` `ExprKind::For`): the
            array lowers to an inline `long long tN[len] = {..};` (or a
            function-`let`-bound array declared directly, since C cannot assign
            arrays by value), and the body binds `x = arr[idx]` under a `0..len`
            counter. The type checker (`tpt_moira_lachesis::infer` `ExprKind::For`) binds
            the loop variable to the array element type and rejects non-ranges /
            non-arrays. Validated by `atropos/tests/codegen.rs`
            (`for_over_array_literal_runs`, `for_over_let_bound_array_runs`,
            `array_index_and_let_bound_array_run`,
            `for_over_array_lowers_to_index_loop`) — all compile + run via a real
            C compiler — and by `lachesis/tests/lachesis.rs`
            (`for_over_array_is_type_checked`).
    - [x] `mod` declarations / multi-file module resolution
          - **Status: Resolved.** `tpt_moira_clotho::compile_file` resolves `mod name;`
            (`ModuleKind::External`) into the sibling `name.rs` (or `name/mod.rs`)
            via `flatten_modules` in `clotho/src/lib.rs`; inline `mod { .. }` is
            already flattened. The driver wires source files through
            `compile_file`, so the frontend parses the compiler's own crate
            layout. The remaining self-host gap is the type checker / codegen
            (not module resolution).
    - [x] Fix the unclosed-delimiter parse failure in a `lib.rs` test file
          - **Status: Resolved.** All crate `lib.rs` / root files now tokenize
            and parse cleanly (no unclosed-delimiter errors). The remaining
            parse-stage failures on the compiler's own source are type-check
            depth issues (`expected expression, found :`/`=>` on generic/
            `match`-pattern syntax), not delimiter mismatches — see the
            `match` / generics / closures tasks below. The driver routes real
            source files through `tpt_moira_clotho::compile_file`, which recursively
            flattens `mod` declarations.
    - [x] Re-run `x.py self-host` probe after each fix and track crate-by-crate parse pass/fail in `docs/SELF_HOSTING.md`
      - Status: **Resolved (2026-08-22).** `x.py`'s `probe_crate` now runs a
        dedicated parse stage (`moira --emit ast`, which only lexes + parses +
        macro-expands and never invokes the Lachesis checker or Atropos lowering)
        to set `parse_ok` independently of `check_ok`/`codegen_ok`, and the
        report renders separate Parse/Check/Codegen columns. The 2026-08-22 probe
        confirms **all five crates parse cleanly** (`parse_ok == true` for
        `moira-alloc`/`clotho`/`atropos`/`lachesis`/driver); `docs/SELF_HOSTING.md`
        is updated with the corrected, separated per-crate parse/check/codegen
        matrix and the current per-crate type/borrow error counts
        (`clotho` 1985, `lachesis` 2033, `atropos` 1667, `driver` 196, `alloc` 25).
  - [ ] **Type checker / MIR: support constructs the compiler's own source uses**
    - [x] Tuple values end-to-end (`tpt_moira_lachesis::infer` typing + `atropos` MIR lowering + codegen)
          - **Status: Resolved (construction + numeric field access).** Tuple
            literals `(a, b, ..)` and numeric field access `t.0` now work
            end-to-end on the C backend. `tpt_moira_lachesis::infer` already inferred
            `Ty::Tuple`; `check_field` now resolves `t.0`/`t.1` to the element
            type and rejects out-of-range indices. `tpt_moira_atropos::lower` lowers
            `ExprKind::Tuple` to a generated anonymous `struct __tuple_<ctys>`
            (`Rvalue::Tuple`), records the struct into the MIR, and maps field
            access `t.N` to `.fN`; `codegen_c` emits the struct def and the
            `(struct __tuple_..){ .f0 = .. }` literal. Validated by
            `lachesis/tests/lachesis.rs` (`tuple_values_type_check`) and
            `atropos/tests/codegen.rs` (`tuple_lowers_to_generated_struct`).
            The native ASM backend does not model tuples yet (it rejects
            `Rvalue::Tuple`); that is the documented Stage-4 follow-up.
            - Remaining: tuple-pattern destructuring in `for (x, y) in ..` for
              *`let`-bound* (non-literal) arrays of tuples — literal array-of-tuples
              destructuring is now resolved (see below); the `Pat::Tuple` parse path
              exists; the binding lowering for non-literal arrays is not yet wired.
            - [x] Tuple-pattern destructuring in `for (x, y) in array_of_tuples`
              (the `Pat::Tuple` parser path already existed; the binding is now wired
              end-to-end for array literals)
              - **Status: Resolved (array-literal tuples).** `tpt_moira_lachesis::infer::infer_block`
                now binds a `for` pattern via `check_pattern` + `bind_pattern`, so
                `for (a, b) in [(1, 2), (3, 4)]` binds each component to the array's
                element type. `tpt_moira_atropos::lower` routes the per-iteration element
                through the new `lower_bind_pattern`, which reads each component from
                the element's generated `.fN` field (`Rvalue::Field`). The loop-var
                type is now derived from the array element (so tuple-element arrays
                bind correctly) instead of being hardcoded to `long long`.
                Validated by `tpt_moira_lachesis::tests::for_over_tuple_array_destructure_type_checks`
                and `tpt_moira_atropos::tests::codegen::{for_over_tuple_array_destructures_and_runs,
                for_over_tuple_array_lowers_to_field_reads}`.
            - [x] Tuple-pattern destructuring in `let (a, b) = ..` (the `Pat::Tuple`
              parser path already existed; the binding is now wired end-to-end)
              - **Status: Resolved.** `tpt_moira_lachesis::infer::infer_block` now runs the
                local pattern through `check_pattern` + `bind_pattern` (which already
                handled `Pat::Tuple` for `match`), so `let (a, b) = t` binds each
                component to its `Ty::Tuple` element type. `tpt_moira_atropos::lower` gained
                `lower_tuple_destructure`: a tuple-literal RHS is decomposed
                element-wise (`a = 1; b = 2;`), while any other RHS is lowered once
                into a tuple value and each binding reads its generated `.fN` field.
                `type_to_ty` now maps `TypeKind::Tuple` (so `let (a, b): (i32, i32) =
                ..` is honored) and `compatible` compares tuples element-wise.
                Validated by `tpt_moira_lachesis::tests::tuple_destructure_type_checks` and
                `tpt_moira_atropos::tests::codegen::{tuple_destructure_lowers_and_runs,
                tuple_destructure_from_call_lowers_and_runs,
                tuple_destructure_literal_lowers_to_direct_assignments}`.
    - [x] `match` expressions in `tpt_moira_lachesis::infer` + MIR lowering (beyond the existing `Pat::Path` derive-macro subset)
          - **Status: Resolved (common subset).** `tpt_moira_lachesis::infer` now type-checks
            `match` via `infer_match`: the scrutinee is inferred, each arm's
            pattern binds its names into the arm-body scope (`bind_pattern`
            handles `Ident`/`Wild`/`Ref`/`Or`/`Tuple` of identifiers), and arm
            bodies aggregate to a common result type (rejecting mismatches).
             Unsupported patterns (`Struct`) are rejected with a precise
             diagnostic. `tpt_moira_atropos::lower` lowers `match` to a conditional chain
             (`lower_match` + `pattern_guard`): literal arms become `Eq`
             comparisons driving `If`/`else` blocks; identifier / wildcard / path /
             or / tuple-pattern arms are irrefutable and bind the scrutinee (or
             tuple element `fN`) before the body; **range arms** (`lo..hi`,
             `lo..=hi`, `lo..`, `..hi`, `..`) become bounded `>=`/`<=`/`>`/`<`
             comparisons combined with `&&`. Validated by
             `lachesis/tests/lachesis.rs` (`match_expressions_type_check`,
             `range_patterns_type_check`) and `atropos/tests/codegen.rs`
             (`match_lowers_to_conditional_chain`,
             `range_pattern_lowers_to_comparison_and_runs`,
             `exclusive_and_open_range_patterns_run`).
            Output is cross-process byte-identical
            (`tests/determinism.rs`).
           - **Caveat:** enum-variant destructuring patterns (`Some(x)`,
             `Ok(x)`) are not yet supported (the `Pat::Path` node is treated as an
             irrefutable binder, not a variant test) — that remains the enum
             destructuring follow-up; struct/tuple-variant field patterns are
             rejected. A `match` may also be a tail/return expression (the parser
             was fixed to treat a trailing `match` like an `if`-as-tail rather than
             always forcing a `;` statement).
    - [x] `if let` / `while let` in the type checker and MIR (parser support already landed — see Phase-2 "Resolved" note above)
          - **Status: Resolved (identifier patterns).** `tpt_moira_lachesis::infer` now detects
            the `If(Block{Local})` / `While(Block{Local})` shape (how the parser
            represents `if let`/`while let`), binds the pattern name in the
            then-block / loop-body scope, and type-checks the branch. `tpt_moira_atropos::lower`
            lowers the binding to a `long long` local and desugars the branch to an
            unconditional `if (1) { .. } else {.. }` (irrefutable) / `while (1)
            {..}` (looping until `break`). Non-identifier patterns (`if let
            Some(x) = opt`) are rejected with a precise diagnostic pointing at the
            `match`/enum task. Validated by `lachesis/tests/lachesis.rs`
            (`if_let_and_while_let_identifier_patterns_type_check`) and
            `atropos/tests/codegen.rs` (`if_let_identifier_pattern_runs`,
            `while_let_identifier_pattern_runs` — both compile + run via a real C
            compiler).
          - **Caveat:** enum-variant patterns (`Some(x)`, `Ok(x)`) are not yet
            supported — that is the `match`/enum destructuring task below.
     - [ ] Closures: parsing, capture typing, and MIR/codegen lowering
            - **Parsing resolved (2026-08-21).** The parser did *not* actually
              recognize closure bodies: `parse_pat` folded the closure's closing `|`
              into a top-level OR-pattern (`|a| body` was read as the single pattern
              `a | body`), so any closure with a body failed with "expected
              expression". Fixed by splitting `parse_pat` into `parse_pat_single` (no
              OR-pattern continuation) used for closure parameters, while `parse_pat`
              keeps the OR-pattern loop for `match` arms. Closures with block bodies,
              expression bodies, multiple params, trailing `-> Ret`, and closures
              nested in call args (`map(|e| e + 1)`) now parse. Regression-covered by
              `clotho::tests::frontend::parse_closures`.
            - **Status: Partial (parsing + type-check acceptance + capture analysis).**
              `tpt_moira_lachesis::infer::check_closure` exists: parameters are typed from their
              annotations (defaulting to `i32`), the body is checked in a scope binding
              each parameter, and the closure's type is the function type
              `Ty::Fn(param_tys, ret_ty)`.
              **Capture analysis is now implemented** (`closure_free_vars` /
              `bind_pat_names` in `tpt_moira_lachesis::infer`): the free variables the body
              references from the enclosing environment are computed and, because the
              C/native backends do not yet lower `ExprKind::Closure` with captures, a
              closure that captures variables is rejected at type-check with a precise
              diagnostic naming exactly the captured variables (e.g.
              `closures that capture variables are not yet supported in the codegen
              subset (captured: a, b)`). Non-capturing closures continue to be
              accepted. Validated by the `infer` unit tests `non_capturing_closure_is_accepted`,
              `closure_capturing_variable_is_rejected_with_precise_message`,
              `closure_referencing_function_name_is_accepted`,
              `closure_capturing_multiple_variables_lists_all`,
              `nested_closure_capture_is_detected`. MIR/codegen lowering of closures
             remains open.
             - **Status (2026-11-13, verified by probe programs through `moira --emit c`
               + gcc):** capturing closures now lower and run for let-bound closures
               called directly, closures passed as `fn(..)` arguments, closures nested
               inside closures, and closures created and called inside a `while` loop.
               A function that **returns a non-capturing closure** (`fn make() -> fn(i64)
               -> i64 { |x| x + 1 }`) now emits valid C: the function-pointer return
               declarator is wrapped around the name (`R (*make(..))(P)`), temporaries
               and call results of `Fn<..>` type take that type, and the call result is
               bound as a function pointer. Locked in by
               `atropos/tests/codegen.rs::returned_non_capturing_closure_runs`.
               **Returned capturing closures, `let`-bound call sites (2026-11-14):**
               `fn make(k) -> impl Fn(i64) -> i64 { move |x| x + k }` with `let f =
               make(4);` is inlined at the call site by `tpt_moira_clotho::closure_return`
               (arguments become fresh `let` bindings the closure captures), and the
               function is removed once unreferenced. Conservative scope: the candidate
               body must be only a closure over identifier parameters, the closure body
               must bind no names and contain no macros, and the call must be a `let`
               initializer. Locked in by `atropos/tests/codegen.rs::
               returned_capturing_closure_runs_when_bound_by_let`.
               Argument position: `apply(make(4), 1)` is hoisted to `let __hoist_make_N =
               make(4);` before the statement when the arguments are pure (literals,
               names, arithmetic on them), then inlined the same way. Locked in by
               `returned_capturing_closure_passed_as_argument_runs`.
               Inside `println!`/`print!`/`eprintln!`/`eprint!`/`format!` arguments the
               call is hoisted at the token level (`hoist_macro_tokens`), so
               `println!("{}", apply(make(4), 1))` works too. Locked in by
               `returned_capturing_closure_inside_println_runs`.
               **Impure arguments (2026-11-14):** `apply(make(side()), 1)` is hoisted when
               the statement is `f(.., g(impure..), ..)` with a single impure operand, i.e.
               nothing else in the statement can run before the hoisted call and evaluation
               order is preserved (`impure_hoist_ok`). Locked in by
               `atropos/tests/codegen.rs::returned_capturing_closure_with_impure_argument_runs`.
               Impure arguments with other effectful operands in the same statement are still
               not hoisted, to keep evaluation order.
               **Eagerly evaluated built-in macros (2026-11-14):** calls inside `assert!`,
               `assert_eq!`/`assert_ne!` (and `debug_` variants) and `vec!` are hoisted from the
               token stream like the format macros. Locked in by
               `atropos/tests/codegen.rs::returned_capturing_closure_inside_assert_runs`.
               User `macro_rules!` and `matches!` are still not hoisted (lazy or repeated
               evaluation is not known statically).
               **Still open:** (c) closures returned from impl methods,
               from functions with statements or generics, or through a struct field or
               `Vec` are not inlined, and a remaining capturing `make` call in those
               positions still emits uncompilable C.
    - [x] Full generics + `where`-clause solving wired into `infer` for real (non-toy) generic code
      - **Status: Resolved for the type-checking layer (2026-08-22).** `infer`
        now handles, end to end: multi-parameter generics on structs and
        functions (`struct Pair<A, B>`, `fn swap<A, B>(p: Pair<A, B>) ->
        Pair<B, A>`), generic call-site type-argument inference with
        substitution through the return type, bounds declared inline
        (`<T: Show>`) **and** in `where` clauses (`fn f<T>(t: T) -> i32 where
        T: Show` — `check_call` now enforces where-predicates against the
        solver's impl search exactly like inline bounds, rejecting call sites
        whose inferred concrete types violate them), and trait-bound method
        calls on generic receivers (see the solver-wiring entry below).
        Supporting fixes: string literals coerce to `&str` parameters and
        receivers (`s.len()` resolves via the new prelude `str` methods);
        primitive receivers report their source-level names so `impl i32`
        inherent methods resolve. Locked in by `multi_param_generics_and_struct_swap`,
        `where_clause_bound_method_resolves`,
        `where_clause_bound_violation_is_rejected`,
        `string_literal_coerces_to_str_ref`, `str_len_method_resolves`.
        Remaining (out of scope here, tracked by the self-host entries):
        substituting generic arguments *inside* struct-typed returns at field
        access (fields of a `Pair<B, A>` result still read as bare
        `Ty::Generic`), const generics, GATs, and higher-ranked lifetime
        bounds.
    - [x] Wire the Chalk-like trait solver (`tpt_moira_lachesis::solver`) into `infer`'s method-resolution path so trait-object/generic-bound method calls in the compiler's own code resolve
      - **Status: Resolved (2026-08-22).** `TypeChecker` now keeps a
        `trait_methods` table keyed `(trait_name, method_name)` populated from
        `impl Trait for Type { .. }` blocks in Pass 1. When `receiver.method(args)`
        matches no inherent/prelude method, `infer::resolve_trait_method`
        consults the solver: a **generic receiver** (`Ty::Generic("T")`)
        resolves against the signatures of traits named in `T`'s bounds
        (`<T: Show>` or `where T: Show`, via `generic_bounds_of`); a
        **concrete receiver** resolves via `Solver::implements_trait` impl
        search over every candidate trait declaring that method (candidates in
        sorted, deterministic order; `Ty::Int` normalized to `i32`). Also fixed:
        `Self` in an impl over a primitive (`impl Show for i32`) now resolves to
        the primitive `Ty` rather than a struct named "i32". Locked in by
        `trait_bound_method_call_resolves_via_solver`,
        `concrete_receiver_trait_method_resolves_via_impl_search`, and
        `trait_method_without_matching_impl_is_rejected_for_concrete_receiver`.
  - [ ] **Coverage tracking**
    - Status (2026-11-15): probe `1/5` (`moira-alloc`). The driver's checker now passes:
      its two `&str`-for-`&Path` errors are fixed, since `&str` coerces to `&Path` and
      `PathBuf` is modeled as `str`. `std::mem::take(&mut place)` is typed as the place's
      type and lowered for `Vec` locals and fields (tests in
      `tpt-moira-lachesis/tests/mem_take_run.rs`). `moira-alloc`'s arena was rewritten as
      chunked storage, and its `Vec::len`/`is_empty` uses became loops, since a method path
      as a function value is not yet checked. Remaining own C errors: clotho 235,
      atropos 1206 (with 139 checker errors), lachesis 1256 (88 checker), driver 202.
      See `docs/SELF_HOSTING.md` for the matrix.
    - Status (2026-11-15, later): **floats were miscompiled**. A float literal was
      truncated to an integer, and float locals and temporaries were `long long`, so
      `2.5` printed `2` and `f64` arithmetic was integer arithmetic. Fixed in the C
      backend and lowering: `mir::Operand::Float` (exact bits), `double` temporaries and
      locals, and a shortest round-trip `Display` (`moira_f64_format`) that matches rustc
      (tests: `tpt-moira-lachesis/tests/float_run.rs`, checked against `rustc` output). The
      native x86 backend has no float support: a float there traps (`ud2`). `format!` of a
      pointer, struct, or `f32` is now a lowering error instead of an integer.
      Also fixed: derived `Clone` of a `Box` payload (`Box::new((**a).clone())`), which
      stored a struct where a box was needed (test: `tpt-moira-lachesis/tests/box_clone_run.rs`).
      Probe now: clotho C 230, atropos 1215 (check 143, up 4 from an untraced cause), lachesis
      1250 (check 88), driver 209 (check ok).
    - [ ] Re-run `x.py self-host` after each milestone above; update the crate-by-crate self-hostable count (the `mod` and generics-`<` parse gaps are resolved — remaining blockers are type-check depth: `match`/closures/generics-in-`infer`/tuple-destructure)
          - Status (2026-11-15, re-run): `x.py self-host` reports `1/5` self-hostable
            (`tpt-moira-alloc` only). Clotho's C had 234 `gcc -fsyntax-only` errors at the
            start of the session and 229 after these fixes: `is_some_and` on `Option`,
            `Vec<String>::contains` (content comparison helper), and a `format!` of a
            non-scalar now fails in lowering with a named error instead of printing the
            value's bits as an integer. Tests: `tpt-moira-atropos/tests/codegen.rs`.
          - Status (2026-11-15, Display): user `impl Display` now works for `format!` and
            `write!`/`writeln!`. `Formatter` is a `fmtbuf` (a `char**` buffer pointer),
            `format!` of a value whose type has a `Display` impl calls its `fmt` into a fresh
            buffer, and `fmt::Result` is a status code. `println!` of a user `Display` type
            still uses the old path and is a named lowering error. Also fixed: an unclosed
            `impl`/`struct` body hung the parser at EOF; it is now a parse error. Clotho now
            lowers and emits C, with 223 `gcc -fsyntax-only` errors, the same class as before
            (raw method calls on values whose type the lowering can't see, and Option/Vec
            helper gaps).
          - Status (2026-11-15, later): clotho's C is at 222 errors. Fixed: unannotated
            `Vec::new()` takes its element type from later pushes (the checker keeps waiting
            while the element type has unknowns); `mem::take` on a vector local; `&arr` and
            `&[..]` passed to slice parameters. Largest remaining groups: values assigned
            from a fallback `0` where a struct is expected (e.g. `entry(..).or_insert_with`
            on a map whose value is a tuple struct), raw method calls on receivers with no
            known type (`next`, `iter`, `map`, `filter`, `take`), and Option/struct field
            access on fallback temporaries. Atropos and lachesis still fail the checker. Tests: `codegen.rs` (`user_display_impl_formats_through_format_and_write_runs`,
            `unclosed_impl_is_a_parse_error_not_a_hang`). Workspace `cargo test` is green.
          - Status (2026-08-22): the `x.py self-host` probe now measures parse
            independently of type-check (`moira --emit ast` sets `parse_ok`;
            `moira --check` sets `check_ok`; `moira --emit c` sets `codegen_ok`), so
            the per-crate matrix tracks parse pass/fail as required. The 2026-08-22
            run reports `0/5` self-hostable, but **all five crates parse cleanly**
            (`parse_ok == true`), confirming the `impl FnMut(...) -> Ret` in type
            position and block-bodied `match` arm parse blockers are resolved. The
            remaining gate is the type checker + code generator. `docs/SELF_HOSTING.md`
            is refreshed with these numbers each probe run. Run with
            `python x.py self-host` (use the `python` launcher; `python3` is the
            Windows Store alias and is unavailable).
          - **Re-run (2026-11-14, later): still `1/5`; type-check errors fell in every crate.**
            clotho `33 -> 18`, atropos `169 -> 152`, lachesis `121 -> 114`, driver `3`.
            Fixes that landed: prelude signatures for `Vec`/`Option`/`bool`/`char`/`String`
            methods the compiler source calls; a struct literal of a generic struct keeps its
            solved type arguments (`Spanned<i64>`, not bare `Spanned`); solver impl heads
            match instantiated generic structs (`Apply`), and impl bounds on builtin traits
            (`T: PartialEq`) resolve to the built-in solver facts; a trait method's bare
            `-> Self` resolves to the receiver (`clone` on a `T` field returned the impl's
            bare struct, which monomorphized to `Spanned__Spanned`); `#[derive]` on a generic
            struct bounds each parameter by the derived trait, as rustc does; bare
            generic-struct operands and bounds are lenient, like `Ty::Unknown`.
            Still blocking, with root causes recorded:
            (1) **Option of reference: done (2026-11-14).** Backend support for `Option<&T>`
            landed: `println!` of a reference to a scalar derefs it; `Vec::first`/`last` (and
            slice `first`/`last`) are typed and lowered as `Option<&T>` with a pointer
            payload, like `get`; `copied()`/`cloned()` on a pointer payload dereference only
            on the `Some` path; `&base.field` borrows the field in place for any pointer-typed
            base (a call, say); a closure parameter of reference type is a pointer in the
            lifted function (checker records parameter types under their pattern spans, kept
            apart from expression hints). Regression tests: `typed_lowering.rs::
            references_to_scalars_print_and_option_refs_run` and
            `closure_borrowing_a_field_through_option_ref_runs`.
            **Latent escapes exposed and fixed in clotho:** the borrow check never ran on the
            clotho crate while `Lexer::finish_utf8` failed to lower (the lowering error aborted
            the crate first), which hid real dangling references. Fixed: `frag_stoppers`
            returns an owned `Vec` (it returned `&[..]` of a temporary array), and a
            `matches!` on `slice.last().map(|t| &t.kind)` was rewritten as a `match`. The
            lexer's `finish_utf8` no longer uses `iter_mut().take().skip()`: it reads the
            continuation bytes with a `match` on the length and decodes the code point from
            them (the input is a `&str`, so the bytes are valid UTF-8).
            Known gap, not fixed: a user program that returns a reference to an array literal,
            `fn f() -> &'static [i64] { &[1, 2, 3] }`, is rejected by the escape check (the
            array is a local temporary). HEAD rejects it too. Static storage for literal
            arrays is needed to accept it.
            (1b) **Module-scoped std `Path` (done, 2026-11-14).** `tpt-moira-clotho/src/std_imports.rs`
            resolves the name per file, before flattening: a file that imports
            `std::path::Path` (and defines no `Path` of its own) has its type positions
            rewritten to `PathBuf`, which is a `str` in the checker and in the backend. The AST's
            own `Path` is untouched. No public API changed. Unit tests in that module.
            Also done for clotho's C: `PathBuf` is `char*`; the `str` prelude has path methods;
            `&Fn` is not a pointer to a pointer; map instances behind a pointer are emitted;
            `HashMap::default()` lowers to an empty map; hints that name an unsolved generic
            struct are dropped (the backend cannot name an uninstantiated struct).
            **Where clotho's C stands:** `gcc -fsyntax-only build/clotho.c` gives 642 errors
            (`python x.py self-host` shows only the first). They are mostly untyped `long long`
            temporaries assigned to pointer and struct values (about 300), unmodeled methods
            (`HashMap::entry`, the `Option` combinators `ok_or_else`/`unwrap_or_default`/`take`,
            `Vec::{pop,push,truncate,to_vec}`, `str::{parse,..}`, `Iterator::{zip,position,next}`),
            and clotho's own functions called before they are declared. The std surface still
            needed: `mem::take`/`replace`, `fs::read_to_string`, `fs::File`, `io::Read`/`Error`,
            `fmt::Formatter::write_str`, `sync::atomic::AtomicUsize`, `str::from_utf8`.
            **Probe reports error counts (2026-11-14, after the C keyword escape below):** clotho 593,
            atropos 1374, lachesis 1775, driver 267. The probe compiles each crate's own source, so
            the escape pass (`escape_c_names`, which renames C-reserved names such as a function
            `double`) costs atropos 64 errors: its `match` over `Rvalue` with or-patterns is the
            same shape the subset does not lower (`rename_rvalue` already has 62). Fixing that
            match shape in the lowering recovers those.
            **Update (2026-11-14, later):** the Rvalue matches now name `mir::Rvalue::...`
            (commit "atropos: write Rvalue patterns with their full path"). A pattern whose path
            goes through a `use ... as` alias (`R::Use(a)` with `use mir::Rvalue as R`) was not
            resolved: its bindings took the types of same-named variants of other enums. The
            lowering should resolve aliased paths; until then, write full paths in the source.
            Atropos is now 1255 errors. Its largest group is `unknown type name 'Expr'` (225),
            with `Spanned__String`, `Pat`, `AliasTable`, `clotho_Block` and others: the AST types
            of clotho, which the per-crate probe never defines, because `--extern` is for
            checking only. `--bundle` defines them, but it emits the extern crates too, so
            atropos with its externs is 5683 errors: a different measurement.
            **Decision made: option 2 (2026-11-14).** The probe bundles each crate with its
            Cargo path dependencies only (`crate_dependencies` in `x.py`), and attributes each
            gcc error to the crate or to an extern crate: an error inside a function named
            `<crate>__...` is the extern crate's; a top-level error on a line that names one of
            the extern crate's items is theirs; everything else is the crate's own. A crate's
            `codegen_ok` depends only on its own errors. The report gives both counts.
            Current (own / extern): clotho 525 / 0; atropos 2434 / 499; lachesis 2586 / 3016;
            driver 246 / 5457; moira-alloc has no dependencies and is self-hostable.
            **Progress (2026-11-14, later).** `Result::map_err` is typed and lowered (commit
            89cc005). The lowering now knows extern structs' field types, and the borrow check
            lowers with its extern crates (commit 4af9b77). Current (own / extern): clotho 518 / 0;
            atropos 1408 / 492 (check: 158 errors, of which the first three real borrow findings
            are now visible: `cannot borrow out as mutable more than once` in `emit_asm`, and two
            "reference to local escapes" in lifted closures of `Lower::lower_stmt` and
            `Lower::lower_tuple_destructure`); lachesis 1399 / 2050; driver 225 / 3235.
            The largest own group is now `request for member 'tag'` on values the lowering types
            as `long long` (scrutinees of matches on extern-crate values, most likely).
            **Later the same round.** `Vec::retain` is typed and lowered (in-place compaction;
            `request for member 'tag'` falls from 267 to 25). `wrapping_*` integer methods lower
            to the C operators. Current (own / extern): clotho 518 / 0; atropos 1402 / 492;
            lachesis 1398 / 2045; driver 225 / 3229.
            The remaining own errors are mostly downstream of implicit declarations (99 distinct
            names in atropos). Still unmodeled, by family: Option combinators (`unwrap`,
            `unwrap_or`, `unwrap_or_else`, `unwrap_or_default`, `then_some`, `take`); string
            methods (`split`, `split_at`, `starts_with`, `strip_suffix`, `trim_end_matches`,
            `trim_end`, `to_string` on an unknown receiver); slice/iterator methods (`zip`,
            `split_last`, `to_vec`, `values`, `with_capacity`); `saturating_*` (typed, not
            lowered). The driver's `spawn`/`scope` need thread support in the subset.
            **Option combinators (2026-11-14, later).** Lowered: `unwrap_or_default`, `Result::ok`,
            `Option::ok_or_else`, `Option::take` (local or field), `bool::then_some`. Commits
            ae83088 and 6bf7a21. Counts (own / extern): clotho 503 / 0; atropos 1379 / 481;
            lachesis 1383 / 2014; driver 222 / 3182. Still open: the literal in `|| 4` gets the
            checker's default `i32` even where the context says `i64` (`ok_or_else(|| 4)` against
            `Result<i64, i64>` mismatches in C); the checker should take the expected type of a
            closure's result from the context.
            **Option typing (2026-11-14, latest).** The checker types `Option::{as_ref, as_deref,
            or_else, filter, and_then}`; `as_ref` lowers to the address of the payload, and
            `map` takes the payload type of its expression when the closure's C type is only the
            default (commit 88196de). Own errors: clotho 510, atropos 1365, lachesis 1381,
            driver 222 (sum 3478, from 3487). Not lowered yet: `filter`, `and_then`, and
            `or_else` with a closure that returns `None` (its body has no hint).
            **Closures (2026-11-14, latest).** `and_then`, `filter` (closure gets a pointer to the
            payload) are lowered; `or_else` and `ok_or_else` retype the lifted closure and the
            value it returns to the type the call expects. A closure literal takes the annotated
            type (`let e: Result<i64, i64> = b.ok_or_else(|| 4)`). The probe counts the crate's
            own source too, so each new helper costs a few C errors in its crate (lachesis +7 for
            `refine_closure_result`, the same effect as the escape pass before); write new code
            in the subset's shape where that matters.
            **Iterators (2026-11-14, latest).** The bare `map` calls are gone: chains already lower
            when a `for`, `collect`, `any`/`all`/`find`/`count`/`sum`/`for_each` consumes them.
            `max`, `min` and `last` are now terminals too, for scalar sources (commit 12205cd).
            Still bare: chains with `zip`, `filter_map`, `flat_map`, `rev`, `take_while`, and
            `position`, and any terminal over a non-scalar element. The large unhinted groups
            are `iter` receivers (`x.iter().something` with a non-chain consumer), `entry`
            (HashMap entry API: `or_insert`, `or_default`, `or_insert_with`), `last_mut`, and
            string splitting (`split`, `trim_end_matches`, `parse`).
            Measurement note: new lowering and checker code is compiled by the probe as part of
            its crate, so each addition costs a few own errors (atropos +4 for this step).
            Own errors now: clotho 510, atropos 1353, lachesis 1377, driver 223.
            **HashMap entry API and compound assignment (2026-11-14, latest; commit 2bbdd00).**
            `entry(k).or_insert(v)`, `.or_default()` (scalar and string values) and
            `.or_insert_with(f)` are typed and lowered; each map instance gets an
            `entry_or_insert` helper. `place op= rhs` on a field, element or dereference is
            lowered as `place = place op rhs` (before it was lost: it wrote a temporary).
            Own errors: clotho 511, atropos 1376, lachesis 1378, driver 223. The rise in
            atropos (+23) is the new lowering code, compiled as part of atropos.
            **Later (2026-11-14).** Done: `entry(k).or_default()` for vector values and
            `.push(x)` on an entry slot (0146152); `Vec::first_mut`/`last_mut` (e76b3aa);
            `entry(k).and_modify(f)` (e044c1c). Own errors now: clotho 503, atropos 1386,
            lachesis 1376, driver 223.
            Still open: `values_mut`, and `Option::take` on a receiver that is not a place.
            **Clotho's checker: 4 errors left (2026-11-14, commit 2683e14).** Found by mapping the
            diagnostic offsets to source (the flattened spans: root file at 0, then each `mod`
            file in depth-first order, each reserving its length + 1): `alias.rs` `substitute`
            (`expected &Type, found Type`: `(*arg).clone()` on a `&&Type` lookup); `parser.rs`
            `tokens.last().map(|t| t.span.end)` (`expected &Item, found Item`, the offset may sit
            in a nearby item); `std_imports.rs` (`expected bool, found &bool`, likely the closure
            over `on`); `resolve.rs` `expr` or-pattern (`cannot dereference a non-reference`).
            Each is a reference/value rule in the checker, not a missing method.
            **Why only moira-alloc is self-hostable (2026-11-14, measured).** A crate is
            self-hostable when it parses, checks, and its own C compiles (with its dependencies
            bundled). Per crate, as of commit a453d14:
            - clotho: check 4 errors; own C 502 errors (untyped temporaries, unmodeled std APIs:
              `AtomicUsize`, `read_to_string`, `File`, `mem::take`, `fmt`).
            - atropos: check 135 errors; own C 1385 (the largest share: its lowering source).
            - lachesis: check 118 errors; own C 1386.
            - driver: check 2 errors; own C 220. It uses threads for its large stack (outside the
              codegen subset); the two checker errors are `&Path` parameters of callees in
              lachesis modules whose cross-module rewrite has not been located yet.
            **Clotho first (2026-11-14).** Added a type-propagation pass before C emission
            (`tpt-moira-atropos/src/retype.rs`, commit 39eea12): temporaries take the type of
            their definition (call result, field, index, reference, struct literal), a returned
            value takes the return type, and a default return type takes the pointer or struct
            type it returns. Clotho's own C: 502 -> 407 errors; atropos 1360 (bundled), lachesis
            1372, driver 220. What remains in clotho's C, by kind: (1) methods on pattern
            bindings with no hint (`item.is_cfg_test()` in a match arm, emitted as a bare
            `is_cfg_test(item)`); (2) iterator chains with a `zip` or `map(fn)` source or a
            `collect` into a map (`params.iter().map(|p| p.as_str()).zip(args.iter()).collect()`,
            emitted as bare `iter`/`map`/`zip`/`collect`); (3) some `int` results assigned to
            tuple and map temporaries (`t = 0` for a `Tuple`); (4) closure arguments whose type
            differs from the parameter's (`expand_depth(depth, self, t)`). Next: (1) and (2) are
            lowering coverage; (4) is a closure-type mismatch in the caller's hint.
            **Pattern-binding methods (1d802d2).** Done: a method on a variable with a known C type
            (a match-arm binding, say) resolves to the struct's method. Own C: clotho 406,
            atropos 1359, lachesis 1367, driver 218.
            **Zip stage (2026-11-14).** Done: `iter().zip(y)` / `zip(y.iter())` in an iterator
            chain. `iter_chain_of` accepts the other sequence; the chain loop reads it through its
            own cursor, stops at the shorter sequence (the cursor's bound is an early exit from the
            loop), and pairs each item as a two-field tuple. Pair closures after the stage take
            their components from a `Tuple<..>` hint (a `(a, b)` pattern of two names was always
            untyped before). `max`/`min`/`last` over a zip stay bare (`chain_source_is_scalar`
            is false for them). `lower_iter_chain` is an index `while` loop now: a `for` over
            `.zip(..)` is outside the codegen subset. Regression test:
            `iterator_zip_pairs_items_and_stops_at_shorter_run`. Own C: clotho 399, atropos 1297,
            lachesis 1288, driver 211; checker counts unchanged (atropos 137).
            **Function-item adapters (2026-11-14).** Done: `map(f)` / `filter(f)` with a named
            function `f`; a function whose first parameter is a pointer gets the item's address
            (`fn_item_takes_pointer`). `collect` into a map from a zip chain works (test
            `iterator_zip_collects_into_map_runs`). Own C: clotho 398, atropos 1294, lachesis 1280,
            driver 211; checker counts unchanged (atropos 137).
            Still open for clotho: `map` with an associated function item (`map(T::f)` through
            `closure_callee`'s second branch is untested here), and a zip followed by
            `max`/`min`/`last` (bare call today).
            **Checker diagnostics name their file (2026-11-14).** `compile_file_mapped` records
            each loaded file's span base, path and text in a `SourceFile` list; the driver prints
            `at path:line:col` under every located checker error (`locate_span`). A global
            registry was tried first and reverted: a `static Mutex` and `..` slicing are outside
            the codegen subset. Using the locations, clotho's four checker errors were fixed
            with checker-compatible rewrites (`alias.rs` subst `while` loop, `&body`;
            `parser.rs` `&item`; `resolve.rs` `*is_root` and `p.0`/`&p.1`), each with the
            clippy `needless_borrow` allow where Rust itself would not need the borrow.
            **Clotho's check now passes** (exit 0; probe: check ok). Own C rose 398 -> 412:
            four of the new errors name the new functions, the rest come from their `while`
            loops and `Option` handling. Next: clotho's C kinds, led by `Option` initialisers
            (16), subscripted non-arrays (14), and `ExprKind` assigned from `int` (13).
            **Retype: tuple temporaries (2026-11-14).** `retype.rs` now types a tuple literal's
            temporary as its generated struct. Clotho's own C count did not move (412): the
            failing initialisers in `parse_rep_tmpl` are the tuple's fields `.f2 = kind`, where
            the local `kind` is declared `long long` although it holds a `RepKind` (a `match`
            binding that got the default type). That is a lowering typing gap for pattern
            locals, the next thing to fix there.
            **`let x = None` takes the payload type (2026-11-14).** The checker's hint for a
            `None` initialiser stayed the bare `Option` (its payload was only known from a later
            `x = Some(..)`), so the local was declared `struct Option`. `pending_option` now
            refines the local and its initialiser on that assignment, as `pending_vec` does for
            `push`. Test: `let_none_takes_payload_type_from_later_assignment_runs`. Own C:
            clotho 412 -> 399. Also: `check_program_cx` built `TypeChecker` with `..Default::default()`,
            which the checker rejects as `..base` and reports as a missing field for every
            field (about 30 errors); it now uses `default()` and assigns the one field.
            Lachesis checker: 118 -> 89. Clotho's check still passes.
            **Diverging match arms in blocks (2026-11-14).** An arm `=> { return x; }` produced
            a placeholder `0` for the match's result, which C rejects where the result is a
            struct (`ExprKind`). `arm_diverges` now looks through a block's tail or last
            statement, as the direct `return`/`break`/`continue` check already did. Test:
            `match_arm_block_that_returns_yields_no_value_runs`. Own C: clotho 399 -> 380.
            **Lexer: UTF-8 (2026-11-14).** `finish_utf8` reads its continuation bytes into
            scalars (an array in a `match` arm is not lowered as an array, which broke the
            `buf[..]` subscripts): clotho C 380 -> 368. Also a real bug: a normal string literal
            appended each byte as a `char`, so `"é"` lexed as two characters; it now decodes
            the sequence (`lex_string`). Test: `multi_byte_characters_in_string_literals_decode`.
            **Expected payload types of `return` and tails (2026-11-14).** `return Ok(None);`,
            `return Ok(Some(3))` and a function body's tail `Ok(Some(3))` now take the declared
            return type: a call in those positions gets it as its expectation, and an argument
            left untyped (`None`, an unsuffixed literal) takes the payload type of the expected
            `Option`/`Result`. Test: `return_ok_none_takes_declared_payload_runs`. Own C:
            clotho 368 -> 360.
            **Closure literals behind `&mut` (2026-11-14).** `&mut |x| ..` lowered to a
            temporary typed `long long` holding a function pointer, and a pointer to it. A closure
            literal is now passed as its function pointer. Test:
            `closure_literal_passed_by_mut_reference_runs`. Own C: clotho 360 -> 339. Next kind:
            `visit_expr` argument types (6+6+6), which are the closure parameter types of its
            callers, and `struct Option` initialisers (9).
            **Capturing closures were wrong under Moira (2026-11-14).** `mentions_any` and
            `rename_safe` set a captured flag inside a `visit_expr` callback. A capture is passed
            by value, so the flag never reached the caller: both results were wrong once compiled
            by Moira (`rename_safe` always true). The walk now threads its state explicitly:
            `visit_expr_with`/`visit_block_with` pass `&mut S` to every callback, and the two
            callers keep their flags there (`MentionScan` holds the names too). Also a real
            capture-analysis bug: `bind_pat_names_in` ignored variant, reference, or-, struct and
            `name @` patterns, so a name bound by `Slot::Val(v)` inside a closure body was taken
            for a capture of an outer `v`. Test: `pattern_bound_name_in_closure_body_is_not_a_capture_runs`
            (fails without the fix). Own C: clotho 339 -> 322.
            **Closure arguments take the `Fn` parameter types (2026-11-14).** A closure passed to
            a user function takes the parameter types of the callee's `impl Fn(..)` parameter
            (`closure_param_hint` at the call's arguments). An `Option<i64>` parameter was typed
            as the bare `struct Option` before. Test: `closure_argument_takes_fn_parameter_types_runs`.
            Clotho's C count is unchanged (322): none of its closure arguments reach this case.
            **Call arguments and variant payloads (2026-11-14).** A call argument that is a call
            (`Vec::new()`) takes its parameter's concrete type as expectation (`has_generic`
            keeps generic parameters open). A tuple variant's constructor takes its declared
            payload types as parameters, except generic ones and user generic struct instances
            (they are named differently at a constructor, `Spanned__String`, so they stay open).
            Test: `variant_payload_constructor_takes_declared_type_runs`. Own C: clotho 322 -> 305;
            lachesis checker 89 (unchanged).
            **`if` branches (2026-11-14).** A bare `None` in one branch of an `if`/`else` takes the
            other branch's type, and its tail records it (`record_tail_type`), as `match` arms do.
            Test: `if_branch_none_takes_other_branch_type_runs`. Own C: clotho 305 -> 274.
            **Reference to a box (2026-11-14).** `Option<&Box<T>>` payloads and closure parameters
            of that type were typed with an extra pointer (`Expr**`); a box is already its
            pointer, so `hint_elem_name` is used without the `*`. Also the macro expander passes
            `&**b` for boxed sub-expressions (the lowering takes `&Box<Expr>` as a pointer to a
            pointer). Own C: clotho 274 -> 252.
            **Reference to a string (2026-11-14).** `&s` / `&n.text` for a string (`str`, C
            `char*`) is the string's pointer, as a slice is; it was a `char**`. Test:
            `reference_to_string_field_reads_its_text_runs`. Own C: clotho 252 -> 250.
            **Open:** `&b` for a boxed local `b: Box<P>` lowers to `P**` (a box and a reference are
            both pointers in C, so the lowering cannot tell them apart without the checker's type).
            A test for `Option<&Box<P>>` was tried and removed for this reason.
            **Open:** `format!` of a user type with a `Display` impl (`{}` on a `TokenKind`) is
            lowered as an integer (`moira_int_to_string`): the impl's `fmt` is not called.
            **Open:** an unsuffixed literal payload (`Some(5)`) in an `if` initialiser gets the
            default `i32`: an `if` does not pass an annotation's expectation to its branches, and
            an unannotated `let` is not unified with the return type later.
            **Note:** `tpt-moira-lachesis/src/infer.rs` was edited by another process during this
            session (a `is_int_ty` helper and a body-tail change appeared and were removed
            again). The committed change is only the hunks listed above; that edit was not kept.
 each crate uses the std
            surface and the typed-temporary lowering broadly, and each new feature also adds
            its own source to the probe. The route that worked for the four checker-side
            blockers (path types, threads, `use` aliases) is to remove std surface from each
            crate where the code allows it, then model what remains.
            Remaining families: string methods (`split`, `split_at`, `starts_with`,
            `strip_suffix`, `trim_end_matches`); slice and iterator methods (`zip`, `split_last`,
            `to_vec`, `values`, `with_capacity`); `saturating_*`; and receivers that have no
            checker type (chained calls the checker leaves `Unknown`).
            These numbers are not comparable with the earlier per-crate ones: the bundled unit
            now type-checks the extern types, so errors that were hidden behind `unknown type
            name 'Expr'` appear. The largest own group is `request for member ... not a
            structure` (851): the root's code reads fields of extern structs, and the lowering
            does not know those fields' C types across crates. That is the next target.
            Previously: clotho 592, atropos 1310, lachesis 1779, driver 270 `gcc -fsyntax-only` errors on the emitted C (the driver's
            large-stack thread, below, adds 7 that the backend cannot lower yet). Fixed: diverging match
            arms (`=> return x`) no longer assign a placeholder to the result; a function-typed
            reference to a user struct hints its pointer.
            **The atropos stack overflow was stack depth, not recursion (located 2026-11-14).**
            LLDB showed `lower_expr`/`lower_expr_inner` nesting with large frames (`__chkstk`
            in every frame), so the default 1 MiB main stack ran out. The driver now runs the
            compiler on a 256 MiB thread (`src/main.rs`). Atropos checks and lowers again.
            Pointer hints in `hint_applies` (`apply_hint`) were measured with that fixed: they
            left clotho at 632 and made the driver worse, so they stay out.
            **Method calls on reference parameters (2026-11-14).** A method on a variable whose
            checker type is a reference to a struct (`|c| c.bump()`, `|p| p.parse_expr()`) got
            no hint, so the call was emitted as a bare `bump(c)`. `lower_user_method` now falls
            back to the variable's pointer type. Clotho 632 -> 608, atropos 1318 -> 1312,
            lachesis 1789 -> 1784. Test: `typed_lowering.rs::method_call_on_closure_reference_parameter_runs`.
            Tried and reverted: taking a plain variable's C type as the hint for Option/Result
            methods (atropos +4, clotho unchanged).
            **Method-call fallbacks, counted (2026-11-14).** Clotho's bare-call fallbacks (a method
            with no lowering, emitted as `name(recv, args)`): 160 calls, of which 71 have no
            checker hint on the receiver and 89 have one but no lowering for the method.
            Hinted but unlowered, by count: `contains` 11, `to_vec` 10, `is_some_and` 6, `last_mut`
            5, `join` 5 (done now), `entry` 5, `ok` 4, `map_err` 4, `iter` 4, `to_path_buf` 3
            (done), `parent` 3, `ok_or_else` 3, `is_empty` 3 (done for Vec), `display` 3 (done),
            `unwrap_or_default` 2, `truncate` 2 (done), `pop` 2 (done), `into_iter` 2, `insert` 2.
            Unhinted, by count: `map` 16 (iterator adapters, whose checker type is `Unknown`),
            `push` 11, `as_ref` 6, `unwrap` 4, `or_default` 4, `len` 3, `contains` 3.
            Done this round: `join`, `to_path_buf`, `display` on paths (`moira_path_join` C helper);
            `Vec::{is_empty, truncate, pop}` lowered in place (`lower_vec_mut_method`), `Vec::pop`
            typed as `Option<T>` in the checker. Clotho 608 -> 592, atropos 1312 -> 1310,
            lachesis 1784 -> 1779, driver 272 -> 270. Test: `typed_lowering.rs::vec_pop_truncate_and_is_empty_run`.
            Correction (2026-11-14): the checker already types iterators (`Ty::Apply("Iter", [item])`,
            `check_iter_method`); the unhinted `map` calls are receivers the checker types as
            `Iter` whose hint is absent (`hint_of_ty` has no arm for `Iter`), and chains that
            the lowering does not lower outside `for` loops and `collect` into a `Vec`/map. Some
            pass a function item (`args.iter().map(map)` in alias.rs), which the lowering does not
            accept as a closure. Next: lower iterator chains as a whole (a source, then adapters,
            then a terminal), and accept a function item as an adapter argument.
            (2) atropos and lachesis: the 152 and 114 remaining checker errors are the
            std-library surface (`PathBuf`/`Path` methods, `AtomicUsize`, `Formatter`,
            `str::from_utf8`, `String::replace`, `read_to_string`) and `Item`/`&Item`
            mismatches; each needs a prelude entry plus a backend lowering.
          - Re-run (2026-08-22, post solver-wiring): still `0/5` self-hostable;
            all five crates parse; type-check/codegen gaps unchanged. The new
            trait-bound method resolution does not yet flip a crate — the
            remaining blockers are the ones listed above (`match` lowering,
            closures in MIR/codegen, full generics inference, tuple
            destructure).
          - **Re-run (2026-08-22, later): `1/5` self-hostable — `moira-alloc`
            flipped.** Enabled by: `#[cfg(test)] mod tests` blocks are now
            skipped by the type checker, borrow checker inputs, and codegen
            (attributes propagate through module flattening in
            `clotho::flatten_items`; `Item::is_cfg_test()`); the prelude gained
            `RefCell::new`/`.borrow`/`.borrow_mut`, `Vec::last`, and
            `str::to_string`; two-segment paths (`Arena::default()`) now fall
            back to inherent-impl methods; and receivers whose type is
            `Unknown` absorb method calls silently instead of cascading
            diagnostics. Remaining blockers for the other four crates are the
            large type-check debt (hundreds of diagnostics each) plus codegen
            gaps (`for` over iterators, closures, enum destructuring).
          - Re-run (latest, fixes to the checker and derive): still `1/5`, but the
            diagnostic load fell: clotho 857 -> 620, atropos 702 -> 686, lachesis
            1136 -> 985. Closed: enum `derive(Clone)` on payload variants generated
            `Self::V => Self::V` (now shape-aware); `matches!(e, pat)` desugars to a
            real `match` (its pattern was checked as a bitwise OR); inherent impls no
            longer report coherence conflicts; panic-family macros diverge (`!`).
            Remaining top classes are std API modeling (`no method`, `cannot find
            value`, indexing) and trait bounds.
            Also: nested items (`fn` / `struct` inside a body) are hoisted to the top
            level when their names are unique; clashing ones are dropped (a call to
            them is reported, not silently rebound). The C backend now emits a
            prototype for every function, so forward calls compile. Latest counts:
            `vec![..]` / `vec![x; n]` type as `Vec<T>` (heap storage is still not in
            the codegen subset). Glob-imported unit variants (`use Ty::*;`) resolve as values
            and patterns (matching codegen). Lenient prelude entries (`into`, `cloned`,
            `as_ref`, `clone` on primitives, `String::push`) return `Unknown`, which can hide
            real type errors. Indexing types `Vec`, slices, arrays, `String`/`str` (ranges
            slice) and maps; slice methods also serve `Vec` through auto-deref. Latest
            counts: clotho 269, atropos 309, lachesis 220. Bare unit-variant names prefer
            the program's own enums over extern crates' (a `Str` no longer resolves to
            `TokenKind`); `impl Into<T>` parameters accept convertible arguments.
            Struct and struct-variant patterns (`E::B { x, y: p, .. }`, `ref`/`mut`
            shorthand) parse and bind their names; tuple patterns bind nested elements;
            the arities of the lenient prelude methods now follow the std signatures.
          - Re-run (2026-08-22, later still): `1/5` holds. Path/const
            resolution landed: multi-segment call paths
            (`std::env::args()`, `tpt_moira_clotho::compile_file(...)`)
            resolve against their **last two segments** (module prefixes are
            not modeled); top-level `const`/`static` items are registered as
            values and literal consts fold to constant operands in codegen
            (`lower.rs` no longer rejects `const`/`static` items); associated
            constants (`ExitCode::FAILURE`) resolve via the new
            `assoc_consts` table; the prelude gained `to_string` on numeric
            primitives and `Vec::first`/`contains`. Driver type/borrow errors
            fell 158 → 115; `atropos` 1548 → 1369. The driver's dominant
            remaining class is real (non-test) macro use — 37 sites of
            `format!`/`write!`/`println!`-family — which needs macro
            type-checking.
          - **Enum codegen landed (2026-08-22).** Enums lower to tagged structs
            (`{ long long tag; long long f0; }`): variant construction
            (`Some(5)`, `None`, user `MyEnum::Var(x)`) becomes a struct
            literal, `match` arms with variant patterns become tag-test guards
            with `f0` payload projections, and `if let`/`while let` now accept
            variant patterns (previously rejected outright). The prelude
            `Option`/`Result` variants are registered even without `enum`
            items; user `enum` definitions register their own variants. Also
            fixed a latent invalid-C bug: struct-typed locals were declared
            twice (once as a local, again as a "temp" — `lower` publishes
            local types into `temp_types`), so the C backend now skips temp
            declarations whose names are already declared. Verified
            end-to-end: `enum_match_and_if_let_run` compiles and runs a
            `match`/`if let Some(x)` program printing `7 0 8`. Remaining
            codegen blockers for the large crates: `for` over iterators and
            macro expansion.
          - **Builtin print macros landed (2026-08-22).** `println!` /
            `eprintln!` / `print!` / `eprint!` are now type-checked with real
            re-parsed arguments (`Parser::parse_macro_args` splits the raw
            token stream on top-level commas) and lower to interleaved
            `print_str`/`print_i64`/`print_f64` runtime calls — the format
            string splits on `{}` placeholders, each placeholder consumes the
            next argument with the printer chosen from its C type, and the
            `ln` variants append a newline. `format!` type-checks as `str`
            and evaluates its arguments (the value is the string placeholder —
            no heap string runtime yet). A new `mir::Operand::CStr` carries
            string-literal segments through MIR/SSA. Driver type/borrow errors
            fell 115 → **98**; its remaining classes are std-surface modeling
            (`ExitCode`/`args().skip().collect()` iterator chains, `write!`)
            and `format!` value construction.
          - **Cascade absorption + std surface round (2026-08-22, final).**
            Field access on an `Unknown` base is now absorbed (like method
            calls and deref), killing whole cascade families. The prelude
            gained builder-style `Command` methods (`args`/`arg`/`status`/
            `output`), `Option::{map, unwrap_or, unwrap_or_else, as_deref}`,
            `Stdio::piped`, `fs::{temp_dir, remove_file, open}`,
            `env::{temp_dir, current_dir}`, `File::open`,
            `Vec::from_utf8_lossy`, a free `write`, and registered the modeled
            std types as field-less structs (so `let v: Vec<u8> = …` resolves).
            Driver errors 98 → **69**; clotho 1721, lachesis 1409, atropos
            1412. The driver's remaining errors are dominated by **cross-crate
            resolution** (`tpt_moira_atropos::lower_program(...)` — items from
            other workspace crates are invisible to the single-crate checker)
            and iterator chains, both of which need new infrastructure rather
            than prelude entries.
          - **Cross-crate resolution landed (2026-08-22).** New
            `--extern name=lib.rs` CLI flag (repeatable) plus
            `TypeChecker::check_program_with` /
            `lachesis::check_with_externs`: each extern crate contributes its
            top-level function signatures, impl methods, and struct shapes
            (recursing through inline modules; only signatures, bodies are
            not re-checked). Multi-segment call paths now also try the
            crate-qualified `crate::item` form, so
            `tpt_moira_atropos::codegen_c::emit_c(...)` and
            `tpt_moira_clotho::compile_file(...)` resolve. The self-host probe
            passes every other workspace crate as an extern automatically.
            With externs the driver's errors shift from resolution failures
            ("cannot find function") to **real type mismatches** against
            accurate signatures (`&Path` vs `PathBuf` auto-coercion, closure
            arguments, array-ref parameter types) — 84 errors, all of them
            genuine language-modeling gaps rather than missing symbols.
          - **Deref coercions landed (2026-08-22).** `compatible` now models
            Rust's `Deref`-driven argument coercions: `String`/`&String` →
            `&str`, `PathBuf`/`&PathBuf` → `&Path`, `Vec<T>`/`&Vec<T>` →
            `&[T]`, and `Path::new` now returns `&Path` (matching real Rust).
            The fs/file prelude signatures take `&Path`. Driver errors with
            externs registered: 84 → **71**. Remaining classes are generic
            callables (`F: FnMut()` bounds on extern signatures), closure
            capture validation, iterator chains, and match-arm aggregate
            mismatches — each needing individual modeling work.
          - **Closure captures landed (2026-08-22).** Capturing closures are
            now accepted by the type checker (previously rejected outright)
            and lowered correctly at codegen: captured variables become
            *leading parameters* on the lambda-lifted function, and direct
            calls through a bound local (`let add = |x| x + base; add(5);`)
            are rewritten to pass the captured operands
            (`__moira_closure_0(base, 5)`). Locals bound to closures are also
            now callable in the checker (`resolve_call` synthesizes the
            closure's own signature). Verified end-to-end:
            `capturing_closure_direct_call_runs` compiles and runs a
            capture-and-call program printing `105 120`. Documented
            limitation: a capturing closure *passed to another function*
            still lowers to its bare pointer (an environment struct is a
            Phase-4 follow-up). Driver errors 71 → **64**; clotho 1699,
            lachesis 1353, atropos 1346.
          - **Assignment-is-unit fix + subset-friendly driver (2026-08-22).**
            Assignment expressions now type as `()` (they previously typed as
            the assigned value, making `match x.as_str() { _ => flag = true }`
            arms disagree with unit arms — 5 errors in the driver alone).
            `str::split_once`, `Path::to_path_buf`/`PathBuf::to_path_buf`,
            `Stdin::read_to_string`, and `String::from_utf8_lossy` joined the
            prelude, and the driver's own `--extern` handling was rewritten to
            stay within the self-host subset (no iterator adapters, no `?` on
            `Option`, no capturing closures) — dogfooding the compiler's own
            target language. Driver errors 64 → **54**.
          - Fixed the flaky `content_addressed_cache_round_trips` test: its
            cache directory was keyed only by pid, so a stale prior run (or a
            concurrent test sharing the pid) could pre-insert the asserted
            absent key. The directory is now unique per invocation
            (pid + nanos); two consecutive full-suite runs pass cleanly.
          - **Driver gap-closing round (2026-08-22, this session).** Driver
            type/borrow errors fell **54 → 1**; with externs registered,
            lachesis 1354 → 1221, atropos 1346 → 1154, clotho 1699 → 1631.
            Landed:
            * **Macro token capture fix (clotho)**: nested delimiters inside a
              paren-delimited macro (`println!("{}", v.len())`) were consumed
              as depth markers and dropped from the captured token stream, so
              `v.len()` re-parsed as a *field* access ("no field `len` on type
              `Vec`"). Nested `(`/`)` are now kept as content.
            * `for x in &vec` / `for x in vec` accepted by the checker
              (auto-ref iteration; element typed `Unknown`), and an `Unknown`
              iterator is absorbed instead of erroring.
            * Ranges now infer as `Ty::Range` (were `()`), fixing `for` over
              computed ranges.
            * Prelude additions: `Result::{and_then, map_err, map, ok}`,
              `Args::{skip, collect}`, `Vec::{filter, map, collect}`,
              `String::as_str`/`contains`, `str::{as_bytes, contains}`,
              `Cow::{trim, lines, to_string}`, `Path{,Buf}::{to_string_lossy,
              display}`, `Command::{stdin, stdout, stderr, spawn}`,
              `Child::wait`, `ExitStatus::success`, `Stdio::{null, inherit}`,
              `PathBuf::{new, from}`, `String::from`, `Vec::from`,
              `env::var`, `process::id`, `fs::write`; `Stdin::read_to_string`
              now takes `&mut String`; `Command::new` takes `&str`.
            * Associated functions used as values (`list.map(Path::new)`)
              resolve to their function type.
            * Associated constants in `impl` blocks (`Allowlist::
              DEFAULT_FILE`) resolve, locally and from extern crates.
            * Same-named structs across extern crates are *merged* (field-set
              union) instead of overwritten — `clotho::ast::Program` no longer
              clobbers/loses to `atropos::mir::Program`.
            * Extern impl-method signatures carry the method's own generics;
              generic parameters in extern signatures (`F`) are accepted at
              call sites without full site inference, and a generic return
              models as `Unknown`.
            * `Err(e)` patterns bind `e` to the **error** type (`args[1]`),
              not the Ok payload — `if let Err(diags) = ...` no longer types
              `diags` as `()`.
            * Unannotated closure params infer `Unknown` (absorbing) instead
              of the `i32` default that caused "no method `contains` on i32".
            * Function-local `use …` items are a no-op in the checker and
              lowering (was "nested items are not supported").
            * Atropos lowering: `if let` / `while let` now support variant
              patterns via the same discriminant-test machinery as `match`
              (`pattern_guard`); identifier patterns remain always-taken.
            The driver's single residual diagnostic is the codegen-subset
            restriction "`for` iterator must be a range/array" (iterating a
            modeled `Vec` needs the heap-Vec runtime — a Phase-4 codegen
            follow-up); its *type-check* pass is otherwise clean.
          - **Second gap-closing round (2026-08-22, continued).** With the
            same extern sets: clotho **1631 → 985**, lachesis **1221 →
            1021**, atropos **1154 → 1100**. Landed:
            * Nullary enum variants register as associated constants
              (`TokenKind::Fn` used as a value resolves), including items
              nested in inline modules (Pass 1 now flattens inline `mod`s).
            * `Self::Variant` constructor expressions and `Self { .. }`
              struct literals resolve against the enclosing impl's self type
              (derive-generated `clone()` uses both shapes).
            * `matches!` type-checks its arguments and yields `Bool`.
            * Prelude: `String::is_empty`, `str::chars`, `Vec::with_capacity`,
              `mem::take`, `saturating_add/sub/mul` on all numeric primitives
              (`u128`/`i128` included).
            * `()` (empty tuple type) maps to `Unit`, not a zero-tuple.
            * Operator/derivable-trait bounds are satisfied by solver-known
              user or derive-generated impls, and modeled std types
              (`String`/`Option`/…) plus `Unknown` are lenient — previously
              every `==` on a derived type errored "the trait bound …:
              PartialEq is not satisfied".
          - Benchmarks re-recorded after these changes (run #11): geomean
            **6.69x**, within this host's documented single-run noise band
            (runs #6–#10 spanned 5.56x–6.91x); no sustained regression.
          - **Probe (2026-11-14): `1/5`, `moira-alloc` self-hostable; clotho now lowers
            to C.** The new closure-inlining pass (`tpt-moira-clotho/src/closure_return.rs`)
            is written in the subset the Moira frontend parses (no slice patterns, no
            `dyn FnMut`, no `let`-bound `for` over call results, closure parameters
            annotated), and adds no checker errors beyond the shared pattern below.
            Clotho's remaining blockers, from `build/self_host_report.md`: `101–113`
            type/borrow errors (most are `match arm type mismatch: expected i32, found ()`
            on unit-typed statement matches, and `Vec::retain`/`remove` are not in the
            prelude) and generated C that references the unmonomorphized generic
            `Spanned`. Checker gaps observed while writing that file: unannotated closure
            parameters are typed `i32` (so `|b| b.stmts` has no hint), `dyn FnMut`
            parameters are rejected, and `for` over a call result or an iterator chain
            over a slice parameter does not lower.
          - **Probe follow-up (2026-11-14, later): still `1/5`; clotho's checker errors
            `112 → 84`.** Fixed in this round: (1) a compound assignment (`depth += 1`)
            is typed `()` like `=` (it was typed as its left operand, so unit-typed
            match arms disagreed); (2) generic struct literals inside macro arguments
            (`vec![Spanned { .. }]`) are renamed to their instance (mono's
            `struct_by_start` table, applied to macro tokens); (3) `.collect::<Vec<_>>()`
            turbofish sites in `derive.rs` and `closure_return.rs` are now typed `let`
            bindings. Open blockers, in order: the AST drops method-call turbofish
            arguments (`MethodCall` carries no type args, so `collect::<T>` is untyped
            unless a `let` supplies the type; 42 match sites would change); the clotho
            `macros.rs` expansion uses `std::thread::scope`, which is outside the codegen
            subset: keeping the parallel pipeline and making it subset-compatible are a
            design choice for the maintainers; a generic struct literal inside `vec!`
            takes its type arguments from its literal fields, not from the expected
            field type (`Vec<Spanned<i64>>` with an untyped `4` yields `Spanned__i32`);
            the probe compiles per crate without `--bundle`, and a bundled emit of
            atropos currently produces about 5,800 C errors (unmonomorphized `Spanned`,
            `Arc<HashMap<..>>`, and `_` types), so the probe's per-crate view is kept.
          - **Probe follow-up (2026-11-14, round 3).** Followed the recommendation
            to remove the thread-based macro expansion: `expand_program_concurrent`
            now runs the sequential `expand_program` path (same output, same order;
            the codegen notes already record threads as outside the subset).
            Also fixed: `Box<T>` is now `Box` in the checker (was erased to `T`), so
            `Box::new` types as a box, `*b` and field/method access see through it,
            `compatible` accepts a box where its value is expected, and a box hints as
            `T*` (matching the C lowering's `Box::new` heap pointer). Locked in by
            `boxed_recursive_option_runs`. Clotho's checker errors are now `81`, and its
            C fails next on `Option<HashMap<..>>`: `HashMap::new()` inside `Some(..)`
            gets no type from the surrounding annotation (the expected type does not
            reach a call nested in a variant constructor), so the lowering emits an
            undefined `new`. Changing the map constructors' prelude signatures alone did
            not fix it, so that change was reverted.
          - **Probe follow-up (2026-11-14, round 4).** The expected type now reaches
            calls in three more contexts: the payload of `Some(..)`/`Ok(..)` against an
            annotated `Option`/`Result`, the right side of an assignment (against the
            target's type), and `HashMap::new()`/`HashSet::new()`/`BTreeMap::new()`/
            `BTreeSet::new()` (which return the expected collection). The lowering builds
            a map or set instance for these constructors in expression position too.
            Locked in by `option_of_hashmap_assigned_from_constructor_runs`. Clotho's C
            now fails on `std::path::PathBuf` (used by `compile_file` in `lib.rs`:
            `path.parent().map(..).unwrap_or_else(..)`). Modeling it needs `Path`/`PathBuf`
            as `str` in the checker and lowering, plus `parent()` returning an `Option`
            and runtime helpers. The probe's check column still blocks clotho on 81
            checker errors (format! named arguments, `u8` char methods, `Option::is_some_and`,
            `Path` methods), so this is not the next step that flips a crate.
          - **Probe follow-up (2026-11-14, round 5).** Clotho's checker errors are now
            `51` (from `81`). Fixed: named format arguments (`fmt = expr`) type only their
            value; `matches!` types only its scrutinee (the pattern may use `ref`, `|`, or an
            `if` guard, so it is not an expression); `u8` takes the ASCII methods
            (`is_ascii_digit`/`alphabetic`/`alphanumeric`/`hexdigit`,
            `to_ascii_lowercase`/`uppercase`); `char::from_u32` and `char::to_digit` return
            `Option`s (runtime check + branch, as `str::find`); `compile_reader` reads on the
            calling thread (threads are outside the codegen subset, as in
            `compile_concurrent`). Locked in by `byte_ascii_methods_and_char_from_u32_run` and
            `char_to_digit_runs`. The 51 that remain, grouped: `if let`/`while let` on enum
            patterns (2 + the `value` binding); bare variant names that exist in several
            enums (`match arm type mismatch: expected TokenKind, found ExprKind`, 5);
            `Vec` mutators `truncate`/`insert`/`sort`/`dedup` (4, need per-type runtime
            helpers); `Option::is_some_and` (2, closure-taking); `bool::then_some` (1);
            `PathBuf`/`Path` methods and conversions (7); `Formatter::write_str`,
            `char/String::escape_default`, `str::from_utf8` (4); `AtomicUsize::fetch_add`
            (1); `String::as_str` as a function value (1); `read_to_string` on a generic
            `R: Read` (1, newly visible because the thread version failed earlier); and a
            handful of type mismatches (`&T` vs `T`, `&bool` vs `bool`, empty array literals).
          - **Probe follow-up (2026-11-14, round 6).** Clotho's checker errors are now
            `43` (from `51`). Fixed: a function-local `use Enum::*;` names its enum for
            bare unit variants (the guess picked another enum that shares the name); an
            empty array literal takes its element type from context (`&[]` in a match
            arm); and `if let` / `while let` are desugared to `match` (and `loop` +
            `match` with a `break` arm) in the parser's output, so any pattern works
            (`if let Shape::Circle(r) = s`). Locked in by
            `if_let_enum_pattern_and_while_let_run` and `glob_imported_variant_resolves_to_its_enum`.
    - [x] Once a crate parses + type-checks, attempt full codegen and record pass/fail per crate in `docs/SELF_HOSTING.md`
      - **Status: Resolved as a process (2026-08-22)** — the probe records
        `codegen_ok` (and the failure reason) per crate and refreshes
        `docs/SELF_HOSTING.md` on every run. With the cfg(test)/prelude fixes,
        `moira-alloc` became the first crate to pass parse + check + codegen
        (**1/5 self-hostable**); the other four crates' per-crate "Blocks
        self-host until" entries stay accurate via each probe run (their
        remaining gaps: heavy type-check debt; codegen for `for` over
        iterators, closures, enum destructuring, and non-test top-level items).
  - [x] Bootstrap build script wiring the three crates (like `x.py`) — `x.py`
        orchestrates Stage-0 host build + Stage-1 self-host readiness probe
        (see `docs/SELF_HOSTING.md`); probe currently reports 0/5 crates
        self-hostable, blocked by generics (`<`) and `mod` parsing gaps.
  - [ ] Verify behavior parity of the self-compiled binary
    - Blocked (2026-11-15): no Stage-1 binary exists; the probe is `1/5`. The parity
      harness (`tests/stage_parity.rs`) and the CI gate are ready and skip until
      `MOIRA_STAGE1_BIN` is set.
    - [ ] Once Moira can compile itself, build the Stage-1 `moira` binary
    - [ ] Run the existing `clotho`/`lachesis`/`atropos` test suites through the Stage-1 binary and diff results against the Stage-0 (rustc-built) binary
    - [ ] Run `tpt-moira-bench` and `tests/determinism.rs` against the Stage-1 binary
    - [ ] Document any behavioral divergences and file them as follow-up bugs
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
    - [ ] Build Stage-2: compile Moira's source with the Stage-1 (Moira-built) binary
    - [ ] Byte-diff the Stage-1 and Stage-2 `moira` binaries (or their `--emit asm`/`c` output on a fixed corpus) for identity
    - [x] Extend `tests/determinism.rs` (or a new `tests/stage_parity.rs`) to automate the Stage-1-vs-Stage-2 comparison in CI
      - **Status: Resolved (2026-08-22).** New `tests/stage_parity.rs` runs the
        Stage-0 binary and a Stage-1/Stage-2 binary (path from the
        `MOIRA_STAGE1_BIN` env var) as separate processes over a fixed corpus
        (`mir`/`ssa`/`c` on the struct sample, `asm` on the struct-free
        sample), caching disabled via `MOIRA_NO_CACHE=1`, and asserts
        byte-identical output. When `MOIRA_STAGE1_BIN` is unset — today's
        default, since no Stage-1 binary exists yet — each test skips itself
        with a note instead of failing. The release workflow's determinism
        gate now runs the harness with `MOIRA_STAGE1_BIN` sourced from a repo
        variable of the same name, so the comparison activates in CI the
        moment a Stage-1 artifact is published.
    - [ ] Investigate and fix any non-determinism surfaced (timestamps, hash-map iteration, path embedding, etc.)
- [ ] **Final cross-subsystem optimization pass**
  - [x] Parallelize type-check + borrow-check passes
    - **Status: Resolved.** `tpt_moira_lachesis::check` now runs the type-check pass and
      the borrow-check pass **concurrently** on separate OS threads
      (`std::thread::scope`, no new dependency). This is safe because the
      borrow checker lowers the AST to MIR structurally and does not depend on
      the inference engine's results, so both passes read only the shared,
      immutable `Program`. The two threads write to independent
      `Result<(), Vec<Diag>>` slots; after they join, diagnostics are
      concatenated in a fixed order (type errors first, then borrow errors) and
      the coherence / well-formedness reports are appended — so the reported
      errors stay deterministic for the test suite while the work is parallel.
      Locked in by `tpt_moira_lachesis::tests::parallel_check_aggregates_type_and_borrow_errors`.
  - [ ] Profile the end-to-end pipeline on a large corpus and identify the next hot path (flamegraph/`perf`/`cargo flamegraph`)
    - Partial (2026-11-15): `MOIRA_TIMINGS=1` prints per-phase times (read+parse, emit,
      and check inside emit). Findings on the xlarge battery program (median, cache off):
      read+parse ~0.5 ms; lower+SSA ~0.9 ms; `--emit c` ~2.7 ms, of which a full type check
      to produce typed hints is ~1.4 ms (`lower_for_emit` calls `expr_type_hints`). The
      `--check` flag also prints the full AST debug dump under the default `ast` emit
      (~0.3-0.9 ms). Changing that is a CLI output change, so it goes through
      `docs/STABILITY.md`. Not done: a large-corpus profile, which needs a corpus Moira can
      check (see the crates item below), and a sampling profiler on Windows.
  - [x] Parallelize codegen (SSA→C/ASM emission) across functions/modules, following the type-check/borrow-check pattern above
        - **Status: Resolved.** `tpt_moira_atropos::codegen_c::emit_c` now emits each
          function body on its own OS thread (`std::thread::scope`) and joins the
          per-function strings in the original (deterministic) function order —
          mirroring the `tpt_moira_lachesis::check` type-check / borrow-check parallelization
          (both passes read only the shared, immutable `Program`/`mir::Program`).
          Function bodies reference each other only by name, so the emission
          passes are independent and their results concatenate in a fixed order,
          preserving the cross-process byte-identical determinism guarantee
          (`tests/determinism.rs` still passes). The from-scratch native ASM
          backend (`emit_asm`) is threaded the same way (one thread per function,
          joined in order). This note previously said otherwise.
  - [ ] Tune `moira-alloc` arena sizing/reuse based on profile data from real (not synthetic) compiles
    - Blocked (2026-11-15): the arena is not on the compile path (no pipeline stage calls
      `Arena::alloc`), so no profile can show its cost. Done meanwhile: the arena is now
      chunked bump storage (chunks double from 64 to 65,536 values; no per-value `Box`),
      with tests for reference stability across growth and drop-once.
  - [ ] Re-run `tpt-moira-bench --dashboard` after each optimization to confirm no regressions
    - Harness changes (2026-11-15): timed runs set `MOIRA_NO_CACHE=1` (cold compiles; the
      cache had been answering repeated runs of the same source); the run prints the
      process-start floor and the wall-clock ceiling; `--corpus <dir>` times every `.rs`
      file of a directory (a file Moira rejects is n/a); `--dashboard` also prints the
      crates.io pass-rate trend. Results recorded before this change used the cache, so
      compare only runs made after it. No run was recorded this session: the working tree
      is uncommitted, and a record would carry the wrong commit.
      - Latest run recorded 2026-08-22: geomean **6.91x** vs
        `rustc --emit=metadata` (small 6.53x / medium 7.60x / large 6.77x /
        xlarge 6.93x / nested 6.78x); no regressions vs the previous run
        (geomean improved from 6.36x). History in
        `moira-bench-results/history.mbh`.
      - Note (2026-08-22): back-to-back recorded runs on the same machine
        showed ±30-70% wall-time swings *between identical binaries* (runs
        #7-#10), so single-run deltas under ~50% on this Windows host are
        noise — treat only sustained multi-run trends as regressions. The
        latest run (#10, after the trait-solver/generics checker work) sits at
        6.55x geomean, within noise of the pre-change 6.91x.
  - [x] Incremental caching across the whole pipeline (not just expressions)
    - **Status: Resolved.** `tpt_moira_lachesis::pipeline_cache` generalizes the Phase-2
      SSA cache into a whole-pipeline content-addressed store: the type-check +
      borrow-check outcome (`PipelineCache::checked`) and every textual artifact
      (`mir`, `ssa`, `c`, … via `PipelineCache::text`, label-scoped) are cached
      by the same source hash. `PipelineCacheExt` (in the `moira` driver) wraps it
      so `--check` / `--emit mir` / `--emit ssa` transparently reuse cached
      results when `MOIRA_NO_CACHE` is unset, and bypass the cache entirely when
      it is set (preserving the cross-process determinism guarantee). Unit-tested
      in `lachesis/src/pipeline_cache.rs` (`checked_caches_and_reuses`,
      `checked_ok_roundtrips`, `text_caches_and_is_label_scoped`).
- [ ] Validate against a full Linux kernel build
  - [x] Confirm scope: target the Rust-for-Linux kernel modules (the actual Linux kernel is C; Rust support there is the relevant subset)
        - Scope: the kernel crate `rust/kernel/lib.rs` (its `mod` files are resolved by
          `compile_file`) and the `samples/rust/*.rs` modules that import it as `kernel::...`.
          `tools/kernel_probe.py` probes both; see its docstring.
  - [ ] Stand up a build environment that can drive Rust-for-Linux's build with an alternate compiler front end
  - [ ] Attempt compilation, triage the first wave of unsupported-construct failures via `moira --diff`
    - Blocked (2026-11-15): no kernel source tree is on this machine. `tools/kernel_probe.py
      --kernel-tree <path>` runs the probe and writes the triage. It was checked on a small
      synthetic tree with the same layout (`rust/kernel/lib.rs`, `samples/rust/*.rs`).
  - [ ] Iterate: fix or document each blocking gap until the target module set builds
  - [ ] Record pass/fail + divergence report for the attempted module set
- [ ] Validate against large crates.io dependency graphs
  - [x] Pick a representative sample of popular crates (e.g. `serde`, `tokio`, `regex`, `clap`) spanning common language features
        - 19 crates from the local cargo registry, sizes from `itoa` to `tokio` (`tools/crates_probe.py`, `DEFAULT_SAMPLE`).
  - [x] Script bulk-compiling the sample via `moira --check`/`--diff` and collecting pass/fail per crate
        - `python tools/crates_probe.py` runs `--emit ast` then `--check` per crate under a time and memory cap; output in `build/crates_probe/` (`report.md`, `results.json`).
        - Status: parse 19/19, check 2/19 (cfg-if, clap). Remaining check gaps are type-checker work: nested items, std API modeling, macros outside `println!`-family, method and field resolution.
  - [x] Triage top blocking gaps by how many crates they unblock (highest leverage first)
        - The report ranks gap classes by the crates they alone block. Parser gaps closed so far: `const fn`, `-> !`, `use core::str`, brace `use {..}`, item-position macro calls, `macro_rules!` brace skipping, `vis`/`lifetime`/`meta` fragments, statement-vs-expression macro expansion, `#[cfg]` evaluation for the fixed target.
  - [x] Track pass-rate trend over time in the same dashboard used by `tpt-moira-bench`
        - Done (2026-11-15): `tpt-moira-bench --dashboard` prints the crates probe history
          (`build/crates_probe/history.jsonl`) beside the speed runs.
        - Also fixed: the probe counted each `at file:line:col` location line as a separate
          diagnostic, which made every location its own gap class in the triage.
- [ ] Achieve ≥5x speedup over `rustc` on production-scale codebases
  - [ ] Depends on: Linux-kernel-module and crates.io validation above producing a large-codebase corpus to benchmark
  - [x] Extend `tpt-moira-bench` to accept an external large-codebase corpus (not just the Phase-1 synthetic battery)
        - `tpt-moira-bench --corpus <dir>` times every `.rs` file in the directory.
  - [ ] Measure and record geomean speedup on that corpus via `tpt-moira-bench --record`
    - Blocked: the crates.io sample checks for 2 of 19 crates, so no corpus of meaningful
      size is checkable yet. On this host the process floor (see Phase 1) also caps any
      wall-clock ratio.
  - [ ] If short of 5x, profile and address the largest remaining bottleneck (see "Final cross-subsystem optimization pass" above)
- [x] Error message parity audit against `rustc`
  - **Status: Resolved.** `tpt_moira_lachesis::diff::parity` scores diagnostic fidelity
    between Moira and `rustc` for a given source: it counts how many `rustc`
    errors have a Moira error in the *same coarse category* (category coverage)
    and averages the best word-overlap (Jaccard) between each `rustc` error and
    the most similar Moira error (wording similarity), combining them into a
    single 0..1 `score()`. `moira --diff --parity` prints the audit inline.
    Unit-tested in `lachesis/src/diff.rs` (`full_parity_when_no_errors`,
    `category_match_boosts_score`, `mismatch_scores_low`).
- [x] Stability guarantees / versioning policy
  - **Status: Resolved.** `docs/STABILITY.md` is the authoritative policy. It
    covers: SemVer for the CLI surface (`--emit` modes, flags) and the
    on-disk cache (explicitly *not* a public format); a divergence policy that
    treats crashes / mis-compiles / rejected-but-should-accept on the supported
    subset as release blockers tracked via the `moira --diff` allowlist and
    triage queue, while diagnostic-wording-only divergences may persist; a
    breaking-change + deprecation-window process for the post-1.0 contract; and
    a milestone-based release cadence aligned to `spec.txt` / `TODO.md`.
  - [x] Decide SemVer policy for the CLI surface (`--emit` formats, flags) and for the on-disk incremental-cache format
  - [x] Decide how long divergences from `rustc` behavior are allowed to exist before being treated as regressions (ties into the triage queue)
  - [x] Write an RFC/doc (e.g. `docs/STABILITY.md`) covering: supported-subset growth policy, breaking-change process, deprecation windows
  - [x] Define a release-cadence policy (e.g. time-based vs. milestone-based) and document it alongside the release workflow
- [x] Release packaging & distribution
  - [x] CI release workflow (`.github/workflows/release.yml`) builds release
        artifacts for the three OSes via `x.py dist` on tag push, gates on the
        Stage-2 cross-process determinism check, and publishes a GitHub release
        with the staged `dist/` trees. `x.py dist` stages the `moira` binary plus
        licenses/README into `dist/`. It now also generates the changelog
        (`python3 x.py changelog`) as a release artifact.
  - [x] Package manager distribution (e.g. Homebrew formula, apt/deb, or a `rustup`-style installer script)
        — added `packaging/homebrew/moira.rb`, a fill-in-the-version/sha256
        Homebrew formula template wired to `bin/moira` with a smoke test.
  - [x] Automated changelog generation from commit history/PR labels for each tagged release
        — `x.py changelog` derives a grouped changelog from `git log` since the
        last tag (or full history) and writes `dist/tpt-moira-<version>-CHANGELOG.md`;
        the release workflow runs it on tag push.
- [x] Publish versioned docs (architecture, self-hosting status, benchmark dashboard) to a docs site tied to releases
      - **Status: Resolved (2026-08-22).** `x.py docs` builds a versioned static
        docs site into `dist/docs/` (`index.html` + per-page HTML under
        `html/`, raw markdown under `md/`, and a `version.txt` stamp) from
        README, ARCHITECTURE, SELF_HOSTING, STABILITY, CONTRIBUTING, TODO, the
        generated changelog, and the latest self-host probe report. The
        converter is dependency-free (`md_to_html`: fenced code, headings,
        lists with soft-wrap continuation folding, block quotes, inline
        formatting). The release workflow runs it after the changelog step so
        every tagged release ships its own documentation bundle.
  - [x] Signed/checksummed release artifacts (align with the stability policy above)
        — `x.py dist` writes `dist/tpt-moira-<version>-checksums.txt` (SHA-256 over
        every staged artifact). With `--sign` (or `--sign-key <id>`) it also
        produces detached ASCII-armored GPG signatures (`<artifact>.asc`);
        when `gpg` is unavailable it warns and falls back to checksums only.

## Cross-Cutting / Ongoing Tracks
- [x] Architecture documentation (`docs/ARCHITECTURE.md`)
- [x] Contributor guide (`CONTRIBUTING.md`)
- [x] Test suite growth alongside `rustc` compatibility work (added `lachesis` integration tests: type inference, trait resolution, borrow checking, incremental cache)
- [x] Performance regression tracking / dashboards
  - Status: `tpt-moira-bench` now supports `--record` (appends a run to
    `moira-bench-results/history.mbh` with commit + timestamp) and `--dashboard`
    (prints the run history and a per-run geomean). After each recorded run it
    compares against the previous run and flags any Moira compile-time increase
    ≥10% or geometric-mean speedup drop ≥10% (`results::detect_regressions` /
    `geomean_regression`). Pure comparison logic is unit-tested.
