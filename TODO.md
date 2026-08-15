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
- [ ] Concurrent macro-expansion pipeline (runs alongside file I/O so CPU never waits on disk)
- [x] Define and enforce the Phase-1 language subset grammar

### Atropos (Basic Optimizer & Codegen)
- [ ] MIR lowering for the Phase-1 subset
- [ ] Custom lightweight SSA backend (built from scratch, not Cranelift)
- [ ] Native codegen for the restricted subset
- [ ] Minimal optimization passes

### Core Infrastructure
- [ ] Region-based memory allocator for compiler-internal data structures
- [ ] Stage 0 bootstrap transpiler (macro-free Moira subset → C → native binary)
- [ ] Benchmark harness comparing Moira vs. `rustc` on the Phase-1 subset
- [ ] Validate 10x speedup target for the subset

## Phase 2 (Months 9-16) — The Lachesis Integration
Target: compile standard, idiomatic Rust code safely.

- [ ] Type inference engine
- [ ] Basic trait resolution (pre-specialization)
- [ ] Full NLL borrow graph solver (graph-traversal algorithm)
- [ ] Fast-path borrow checker for simple/local scopes
- [ ] Fallback logic: fast-path → full NLL graph analysis for complex/cross-function lifetimes
- [ ] Integrate Lachesis with Clotho and Atropos end-to-end
- [ ] Expression-level incremental cache (disk-backed, content-addressed, Salsa-like)
- [ ] Validate idiomatic standard-library-using Rust programs compile correctly

## Phase 3 (Months 17-24) — The Full Language
Target: full language support, matching `rustc` behavior.

- [ ] Async/await state machine generation
- [ ] Complex/full macro expansion support
- [ ] Next-generation Chalk-like Prolog-style trait solver
- [ ] Memoization + early-exit heuristics for trait coherence/specialization solving
- [ ] Stand up CI pipeline running the full `rustc` UI/error-message test suite nightly
- [ ] Build AI-assisted diffing tool to flag divergence from `rustc` output
- [ ] Triage and fix flagged divergences continuously

## Phase 4 (Months 25-30) — The Ouroboros & Production
Target: self-hosting, production-grade, ≥5x faster than `rustc` at scale.

- [ ] Stage 1 self-host: Moira compiles its own full Rust source
- [ ] Stage 2 self-host: Moira-compiled-Moira compiles itself again, verify output parity
- [ ] Final cross-subsystem optimization pass
- [ ] Validate against a full Linux kernel build
- [ ] Validate against large crates.io dependency graphs
- [ ] Achieve ≥5x speedup over `rustc` on production-scale codebases
- [ ] Error message parity audit against `rustc`
- [ ] Stability guarantees / versioning policy
- [ ] Release packaging & distribution

## Cross-Cutting / Ongoing Tracks
- [ ] Architecture documentation
- [ ] Contributor guide
- [ ] Test suite growth alongside `rustc` compatibility work
- [ ] Performance regression tracking / dashboards
