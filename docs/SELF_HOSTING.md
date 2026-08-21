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

Run `python3 x.py self-host` and read `build/self_host_report.md`. The probe
reports, per crate: parse/check acceptance and C-backend codegen acceptance.

| Crate | Role | Blocks self-host until |
|-------|------|------------------------|
| `moira-alloc` | region allocator | subset covers it |
| `clotho` | lexer / parser / AST | subset covers it |
| `atropos` | MIR / SSA / codegen | subset covers it |
| `lachesis` | type + borrow + traits | subset covers it |
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
- **Byte-char literals `b'\n'`** — `clotho::lexer::lex_byte_char` now consumes
  the closing `'` (it previously left the quote in the stream, corrupting the
  next token and surfacing as `expected {, found char ' '` in `lexer.rs`).
- **Tuple structs & unit structs** (`struct E(pub String);`, `struct Unit;`) —
  `parse_struct` now handles the `(..)` and bare-`;` forms (previously only brace
  structs), unblocking `atropos::lower` / `lachesis::cache` / `pipeline_cache`.
- **Attributes on enum variants** (`#[default]`) — `parse_enum` now parses and
  drops variant attributes, unblocking `atropos::mir`.
- **`let … else`** (`let Some(x) = e else { … }`) — `parse_local` now parses and
  discards the diverging `else` block.
- **Labeled loops** (`'outer: for …`) — the lexer now drops a `'name:` lifetime
  label entirely so the dangling `:` no longer breaks the following `for`/`while`.
- **`mut` binding patterns** (`fn f(mut x: T)`) — `parse_pat` now accepts a
  leading `mut`.
- **Lifetime params/bounds in generics** (`<R: Send + 'static>`, `<'a, T>`) —
  `parse_generics` and `parse_where_clause` tolerate the lexer-dropped lifetime
  tokens (dangling `,` / trailing `+` before `>`), unblocking `clotho::pipeline`.
- **`Fn`/`FnMut`/`FnOnce` trait types & bounds** (`Box<dyn Fn(&str) -> String +
  Send>`, `F: FnOnce() -> String`, `F: Fn(&str) -> String + 'static`) — `parse_type`
  and the bound parser consume the parenthesized args, optional `-> Ret`, and
  trailing `+ Bound`s, unblocking `lachesis::propose` / `cache` / `pipeline_cache`.
- **`where`-clause termination** — the where-clause loop now stops at the body
  `{` rather than parsing it as a type.

### Open — surfaced by `python3 x.py self-host` (current first-parse blockers)
Offsets below are byte offsets **into the leaf `.rs` module file** (measured with
`moira --emit mir <file>`, which parses the module without lowering). These are
the deepest remaining gating constructs; most require new AST nodes that then
cascade into the type-checker / MIR lowers (the documented full-self-host wall).
- **moira (driver)**: **parses fully** — reaches type-check (the type-checker
  wall: `match`/closures/generics/`std`).
- **moira-alloc**: **parses fully** — type-check still reports borrow/type errors
  (subset does not model the allocator's raw-pointer patterns yet).
- **clotho**: `lexer.rs` ~byte 4217 `expected =>, found ..` — an inclusive range
   **pattern** `b'0'..=b'9'` in a `match` arm; **range patterns are now modeled**
   (see `Pat::Range` support in `lachesis::infer` + `atropos::lower::pattern_guard`),
   so this blocker is resolved at the type-check + codegen level. Still open: range
   **expressions** (`tts[..idx]`, `&sig.params[1..]` slice indexing) are not yet modeled.
- **atropos**: `lower.rs` ~byte 6652 `expected :, found ,` — a struct/enum field
  list the parser diverges on (likely a tuple/enum-variant field or a
  type-ascription `:` edge case).
- **lachesis**: `borrow.rs` ~byte 9056 `expected expression, found =` — a `let`
   binding (enum/tuple destructuring or `let … = …` with a pattern the subset
   does not yet lower); plus `solver.rs` tuple patterns in `match`, and
   `infer.rs` slice indexing. (The `diff.rs` float-literal suffix `0.0f64` is now
   parsed by `lex_number`, so it is no longer a parse blocker.)


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
       existed; `lachesis::infer` + `atropos::lower` now bind each component —
       validated by `tuple_destructure_*` tests). `for (x, y) in ..` destructuring
       is now also resolved for array-of-tuples (each component is read from the
       element's `.fN` field — validated by `for_over_tuple_array_*` tests); only
       `let`-bound (non-literal) array-of-tuples destructuring remains.
- [x] Closures type-check acceptance — `lachesis::infer::check_closure` now
       exists (the type checker previously failed to compile because it referenced
       the unimplemented method); closures are accepted at the type level. Capture
       analysis and MIR/codegen of closures remain open.
- [x] `match` accepted by `lachesis::infer` + lowered to MIR (common subset:
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
