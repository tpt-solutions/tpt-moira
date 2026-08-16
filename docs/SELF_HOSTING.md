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

- [ ] Deterministic MIR/SSA output across stages
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
- [x] `as` casts parsed (target type currently **discarded** — a tracked gap)

### Open — surfaced by `python3 x.py self-host` (parse now reaches deep into each crate)
Per-crate parse errors after the module/generics/attribute pass:
- **moira-alloc** (`impl` generics): `expected for, found <` — `impl<T> Trait for Ty<T>` generics-in-`impl` edge
- **clotho / lachesis** (closures / pipes): `expected expression, found |` — closures `|a, b| …` and `|` patterns
- **atropos** (`as`): `expected item, found as` — `x as T` casts and `use … as …`
- **moira (driver)** (`let` in expr): `expected expression, found let` — `let`-chains / `let-else` / control-flow in expr position

### Broader remaining gaps (not yet drilled into)
- [ ] `impl` inherent + trait methods with `self`/`&self` receivers
- [ ] `match` over enum variants + pattern matching
- [ ] `Vec` / `Option` / `Result` + `?` propagation
- [ ] Library codegen: emit `.rlib`/object archives, not just executables
- [ ] Full trait solving + `where`-clause lowering for the type checker

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
