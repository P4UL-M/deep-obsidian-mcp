//! Indexing a CouchDB / LiveSync mount: the sync→async bridge and the manifest pin.
//!
//! [`NoteSource`] is synchronous by design (the whole indexing pipeline is, and it
//! already runs inside `spawn_blocking`), while the sidecar is async. This module is
//! the seam.
//!
//! # Why `Handle::block_on` and not `block_in_place`
//!
//! `block_in_place` would be wrong here twice over. It only works on the
//! multi-threaded runtime, so it would panic under `#[tokio::test]`'s default
//! current-thread runtime — every test of this path. More importantly it *converts
//! the current worker*, which is only sound when the caller is itself on a runtime
//! thread; indexing runs on a `spawn_blocking` pool thread, which is not one. A
//! stored [`Handle`] plus `block_on` is the supported way to re-enter a runtime from
//! a blocking thread, and it cannot starve the reactor because the blocking pool is
//! not the reactor.
//!
//! # The manifest pin (the double-collection fix)
//!
//! One index refresh asks a source for its manifests up to four times: the
//! snapshot-reuse check reads notes+artifacts, then `get_search_index_from_source`
//! reads notes+artifacts again. On a filesystem vault each is a cheap `read_dir`
//! walk. Here each is a cursor-looped `manifest` conversation with CouchDB, so four
//! walks is four times the round trips *and* four chances to disagree with each
//! other mid-refresh.
//!
//! [`CouchDbSource`] therefore pins ONE collected manifest for the lifetime of the
//! source value, and [`IndexTarget`](crate::runtime::IndexTarget) holds a FACTORY
//! rather than an instance, so `RuntimeState` mints one source per refresh and
//! threads it through both the reuse check and the build.
//!
//! Both halves of that are load-bearing, and getting either wrong is silent:
//!
//! * without the pin, one refresh issues four manifest walks and they can disagree
//!   with each other mid-refresh;
//! * without the per-refresh factory, the pin outlives its refresh — the second
//!   refresh reads the first refresh's manifest, compares it against the index built
//!   from that same manifest, concludes "unchanged", and clears the stale flag. The
//!   mount would then serve its startup snapshot forever and no change feed could
//!   move it. `runtime::tests::each_refresh_gets_a_freshly_minted_source` is the
//!   regression guard.
//!
//! That is why this is a pin plus a factory rather than a TTL cache: a TTL would let a
//! change-triggered refresh read a manifest collected before the change.

use std::sync::{Arc, Mutex, OnceLock};

use deep_obsidian_backend::sidecar::{EntryKind, ManifestEntry, ReadPayload, SidecarSupervisor};
use deep_obsidian_index::index::{
    artifact_mime_and_kind, ArtifactSnapshot, FileSnapshot, IndexError, Result,
};
use deep_obsidian_index::source::NoteSource;
use tokio::runtime::Handle;

/// A LiveSync vault, indexable.
///
/// Cheap to construct (no IO) and cheap to drop: it holds a supervisor handle and a
/// lazily-filled manifest, never a child process of its own.
pub struct CouchDbSource {
    supervisor: Arc<SidecarSupervisor>,
    runtime: Handle,
    /// The pinned manifest. Collected at most once per source value; see the module
    /// docs for why this is a pin rather than a cache.
    manifest: OnceLock<Arc<Vec<ManifestEntry>>>,
    /// Serializes the first collection so two concurrent snapshot calls issue one
    /// manifest conversation rather than two.
    collect_lock: Mutex<()>,
}

impl std::fmt::Debug for CouchDbSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CouchDbSource")
            .field("bundle", &self.supervisor.bundle())
            .field("manifest_pinned", &self.manifest.get().is_some())
            .finish()
    }
}

impl CouchDbSource {
    /// Build a source over a supervisor, bridging onto `runtime`.
    ///
    /// `runtime` must be a handle to a runtime that stays alive as long as the
    /// source: `block_on` against a dropped runtime panics. The server passes the
    /// handle of the runtime the service itself runs on, which outlives every mount.
    pub fn new(supervisor: Arc<SidecarSupervisor>, runtime: Handle) -> Self {
        Self {
            supervisor,
            runtime,
            manifest: OnceLock::new(),
            collect_lock: Mutex::new(()),
        }
    }

