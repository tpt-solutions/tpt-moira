# Changelog

All notable changes to `lachesis` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-20

### Added
- Constraint-checking type inference engine over the Clotho AST.
- Built-in trait database for operator applicability queries.
- User-`impl` aware trait solver with coherence and well-formedness reporting.
- Fast-path + NLL-style CFG liveness borrow checker over the Atropos MIR.
- Disk-backed, content-addressed incremental cache with Salsa-like memoization.
- `moira-diff` divergence classification and `rustc` error-message parity audit.
- Offline AI root-cause proposer and triage queue.
- Concurrent type-check + borrow-check passes with deterministic diagnostics.
