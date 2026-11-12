# Moira Stage-1 Self-Hosting

This document tracks Phase 4 / Ouroboros progress toward **Moira compiling its
own source**. It is the human-readable companion to `x.py`'s self-host probe
(`build/self_host_report.md` + `build/self_host_report.json`).

## Why this is a staged effort

Moira currently implements the strict **Phase-1 language subset** (no generics,
no full trait solving, no modules-with-visibility beyond the basics). The
compiler's own source (`clotho`, `atropos`, `lachesis`, the driver) is written
in full, idiomatic Rust. Until Moira reaches **full-subset coverage**, the
self-host stage is a *readiness probe*: `x.py self-host` compiles each crate
with Moira's `--check` and `--emit c` and records how far it gets, rather than
failing the build.

## Current status

Run `python x.py self-host` (the `python3` Store-alias is unavailable on
Windows; use the `python` launcher) and read `build/self_host_report.md`. The
probe reports, per crate: parse/check acceptance and C-backend codegen
acceptance.

**Latest probe: 1 of 5 self-hostable under the strict gate** (`moira-alloc`: it
parses, checks, and its emitted C compiles). The probe compiles each emitted C file,
so this is the real gate. Its tests are not run through Moira yet.

Check errors: `clotho` 113, `atropos` 182, `lachesis` 128. The driver checks cleanly
but its C needs types from other crates. Blockers, in the order they block:

- **Cross-crate types in per-crate C.** `Diag`, `Expr`, `Ty` and `Program` are used
  but not emitted into a per-crate file. `--bundle` emits them, but bundling needs
  every crate to lower.
- **Closures with aggregate results.** A closure is lowered as a `long long` function
  pointer, so passing `|s| vec![..]` to a generic method (clotho's `parse_delimited`)
  does not type in C.
- **A variant named like a type** (`Frag::Ty` in clotho's `macrorules`) can be typed
  as the type `Ty` once the other crates are loaded.
- **Lachesis `Allowlist::save`**: iterating a `BTreeSet` of tuples, file writes, and
  `format!`.
- **Checker errors** in all three crates (the counts above).

What has been added to the pipeline, with tests in `tpt-moira-lachesis/tests/`:

- Crate-qualified names for same-named types and functions across crates
  (`tpt-moira-clotho/src/resolve.rs`).
- Monomorphization (`tpt-moira-lachesis/src/mono.rs`): generic functions, generic
  structs with their impl blocks, and methods with their own type parameters, each
  specialized per instantiation before checking. Derived impls of generic structs are
  generic impls.
- Inference: a `let` annotation seeds a generic call or struct literal; applied types
  unify argument by argument; `impl Fn(..) -> R` keeps its signature, so a generic
  parameter is solved from a closure's result; method return types get their solved
  parameters.
- Atropos: `Option::map`, `seq.get(i)` as a pointer payload, `&p.field` as a field
  address, `lines()`, and `read_to_string`.

Known limits: generic methods of generic structs; expected-type propagation beyond
`let` annotations; closures whose results are aggregates.

**Probe health (2026-08-22):** the self-host probe was previously *dead* — commit
`4a55ae5` renamed the workspace crates to `tpt-moira-*` but left the driver
binary as `tpt-moira` while `x.py`/`ci.yml`/`dist`/Homebrew still referenced the
old `moira` name and the old crate directory names. Both were fixed in an earlier
session: the `[[bin]]` is once again named `moira` (matching every reference),
and `x.py`'s `CRATES` entry points point at `tpt-moira-{alloc,clotho,atropos,lachesis}`.
The probe now runs and produces a *real* per-crate parse/check/codegen report
instead of "entry point not found".

The probe (2026-08-22) now measures **parse independently of type-check**: it
runs `moira --emit ast` (which only lexes + parses + macro-expands and never
invokes the Lachesis checker or Atropos lowering) to set `parse_ok`, then runs
`moira --check` for `check_ok`, then `moira --emit c` for `codegen_ok`. This
directly tracks the crate-by-crate parse pass/fail that the "real self-host
wall" section below describes (see `x.py`'s `probe_crate`).

**Parse gate is closed for all five crates.** The latest probe reports every
crate with `parse_ok == true` (clotho, atropos, lachesis, the driver, and
moira-alloc all parse their own `lib.rs` cleanly). The remaining gate is purely
the **type checker + code generator**. The earlier "lachesis/atropos parse-blocked
on deeper constructs" blockers were all closed in prior sessions (the three
parse blockers below are historical, now resolved):

- `impl FnMut(&mut Self) -> ParseResult<T>`-style closure trait types in type
  position: the `impl Trait` parser consumed the `(args)` but not the `-> Ret`
  return type, so the `->` leaked into the surrounding field/param. It now
  consumes the optional `-> Ret` and trailing `+ Bound` (mirroring
  `consume_fn_trait`).
- A `match` arm whose body is a block `{ .. }` immediately followed by the next
  arm's `(pat) =>` pattern was mis-parsed as a function call `block(args)`,
  because `parse_postfix` treats a `(` after *any* primary as a call and a block
  is not callable in Rust. `parse_postfix` now breaks out of the call loop when
  the callee is a `Block`, so the `(` belongs to the following arm.
- **Closure bodies did not parse.** `parse_pat` folded the closure parameter
  list's closing `|` into a top-level OR-pattern (`|a| body` was read as the
  single pattern `a | body`), so any closure with a body failed with "expected
  expression". Split `parse_pat` into `parse_pat_single` (no OR-pattern
  continuation, used for closure params) while `parse_pat` keeps the OR-pattern
  loop for `match` arms. This unblocked `clotho`/driver parsing (both use
  `map_err(|e| { .. })`-style closures) and advanced `lachesis`/`atropos`.