    /// The pinned manifest, collecting it on first use.
    fn entries(&self) -> Result<Arc<Vec<ManifestEntry>>> {
        if let Some(entries) = self.manifest.get() {
            return Ok(entries.clone());
        }
        let _guard = self
            .collect_lock
            .lock()
            .map_err(|_| IndexError::source("couchdb manifest lock poisoned"))?;
        // Re-check under the lock: a concurrent caller may have filled it.
        if let Some(entries) = self.manifest.get() {
            return Ok(entries.clone());
        }
        let collected = self.block_on(self.supervisor.collect_manifest())?;
        let entries = Arc::new(collected);
        // `set` can only fail if another thread won, in which case its value is just
        // as good; either way `get` below is populated.
        let _ = self.manifest.set(entries.clone());
        Ok(self.manifest.get().cloned().unwrap_or(entries))
    }

    /// Run one sidecar call from this blocking thread.
    fn block_on<T, E: std::fmt::Display>(
        &self,
        future: impl std::future::Future<Output = std::result::Result<T, E>>,
    ) -> Result<T> {
        self.runtime
            .block_on(future)
            .map_err(|error| IndexError::source(error.to_string()))
    }
}

/// True when a manifest entry is indexable content rather than a tombstone or an
/// internal document.
fn is_indexable(entry: &ManifestEntry) -> bool {
    !entry.deleted && !matches!(entry.kind, EntryKind::Internal)
}

