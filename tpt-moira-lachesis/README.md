# Lachesis

**Lachesis** is the type system and borrow checker of
[Moira](https://github.com/tpt-solutions/tpt-moira), a from-scratch Rust compiler.
It is named after the second of the three Fates — she who *measures* the thread
of lifetimes.

Lachesis wires the Clotho AST through type inference, trait resolution, and
borrow checking. The top-level [`check`](crate::check) function runs the full
end-to-end pass: type inference + MIR lowering + borrow checking + trait
coherence / well-formedness validation.

## Features

- **Type inference** (`tpt_moira_lachesis::infer`) — a direct, constraint-checking
  inference engine (`TypeChecker`) over the Clotho AST for the Phase-1 subset.
- **Trait resolution** (`tpt_moira_lachesis::traits`) — a built-in trait database the
  checker queries for operator applicability, plus a user-`impl` aware
  `Solver` (`tpt_moira_lachesis::solver`).
- **Borrow checking** (`tpt_moira_lachesis::borrow`) — a fast-path plus NLL-style CFG
  liveness analysis over the Atropos MIR.
- **Incremental caching** (`tpt_moira_lachesis::cache`) — a disk-backed,
  content-addressed store with Salsa-like memoization; whole-pipeline caching via
  `tpt_moira_lachesis::pipeline_cache`.
- **Diff / parity tooling** (`tpt_moira_lachesis::diff`) — divergence classification and
  error-message parity audit against `rustc`, with an offline AI root-cause
  proposer and triage queue (`tpt_moira_lachesis::propose`).

## Installation

```toml
[dependencies]
lachesis = "0.1.0"
clotho = "0.1.0"
```

## Usage

Run the full end-to-end pass on a Clotho program:

```rust
use tpt_moira_clotho::parse_source;
use tpt_moira_lachesis::check;

let prog = parse_source("fn main() { let x = 1 + 2; }").unwrap();
check(&prog).expect("program is well-typed and borrow-safe");
```

Type-check only:

```rust
use tpt_moira_lachesis::type_check;
type_check(&prog)?;
```

Lower to MIR and borrow-check only:

```rust
use tpt_moira_lachesis::borrow_check_program;
borrow_check_program(&prog)?;
```

Query trait coherence / well-formedness and associated-type projection:

```rust
use tpt_moira_lachesis::{coherence_report, normalize_assoc_type, ty::Ty};

let diags = coherence_report(&prog);
let proj = normalize_assoc_type(&prog, "Add", "Output", &Ty::I32);
```

The type-check and borrow-check passes are independent and run concurrently on
separate OS threads; diagnostics are reported deterministically (type errors
first, then borrow errors, then coherence / well-formedness).

## Status

| Phase | Status |
|-------|--------|
| Phase 2 — type system & borrow checker | 🚧 stub / in progress |

See the workspace [`spec.txt`](https://github.com/tpt-solutions/tpt-moira/blob/main/spec.txt)
and [`TODO.md`](https://github.com/tpt-solutions/tpt-moira/blob/main/TODO.md) for the
full design and roadmap.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license

at your option.
