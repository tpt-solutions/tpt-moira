//! Region-based memory allocator for Moira's compiler-internal data structures.
//!
//! Moira avoids per-node heap allocations during compilation by parking all
//! long-lived IR nodes in an [`Arena`]. An arena is a *region*: every value
//! handed to [`Arena::alloc`] lives until the arena itself is dropped, at which
//! point the whole region is reclaimed in O(1).
//!
//! Storage is bump-allocated in chunks. Each chunk is a `Vec<T>` whose capacity
//! is fixed when it is created and is never exceeded, so a chunk never
//! reallocates: the address of a value handed out by [`Arena::alloc`] is stable
//! for the arena's lifetime, even as later allocations add chunks. Chunk sizes
//! double up to [`MAX_CHUNK_LEN`], so an arena of `n` values costs O(log n)
//! heap allocations rather than one per value.
//!
//! This gives the predictable, near-zero-GC memory profile called for by the
//! design doc's "Memory Model" section, at a fraction of the cost of a general
//! allocator.

use std::cell::RefCell;

/// Length of the first chunk. Later chunks double until [`MAX_CHUNK_LEN`].
pub const FIRST_CHUNK_LEN: usize = 64;

/// Upper bound on a single chunk's length, so one arena never holds a very large
/// contiguous block that would be wasteful for a small compilation.
pub const MAX_CHUNK_LEN: usize = 1 << 16;

/// A region allocator that vends `&T` references valid for as long as the
/// arena is alive.
///
/// All values of type `T` share one region. Drop the `Arena` to free them all
/// at once.
pub struct Arena<T> {
    /// Filled chunks plus the one being filled (always the last). A chunk is
    /// only ever pushed to while its length is below its capacity, so its
    /// buffer never moves.
    chunks: RefCell<Vec<Vec<T>>>,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena {
            chunks: RefCell::new(Vec::new()),
        }
    }
}

impl<T> Arena<T> {
    /// Create an empty region.
    pub fn new() -> Self {
        Arena::default()
    }

    /// Allocate `value` in the region and return a stable reference to it.
    ///
    /// The returned reference is valid for as long as the arena is alive.
    pub fn alloc(&self, value: T) -> &T {
        let mut chunks = self.chunks.borrow_mut();
        let full = match chunks.last() {
            Some(chunk) => chunk.len() == chunk.capacity(),
            None => true,
        };
        if full {
            let cap = match chunks.last() {
                Some(chunk) => (chunk.capacity() * 2).clamp(FIRST_CHUNK_LEN, MAX_CHUNK_LEN),
                None => FIRST_CHUNK_LEN,
            };
            chunks.push(Vec::with_capacity(cap));
        }
        let chunk = chunks.last_mut().expect("a chunk was just ensured");
        // Never reallocates: `len < capacity` was ensured above.
        chunk.push(value);
        let ptr: *const T = chunk.last().expect("a value was just pushed") as *const T;
        // SAFETY: the value lives in a chunk buffer owned by `self`. That buffer
        // is never reallocated (we only push below its capacity) and is never
        // shrunk or dropped before the arena is, so `ptr` stays valid for the
        // lifetime of the borrow of `self`. The `RefMut` only guarded the push.
        unsafe { &*ptr }
    }

    /// Number of values currently parked in the region.
    pub fn len(&self) -> usize {
        // Explicit loops rather than `map(Vec::len)`: Moira's checker does not yet take
        // a method path as a function value, and this crate must stay self-hostable.
        let chunks = self.chunks.borrow();
        let mut n = 0;
        for c in chunks.iter() {
            n += c.len();
        }
        n
    }

    /// Whether the region currently holds no values.
    pub fn is_empty(&self) -> bool {
        let chunks = self.chunks.borrow();
        for c in chunks.iter() {
            if !c.is_empty() {
                return false;
            }
        }
        true
    }

    /// Number of heap chunks the region has grown into (for diagnostics and
    /// tests of the growth policy).
    pub fn chunk_count(&self) -> usize {
        self.chunks.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn alloc_returns_stable_refs() {
        let arena: Arena<String> = Arena::new();
        let a = arena.alloc("hello".to_string());
        let b = arena.alloc("world".to_string());
        assert_eq!(arena.len(), 2);
        // References remain valid even after more allocations.
        let _c = arena.alloc("!".to_string());
        assert_eq!(a, "hello");
        assert_eq!(b, "world");
    }

    #[test]
    fn region_freed_on_drop() {
        let arena: Arena<Vec<u8>> = Arena::new();
        arena.alloc(vec![1, 2, 3]);
        assert!(!arena.is_empty());
        drop(arena); // entire region reclaimed at once
    }

    #[test]
    fn references_stay_valid_across_chunk_growth() {
        let arena: Arena<u64> = Arena::new();
        let n: u64 = 10_000;
        let refs: Vec<&u64> = (0..n).map(|i| arena.alloc(i * 3)).collect();
        assert_eq!(arena.len(), n as usize);
        // Growth went past the first chunk, so several chunks exist...
        assert!(arena.chunk_count() > 1, "expected the region to grow");
        // ...and chunk doubling keeps the count logarithmic in `n`.
        assert!(arena.chunk_count() <= 12, "chunk count {}", arena.chunk_count());
        // Every reference, handed out before later chunks existed, still reads back.
        for (i, r) in refs.iter().enumerate() {
            assert_eq!(**r, i as u64 * 3);
        }
    }

    #[test]
    fn chunk_lengths_double_then_cap() {
        let arena: Arena<u8> = Arena::new();
        for i in 0..(FIRST_CHUNK_LEN + MAX_CHUNK_LEN + 1) {
            arena.alloc(i as u8);
        }
        let chunks = arena.chunks.borrow();
        assert_eq!(chunks[0].capacity(), FIRST_CHUNK_LEN);
        assert_eq!(chunks[1].capacity(), FIRST_CHUNK_LEN * 2);
        assert!(chunks.iter().all(|c| c.capacity() <= MAX_CHUNK_LEN));
    }

    /// Counts how many live instances exist, to check every value is dropped once.
    struct Tracked(Rc<Cell<usize>>);

    impl Tracked {
        fn new(live: &Rc<Cell<usize>>) -> Self {
            live.set(live.get() + 1);
            Tracked(Rc::clone(live))
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }

    #[test]
    fn every_value_is_dropped_exactly_once() {
        let live = Rc::new(Cell::new(0));
        {
            let arena: Arena<Tracked> = Arena::new();
            for _ in 0..500 {
                arena.alloc(Tracked::new(&live));
            }
            assert_eq!(live.get(), 500);
        }
        assert_eq!(live.get(), 0);
    }
}