The remaining gate is the **type checker + code generator** (the documented
"real self-host wall"). The latest probe (2026-08-22) reads **`1/5`
self-hostable — `moira-alloc`** is the first crate to pass parse + type-check +
codegen end to end. Two type-check blockers were closed this session:

- **`?` operator** — `infer::check_try` now type-checks `Result`/`Option`/`ParseResult`
  (any `Apply`/`Struct`/`Generic` `Try` type) and yields the opaque payload; only
  genuinely non-`Try` operands (`5?`) are rejected. Codegen of `?` (early-return
  lowering + a `Result`/`Option` backend representation) is still a follow-up.
- **Diverging expressions** — `return`/`break`/`continue`/`loop` now type as
  `Ty::Never`, `infer_block` propagates a diverging final statement, and
  `infer_match` skips `Never` arms when aggregating the result type. This clears
  the `match`-arm "expected (), found str" errors.

What unblocked `moira-alloc` (2026-08-22):

- **`#[cfg(test)] mod tests` is now skipped** by the type checker and codegen:
  `Item::is_cfg_test()` detects the gate and `clotho::flatten_items` propagates
  a module's cfg attributes onto its flattened children. Test-only macros
  (`assert!`/`assert_eq!`) and library surface (`drop`, `to_string`) therefore
  no longer fail non-test compilation.
- **Prelude additions**: `RefCell::new` / `.borrow()` / `.borrow_mut()`,
  `Vec::last`, `str::len` / `.is_empty()` / `.to_string()`; string literals
  coerce to `&str`; primitive receivers report source-level names.
- **`Type::method(...)` paths resolve inherent-impl methods**
  (e.g. `Arena::default()`), not just prelude associated functions.
- **Solver-wired method resolution**: generic receivers bounded by a user trait
  (`T: Show`) and concrete receivers covered by an impl both resolve through
  the Chalk-like solver; `where`-clause bounds are enforced at call sites.
- **Driver gap-closing round (2026-08-22)**: macro token capture keeps nested
  parens (`println!("{}", v.len())` re-parses as a method call); `for x in
  &vec`; `Ty::Range` for range expressions; broad prelude additions
  (`Result` combinators, `Args`/`Vec` adapters, process/fs surface);
  associated functions as values; impl-block associated consts (local +
  extern); same-named extern structs merge field sets; extern method-level
  generics accepted at call sites; `Err(e)` binds the error payload;
  unannotated closure params infer `Unknown`; function-local `use` is a
  no-op; `if let`/`while let` with variant patterns lower via the `match`
  discriminant machinery. Driver type/borrow errors: **54 → 1**.
- **Second gap-closing round (2026-08-22)**: nullary enum variants as
  associated constants (with inline-module flattening in checker Pass 1);
  `Self::Variant` expressions and `Self { .. }` literals resolve via the impl
  self type; `matches!` supported; prelude additions (`String::is_empty`,
  `str::chars`, `Vec::with_capacity`, `mem::take`, `saturating_*`); `()` is
  `Unit`; operator bounds satisfied by solver-known/derive-generated impls
  and lenient for modeled std types. Counts: clotho **1631 → 985**,
  lachesis **1221 → 1021**, atropos **1154 → 1100**.

