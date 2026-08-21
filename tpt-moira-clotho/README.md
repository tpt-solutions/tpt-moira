# Clotho

**Clotho** is the frontend of [Moira](https://github.com/tpt-solutions/tpt-moira), a
from-scratch Rust compiler. It is named after the first of the three Fates — she
who *spins* the thread of syntax.

Clotho provides a streaming lexer, a recursive-descent parser, and the abstract
syntax tree (AST) for the Phase-1 Rust language subset. It is the entry point the
Moira driver uses to turn source text into a typed, flattenable program
representation.

## Features

- **Streaming lexer** (`tpt_moira_clotho::lexer`) — single-pass, token-by-token, with
  bounded lookahead. Skips whitespace, line (`//`) and nested block (`/* */`)
  comments, and decodes string/char escapes.
- **Recursive-descent parser** (`tpt_moira_clotho::parser`) — a Pratt-style precedence
  parser with binding powers, producing a complete `tpt_moira_clotho::ast` tree with
  correct operator precedence.
- **Rich AST** (`tpt_moira_clotho::ast`) — items, expressions, types, patterns, and
  attributes for the supported subset.
- **Macro expansion pipeline** (`tpt_moira_clotho::pipeline`, `tpt_moira_clotho::macros`,
  `tpt_moira_clotho::macrorules`) — `macro_rules!` expansion with typed `derive` support
  (`tpt_moira_clotho::derive`).
- **Module resolution** (`tpt_moira_clotho::compile_file`) — resolves `mod` declarations
  into sibling `.rs` files (or `<name>/mod.rs`), flattening a crate into a
  single program. This lets Moira parse its own layout — a prerequisite for
  self-hosting.
- **Diagnostic model** (`tpt_moira_clotho::error`) — structured `Diag` errors with spans.
- **Token model** (`tpt_moira_clotho::token`) — the lexical token vocabulary.

## Installation

```toml
[dependencies]
clotho = "0.1.0"
```

## Usage

Parse a snippet (no file path required — e.g. from stdin):

```rust
use tpt_moira_clotho::parse_source;

let prog = parse_source("fn main() { let x = 1 + 2; }").unwrap();
assert_eq!(prog.items.len(), 1);
```

Tokenize a source string:

```rust
use tpt_moira_clotho::tokenize_source;

let tokens = tokenize_source("let x = 1;");
assert!(!tokens.is_empty());
```

Parse a real source file, resolving modules:

```rust
use std::path::Path;
use tpt_moira_clotho::compile_file;

let prog = compile_file(Path::new("src/main.rs"))?;
```

Run the concurrent macro-expansion pipeline:

```rust
use tpt_moira_clotho::compile_source;

let prog = compile_source(include_str!("../examples/phase1_demo.rs"))?;
```

## Supported Phase-1 subset

Functions, `let`/`mut` bindings, blocks, trailing tail expressions; control flow
(`if`/`else`, `while`, `loop`, `for ... in`, `match`); primitives and composites
(integers, floats, `bool`, `char`, `&T`, `&mut T`, arrays, slices, tuples,
paths, structs); struct definitions and struct-literal expressions; binary/unary
operators with correct precedence; assignment, ranges, calls, field access,
indexing, references, and deref; `use` declarations.

Deliberately out of scope for Phase 1: complex trait resolution, async/await,
advanced lifetime elision, and full macro expansion.

## Status

| Phase | Status |
|-------|--------|
| Phase 1 — frontend | ✅ implemented |

See the workspace [`spec.txt`](https://github.com/tpt-solutions/tpt-moira/blob/main/spec.txt)
and [`TODO.md`](https://github.com/tpt-solutions/tpt-moira/blob/main/TODO.md) for the
full design and roadmap.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license

at your option.
