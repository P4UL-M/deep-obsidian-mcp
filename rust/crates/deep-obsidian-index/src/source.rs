//! The source boundary that decouples indexing from the filesystem.
//!
//! The indexing pipeline used to walk the vault directory itself: it listed markdown
//! files, stat'ed them, and `fs::read_to_string`'d each one inline. That hard-wired the
//! index crate to a local vault. Backends whose notes are not on disk — a CouchDB /
//! LiveSync vault reached through the sidecar, for instance — cannot be indexed that
//! way.
//!
//! Indexing now consumes a [`NoteSource`]: a *manifest* of note and artifact snapshots
//! plus a way to fetch one note's text or one artifact's bytes. [`FilesystemSource`] is
//! the local-vault implementation and carries over the previous code paths verbatim, so
//! filesystem indexing is byte-for-byte what it was (same walk order, same ignore rules,
//! same vault-relative path guard, same error payloads).
//!
//! The trait is deliberately **synchronous**. Indexing already runs inside
//! `spawn_blocking`, and forcing the whole pipeline async to accommodate one backend
//! would be invasive; an async source is expected to bridge to its own runtime
//! internally (e.g. `Handle::block_on` onto the sidecar RPC).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::index::{
    artifact_mime_and_kind, ensure_inside_vault, ensure_vault_path, list_artifact_files,
    list_markdown_files, ArtifactSnapshot, FileSnapshot, IndexError, Result,
};

/// A vault whose notes and artifacts can be enumerated and read.
///
/// Implementations must be cheap to clone/share across the blocking pool, hence
/// `Send + Sync`.
pub trait NoteSource: Send + Sync {
    /// Cheap up-front reachability check, run before any indexing work.
    ///
    /// [`FilesystemSource`] asserts the vault directory exists, preserving the
    /// `InvalidVaultPath` error (and its position in the call order) that the previous
    /// inline `ensure_vault_path` produced.
    fn ensure_ready(&self) -> Result<()>;

    /// Manifest of indexable notes: vault-relative path, size and mtime. Ordering is
    /// part of the contract — it fixes note/chunk ids and therefore retrieval scores.
    fn note_snapshots(&self) -> Result<Vec<FileSnapshot>>;

    /// Manifest of indexable binary artifacts, with MIME type and kind already
    /// classified by the source.
    fn artifact_snapshots(&self) -> Result<Vec<ArtifactSnapshot>>;

    /// Validate that `path` is addressable by this source *without* reading it.
    ///
    /// The artifact pipeline calls this even when it will not load the bytes (over
    /// budget, or artifact embeddings disabled), because the previous filesystem code
    /// resolved the path unconditionally and so rejected an out-of-vault path either
    /// way. Callers may pass snapshots they did not obtain from this source.
    fn ensure_path(&self, path: &str) -> Result<()>;

    /// Read one note as UTF-8 text.
    fn read_note(&self, path: &str) -> Result<String>;

    /// Read one artifact's bytes.
    ///
    /// `max_bytes` is an **advisory** ceiling: it lets a remote source avoid streaming a
    /// huge attachment it is about to have discarded, and `None` means the source
    /// declined for that reason. It is not the authority on whether an artifact is
    /// vectorizable — the caller decides that from the manifest `size`, because the same
    /// comparison also feeds the artifact's persisted `"vectorization"` metadata and the
    /// index-wide `skipped_artifact_count`. A source that reported one size in the
    /// manifest and then declined a read at a different threshold would produce an index
    /// claiming an artifact is `eligible` while storing no embedding for it. Callers
    /// therefore only call this when the manifest size already fits, and
    /// [`FilesystemSource`] consequently never returns `None`.
    fn read_artifact(&self, path: &str, max_bytes: u64) -> Result<Option<Vec<u8>>>;

    /// The local vault directory this source reads, when it has one.
    ///
    /// Recorded on the built index for diagnostics and for callers that still want to
    /// relate an index back to a directory. Non-filesystem sources return `None`, which
    /// is why the index's own SQLite location is tracked separately from this.
    fn local_vault_path(&self) -> Option<&Path> {
        None
    }
}

/// A vault stored as a directory tree on the local filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemSource {
    vault_path: PathBuf,
}

impl FilesystemSource {
    pub fn new(vault_path: impl Into<PathBuf>) -> Self {
        Self {
            vault_path: vault_path.into(),
        }
    }
}

/// Snapshot the mtime/size of one absolute path.
///
/// Lifted unchanged out of the previous `collect_snapshots` /
/// `collect_artifact_snapshots` bodies: an unreadable mtime degrades to the epoch, and a
/// pre-epoch mtime degrades to `0`, rather than failing the whole scan.
fn file_stat(absolute: &Path) -> Result<(u64, u64)> {
    let metadata = fs::metadata(absolute).map_err(|source| IndexError::Io {
        path: absolute.to_path_buf(),
        source,
    })?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let mtime_ms = modified
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    Ok((mtime_ms, metadata.len()))
}