Latest per-crate type/borrow error counts (from the 2026-08-22 probe):
`clotho` **985**, `lachesis` **1021**, `atropos` **1100**, `moira` (driver,
with externs) **0** — the driver now passes its own check and lowers for codegen
(`moira-alloc` and the driver are the two self-hostable crates; see the
`?`/`&`-pattern entries below),
`moira-alloc` 0 (self-hostable). Landed this session: enum codegen
(tagged structs, verified end-to-end), the builtin print macros
(`println!`/`eprintln!` with real type-checked arguments), cascade absorption
for unknown receivers/fields, broad std-surface prelude modeling, cross-crate
signature resolution, Rust's deref coercions (`String`→`&str`,
`PathBuf`→`&Path`, `Vec<T>`→`&[T]`), closure captures (leading-parameter
lowering with direct-call rewriting, verified end-to-end), and
assignment-expressions-are-`()` typing. These are the Phase-4 features tracked
below and in `TODO.md`.

| Crate | Role | Blocks self-host until |
|-------|------|------------------------|
| `moira-alloc` | region allocator | — (**self-hostable**) |
| `clotho` | lexer / parser / AST | full type-check (generics, `?`, macros, enum patterns) |
| `atropos` | MIR / SSA / codegen | full type-check + codegen of its own constructs |
| `lachesis` | type + borrow + traits | full type-check + codegen of its own constructs |
| `moira` (driver) | CLI wiring | all of the above |

## Dependency order (must compile in this sequence)

```
moira-alloc -> clotho -> atropos -> lachesis -> moira (driver)
```

A crate can only self-host after every crate it depends on self-hosts, because
Moira must *produce* the dependency's compiled artifact before consuming it.

## Stage-1 checklist (`TODO.md` Phase 4)

- [ ] Reach full-subset coverage so Moira can compile `clotho`/`lachesis`/`atropos`
- [x] Bootstrap build script wiring the three crates (like `x.py`) — `x.py`
- [ ] Verify behavior parity of the self-compiled binary

## Stage-2 checklist

- [x] Deterministic MIR/SSA output across stages
  - The MIR/SSA/C/native-ASM emit no longer depends on per-process hash-map
    iteration order, so the same source compiled by the stage-0 driver and the
    stage-1 driver yields byte-identical text. Verified by `tests/determinism.rs`,
    which runs `moira --emit {mir,ssa,c,asm}` twice as separate processes
    (with `MOIRA_NO_CACHE=1`) and diffs the output.
- [ ] Verify output parity (stage-1 == stage-2 binary)

## Blocking subset gaps (to fill before true self-host)

These are the features the compiler's own source relies on that the Phase-1
subset does not yet model. Each is a prerequisite for the matching crate's
probe to flip from *blocked* to *self-hostable*.

### Resolved — parser now accepts the compiler's own syntax
- [x] `mod` declarations (inline `mod { }` + external `mod x;` file loading)
- [x] Generic parameters + where-clauses on items (`fn f<T>`, `impl<T>`)
- [x] Generic type arguments (`Vec<Box<T>>`, with `>>` splitting)
- [x] `#[...]` / `#![...]` attribute skipping
- [x] Frontend `compile_file` recursively flattens modules (driver wired to it)
- [x] Inherent `impl T { }` (no `for`) + trait generics (`impl Trait<T> for …`)
- [x] Method receiver `self` / `&self` / `&mut self` in params
- [x] Raw pointer types `*const T` / `*mut T`
- [x] `as` casts parsed **and** lowered / codegen'd / type-checked (`x as T` →
   `(T)x` in C via a new `Rvalue::Cast`; the inner operand is still type-checked
   and the cast's type is the explicit target type) — see `lachesis`/`infer.rs`
   and `atropos`/`lower.rs` + `codegen_c.rs`
- [x] `use … as` renames parse (`UseTree::alias` records the rename; `use` is
   otherwise dropped by the Phase-1 subset)
- [x] Field-level `pub` visibility on struct fields, enum variants, and enum
      variant fields (the `expected identifier, found pub` blocker)
- [x] Struct-literal field shorthand (`Foo { x }` desugars to `Foo { x: x }`)
- [x] Literal patterns (`"--emit"`, `42`, `'c'`, `true`), tuple patterns
      `(a, b)`, reference patterns `&pat`, and or-patterns `A | B =>` in `match`
