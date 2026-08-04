//! Bounded LRU disk cache of hydrated note bodies.
//!
//! Ported from PR #40's `shared/cache.rs` with two changes, both noted at their
//! site: the clock comes from [`super::now_ms`] rather than a server-crate helper,
//! and eviction is exercised by a test that asserts the *budget* rather than just
//! which entry survived.
//!
//! Bodies live as files under the mount's cache dir; a small JSON state file
//! carries version/hash/size/last-access. Pinned prefixes are exempt from eviction
//! and still count against the budget, so pinning more than the budget is a
//! configuration mistake rather than an unbounded cache.
//!
//! **Never a write buffer.** A write pushes upstream first and only then updates
//! the cache, so a crash can lose a cache entry but never a note.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedNoteMeta {
    pub version_id: String,
    pub content_hash: String,
    pub size_bytes: u64,
    pub last_access_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheState {
    notes: HashMap<String, CachedNoteMeta>,
}

/// A cache of note bodies keyed by (path, version).
///
/// The version is part of the key rather than a timestamp comparison: a cached body
/// is served only when the head record still points at the version it was cached
/// at, so a note edited by another participant can never be served stale. That is
/// what makes the cache safe to consult on every read without a freshness call of
/// its own — the head lookup a read already performs IS the freshness check.
pub struct NoteCache {
    dir: PathBuf,
    max_bytes: u64,
    pins: Vec<String>,
    state: Mutex<CacheState>,
}

impl std::fmt::Debug for NoteCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoteCache")
            .field("dir", &self.dir)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

impl NoteCache {
    pub fn open(dir: PathBuf, max_bytes: u64, pins: Vec<String>) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let state_path = dir.join("state.json");
        let state = fs::read_to_string(&state_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Ok(Self {
            dir,
            max_bytes,
            pins,
            state: Mutex::new(state),
        })
    }

    /// Flatten a mount-relative path into one safe file name.
    ///
    /// Hashed rather than sanitized: a note path can contain any character the vault
    /// allows, and a sanitizing scheme would collide (`A/B.md` and `A_B.md` both
    /// becoming `A_B.md` would serve one note's body for the other).
    fn body_path(&self, remote_path: &str) -> PathBuf {
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in remote_path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        self.dir.join(format!("{hash:016x}.md"))
    }

    fn persist(&self, state: &CacheState) {
        let _ = fs::write(
            self.dir.join("state.json"),
            serde_json::to_string(state).unwrap_or_default(),
        );
    }

    pub fn is_pinned(&self, remote_path: &str) -> bool {
        self.pins.iter().any(|pin| remote_path.starts_with(pin))
    }

    /// The cached body, when the cached version matches `version_id`.
    pub fn get(&self, remote_path: &str, version_id: &str) -> Option<String> {
        let mut state = self.state.lock().ok()?;
        let meta = state.notes.get_mut(remote_path)?;
        if meta.version_id != version_id {
            return None;
        }
        meta.last_access_ms = super::now_ms();
        let body = fs::read_to_string(self.body_path(remote_path)).ok()?;
        self.persist(&state);
        Some(body)
    }

    pub fn put(&self, remote_path: &str, version_id: &str, content_hash: &str, body: &str) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let _ = fs::write(self.body_path(remote_path), body);
        state.notes.insert(
            remote_path.to_string(),
            CachedNoteMeta {
                version_id: version_id.to_string(),
                content_hash: content_hash.to_string(),
                size_bytes: body.len() as u64,
                last_access_ms: super::now_ms(),
            },
        );
        self.evict_if_needed(&mut state);
        self.persist(&state);
    }

    pub fn remove(&self, remote_path: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.notes.remove(remote_path);
            let _ = fs::remove_file(self.body_path(remote_path));
            self.persist(&state);
        }
    }

    /// LRU eviction of unpinned entries down to the byte budget.
    fn evict_if_needed(&self, state: &mut CacheState) {
        let mut total: u64 = state.notes.values().map(|meta| meta.size_bytes).sum();
        if total <= self.max_bytes {
            return;
        }
        let mut candidates: Vec<(String, u64, u64)> = state
            .notes
            .iter()
            .filter(|(path, _)| !self.is_pinned(path))
            .map(|(path, meta)| (path.clone(), meta.last_access_ms, meta.size_bytes))
            .collect();
        candidates.sort_by_key(|(_, last_access, _)| *last_access);
        for (path, _, size) in candidates {
            if total <= self.max_bytes {
                break;
            }
            state.notes.remove(&path);
            let _ = fs::remove_file(self.body_path(&path));
            total = total.saturating_sub(size);
        }
    }

    /// (entry count, total cached bytes).
    pub fn stats(&self) -> (usize, u64) {
        self.state
            .lock()
            .map(|state| {
                (
                    state.notes.len(),
                    state.notes.values().map(|meta| meta.size_bytes).sum(),
                )
            })
            .unwrap_or((0, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache(max_bytes: u64, pins: Vec<String>) -> NoteCache {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dob-algolia-cache-{}-{nanos}-{unique}",
            std::process::id()
        ));
        NoteCache::open(dir, max_bytes, pins).expect("open cache")
    }

    /// The version is part of the key: a body cached at `v1` is not served once the
    /// head has moved to `v2`, which is the whole reason the head lookup can double
    /// as the freshness check.
    #[test]
    fn get_respects_version_and_put_round_trips() {
        let cache = temp_cache(1024 * 1024, Vec::new());
        cache.put("_Wiki/A.md", "v1", "hash1", "body one");
        assert_eq!(cache.get("_Wiki/A.md", "v1").as_deref(), Some("body one"));
        assert!(cache.get("_Wiki/A.md", "v2").is_none());
    }

    #[test]
    fn eviction_skips_pinned_entries_and_returns_under_budget() {
        let cache = temp_cache(20, vec!["_Wiki/Pinned/".to_string()]);
        cache.put("_Wiki/Pinned/keep.md", "v1", "h", "0123456789"); // 10 bytes
        cache.put("_Wiki/other.md", "v1", "h", "0123456789012345"); // 16 -> over budget
        assert!(cache.get("_Wiki/Pinned/keep.md", "v1").is_some());
        assert!(cache.get("_Wiki/other.md", "v1").is_none());
        // ...and the budget is actually respected afterwards, not merely one entry
        // dropped.
        let (count, bytes) = cache.stats();
        assert_eq!(count, 1);
        assert!(
            bytes <= 20,
            "{bytes} bytes still cached against a 20 budget"
        );
    }

    /// A body survives a reopen: the state file is the cache's index, so a restarted
    /// process does not re-hydrate every note it already has.
    #[test]
    fn state_survives_a_reopen() {
        let cache = temp_cache(1024, Vec::new());
        cache.put("_Wiki/A.md", "v1", "h", "persisted body");
        let reopened = NoteCache::open(cache.dir.clone(), 1024, Vec::new()).expect("reopen");
        assert_eq!(
            reopened.get("_Wiki/A.md", "v1").as_deref(),
            Some("persisted body")
        );
    }
}
