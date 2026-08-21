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

Latest per-crate type/borrow error counts (from the 2026-08-22 probe):
`clotho` 1721, `lachesis` 1409, `atropos` 1412, `moira` (driver) 69,
`moira-alloc` 0 (self-hostable). The driver's remaining errors are dominated
by **cross-crate resolution** (items from other workspace crates are invisible
to the single-crate checker) and iterator chains; the large crates' dominant
classes are macros beyond the print family, generics at field-access depth,
and `for` over Vec-backed iterators. Landed this session: enum codegen
(tagged structs, verified end-to-end), the builtin print macros
(`println!`/`eprintln!` with real type-checked arguments), cascade absorption
for unknown receivers/fields, and broad std-surface prelude modeling. These
are the Phase-4 features tracked below and in `TODO.md`.

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
- [ ] `Vec` / `Option` / `Result` + `?` propagation as real, checked types
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