- [x] Lifetimes (`'a`, `'src`, `'_`, …) are dropped by the lexer — safe for the
      structural Phase-1 borrow checker (`expected type, found char 's'` / `'_'`)
- [x] Enum tuple variants with unnamed fields (`Some(T)`, `Ok(T)`, `Err(E)`)
- [x] `if`/`while`/`for` heads no longer greedily consume the following block as
      a struct literal (`if c { .. }` was being parsed as `if (c { .. })`)
- [x] `if let` / `while let` / `else if` now parse (`let` binds as a block-less
      expression; the surrounding `if`/`while` owns the then-block)

### Resolved this session (parser-level)
- **Binary-operand struct literals no longer eat the following `if`/`while`/`for`
  then-block.** `parse_expr_bp` propagated `allow_struct = true` into the right
  operand of every binary operator, so `if a > b { a } else { b }` misparsed
  `b { a }` as a struct literal (yielding "expected {, found else"). The flag now
  propagates the caller's `allow_struct` value into the RHS, so the
  `if`/`while`/`match`/`for` condition parsers (which disable struct literals to
  protect their then-block `{`) keep that guarantee. Top-level `a + Foo { .. }`
  still parses. Locked in by `clotho/tests/frontend.rs`
  (`parse_if_with_binary_condition_does_not_eat_then_block`).
- **Leading-dot floats `.5` / `.25e-3` lex correctly** (`lex_leading_float`),
  and a `.N` following a receiver is recovered as numeric tuple/struct field
  access (`expr.0`) in `parse_postfix`. Standalone floats with a type suffix
  (`0.0f64`, `3.14f32`) already lex via `lex_number`, so the `diff.rs`
  `0.0f64` float-suffix parse gap is resolved.
- **Tuple literal expressions `(a, b, c)` now parse.** The tuple branch in
  `parse_atom` previously called `self.expect(TokenKind::RParen)` *after*
  `parse_delimited` had already consumed the closing `)` (a double-consume that
  surfaced as `expected ), found ;`). For the single-element trailing-comma form
  `(a,)` the `)` was never consumed. Fixed so the `)` is consumed exactly once.
- **Trailing-comma and single-element tuple *patterns* `(a,)` now parse** in
  `parse_pat` (the loop now breaks on a trailing comma before the `)`).
- **Method-call turbofish `.collect::<Vec<_>>()` now parses.** `parse_postfix`
  consumes an optional `::<…>` type-argument list after the method name; the
  Phase-1 subset ignores the inferred args but the code now parses.
- **Enum struct variants `Variant { field: Type, … }` now parse** (unit and tuple
  variants already did). `parse_enum` gained a `{ … }` branch; trailing commas
  before the closing `}` are tolerated.
- **`if let` / `while let` now parse with any RHS.** `parse_let_expr` (the
  expression-position `let` used by `if let`/`while let`) now parses its
  initializer with struct-literal disallowed, so `if let P = path { … }` no
  longer greedily swallows the then-block as a struct literal `path { … }`.
  Statement-position `let x = Foo { … }` still allows struct literals.
- **Match guards `Pat if cond => body` now parse.** `parse_match` accepts an
  optional `if <expr>` between the pattern and `=>`; the guard is parsed and
  (for the Phase-1 subset, which does not model guards) ignored — enough to get
  real code past the parser.
- **`impl Trait` in type position now parses.** `impl` is now a keyword token;
  `parse_type` maps `impl Trait` (and `impl FnMut(&str)` parenthesized args) to
  the trait path opaquely. This unblocked `clotho`/`atropos` past their
  `impl Into<String>` / `&mut impl FnMut(&str)` gating errors.
- **Tuple field access `expr.0` and tuple *types* `(A, B)` now parse.** Tuple
  index `.0` is represented as a numeric field; the tuple-type branch no longer
  double-consumes the closing `)` (the same bug class as tuple literals).
- **Generic type aliases `type Pair<T> = (T, T);` now parse.** `TypeAliasDef`
   gained an optional `generics` field, parsed via `parse_generics`.

### Resolved this session (parser-level) — self-host parse progress
The following constructs in the *compiler's own source* now parse (verified by
running `moira --emit mir <file>` per leaf module and by a clean `cargo test
--workspace`, 0 regressions). Each unblocked several crates' first-parse errors:
- **Byte-char literals `b'\n'`** — `tpt_moira_clotho::lexer::lex_byte_char` now consumes
  the closing `'` (it previously left the quote in the stream, corrupting the
  next token and surfacing as `expected {, found char ' '` in `lexer.rs`).
