# moira-alloc

A region-based (arena) allocator for [Moira](https://github.com/tpt-solutions/moira)'s
compiler-internal data structures.

Moira avoids per-node heap allocations during compilation by parking all
long-lived IR nodes in an [`Arena`]. An arena is a *region*: every value handed
to [`Arena::alloc`] lives until the arena itself is dropped, at which point the
whole region is reclaimed in O(1). References returned by `alloc` are stable for
the arena's lifetime because each value is heap-backed individually (a `Box`),
so the arena never moves existing data.

This gives the predictable, near-zero-GC memory profile called for by the
design doc's "Memory Model" section, at a fraction of the cost of a general
allocator.

## Installation

```toml
[dependencies]
moira-alloc = "0.1.0"
```

## Usage

```rust
use moira_alloc::Arena;

let arena: Arena<String> = Arena::new();
let a = arena.alloc("hello".to_string());
let b = arena.alloc("world".to_string());

assert_eq!(arena.len(), 2);
// References stay valid even after more allocations.
let _c = arena.alloc("!".to_string());
assert_eq!(a, "hello");
assert_eq!(b, "world");

// Drop the arena to reclaim the entire region at once.
drop(arena);
```

## Why an arena?

- **O(1) teardown** — free every node by dropping one `Arena`.
- **Stable references** — `&T` outlives intermediate pushes; no moving GC.
- **Low fragmentation** — one region per value type, boxed individually.
- **Predictable** — no hidden allocation pressure during massive builds.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license

at your option.
