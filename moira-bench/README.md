# moira-bench

Benchmark harness comparing [Moira](https://github.com/tpt-solutions/moira)
compile times against `rustc` on the Phase-1 language subset.

`moira-bench` compiles a fixed battery of Phase-1 programs (which are also valid
stock Rust) two ways and reports the median end-to-end compile time:

- **Moira** — the full Clotho front-end + Atropos mid-end (`--emit ssa`, which
  runs lex → parse → macro-expand → MIR lower → SSA build → optimize). No
  external assembler/linker is involved, isolating Moira's own work.
- **`rustc`** — `rustc --emit=metadata` (parse + full inference + borrow check +
  codegen to metadata), the closest apples-to-apples "compile this file"
  invocation that does not require a system linker.

The reported *speedup* is `rustc_ms / moira_ms`. Because Moira defers type
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

A table of per-program Moira vs. `rustc` times plus the geometric-mean speedup,
e.g.:

```
program  moira(ms)   rustc(ms)   speedup
----------------------------------------------
small        0.42        4.81      11.46x
medium       1.10       12.30      11.18x
...
```

When `--record` is set, results are appended to a JSON history and any slowdown
greater than the 10% regression threshold is reported.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license

at your option.