- **Tuple structs & unit structs** (`struct E(pub String);`, `struct Unit;`) —
  `parse_struct` now handles the `(..)` and bare-`;` forms (previously only brace
  structs), unblocking `tpt_moira_atropos::lower` / `tpt_moira_lachesis::cache` / `pipeline_cache`.
- **Attributes on enum variants** (`#[default]`) — `parse_enum` now parses and
  drops variant attributes, unblocking `tpt_moira_atropos::mir`.
- **`let … else`** (`let Some(x) = e else { … }`) — `parse_local` now parses and
  discards the diverging `else` block.
- **Labeled loops** (`'outer: for …`) — the lexer now drops a `'name:` lifetime
  label entirely so the dangling `:` no longer breaks the following `for`/`while`.
- **`mut` binding patterns** (`fn f(mut x: T)`) — `parse_pat` now accepts a
  leading `mut`.
- **Lifetime params/bounds in generics** (`<R: Send + 'static>`, `<'a, T>`) —
  `parse_generics` and `parse_where_clause` tolerate the lexer-dropped lifetime
  tokens (dangling `,` / trailing `+` before `>`), unblocking `tpt_moira_clotho::pipeline`.
- **`Fn`/`FnMut`/`FnOnce` trait types & bounds** (`Box<dyn Fn(&str) -> String +
  Send>`, `F: FnOnce() -> String`, `F: Fn(&str) -> String + 'static`) — `parse_type`
  and the bound parser consume the parenthesized args, optional `-> Ret`, and
  trailing `+ Bound`s, unblocking `tpt_moira_lachesis::propose` / `cache` / `pipeline_cache`.
- **`where`-clause termination** — the where-clause loop now stops at the body
  `{` rather than parsing it as a type.

### Open — the type-check / codegen wall (all five crates parse)

Parse is **closed** for every crate (last two parse blockers fixed this
session: `impl FnMut(...) -> Ret` in type position, and block-bodied `match`
arms followed by `(pat) =>` arms). The probe's remaining `0/5` is purely the
type checker + code generator rejecting the compiler's own code. The dominant
error classes (from `moira --check` on each crate) are:

- **`?`-operator propagation** — **resolved at the type-check level (this
  session).** `ExprKind::Try` is now handled by `infer::check_try`: it is accepted
  when the operand is `Result`/`Option`/`ParseResult`/any applied or `Try` struct
  (the Phase-1 subset cannot verify the `Try` impl, so any `Apply`/`Struct`/`Generic`
  is accepted) and yields the opaque payload `<unknown>` (matching how `Some`/`Ok`
  constructors are already modelled); a genuinely non-`Try` operand (`5?`, `&x?`)
  is still rejected. With this, the `?` errors in `clotho` dropped from 196 to ~0.
  Remaining: **codegen of `?`** (early-return lowering + a `Result`/`Option`
  representation in the C/native backends) is still a follow-up, so `?` does not
  yet lower end-to-end. *(TODO: `?` / `Result` codegen.)*
- **`match` arm type aggregation** — **resolved (this session).** Diverging
  expressions (`return` / `break` / `continue` / `loop` and a block whose final
  statement diverges) now type as `Ty::Never`; `infer_match` skips `Never` arms when
  aggregating the result type, so a `match` arm like `true => { return 1; }` no
  longer forces the result to `()` and clashes with value arms. This cleared the
  ~94 "expected (), found str" errors in `clotho`.
- **macros** (e.g. 92 in `clotho` — `vec!`/`format!`/`write!`/`println!`) —
  `ExprKind::Macro` is parsed but rejected. Requires macro expansion to real
  constructs.
- **`X::new` associated-function resolution on user types** (e.g. 70 in `clotho`)
  — `new`/`default`/etc. on user structs/enums need `assoc_fns` registration
  from `impl` blocks, not just the `std` prelude.
- **enum-variant destructuring patterns** (`Some(x)`, `Ok(x)`) — `Pat::Path` is
  treated as an irrefutable binder, not a variant test; `if let Some(x) = …` is
  rejected ("non-identifier patterns"). *(TODO: match/enum destructuring.)*
- **`Self` in type position** — `Ty::Struct("Self")` leaks into diagnostics
  because some `Self` resolutions are not yet substituted.
- **generics in `infer` for real compiler code, slice indexing, closures codegen**
  — see the remaining TODO items below.

