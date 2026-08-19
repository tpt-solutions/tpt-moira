# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-20

### Added
- Initial release of the Moira workspace.
- `clotho`: streaming lexer, recursive-descent (Pratt) parser, and AST frontend.
- `clotho`: module resolution (`compile_file`) and concurrent macro-expansion
  pipeline (`compile_source`), including `macro_rules!` and `derive`.
- `lachesis`: type inference, built-in trait database, borrow checker, and
  incremental content-addressed caching.
- `lachesis`: `moira-diff` divergence classification and parity tooling against
  `rustc`, plus an offline AI root-cause proposer.
- `atropos`: MIR lowering, SSA backend with optimization passes, C transpiler,
  and an x86-64 native backend.
- `moira-alloc`: region-based (arena) allocator for compiler-internal data.
- `moira-bench`: benchmark harness comparing Moira vs `rustc` compile times.
- `moira`: driver binary wiring the three Fates together, with `ast`, `mir`,
  `ssa`, `c`, `asm`, `native`, and `check` emit modes.
