# Changelog

All notable changes to `moira-alloc` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-20

### Added
- Region-based `Arena<T>` allocator with O(1) teardown.
- `alloc` returning stable `&T` references valid for the arena's lifetime.
- `len` / `is_empty` region introspection.
