//! Bounded LRU disk cache of hydrated shared notes (design §6).
//!
//! Bodies live as files under the mount's cache dir; a small JSON state file
//! carries version/hash/size/last-access. Pinned prefixes are exempt from
//! eviction and count against the budget. The cache is never a write buffer —
//! writes push upstream, then update the cache.

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

pub struct NoteCache {
    dir: PathBuf,
    max_bytes: u64,
    pins: Vec<String>,
    state: Mutex<CacheState>,
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

    fn body_path(&self, remote_path: &str) -> PathBuf {
        // Flatten the remote path into a single safe file name.
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

    /// Returns the cached body when the cached version matches `version_id`.
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
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dob-cache-{}-{nanos}", std::process::id()));
        NoteCache::open(dir, max_bytes, pins).expect("open cache")
    }

    #[test]
    fn get_respects_version_and_put_round_trips() {
        let cache = temp_cache(1024 * 1024, Vec::new());
        cache.put("_Wiki/A.md", "v1", "hash1", "body one");
        assert_eq!(cache.get("_Wiki/A.md", "v1").as_deref(), Some("body one"));
        // Version moved on: cached copy no longer served.
        assert!(cache.get("_Wiki/A.md", "v2").is_none());
    }

    #[test]
    fn eviction_skips_pinned_entries() {
        let cache = temp_cache(20, vec!["_Wiki/Pinned/".to_string()]);
        cache.put("_Wiki/Pinned/keep.md", "v1", "h", "0123456789"); // 10 bytes
        cache.put("_Wiki/other.md", "v1", "h", "0123456789012345"); // 16 bytes -> over budget
        // The unpinned entry is the eviction candidate.
        assert!(cache.get("_Wiki/Pinned/keep.md", "v1").is_some());
        assert!(cache.get("_Wiki/other.md", "v1").is_none());
    }
}
