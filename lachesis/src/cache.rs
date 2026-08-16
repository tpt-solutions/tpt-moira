//! Disk-backed, content-addressed incremental cache (Salsa-like).
//!
//! This is the Phase-2 "expression-level incremental cache": artifacts are keyed
//! by the *content hash* of their source, so recompiling unchanged code reuses
//! the cached result instead of recomputing it. `IncrementalCache` wraps the raw
//! [`Cache`] blob store with a `compute_if_absent` helper that mirrors a
//! memoized query: callers pass a key plus a closure; the closure runs only when
//! the key is missing.
//!
//! Keys are 64-bit FNV-1a hashes of the source text. In a full deployment each
//! function/expression would be cached under its own hash and the cache would be
//! invalidated per-dependency; this module provides the storage and lookup
//! primitives the higher-level query layer builds on.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// FNV-1a 64-bit hash of a string. Stable and dependency-free.
pub fn content_hash(src: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in src.as_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A raw content-addressed blob store rooted at a directory.
///
/// Each artifact is written to `<dir>/<hash>.bin`. Missing files read as
/// `None`, so the store is naturally cache-like.
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// Open (creating if needed) a cache rooted at `dir`.
    pub fn open(dir: &Path) -> io::Result<Cache> {
        fs::create_dir_all(dir)?;
        Ok(Cache {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self, key: u64) -> PathBuf {
        self.dir.join(format!("{:016x}.bin", key))
    }

    /// Whether an artifact for `key` is already cached.
    pub fn contains(&self, key: u64) -> bool {
        self.path(key).exists()
    }

    /// Read the cached artifact bytes, if present.
    pub fn get(&self, key: u64) -> Option<Vec<u8>> {
        fs::read(self.path(key)).ok()
    }

    /// Store `data` under `key`.
    pub fn insert(&self, key: u64, data: &[u8]) -> io::Result<()> {
        fs::write(self.path(key), data)
    }
}

/// A higher-level, text-artifact cache with memoized computation.
///
/// Demonstrates the Salsa-like pattern: `compute_if_absent(key, f)` returns the
/// cached string for `key` when present, otherwise runs `f`, stores its result,
/// and returns it.
pub struct IncrementalCache {
    inner: Cache,
}

impl IncrementalCache {
    /// Open an incremental cache at `dir`.
    pub fn open(dir: &Path) -> io::Result<IncrementalCache> {
        Ok(IncrementalCache {
            inner: Cache::open(dir)?,
        })
    }

    /// Return the cached artifact for `key`, computing and storing it via `f`
    /// only when the cache misses.
    pub fn compute_if_absent<F>(&self, key: u64, f: F) -> String
    where
        F: FnOnce() -> String,
    {
        if let Some(bytes) = self.inner.get(key) {
            if let Ok(s) = String::from_utf8(bytes) {
                return s;
            }
        }
        let s = f();
        let _ = self.inner.insert(key, s.as_bytes());
        s
    }
}
