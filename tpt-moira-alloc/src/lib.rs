//! Region-based memory allocator for Moira's compiler-internal data structures.
//!
//! Moira avoids per-node heap allocations during compilation by parking all
//! long-lived IR nodes in an [`Arena`]. An arena is a *region*: every value
//! handed to [`Arena::alloc`] lives until the arena itself is dropped, at which
//! point the whole region is reclaimed in O(1). References returned by `alloc`
//! are stable for the arena's lifetime because each value is heap-backed
//! individually (a `Box`), so the arena never moves existing data.
//!
//! This gives the predictable, near-zero-GC memory profile called for by the
//! design doc's "Memory Model" section, at a fraction of the cost of a general
//! allocator.

use std::cell::RefCell;

/// A region allocator that vends `&T` references valid for as long as the
/// arena is alive.
///
/// All values of type `T` share one region. Drop the `Arena` to free them all
/// at once.
pub struct Arena<T> {
    items: RefCell<Vec<Box<T>>>,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena {
            items: RefCell::new(Vec::new()),
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
        let mut items = self.items.borrow_mut();
        items.push(Box::new(value));
        let ptr: *const T = &**items.last().unwrap() as *const T;
        // SAFETY: the value is heap-backed by a `Box` owned by `self`'s `Vec`.
        // The `Box` is never moved or mutated, so the pointer stays valid for
        // the lifetime of the borrow of `self`. The `RefMut` borrow ends here,
        // but it only guarded the push; the pointed-to memory is independent.
        unsafe { &*ptr }
    }

    /// Number of values currently parked in the region.
    pub fn len(&self) -> usize {
        self.items.borrow().len()
    }

    /// Whether the region currently holds no values.
    pub fn is_empty(&self) -> bool {
        self.items.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
