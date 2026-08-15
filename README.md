# TPT Moira

**Moira** is a from-scratch, high-performance compiler for the Rust programming
language, built by TPT Solutions. It is named after the Moirai — the three Greek
Fates who spun, measured, and cut the thread of life — because the compiler
pipeline is divided into three focused subsystems:

| Subsystem | Role | Status |
|-----------|------|--------|
| **Clotho** | Streaming lexer, parser, and AST (spins the thread of syntax) | ✅ Phase 1 — implemented |
| **Lachesis** | Type inference, trait resolution, NLL borrow checker (measures the lifetimes) | 🚧 Phase 2 — stub |
| **Atropos** | MIR lowering, SSA backend, native codegen (cuts the bloat) | 🚧 Phase 1 — stub |

See [`spec.txt`](spec.txt) for the full design document and [`TODO.md`](TODO.md)
for the phased roadmap.

## Goals

- **Semantic compatibility** with the Rust language.
- **Order-of-magnitude faster** compilation than `rustc` for the supported
  subset, via a single-pass architecture, aggressive incremental caching, and a
  highly parallelized borrow checker.
- A custom, region-based memory allocator for predictable memory use during
  massive builds.

## Repository layout

This is a Cargo workspace:

```
moira/            driver binary (wires the three Fates together)
clotho/           frontend: streaming lexer, recursive-descent parser, AST
lachesis/         type system + borrow checker (Phase 2)
atropos/          MIR lowering + codegen backend (Phase 1+)
examples/         sample Phase-1 sources
```

## Building

Requires a stable Rust toolchain (1.85+):

```sh
cargo build --release
```

Run the tests:

```sh
cargo test
```

## Usage

The driver currently runs the **Clotho** frontend: it lexes and parses a source
file (or stdin) and reports the parsed program.

```sh
# From a file
cargo run -- examples/phase1_demo.rs

# From stdin
echo 'fn main() { let x = 1 + 2; }' | cargo run --
```

## The Phase-1 language subset

Phase 1 ("The No-Magic Compiler") targets a strict subset of Rust:

- Functions, `let`/`mut` bindings, blocks, and trailing tail expressions.
- Control flow: `if`/`else`, `while`, `loop`, `for ... in`, `match`.
- Primitive and composite types: integers, floats, `bool`, `char`, `&T`,
  `&mut T`, arrays `[T; N]`, slices `[T]`, tuples, paths, and struct types.
- Struct definitions and struct-literal expressions.
- Expressions: literals, binary/unary operators (with correct precedence),
  assignment, ranges `..`, function/method calls, field access, indexing,
  references, and deref.
- `use` declarations.

Deliberately **out of scope** for Phase 1: complex trait resolution, async/await,
advanced lifetime elision, and full macro expansion.

## Architecture notes

- **Streaming lexer** (`clotho::lexer`): single-pass, token-by-token, with
  bounded lookahead. Skips whitespace, line (`//`) and nested block (`/* */`)
  comments, and decodes string/char escapes.
- **Recursive-descent parser** (`clotho::parser`): a Pratt-style precedence
  parser with binding powers, producing the `clotho::ast` tree.

## Contributing

Contributions are welcome. Please read the roadmap in [`TODO.md`](TODO.md) and
the design in [`spec.txt`](spec.txt) before starting work, and ensure
`cargo test` passes. This project is in active early development; APIs will
change.

## License

Licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE-2.0`](LICENSE-APACHE-2.0)), or
- MIT license ([`LICENSE-MIT`](LICENSE-MIT))

at your option.

Copyright © 2026 TPT Solutions.
