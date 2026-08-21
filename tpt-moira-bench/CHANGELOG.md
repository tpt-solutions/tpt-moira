# Changelog

All notable changes to `moira-bench` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-20

### Added
- Benchmark battery comparing Moira front-end + SSA mid-end vs `rustc --emit=metadata`.
- Median-over-iterations timing with warm-up, geometric-mean speedup reporting.
- `--record` mode persisting runs to JSON history with 10% regression detection.
- `--full` (Moira semantic pipeline) and `--rustc-mode full` (native codegen + link) modes.
- `--dashboard` textual view of recorded runs and regression analysis.