impl NoteSource for FilesystemSource {
    fn ensure_ready(&self) -> Result<()> {
        ensure_vault_path(&self.vault_path).map(|_| ())
    }

    fn note_snapshots(&self) -> Result<Vec<FileSnapshot>> {
        let files = list_markdown_files(&self.vault_path)?;
        let mut snapshots = Vec::with_capacity(files.len());

        for relative_path in files {
            let absolute = ensure_inside_vault(&self.vault_path, &relative_path)?;
            let (mtime_ms, size) = file_stat(&absolute)?;
            snapshots.push(FileSnapshot {
                path: relative_path,
                mtime_ms,
                size,
            });
        }

        Ok(snapshots)
    }

    fn artifact_snapshots(&self) -> Result<Vec<ArtifactSnapshot>> {
        let files = list_artifact_files(&self.vault_path)?;
        let mut snapshots = Vec::with_capacity(files.len());

        for relative_path in files {
            let absolute = ensure_inside_vault(&self.vault_path, &relative_path)?;
            let (mtime_ms, size) = file_stat(&absolute)?;
            let (mime_type, kind) =
                artifact_mime_and_kind(Path::new(&relative_path)).ok_or_else(|| {
                    IndexError::Embedding(format!("unsupported artifact path: {relative_path}"))
                })?;
            snapshots.push(ArtifactSnapshot {
                path: relative_path,
                mtime_ms,
                size,
                mime_type: mime_type.to_string(),
                kind: kind.to_string(),
            });
        }

        Ok(snapshots)
    }

    fn ensure_path(&self, path: &str) -> Result<()> {
        ensure_inside_vault(&self.vault_path, path).map(|_| ())
    }

    fn read_note(&self, path: &str) -> Result<String> {
        let absolute = ensure_inside_vault(&self.vault_path, path)?;
        fs::read_to_string(&absolute).map_err(|source| IndexError::Io {
            path: absolute,
            source,
        })
    }

    /// `max_bytes` is unused here on purpose: the caller has already gated on the
    /// manifest `size`, and re-deciding from a fresh `metadata()` could disagree with
    /// the snapshot the rest of the index was built from.
    fn read_artifact(&self, path: &str, _max_bytes: u64) -> Result<Option<Vec<u8>>> {
        let absolute = ensure_inside_vault(&self.vault_path, path)?;
        fs::read(&absolute)
            .map(Some)
            .map_err(|source| IndexError::Io {
                path: absolute,
                source,
            })
    }

    fn local_vault_path(&self) -> Option<&Path> {
        Some(&self.vault_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "deep-obsidian-source-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create root");
        root
    }

    #[test]
    fn filesystem_source_manifest_matches_the_path_based_collectors() {
        let root = temp_root("manifest");
        write(&root, "Alpha.md", "# Alpha\n");
        write(&root, "Nested/Beta.md", "# Beta\n");
        write(&root, ".hidden/Skipped.md", "# Skipped\n");
        write(&root, "node_modules/Skipped.md", "# Skipped\n");

        let source = FilesystemSource::new(&root);
        assert_eq!(
            source.note_snapshots().expect("source snapshots"),
            crate::index::collect_snapshots(&root).expect("path snapshots")
        );
        assert_eq!(
            source.artifact_snapshots().expect("source artifacts"),
            crate::index::collect_artifact_snapshots(&root).expect("path artifacts")
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn filesystem_source_reads_notes_and_rejects_escaping_paths() {
        let root = temp_root("reads");
        write(&root, "Alpha.md", "# Alpha\nbody\n");

        let source = FilesystemSource::new(&root);
        assert_eq!(
            source.read_note("Alpha.md").expect("read note"),
            "# Alpha\nbody\n"
        );
        assert!(matches!(
            source.read_note("../escape.md"),
            Err(IndexError::InvalidVaultRelativePath(_))
        ));
        assert!(matches!(
            source.ensure_path(""),
            Err(IndexError::InvalidVaultRelativePath(_))
        ));
        assert!(source.ensure_path("Alpha.md").is_ok());
        assert_eq!(source.local_vault_path(), Some(root.as_path()));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn filesystem_source_ensure_ready_rejects_a_missing_vault() {
        // Unique name so the "absent" precondition cannot be disturbed by a concurrent run.
        let missing = temp_root("absent");
        fs::remove_dir_all(&missing).ok();
        assert!(matches!(
            FilesystemSource::new(&missing).ensure_ready(),
            Err(IndexError::InvalidVaultPath(_))
        ));
    }
}