fn is_markdown_path(path: &str) -> bool {
    path.rsplit_once('.')
        .map(|(_, extension)| extension.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

/// True when any segment of `path` is hidden or an ignored directory.
///
/// The same filter the CouchDB backend's listings apply, so the index covers
/// exactly the paths `list_children` and `find_files` can show.
fn path_is_filtered(path: &str) -> bool {
    path.split('/').any(|segment| {
        segment.starts_with('.')
            || deep_obsidian_core::vault::DEFAULT_IGNORED_DIRS.contains(&segment)
    })
}

impl NoteSource for CouchDbSource {
    /// Reachability, expressed as the sidecar's readiness.
    ///
    /// Fails closed: an unreachable, locked, cleaned, unknown-schema or
    /// encryption-blocked remote refuses here, so the index build stops before it
    /// can write a *partial* index that would then look complete. The error names
    /// the compatibility status and its remediation.
    fn ensure_ready(&self) -> Result<()> {
        self.runtime
            .block_on(self.supervisor.ensure_ready())
            .map_err(|error| IndexError::source(error.to_string()))
    }

    /// Note manifest, sorted by vault-relative path string.
    ///
    /// The ordering is load-bearing: it fixes note and chunk ids and therefore
    /// retrieval scores, so it must be the same total order the filesystem source
    /// produces (`Vec<String>::sort` on the relative path).
    fn note_snapshots(&self) -> Result<Vec<FileSnapshot>> {
        let entries = self.entries()?;
        let mut snapshots: Vec<FileSnapshot> = entries
            .iter()
            .filter(|entry| is_indexable(entry))
            .filter(|entry| is_markdown_path(&entry.path))
            .filter(|entry| !path_is_filtered(&entry.path))
            .map(|entry| FileSnapshot {
                path: entry.path.clone(),
                mtime_ms: entry.mtime_ms,
                size: entry.size,
            })
            .collect();
        snapshots.sort_by(|left, right| left.path.cmp(&right.path));
        snapshots.dedup_by(|left, right| left.path == right.path);
        Ok(snapshots)
    }

    /// Artifact manifest: the binary entries whose extension the index recognizes.
    ///
    /// An unrecognized extension is SKIPPED rather than an error, unlike the
    /// filesystem source. There, the candidate list came from
    /// `list_artifact_files`, which only yields recognized extensions, so an
    /// unrecognized one was a genuine contradiction. Here the candidates come from
    /// whatever the plugin stored as `newnote`, which is not constrained to the
    /// index's MIME table — so an unknown attachment type must not fail the whole
    /// vault's index.
    fn artifact_snapshots(&self) -> Result<Vec<ArtifactSnapshot>> {
        let entries = self.entries()?;
        let mut snapshots: Vec<ArtifactSnapshot> = entries
            .iter()
            .filter(|entry| is_indexable(entry))
            .filter(|entry| matches!(entry.kind, EntryKind::Binary))
            .filter(|entry| !path_is_filtered(&entry.path))
            .filter_map(|entry| {
                let (mime_type, kind) = artifact_mime_and_kind(std::path::Path::new(&entry.path))?;
                Some(ArtifactSnapshot {
                    path: entry.path.clone(),
                    mtime_ms: entry.mtime_ms,
                    size: entry.size,
                    mime_type: mime_type.to_string(),
                    kind: kind.to_string(),
                })
            })
            .collect();
        snapshots.sort_by(|left, right| left.path.cmp(&right.path));
        snapshots.dedup_by(|left, right| left.path == right.path);
        Ok(snapshots)
    }

    /// Path validation with no IO, mirroring what the backend refuses.
    fn ensure_path(&self, path: &str) -> Result<()> {
        let refuse = || Err(IndexError::InvalidVaultRelativePath(path.to_string()));
        if path.trim().is_empty() || path.starts_with('/') || path.contains(':') {
            return refuse();
        }
        for segment in path.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return refuse();
            }
        }
        Ok(())
    }

    fn read_note(&self, path: &str) -> Result<String> {
        self.ensure_path(path)?;
        let result = self.block_on(self.supervisor.read(path))?;
        match result.payload {
            ReadPayload::Text(text) => Ok(text),
            // A `newnote` entry that the note manifest listed: only possible if the
            // plugin's classification and the `.md` extension disagree. Decoded as
            // UTF-8 rather than refused, so one oddly-stored note cannot fail the
            // whole index build.
            ReadPayload::Bytes(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }

    /// Read one artifact, declining over budget.
    ///
    /// Checks the PINNED MANIFEST's size before issuing the read, which is the point
    /// of the advisory ceiling for a remote source: an oversize attachment's chunks
    /// are never pulled across the network at all. The post-read check that follows is
    /// the second line of defence, for a manifest that under-reported — the bytes are
    /// discarded rather than embedded, so the index cannot claim an artifact is
    /// vectorized while storing nothing for it.
    fn read_artifact(&self, path: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        self.ensure_path(path)?;
        let declared = self
            .entries()?
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.size);
        if declared.is_some_and(|size| size > max_bytes) {
            return Ok(None);
        }
        let result = self.block_on(self.supervisor.read(path))?;
        let bytes = match result.payload {
            ReadPayload::Bytes(bytes) => bytes,
            ReadPayload::Text(text) => text.into_bytes(),
        };
        if bytes.len() as u64 > max_bytes {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// `None`: a CouchDB vault has no local directory.
    ///
    /// This is why the index's SQLite location is tracked separately from the vault
    /// path, and why a couchdb mount cannot be the ROOT mount this slice.
    fn local_vault_path(&self) -> Option<&std::path::Path> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_the_backends_listing_filters() {
        assert!(path_is_filtered(".obsidian/workspace.json"));
        assert!(path_is_filtered("Notes/.hidden/x.md"));
        assert!(path_is_filtered("node_modules/pkg/x.md"));
        assert!(!path_is_filtered("Notes/Deep/Gamma.md"));
        assert!(is_markdown_path("A.MD"));
        assert!(!is_markdown_path("A.png"));
        assert!(!is_markdown_path("noextension"));
    }

    /// A tombstone is not indexable content, and neither is an internal document.
    #[test]
    fn tombstones_and_internal_entries_are_not_indexable() {
        let entry = |kind: EntryKind, deleted: bool| ManifestEntry {
            path: "A.md".to_string(),
            size: 1,
            mtime_ms: 1,
            ctime_ms: 1,
            deleted,
            conflicted: false,
            kind,
        };
        assert!(is_indexable(&entry(EntryKind::Markdown, false)));
        assert!(!is_indexable(&entry(EntryKind::Markdown, true)));
        assert!(!is_indexable(&entry(EntryKind::Internal, false)));
        assert!(is_indexable(&entry(EntryKind::Binary, false)));
    }
}
