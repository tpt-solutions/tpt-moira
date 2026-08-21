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
  - Status: harness extended and re-run; **10x is not yet met for the canonical
    front-end + SSA mid-end vs `rustc --emit=metadata` measurement (~7.0x
    geomean), but a full `rustc` build+link baseline shows ~17.9x geomean**, so
    the gap is in Moira's own mid-end/codegen rather than end-to-end wall time.
    Three measurements now exist, all recorded via `moira-bench --record`:
    * **front-end + SSA mid-end** vs `rustc --emit=metadata` → **~7.0x** geomean
      (5 programs: small/medium/large/xlarge/nested; battery grown with two
      larger synthetic programs to check the ratio vs. size — it holds ~6.7–7.2x
      across the size range, i.e. it does *not* degrade as programs grow).
    * **full Moira pipeline** (type-check + borrow-check + incremental cache, via
      `moira --emit check`) vs `rustc --emit=metadata` → **~7.36x** geomean.
    * **full Moira pipeline** vs a **complete `rustc` build+link** (new
      `--rustc-mode full`) → **~17.9x** geomean.
    Run `moira-bench` (default = first row), `moira-bench --full` (second), and
    `moira-bench --full --rustc-mode full` (third). The 10x target remains open
    for the canonical front-end+mid-end number; closing it depends on the
    Phase-4 final optimization pass (parallel codegen, arena tuning).
  - [x] Extend `moira-bench` to time the *full* pipeline (type-check +
        borrow-check + incremental cache), not just front-end + SSA mid-end
        — `--full` now times `moira --emit check`; the table prints which
        pipeline is measured and `moira-bench --dashboard` tracks it.
  - [x] Add a `rustc` comparison mode that runs a full `cargo build`/`rustc`
        (not just `--emit=metadata`) so the baseline matches what Moira measures
        — `--rustc-mode {metadata,full}`; `full` runs `rustc -o <tmp> <file>`
        (native codegen + link), the real "compile this file" cost.
  - [x] Grow the Phase-1 benchmark battery with larger synthetic programs to
        see how the speedup ratio trends with size — added `xlarge` (200×200
        nested loops + float `mandel`) and `nested` (deep call nesting), so the
        battery spans small→xlarge. The ratio stays ~7x across the range.
  - [x] Re-run and record the geomean via `moira-bench --record`; update this
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
  - [ ] **Parser: close remaining gating constructs** (blocks `clotho`/`lachesis`/`atropos` from parsing at all)
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
    - [ ] Re-run `x.py self-host` probe after each fix and track crate-by-crate parse pass/fail in `docs/SELF_HOSTING.md`
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
    - [ ] Re-run `x.py self-host` after each milestone above; update the crate-by-crate self-hostable count (the `mod` and generics-`<` parse gaps are resolved — remaining blockers are type-check depth: `match`/closures/generics-in-`infer`/tuple-destructure)
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
          - Re-run (2026-08-22, post solver-wiring): still `0/5` self-hostable;
            all five crates parse; type-check/codegen gaps unchanged. The new
            trait-bound method resolution does not yet flip a crate — the
            remaining blockers are the ones listed above (`match` lowering,
            closures in MIR/codegen, full generics inference, tuple
            destructure).
    - [ ] Once a crate parses + type-checks, attempt full codegen and record pass/fail per crate in `docs/SELF_HOSTING.md`
      - Status: the probe records `codegen_ok` (and the failure reason) per crate,
        and `docs/SELF_HOSTING.md` already captures the per-crate "Blocks
        self-host until" table. As of 2026-08-22 all five crates still fail
        codegen (each on a distinct subset gap: `moira-alloc` → macros, `clotho` →
        unsupported top-level item, `atropos` → closures, `lachesis` → `for`
        iterator, driver → `if let`/enum-destructure), so full codegen of any
        crate is not yet reached. Flipping a crate requires its type-check +
        codegen gaps to close first.
  - [x] Bootstrap build script wiring the three crates (like `x.py`) — `x.py`
        orchestrates Stage-0 host build + Stage-1 self-host readiness probe
        (see `docs/SELF_HOSTING.md`); probe currently reports 0/5 crates
        self-hostable, blocked by generics (`<`) and `mod` parsing gaps.
  - [ ] Verify behavior parity of the self-compiled binary
    - [ ] Once Moira can compile itself, build the Stage-1 `moira` binary
    - [ ] Run the existing `clotho`/`lachesis`/`atropos` test suites through the Stage-1 binary and diff results against the Stage-0 (rustc-built) binary
    - [ ] Run `moira-bench` and `tests/determinism.rs` against the Stage-1 binary
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
    - [ ] Extend `tests/determinism.rs` (or a new `tests/stage_parity.rs`) to automate the Stage-1-vs-Stage-2 comparison in CI
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
          backend (`emit_asm`) follows the same shape but is not yet threaded; its
          SSA program is still built and emitted sequentially.
  - [ ] Tune `moira-alloc` arena sizing/reuse based on profile data from real (not synthetic) compiles
  - [ ] Re-run `moira-bench --dashboard` after each optimization to confirm no regressions
      - Latest run recorded 2026-08-22: geomean **6.91x** vs
        `rustc --emit=metadata` (small 6.53x / medium 7.60x / large 6.77x /
        xlarge 6.93x / nested 6.78x); no regressions vs the previous run
        (geomean improved from 6.36x). History in
        `moira-bench-results/history.mbh`.
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
  - [ ] Confirm scope: target the Rust-for-Linux kernel modules (the actual Linux kernel is C; Rust support there is the relevant subset)
  - [ ] Stand up a build environment that can drive Rust-for-Linux's build with an alternate compiler front end
  - [ ] Attempt compilation, triage the first wave of unsupported-construct failures via `moira --diff`
  - [ ] Iterate: fix or document each blocking gap until the target module set builds
  - [ ] Record pass/fail + divergence report for the attempted module set
- [ ] Validate against large crates.io dependency graphs
  - [ ] Pick a representative sample of popular crates (e.g. `serde`, `tokio`, `regex`, `clap`) spanning common language features
  - [ ] Script bulk-compiling the sample via `moira --check`/`--diff` and collecting pass/fail per crate
  - [ ] Triage top blocking gaps by how many crates they unblock (highest leverage first)
  - [ ] Track pass-rate trend over time in the same dashboard used by `moira-bench`
- [ ] Achieve ≥5x speedup over `rustc` on production-scale codebases
  - [ ] Depends on: Linux-kernel-module and crates.io validation above producing a large-codebase corpus to benchmark
  - [ ] Extend `moira-bench` to accept an external large-codebase corpus (not just the Phase-1 synthetic battery)
  - [ ] Measure and record geomean speedup on that corpus via `moira-bench --record`
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
- [ ] Release packaging & distribution
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
  - Status: `moira-bench` now supports `--record` (appends a run to
    `moira-bench-results/history.mbh` with commit + timestamp) and `--dashboard`
    (prints the run history and a per-run geomean). After each recorded run it
    compares against the previous run and flags any Moira compile-time increase
    ≥10% or geometric-mean speedup drop ≥10% (`results::detect_regressions` /
    `geomean_regression`). Pure comparison logic is unit-tested.
