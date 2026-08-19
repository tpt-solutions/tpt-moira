# Changelog

All notable changes to `clotho` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-20

### Added
- Streaming, single-pass lexer with bounded lookahead and comment/escape handling.
- Recursive-descent (Pratt-style) parser producing the full AST with correct
  operator precedence.
- Rich AST covering items, expressions, types, patterns, and attributes.
- `macro_rules!` expansion pipeline with typed `derive` support.
- Module resolution (`compile_file`) that flattens `mod` declarations into a
  single program.
- Structured `Diag` diagnostic model with spans.
