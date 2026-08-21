# Atropos

**Atropos** is the mid-end and code generator of
[Moira](https://github.com/tpt-solutions/tpt-moira), a from-scratch Rust compiler.
It is named after the third of the three Fates — she who *cuts* the thread of
bloat.

Atropos lowers the Clotho AST into a lightweight, SSA-style mid-level IR (MIR)
and emits code through pluggable backends. The reference backend is the Stage-0
C transpiler; a from-scratch x86-64 native backend is the Phase-1/Phase-4 target.

## Features

- **MIR** (`tpt_moira_atropos::mir`) — a lightweight, typed mid-level IR suitable for
  lowering from the Clotho AST.
- **Lowering** (`tpt_moira_atropos::lower`) — `lower_program` converts a `tpt_moira_clotho::ast::Program`
  into an `tpt_moira_atropos::mir::Program` (`MirProgram`), with structured `LowerError`s.
- **SSA backend** (`tpt_moira_atropos::ssa`) — `build_ssa` promotes MIR into an SSA program
  (`SsaProgram`) with built-in optimization passes (`SsaProgram::optimize`).
- **C backend** (`tpt_moira_atropos::codegen_c`) — `emit_c` transpiles MIR to C, matching
  the bootstrap path: a macro-free Moira subset is translated to C and compiled
  to a native binary.
- **Native backend** (`tpt_moira_atropos::codegen_native`) — `emit_asm` and
  `compile_to_native` produce x86-64 assembly and native executables behind the
  same lower + emit interface.

All backends slot behind the same `lower` + emit boundary, so the Clotho AST
flows through MIR → SSA → optimized IR → target code without re-parsing.

## Installation

```toml
[dependencies]
atropos = "0.1.0"
clotho = "0.1.0"
```

## Usage

Lower an AST to MIR and emit C:

```rust
use tpt_moira_clotho::parse_source;
use tpt_moira_atropos::lower_program;
use tpt_moira_atropos::codegen_c::emit_c;

let prog = parse_source("fn main() { let x = 1 + 2; }").unwrap();
let mir = lower_program(&prog).expect("lowering succeeded");
let c_src = emit_c(&mir);
println!("{c_src}");
```

Build and optimize an SSA program:

```rust
use tpt_moira_atropos::{lower_program, build_ssa};

let prog = parse_source("fn main() { let x = 1 + 2; }").unwrap();
let mir = lower_program(&prog).unwrap();
let mut ssa = build_ssa(&mir).unwrap();
ssa.optimize();
println!("{ssa}");
```

## Status

| Phase | Status |
|-------|--------|
| Phase 1 — MIR + C backend | ✅ implemented |
| Phase 1/4 — SSA + native backend | 🚧 in progress |

See the workspace [`spec.txt`](https://github.com/tpt-solutions/tpt-moira/blob/main/spec.txt)
and [`TODO.md`](https://github.com/tpt-solutions/tpt-moira/blob/main/TODO.md) for the
full design and roadmap.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license

at your option.
