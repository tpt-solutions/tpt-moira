//! Whole-pipeline incremental cache (Phase-4 follow-up to the Phase-2 SSA cache).
//!
//! The Phase-2 cache ([`crate::cache`]) memoized a single artifact (SSA text) by
//! source-hash. This module generalizes that to the *entire* compiler pipeline:
//! the type-check/borrow-check outcome, the MIR, and the SSA are all cached
//! under the same content-addressed key, so an unchanged source skips the
//! frontend, the checker, and lowering entirely.
//!
//! Artifacts are stored as plain text (diagnostics render via [`Diag`]'s
//! `Display`, MIR/SSA via their `Display`), which keeps the cache
//! dependency-free and trivially inspectable.

use std::io;
use std::path::Path;

use tpt_moira_clotho::error::Diag;

use crate::cache::{content_hash, Cache};

/// Sentinel written when a cached `check` run accepted the program.
const CHECK_OK: &str = "\0OK\0";

/// A content-addressed cache spanning the full Moira pipeline.
pub struct PipelineCache {
    inner: Cache,
}

impl PipelineCache {
    /// Open (creating if needed) a pipeline cache rooted at `dir`.
    pub fn open(dir: &Path) -> io::Result<PipelineCache> {
        Ok(PipelineCache {
            inner: Cache::open(dir)?,
        })
    }

    /// Run/restore a type + borrow check result under `source`'s content hash.
    ///
    /// On a cache hit the stored outcome is reconstructed and returned without
    /// re-running the checker; on a miss `f` runs, its result is persisted, and
    /// it is returned. `f` should perform the same [`crate::check`] the driver
    /// would otherwise run.
    pub fn checked<F>(&self, source: &str, f: F) -> Result<(), Vec<Diag>>
    where
        F: FnOnce() -> Result<(), Vec<Diag>>,
    {
        let key = content_hash(source);
        if let Some(bytes) = self.inner.get(key) {
            let stored = String::from_utf8_lossy(&bytes);
            if stored == CHECK_OK {
                return Ok(());
            }
            // Reconstruct diagnostics as plain errors (spans are not persisted;
            // this matches the text the driver would have printed).
            let diags = stored
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| Diag::error(l.to_string(), None))
                .collect();
            return Err(diags);
        }
        let result = f();
        // Only cache a *successful* check. A rejected program must be
        // re-validated on every run: its diagnostics may depend on transient
        // state (partial files, available memory) and a cached failure would
        // otherwise poison the cache and reject valid code permanently.
        if result.is_ok() {
            let _ = self.inner.insert(key, CHECK_OK.as_bytes());
        }
        result
    }

    /// Run/restore a textual artifact (e.g. MIR or SSA) for `source` under
    /// `label`. `label` distinguishes the several artifacts derived from the
    /// same source (so they do not collide in the blob store).
    pub fn text<F>(&self, source: &str, label: &str, f: F) -> String
    where
        F: FnOnce() -> String,
    {
        let key = self.artifact_key(source, label);
        if let Some(bytes) = self.inner.get(key) {
            if let Ok(s) = String::from_utf8(bytes) {
                return s;
            }
        }
        let s = f();
        let _ = self.inner.insert(key, s.as_bytes());
        s
    }

    /// Whether a textual artifact for `source`/`label` is already cached.
    pub fn contains_text(&self, source: &str, label: &str) -> bool {
        self.inner.contains(self.artifact_key(source, label))
    }

    fn artifact_key(&self, source: &str, label: &str) -> u64 {
        // Fold the label into the content hash so different artifacts for the
        // same source live at distinct keys.
        let mut h = content_hash(source);
        for b in label.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }
}

/// A driver-facing wrapper that transparently disables the cache when
/// `MOIRA_NO_CACHE` is set (used by cross-process determinism checks), so the
/// rest of the driver can call [`PipelineCacheExt::checked`] /
/// [`PipelineCacheExt::text`] unconditionally.
pub struct PipelineCacheExt {
    inner: Option<PipelineCache>,
}