These are the Phase-4 features enumerated in the "Broader remaining gaps" list
and in `TODO.md` (generics + `where`-clause solving, closures MIR/codegen, full
trait solving wired into `infer`, `?`/`Result`, library codegen).


### Broader remaining gaps (the real self-host wall)
Parsing is now the *easy* part; the remaining gate is the **type checker and
code generator**, which still reject most of the compiler's own code:
- [x] Tuple **values** end-to-end (construction + numeric field access):
      `ExprKind::Tuple` + inference (`Ty::Tuple`) + MIR construction
      (`Rvalue::Tuple` → generated `struct __tuple_*`) + C codegen
      (`(a, b)`, `t.0`/`t.1`). Validated by `lachesis`/`infer.rs` and
      `atropos`/`codegen_c.rs` tests. The native ASM backend does not model
      tuples yet.
- [x] Tuple **destructure** in `let (a, b) = ..` (the `Pat::Tuple` parser path
       existed; `tpt_moira_lachesis::infer` + `tpt_moira_atropos::lower` now bind each component —
       validated by `tuple_destructure_*` tests). `for (x, y) in ..` destructuring
       is now also resolved for array-of-tuples (each component is read from the
       element's `.fN` field — validated by `for_over_tuple_array_*` tests); only
       `let`-bound (non-literal) array-of-tuples destructuring remains.
- [x] Closures type-check acceptance — `tpt_moira_lachesis::infer::check_closure` now
       exists (the type checker previously failed to compile because it referenced
       the unimplemented method); closures are accepted at the type level. Capture
       analysis and MIR/codegen of closures remain open.
- [x] `match` accepted by `tpt_moira_lachesis::infer` + lowered to MIR (common subset:
       literal / wildcard / identifier / ref / or / tuple-of-identifiers patterns;
       enum-variant destructuring `Some(x)` is still treated as an irrefutable
       binder, not a variant test). `if let` / `while let` were already resolved
       (identifier patterns); **closures remain unimplemented**.
       A `match` can also be a tail/return expression (the parser now treats it
       like `if`/`while`-as-tail rather than always forcing a `;` statement).
- [ ] Closures: parsing, capture typing, and MIR/codegen lowering
- [ ] Generics in the type checker (the subset parses them but does not solve
      them), plus `where`-clause lowering
- [x] `Vec<T>` runtime in the C backend (annotated locals and `&mut Vec<T>`
      parameters; element types that the backend already expresses — scalars,
      structs, tuples). `let v: Vec<T> = Vec::new()` / `with_capacity` (the
      capacity hint is ignored), `v.push(x)`, `v.len()`, `v.is_empty()`, `v[i]`,
      and `for x in &v` / `for x in v` / `for x in v.iter()` (an index loop over
      `data`/`len`). Lowered as `Vec<T>` type strings; `atropos::codegen_c` emits a
      `moira_vec_<T>` struct and its helpers per element type. Validated by
      `atropos/tests/vec.rs` (compiled and run with the system C compiler).
      Not yet: `Vec<Vec<T>>`, `String`, `Option`/`Result`, slices, vectors in
      the native ASM backend, and iteration over vectors reached through fields
      or pattern bindings (`for x in &program.items`, `Err(diags)` → `for d in &diags`),
      which needs element types from Lachesis because Atropos lowers untyped.
- [x] Typed lowering: Lachesis records the type of every expression and exports
      C-style hints (`bool`, `str`, `Vec<T>`, `&Vec<T>`) keyed by source span
      (`lachesis::expr_type_hints`, `atropos::lower_program_typed`). Atropos uses
      them for `for` over a vector it cannot see locally (a field, a pattern
      binding), for `println!` of a `bool`, and for pattern-bound locals. The
      borrow check and the driver's `mir`/`c` emit paths use the same hints. Fixed
      in the checker: `if let`/`while let` extracted a variant's payload twice
      (`Err(e)` was bound to `Unknown`). Validated by
      `lachesis/tests/typed_lowering.rs`. Limits: hints apply only to the kinds
      above; a user enum's variant payload is not typed by the checker yet
      (`variant_payload_ty` models `Option`/`Result` only); `lower_and_ssa` is still untyped.
- [x] `?` on `Option` / `Result` in functions returning the same enum: the error
      variant (`None` / `Err`) returns the value unchanged, otherwise the payload
      (`f0`) is yielded. Option and Result share one tagged layout, so no
      conversion is needed (`From` is not applied). Also `&x` patterns in `for`.
      Validated by `lachesis/tests/typed_lowering.rs` (runs end to end).
      Limit: enum payload slots are still `long long` (`Option<Vec<T>>` and
      struct payloads are not typed yet), so `?` is correct for scalar payloads only.
- [x] Driver C check (`moira --emit c` on `src/main.rs`, then `gcc -fsyntax-only`):
      228 → 198 errors. Fixed in this pass: scrutinees take their enum type from
      the variant pattern (`type_enum_operand`); matches on `&Enum` read fields
      with `->`; numeric fields are tuple indices (`fN`) even when the base type is
      untyped; `?` types its operand; closure names are qualified by the enclosing
      function (they collided across functions). What remains is cross-crate:
      prototypes that name another crate's types (`Program`, `Diag`, `String`) fail,
      so their functions become implicit `int` declarations, and calls to std or
      other-crate methods have no definition. Only a whole-program build (all
      crates in one C unit, or per-crate objects with shared declarations) clears
      those.
- [x] Whole-program build (`moira --emit c --bundle`): the root and its `--extern`
      crates are lowered separately, then renamed and merged into one MIR program and
      one C unit (`atropos::lower_bundle`). The root keeps its names; an extern
      crate's functions and structs are prefixed `<crate>__`; a reference resolves to
      its owner (the root first, then the single extern that defines it). Validated by
      `atropos/tests/bundle.rs` (two crates with colliding `Point`s, run through gcc).
- [x] Slices `&v[a..b]`, `&arr[a..b]`, `&slice[..]` (all range forms), with `len`,
      indexing and `for` over them; `&slice` passes the slice value. Ranges now bind
      looser than `+` (`comma + 1..` was misparsed).
- [x] `for` over iterator adapters on sequences: `.iter()`, `.iter_mut()`, `.rev()`,
      `.enumerate()` and `&mut v` lower to index loops (`iter_mut` binds pointers via
      `Rvalue::IndexRef`). The checker types the items (`Vec<T>` items are `T`).
- [x] Assignments through a field, an index, or a dereference store into the place
      (`StoreField` / `StoreIndex`). Before, they silently wrote a temporary.
- [x] `continue` in counted loops advances the counter (it used to loop forever).
- [x] `matches!(expr, pattern)`; reads through `&Struct` parameters use `->`.
- [x] Iterator chains over sequences and ranges: `iter()`/`into_iter()` on a `Vec`,
      slice or array, or a range `(lo..hi)`, followed by `map`, `filter`, `enumerate`,
      `cloned`/`copied`, and a terminal `collect`, `any`, `all`, `find`, `count`, `sum`
      or `for_each`. One index loop; closures are lifted once and called with their
      captures. An `enumerate` item is passed to a closure with an `(index, item)`
      pattern as two arguments (closure parameters may be pairs). The checker types
      the chain (`Iter<T>`), so `collect` yields `Vec<T>` and `find` yields `Option<T>`.
      Also: `vec![..]` and `vec![v; n]`; `for c in s.chars()` / `s.bytes()` over a
      string (bytes, exact for ASCII); `for x in (lo..hi).rev()`; nested `Vec<Vec<T>>`;
      `matches!`; struct patterns `Name { field: pat, .. }`; enum-qualified variant
      lookup (`TokenKind::Dollar` no longer clashes with another enum's `Dollar`);
      `as` casts to C scalar types; closures see the enum variant table.
      Validated by `atropos/tests/vec.rs` (untyped runs) and
      `lachesis/tests/typed_lowering.rs` (hinted runs).
- [x] Typed user-enum payloads. Each user enum's struct has one typed slot per payload
      field (`v{tag}_{i}`, C type from the declaration); generic payloads stay `long
      long`. Pattern bindings take the payload's type (through a reference when the
      match is on a reference, as in Rust). The checker types the payloads too.
      Structs are emitted in by-value dependency order. `Option`/`Result` keep their
      untyped `f{i}` payload. Validated by `lachesis/tests/typed_lowering.rs`
      (`typed_enum_payloads_run`). Also: `Box<T>` is modelled as `T`; scalar operators
      accept references to scalars (`i32 + &i32`).
      Effect on the probe: the checker now types what it used to absorb as `Unknown`,
      so the counts rose: atropos 356 → 585, clotho 243 → 274, lachesis 231 → 305. Those
      are checker gaps that were hidden before, not new breakage: the remaining errors are
      listed by kind with `moira --check` (for example `no method` on std types, and
      `String` against `str`).
- [x] Checker triage (same probe, fewer false errors). Fixed modelling mistakes: extern
      structs were overwritten by the prelude (`Path`, `Block`, `Stmt` lost their fields);
      tuple-struct constructors (`LowerError(..)`) are functions; `str` methods that
      return `String` are found on `String`; `String` and `str` are compatible; generic
      struct fields take the instance's type arguments; `&A` implements an operator
      trait when `A` does; iteration items are references (`for x in &v`, `iter()`), so
      `*x` works; calls through a closure-typed local resolve; `HashSet::insert` takes
      one argument; more `Option`/`Result`/`String` methods. Counts (probe): atropos
      585 → 248, clotho 274 → 184, lachesis 305 → 176.
- [x] Typed prelude values. `Option<T>` and `Result<T,E>` are instance structs
      (`Option_T`, `Result_T_E`; `Result`'s error payload is `e0`); a `Some(x)` or `Ok(x)`
      takes its type from its context; `?` converts the error to the return type's
      instance; `take()` on an `Option` local. Tuples are `Tuple<..>` type strings with
      instance structs. Struct literals of the prelude enums take their struct from the
      destination type.
- [x] Hash maps and sets in the backend (`Map<K,V>`; `HashSet<K>` is `Map<K,bool>`).
      Insertion-ordered (Rust's order is unspecified, so programs cannot rely on it):
      `insert` returns the replaced value as an `Option`, `get`, `contains_key`/`contains`,
      `len`, `is_empty`, `for (k, v) in &m`, `m.keys()`, `m.values()`, and `for x in &set`.
      Keys compare as strings (`strcmp`), structs (`memcmp`), or with `==`. Not yet:
      `entry`, `remove`, `iter_mut`, `get_mut`, and maps as struct fields built with
      `HashMap::new()` outside an annotated `let`.
- [x] Loops over `zip` and `chain` of two sequences; `v.drain(..)` (empties the vector);
      `vec![..]`, `vec![v; n]`; cross-crate enum patterns: a unit learns the other units'
      enums (variants and payload types; their structs are emitted once, by the owner).
      A struct or enum the program defines is hinted by its name.
- [x] Checker: `HashMap`/`HashSet` methods and iteration types, `Option::take`, `&pat`
      patterns strip one reference, `Box<T>` is `T`.
- [ ] Remaining blockers for the whole-program build (`moira --emit c --bundle`):
      - clotho `compile_concurrent`: loops over the result of `std::thread::scope`. The
        backend has no threads; the compiler's pipeline uses them for parallel
        compilation, so this needs a decision (a sequential version, or thread support).
      - atropos `emit_c`: loops over a `BTreeSet` (sorted iteration). A sorted set is a
        different runtime from the insertion-ordered map; sorted iteration matters for the
        determinism tests.
      - lachesis: loops whose sources the checker does not type (for example a `HashSet`
        from `Option::cloned().unwrap_or_default()`), and the checker's remaining gaps:
        the probe counts are atropos 291, clotho 181, lachesis 191.
      - clotho `match_rep`: iterates a `HashMap` (`for (k, v) in map`). Needs a
        hash-map runtime.
      - atropos `rename_program`: `for (_, ty) in &mut s.fields`, a `Vec` of tuples. The
        element type of a tuple is not expressible as a hint yet (the backend names tuple
        structs from their component C types, which the checker does not reproduce).
      - lachesis: patterns on another crate's enum (`ItemKind::Use`); the bundle does not
        yet share enum variant tables across crates.
- [ ] Iterators over `chars()`, `lines()`, `values()`, `char_indices()` need string and
      hash-map runtimes; not started.
- [ ] `Option` / `Result` as real, instantiated types (typed payload slots per instantiation)
- [ ] Library codegen: emit `.rlib`/object archives, not just executables
- [ ] Full trait solving (the Chalk-like solver exists but is not wired into
      `infer` for these crates yet)

## Running

```bash
python3 x.py build        # Stage 0: build Moira with cargo
python3 x.py self-host    # Stage 1: probe self-host readiness
python3 x.py all          # build + check + test
python3 x.py dist         # stage a release tree under dist/
```

If `python3` is unavailable (e.g. it opens the Microsoft Store stub on Windows),
use the `python` launcher or the full interpreter path instead.

Set `MOIRA_SELFHOST_STRICT=1` (or `--strict`) to make `self-host` fail CI when
any crate is not fully self-hostable — the gate that turns the probe into a
real self-build once coverage lands.
