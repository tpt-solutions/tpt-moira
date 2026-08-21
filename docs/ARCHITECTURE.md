# Moira Architecture

Moira ("the thread") is a from-scratch, high-performance Rust compiler divided
into three cooperating subsystems, named after the Moirai:

* **Clotho** — the frontend: a streaming lexer, recursive-descent parser, AST, and
  concurrent macro-expansion pipeline.
* **Lachesis** — the middle: type inference, trait resolution, and the borrow
  checker. It measures the thread (proves memory safety) before code is emitted.
* **Atropos** — the backend: MIR lowering, a custom lightweight SSA mid-end with
  optimization passes, and native/x86-64 + Stage-0 C codegen.

A small `moira-alloc` region allocator and the `moira-bench` harness round out
the workspace.

## Compilation pipeline

```
source ──▶ Clotho ──▶ AST ──▶ (macro expand) ──▶ AST
                         │
                         ▼
                    Lachesis (type-check + borrow-check)   ← `--emit check`, `--check`
                         │
                         ▼
                    Atropos lower ──▶ MIR ──▶ SSA ──▶ (optimize) ──▶ SSA
                         │                                      │
            ┌────────────┴───────────────┬───────────────────────┘
            ▼                            ▼
   `--emit c` (Stage-0 C)        `--emit asm` / `--emit native` (x86-64)
```

The driver (`src/main.rs`) selects an `--emit` mode:

| mode    | output                                                        |
|---------|---------------------------------------------------------------|
| `ast`   | parsed item summary                                           |
| `mir`   | Atropos MIR (debug)                                          |
| `ssa`   | SSA text (memoized via the content-addressed cache)          |
| `c`     | Stage-0 C translation (bootstrap path)                        |
| `asm`   | x86-64 assembly (System V) from the from-scratch SSA backend |
| `native`| assembled + linked executable                                |
| `check` | run Lachesis only (type + borrow)                             |

Passing `--check` runs Lachesis *before* any codegen mode and aborts on the
first diagnostic.

## Lachesis internals

* **Type inference** (`tpt_moira_lachesis::infer`) — a direct (non-generic) inference
  engine. Every expression's type is computed locally (literals default to `i32`,
  variables resolve from a scope stack, calls from signatures, fields from struct
  definitions) while operand constraints are checked on the fly. Unknown
  operands become the `Unknown` error sentinel so one mistake does not cascade.
* **Trait resolution** (`tpt_moira_lachesis::traits`) — a built-in trait database the
  checker queries for operator applicability (`Add`, `Sub`, `Mul`, `Div`, `Rem`,
  bitwise, `PartialEq`, `PartialOrd`). This is the seam for the Phase-3
  Prolog-style solver.
* **Borrow checking** (`tpt_moira_lachesis::borrow`) — runs over the Atropos MIR. The fast
  path skips functions with no references. Otherwise it builds the CFG from
  terminators, propagates reference *aliases* (a variable copied from a borrow
  temp keeps the borrow alive), and reports a conflict whenever a place is
  mutated at a point where a reference to it is still live, or when a reference to
  a local escapes through `return`.

**Limitation:** the Clotho parser drops `&`/`&mut` mutability, so the borrow
checker cannot yet distinguish shared from mutable borrows. It enforces the sound
rule "a borrowed place may not be mutated while borrowed", which is correct for
`&mut` and for `&` (you may not mutate through a shared borrow) and never rejects
valid coexisting `&` borrows.

## Incremental cache

`tpt_moira_lachesis::cache` is a disk-backed, content-addressed store. Artifacts are keyed
by the FNV-1a hash of their source text, so recompiling unchanged code reuses the
cached result. `IncrementalCache::compute_if_absent` mirrors a memoized query
(the Salsa-like pattern) and is wired into `--emit ssa`.

## Benchmark harness

`moira-bench` compiles a fixed battery of Phase-1-subset programs with Moira
(`--emit ssa`) and `rustc --emit=metadata`, reporting median compile times and a
geometric-mean speedup. It runs in CI so the speedup is tracked over time.