impl PipelineCacheExt {
    /// Open the cache under the standard temp dir, or a no-op when caching is
    /// disabled via `MOIRA_NO_CACHE`.
    pub fn new() -> PipelineCacheExt {
        let dir = std::env::temp_dir().join("moira_cache");
        let inner = if std::env::var("MOIRA_NO_CACHE").is_err() {
            PipelineCache::open(&dir).ok()
        } else {
            None
        };
        PipelineCacheExt { inner }
    }
}

impl Default for PipelineCacheExt {
    fn default() -> Self {
        PipelineCacheExt::new()
    }
}

impl PipelineCacheExt {
    /// Run/restore a type-check + borrow-check outcome, or compute it.
    pub fn checked<F>(&self, source: &str, f: F) -> Result<(), Vec<Diag>>
    where
        F: FnOnce() -> Result<(), Vec<Diag>>,
    {
        match &self.inner {
            Some(c) => c.checked(source, f),
            None => f(),
        }
    }

    /// Run/restore a textual artifact, or compute it.
    pub fn text<F>(&self, source: &str, label: &str, f: F) -> String
    where
        F: FnOnce() -> String,
    {
        match &self.inner {
            Some(c) => c.text(source, label, f),
            None => f(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn checked_caches_and_reuses() {
        let dir = tmp("moira_pipeline_cache_test");
        let cache = PipelineCache::open(&dir).unwrap();
        let calls = Cell::new(0);
        let src = "fn main() {}";
        let run = || -> Result<(), Vec<Diag>> {
            calls.set(calls.get() + 1);
            Err(vec![Diag::error("boom".to_string(), None)])
        };
        // A failed check is NOT cached: the closure runs on every miss, and the
        // second call re-runs it (rather than replaying a stale rejection).
        let _ = cache.checked(src, &run);
        assert_eq!(calls.get(), 1);
        let res: Result<(), Vec<Diag>> = cache.checked(src, &run);
        assert_eq!(calls.get(), 2);
        assert!(res.is_err());
        // A successful check IS cached: the closure runs once.
        let ok: Result<(), Vec<Diag>> = cache.checked(src, || Ok(()));
        assert!(ok.is_ok());
        let replay: Result<(), Vec<Diag>> = cache.checked(src, || unreachable!());
        assert!(replay.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checked_ok_roundtrips() {
        let dir = tmp("moira_pipeline_cache_ok");
        let cache = PipelineCache::open(&dir).unwrap();
        let src = "fn main() {}";
        let first: Result<(), Vec<Diag>> = cache.checked(src, || Ok(()));
        assert!(first.is_ok());
        // Replaying with a closure that would "fail" must still return Ok from cache.
        let second: Result<(), Vec<Diag>> =
            cache.checked(src, || Err(vec![Diag::error("x", None)]));
        assert!(second.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_caches_and_is_label_scoped() {
        let dir = tmp("moira_pipeline_cache_text");
        let cache = PipelineCache::open(&dir).unwrap();
        let src = "fn main() {}";
        let a = cache.text(src, "mir", || "MIR_A".to_string());
        assert_eq!(a, "MIR_A");
        // Same source+label hits the cache, returning the first value.
        let b = cache.text(src, "mir", || "MIR_B".to_string());
        assert_eq!(b, "MIR_A");
        // A different label is a distinct artifact.
        let c = cache.text(src, "ssa", || "SSA_A".to_string());
        assert_eq!(c, "SSA_A");
        assert!(cache.contains_text(src, "ssa"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ext_disables_under_no_cache() {
        std::env::set_var("MOIRA_NO_CACHE", "1");
        let ext = PipelineCacheExt::new();
        assert!(ext.inner.is_none());
        let calls = Cell::new(0);
        let src = "fn main() {}";
        let res: Result<(), Vec<Diag>> = ext.checked(src, || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert!(res.is_ok());
        assert_eq!(calls.get(), 1);
        std::env::remove_var("MOIRA_NO_CACHE");
    }
}
