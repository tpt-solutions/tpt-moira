# Changelog

All notable changes to `atropos` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-20

### Added
- Lightweight, typed mid-level IR (MIR).
- AST → MIR lowering with structured `LowerError`s.
- SSA backend with optimization passes.
- Stage-0 C transpiler backend.
- From-scratch x86-64 native backend (assembly emission and native compilation).
