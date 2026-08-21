# moira-bench

Benchmark harness comparing [Moira](https://github.com/tpt-solutions/tpt-moira)
compile times against other compilers (`rustc`, `gcc`, `clang`) on the Phase-1
language subset.

`moira-bench` compiles a fixed battery of Phase-1 programs (which are also valid
stock Rust) and reports the median end-to-end compile time for each:

- **Moira** — the full Clotho front-end + Atropos mid-end (`--emit ssa`, which
  runs lex → parse → macro-expand → MIR lower → SSA build → optimize). No
  external assembler/linker is involved, isolating Moira's own work.
- **C compiler** (`gcc`/`clang`) — Moira's Stage-0 C backend emits C
  (`--emit c`), then an external C compiler builds it (`cc -O2`). This is a real
  cross-compiler comparison: the same source, Moira's front-end+mid-end versus a
  production C compiler doing the final codegen. Enable with `--cc gcc` or
  `--cc clang`.
- **`rustc`** — `rustc --emit=metadata` (parse + full inference + borrow check +
  codegen to metadata), the closest apples-to-apples "compile this file"
  invocation that does not require a system linker.

The reported *speedup* is `baseline_ms / moira_ms`. Because Moira defers type
inference and borrow checking to Phase 2, this is not yet a like-for-like
comparison of *semantic* compilation — it measures the cost of the front-end +
mid-end pipeline Moira ships today. The harness tracks that number as the
compiler grows and as incremental caching (Phase 2) and parallelization land.

## Running

```sh
# Build the workspace first so the `moira` binary is available.
cargo build --release

# Run the benchmark battery (median over 30 iterations per program).
cargo run -p moira-bench

# Add a gcc/clang cross-compiler column (compiles Moira's emitted C).
cargo run -p moira-bench -- --cc gcc
cargo run -p moira-bench -- --cc clang

# Record a run and compare against the previous one (regression detection).
cargo run -p moira-bench -- --record

# Time Moira's full semantic pipeline instead (type-check + borrow-check + cache).
cargo run -p moira-bench -- --full

# Compare rustc including native codegen + linking.
cargo run -p moira-bench -- --rustc-mode full

# Print the recorded-run dashboard.
cargo run -p moira-bench -- --dashboard

# Persist results to a custom directory.
cargo run -p moira-bench -- --record --results ./bench-results
```

## Output

A boxed, aligned table of per-program Moira vs. baseline times plus the
geometric-mean speedup. With `--cc` a third column is added comparing Moira's
front-end+mid-end against a production C compiler building Moira's emitted C.

```
╭──────────────────────────────────────────────────────────────────────────╮
│  Moira compile-time benchmark — Phase-1 subset                             │
╰──────────────────────────────────────────────────────────────────────────╯
  30 iterations per program (median reported). Moira = front-end + SSA mid-end.
  baselines: rustc = rustc --emit=metadata   |   C compiler = gcc -O2

  program        moira     gcc -O2        cc×       rustc     rustc×
  ─────────────────────────────────────────────────────────────────────────
  small          0.42ms       4.85ms      11.55x      4.81ms     11.46x
  medium         1.10ms      12.40ms      11.27x     12.30ms     11.18x
  ...
  ─────────────────────────────────────────────────────────────────────────
  geomean                               11.40x                 11.30x

  summary: Moira is 11.40x faster than gcc -O2 and 11.30x faster than rustc --emit=metadata
```

When `--record` is set, results are appended to a plain-text history and any
slowdown greater than the 10% regression threshold is reported.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license

at your option.
