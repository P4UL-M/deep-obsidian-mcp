//! The filesystem-backed vault: today's behaviour, behind the boundary.
//!
//! This is the only backend the server constructs in production. It is a thin
//! router over `deep_obsidian_core::vault` plus three mechanics that used to live
//! in the server and are provider-specific enough that they belong here: the
//! ripgrep spawn, the `notify` watcher, and the temp-file-plus-atomic-rename
//! upload commit.

use std::path::{Path, PathBuf};

use deep_obsidian_core::vault;
use deep_obsidian_core::ContentHasher;
use notify::{RecursiveMode, Watcher as _};
use std::io::Write as _;
use tokio::sync::mpsc;

use crate::grep::{self, GrepParams};
use crate::watch::{watch_reason, ChangeEvent, ChangeStream};
use crate::{
    BackendDescriptor, BackendError, BackendKind, BackendRequest, BackendResponse, Capability,
    ChildListing, ContentRequest, ContentResponse, GrepOutcome, HealthRequest, HealthResponse,
    ManifestRequest, ManifestResponse, MutationRequest, MutationResponse, OpaqueCursor,
    RecallRequest, RecallResponse, VaultBackend,
};

/// Refusal for a ranked search asked of a filesystem mount.
///
/// A filesystem vault has no index of its own: the SERVER builds one over it and ranks
/// there, which is the arrangement every recall tool already uses. So this is not a
/// missing feature to be implemented later — answering it here would mean a second,
/// worse ranker underneath the real one. The message says which layer owns the answer,
/// because a reader who sees "unsupported" from the backend will otherwise conclude
/// recall is broken on their vault.
pub const FILESYSTEM_NATIVE_RECALL_UNSUPPORTED_MESSAGE: &str = "a filesystem vault does not \
perform its own ranked search: the server builds a local search index over it and ranks there, \
which is what hybrid_search, load_knowledge, related_notes and graph_traverse already use. This \
request exists for a backend whose storage IS a search index (a shared corpus that has no local \
copy), and asking a filesystem mount for it would put a second, weaker ranker underneath the real \
one. Nothing is missing — use the index-backed recall tools.";

/// Refusal for a versioned read or a history listing on a filesystem mount.
///
/// Names the storage model rather than an unimplemented feature, and points at what a
/// user actually wants (their own version control), because "not supported" invites the
/// question this answers: could it be?
pub const FILESYSTEM_VERSION_HISTORY_UNSUPPORTED_MESSAGE: &str = "a filesystem vault keeps no \
version history: a file has exactly one content by construction, and an overwrite replaces it, so \
there is no superseded version to list or to read back. This is the storage model, not a missing \
feature — a previous version can only come from something that kept one (git, Obsidian's own file \
recovery, or a Time Machine/backup snapshot).";

/// Refusal for deleting a note on a filesystem mount.
///
/// The most important refusal in this file. MCP has never exposed local file deletion,
/// and a `delete_note` tool reaching a filesystem mount by accident would be a far
/// larger capability than anything else on the surface — so the refusal is
/// unconditional and says out loud that the omission is deliberate.
pub const FILESYSTEM_SOFT_DELETE_UNSUPPORTED_MESSAGE: &str = "this MCP surface exposes no \
deletion of local vault files, deliberately: every other write here creates or replaces a note, \
and an agent that can also remove files is a materially larger capability than the one you \
granted. Delete the file yourself (in Obsidian, or in your file manager). Soft delete exists only \
for a backend whose removal is observable and recoverable — a shared corpus where the note becomes \
a tombstone other participants see and the content stays readable from its version history.";

/// Prefix used for in-progress upload staging files.
///
/// Staging lives in the destination's own parent directory so the final swap is a
/// same-filesystem `rename`, which is atomic. That is why this constant, and the
/// sweep that cleans up after a killed process, belong to the backend.
const TEMP_PREFIX: &str = ".upload-";

/// A vault rooted at a local directory.
///
/// The vault root and the resolved `rg` path are private: nothing outside this
/// module can learn where the vault lives from the backend API.
pub struct FilesystemVaultBackend {
    vault_path: PathBuf,
    ripgrep_path: PathBuf,
    /// Whether `ripgrep_path` resolved to a real executable at construction. Drives
    /// the `GrepSearch` capability, which in turn drives whether the server
    /// advertises `grep_search` at all.
    ripgrep_available: bool,
    /// The deployment's index directory, when one is configured. Only ever read to
    /// keep an index dir that lives *inside* the vault out of grep results.
    index_dir: Option<PathBuf>,
}

impl FilesystemVaultBackend {
    /// Build a backend for `vault_path`, resolving ripgrep once.
    pub fn new(vault_path: impl Into<PathBuf>) -> Self {
        Self::with_ripgrep(vault_path, grep::resolve_ripgrep())
    }

    /// Build a backend with an explicit `rg` path. Exists for tests that need to
    /// pin ripgrep availability regardless of the host.
    pub fn with_ripgrep(vault_path: impl Into<PathBuf>, ripgrep_path: impl Into<PathBuf>) -> Self {
        let ripgrep_path = ripgrep_path.into();
        let ripgrep_available = ripgrep_path.is_file();
        Self {
            vault_path: vault_path.into(),
            ripgrep_path,
            ripgrep_available,
            index_dir: None,
        }
    }

    /// Declare where this deployment keeps its index.
    ///
    /// A vault-internal index dir holds the SQLite index and its sidecar files;
    /// declaring it keeps those files out of `grep_search` results, where they
    /// would otherwise surface as phantom vault paths. An index dir outside the
    /// vault changes nothing.
    pub fn with_index_dir(mut self, index_dir: impl Into<PathBuf>) -> Self {
        self.index_dir = Some(index_dir.into());
        self
    }

    fn manifest(&self, request: ManifestRequest) -> Result<ManifestResponse, BackendError> {
        match request {
            ManifestRequest::ListChildren {
                path,
                include_hidden,
                include_ignored,
            } => Ok(ManifestResponse::Children(ChildListing::exhaustive(
                vault::list_children(
                    &self.vault_path,
                    path.as_deref(),
                    include_hidden,
                    include_ignored,
                )?,
            ))),
            ManifestRequest::WalkMarkdown => Ok(ManifestResponse::MarkdownFiles(
                vault::list_markdown_files(&self.vault_path)?,
            )),
            ManifestRequest::TopLevelFolders => Ok(ManifestResponse::Folders(
                vault::list_top_level_folders(&self.vault_path)?,
            )),
            // The directory HAS no history. See the constant.
            ManifestRequest::Versions { .. } => Err(BackendError::Unsupported(
                FILESYSTEM_VERSION_HISTORY_UNSUPPORTED_MESSAGE.to_string(),
            )),
        }
    }

    fn content(&self, request: ContentRequest) -> Result<ContentResponse, BackendError> {
        match request {
            // Vault-flavoured: core enriches IO failures with the path and, for
            // permission errors, the remediation. Frozen by `error_missing_file`.
            // A versioned read is refused BEFORE the file is opened: answering with the
            // current content would silently serve something other than the version
            // that was asked for, which is worse than refusing.
            ContentRequest::ReadText {
                version: Some(_), ..
            } => Err(BackendError::Unsupported(
                FILESYSTEM_VERSION_HISTORY_UNSUPPORTED_MESSAGE.to_string(),
            )),
            // `known_hash` is IGNORED here, and this backend is the reason the field is
            // an opportunity rather than a contract. `fnv1a64` has no shortcut: the only
            // way to know what this file hashes to is to read every byte of it and hash
            // them, which is exactly what a full read does. There is nothing to skip, so
            // there is no `Unchanged` to answer — the caller compares the hash itself,
            // as it always has, and saves the response body. An `mtime`/`ino` check
            // could stand in for the hash, but that is a precondition this backend has
            // never enforced (see `version` below) and inventing one here would make a
            // read cheaper by making it wrong under any tool that rewrites content
            // without moving the mtime.
            ContentRequest::ReadText { path, .. } => Ok(ContentResponse::Text {
                text: vault::read_text_file(&self.vault_path, &path)?.text,
                // The filesystem mints no version token. A caller therefore gets
                // `BaseVersion::Unobserved` back and the read-then-write window
                // stays exactly as wide as it has always been, which is frozen
                // behaviour rather than an omission: closing it would mean an
                // `mtime`/`ino` precondition this backend has never enforced.
                version: None,
            }),
            // Io-flavoured (bare): `read_artifact` has always reported the raw IO
            // error here, with no path prefix. Frozen public behaviour.
            ContentRequest::ReadBytes { path } => {
                let absolute = vault::ensure_inside_vault(&self.vault_path, &path)?;
                Ok(ContentResponse::Bytes(
                    std::fs::read(&absolute).map_err(BackendError::io)?,
                ))
            }
            ContentRequest::Stat { path } => {
                let absolute = vault::ensure_inside_vault(&self.vault_path, &path)?;
                let metadata = std::fs::metadata(&absolute).map_err(BackendError::io)?;
                Ok(ContentResponse::Stat {
                    size_bytes: metadata.len(),
                })
            }
            ContentRequest::ResolvePath { path } => {
                vault::ensure_inside_vault(&self.vault_path, &path)?;
                Ok(ContentResponse::PathAccepted)
            }
        }
    }

    /// The mutations that complete promptly and need no blocking thread. The
    /// streaming upload commit is handled separately in [`VaultBackend::execute`].
    fn write_text(&self, path: &str, content: &str) -> Result<MutationResponse, BackendError> {
        let result = vault::write_text_file(&self.vault_path, path, content)?;
        Ok(MutationResponse::Written {
            created: result.created,
        })
    }

    async fn recall(&self, request: RecallRequest) -> Result<RecallResponse, BackendError> {
        match request {
            RecallRequest::Grep {
                query,
                regex,
                case_sensitive,
                glob,
                context_lines,
                limit,
            } => {
                if !self.ripgrep_available {
                    return Err(BackendError::Message(
                        grep::RIPGREP_UNAVAILABLE_MESSAGE.to_string(),
                    ));
                }
                // EXHAUSTIVE, and it is ripgrep that makes it so: it opens every file
                // in scope. This is the only backend that can claim it, and the claim
                // is what makes the server's `exhaustive` field meaningful when
                // another backend cannot.
                //
                // # On a blocking thread, which it was always documented to need
                //
                // `run_grep` spawns `rg` and waits for it — tens of milliseconds of
                // genuinely blocking work — and its own doc comment says "the caller runs
                // it on a blocking thread". It did not. Calling it inline held a reactor
                // thread for the whole search, which cost two things:
                //
                // * a federated grep could not overlap its mounts AT ALL. The router
                //   fans out concurrently, but concurrency needs futures that yield, and
                //   a future that blocks inside `poll` runs to completion before its
                //   siblings are polled at all — so N mounts cost N greps end to end
                //   however they were driven (measured: +17.9 ms per additional mount).
                // * every other request on that thread waited behind it, federated or not.
                //
                // So this is not an optimisation for the router's benefit; it is the fix
                // for a blocking call in an async context, and the router's fan-out is
                // what made its absence visible.
                let ripgrep_path = self.ripgrep_path.clone();
                let vault_path = self.vault_path.clone();
                // Owned so it can cross into the blocking task; `run_grep` takes the
                // backend's index dir separately from the cross-backend `GrepParams`.
                let index_dir = self.index_dir.clone();
                let params = GrepParams {
                    query,
                    regex,
                    case_sensitive,
                    glob,
                    context_lines,
                    limit,
                };
                let matches = tokio::task::spawn_blocking(move || {
                    grep::run_grep(&ripgrep_path, &vault_path, index_dir.as_deref(), params)
                })
                .await
                .map_err(|error| BackendError::Message(error.to_string()))??;
                Ok(RecallResponse::Grep(GrepOutcome::exhaustive(matches)))
            }
            RecallRequest::Search(_) => Err(BackendError::Unsupported(
                FILESYSTEM_NATIVE_RECALL_UNSUPPORTED_MESSAGE.to_string(),
            )),
        }
    }

    fn health(&self, request: HealthRequest) -> Result<HealthResponse, BackendError> {
        match request {
            // Errors with core's `vault path does not exist or is not a directory`
            // wording, so a caller can use this directly as a startup gate.
            HealthRequest::Overview => {
                vault::ensure_vault_path(&self.vault_path)?;
                Ok(HealthResponse::Overview { reachable: true })
            }
        }
    }
}

#[async_trait::async_trait]
impl VaultBackend for FilesystemVaultBackend {
    fn descriptor(&self) -> BackendDescriptor {
        let mut capabilities = vec![
            Capability::BinaryRead,
            Capability::BinaryWrite,
            Capability::Upload,
            Capability::Watch,
        ];
        if self.ripgrep_available {
            capabilities.push(Capability::GrepSearch);
        }
        BackendDescriptor::new(BackendKind::Filesystem, capabilities)
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, BackendError> {
        match request {
            BackendRequest::Manifest(request) => {
                self.manifest(request).map(BackendResponse::Manifest)
            }
            BackendRequest::Content(request) => self.content(request).map(BackendResponse::Content),
            // The upload commit pulls a synchronous chunk iterator that is fed by
            // the caller's async body pump, so it must not run on a reactor thread:
            // blocking there would deadlock against the pump.
            BackendRequest::Mutation(MutationRequest::CommitUploadStream {
                path,
                expected_hash,
                max_bytes,
                chunks,
            }) => {
                let vault_path = self.vault_path.clone();
                tokio::task::spawn_blocking(move || {
                    commit_upload_stream(
                        &vault_path,
                        &path,
                        expected_hash.as_deref(),
                        max_bytes,
                        chunks.into_inner(),
                    )
                    .map(|outcome| MutationResponse::UploadCommitted {
                        created: outcome.created,
                        bytes_written: outcome.bytes_written,
                        hash: outcome.hash,
                    })
                })
                .await
                .map_err(|error| BackendError::Message(error.to_string()))?
                .map(BackendResponse::Mutation)
            }
            // `base_version` is deliberately ignored: this backend mints no version
            // tokens, so it never receives one, and its write is an atomic rename
            // that has no precondition to attach. See `BaseVersion`. `resolve_divergence`
            // is ignored for the matching reason: nothing here can record a divergence,
            // so there is none to clear and honouring the flag would be theatre.
            BackendRequest::Mutation(MutationRequest::WriteText { path, content, .. }) => self
                .write_text(&path, &content)
                .map(BackendResponse::Mutation),
            // Refused, and NOT by falling back to `remove_file`. See the constant: the
            // absence of local deletion from this surface is the contract.
            BackendRequest::Mutation(MutationRequest::SoftDelete { .. }) => Err(
                BackendError::Unsupported(FILESYSTEM_SOFT_DELETE_UNSUPPORTED_MESSAGE.to_string()),
            ),
            BackendRequest::Mutation(MutationRequest::SweepOrphanStagingFiles) => {
                sweep_orphan_temp_files_at(&self.vault_path, std::time::SystemTime::now());
                Ok(BackendResponse::Mutation(MutationResponse::Swept))
            }
            BackendRequest::Recall(request) => {
                self.recall(request).await.map(BackendResponse::Recall)
            }
            BackendRequest::Health(request) => self.health(request).map(BackendResponse::Health),
        }
    }

    /// Watch the vault recursively, applying the shared ignore rules.
    ///
    /// The filesystem has no replay log, so `after` is accepted and ignored: a
    /// subscription delivers only changes observed from this point on. A backend
    /// with a durable change feed would honour the cursor here.
    fn changes(&self, _after: Option<OpaqueCursor>) -> ChangeStream {
        let (sender, receiver) = mpsc::unbounded_channel();
        let watched_root = self.vault_path.clone();
        let watcher =
            notify::recommended_watcher(
                move |result: notify::Result<notify::Event>| match result {
                    Ok(event) => {
                        if let Some(reason) = watch_reason(&watched_root, &event) {
                            let _ = sender.send(ChangeEvent::Change(reason));
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(ChangeEvent::Error(error.to_string()));
                    }
                },
            );

        let mut watcher = match watcher {
            Ok(watcher) => watcher,
            // A watcher that cannot be created yields an ended stream rather than
            // panicking; the caller falls back to interval refresh.
            Err(_) => return ChangeStream::empty(),
        };
        if watcher
            .watch(&self.vault_path, RecursiveMode::Recursive)
            .is_err()
        {
            return ChangeStream::empty();
        }
        // The stream owns the watcher: `notify` stops delivering the instant the
        // watcher drops, so it must outlive the receiver.
        ChangeStream::new(receiver, watcher)
    }
}

// ---------------------------------------------------------------------------
// Upload commit
// ---------------------------------------------------------------------------

/// What a committed upload landed. Mirrors [`crate::UploadOutcome`].
#[derive(Debug)]
struct CommitOutcome {
    created: bool,
    bytes_written: usize,
    hash: String,
}

/// Compute the absolute destination, ensuring the canonical parent directory stays
/// within the canonical vault root.
///
/// `ensure_inside_vault` is lexical only; this adds the runtime symlink guard.
/// Directories are created within the vault as needed before canonicalization.
fn resolve_guarded_destination(
    vault_path: &Path,
    dest_path: &str,
) -> Result<PathBuf, BackendError> {
    let absolute = vault::ensure_inside_vault(vault_path, dest_path)
        .map_err(|_| BackendError::PathEscapesVault)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| BackendError::Message("destination has no parent directory".to_string()))?;
    std::fs::create_dir_all(parent).map_err(BackendError::io)?;

    let canonical_parent = parent.canonicalize().map_err(BackendError::io)?;
    let canonical_vault = vault_path.canonicalize().map_err(BackendError::io)?;
    if !canonical_parent.starts_with(&canonical_vault) {
        return Err(BackendError::PathEscapesVault);
    }
    Ok(absolute)
}

/// Stream `chunks` to a staging file in the destination's parent directory,
/// enforcing `max_bytes` during streaming, then atomically rename over the
/// destination. The hash and create/update decision are computed at commit.
///
/// `expected_hash`, when set, triggers an optimistic-concurrency re-read of the
/// destination at commit; a mismatch aborts with [`BackendError::HashConflict`].
///
/// On any failure the staging file is removed and the destination is left untouched.
fn commit_upload_stream(
    vault_path: &Path,
    dest_path: &str,
    expected_hash: Option<&str>,
    max_bytes: usize,
    mut chunks: Box<dyn Iterator<Item = Result<Vec<u8>, String>> + Send>,
) -> Result<CommitOutcome, BackendError> {
    let absolute = resolve_guarded_destination(vault_path, dest_path)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| BackendError::Message("destination has no parent directory".to_string()))?;

    let created = !absolute.exists();

    let temp_path = parent.join(format!("{TEMP_PREFIX}{}.tmp", staging_token()));
    let mut temp_file = match std::fs::File::create(&temp_path) {
        Ok(file) => file,
        Err(error) => return Err(BackendError::io(error)),
    };

    let mut total: usize = 0;
    // Hashed incrementally so the whole file is never buffered in RAM. Uses core's
    // canonical hasher, so the emitted `hash` is by construction the same string
    // `content_hash` would produce over the same bytes.
    let mut hasher = ContentHasher::new();
    let result = (|| -> Result<(), BackendError> {
        for chunk in chunks.by_ref() {
            let chunk = chunk.map_err(BackendError::Message)?;
            total = total.saturating_add(chunk.len());
            if total > max_bytes {
                return Err(BackendError::PayloadTooLarge);
            }
            temp_file.write_all(&chunk).map_err(BackendError::io)?;
            hasher.update(&chunk);
        }
        temp_file.flush().map_err(BackendError::io)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    drop(temp_file);

    // Optimistic concurrency: re-read the destination at commit if requested.
    if let Some(expected) = expected_hash {
        let current_hash = match std::fs::read(&absolute) {
            Ok(bytes) => Some(deep_obsidian_core::content_hash(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(BackendError::io(error));
            }
        };
        if current_hash.as_deref() != Some(expected) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(BackendError::HashConflict {
                expected: expected.to_string(),
                found: current_hash.unwrap_or_else(|| "null".to_string()),
            });
        }
    }

    let hash = hasher.finish();
    if let Err(error) = std::fs::rename(&temp_path, &absolute) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(BackendError::io(error));
    }

    Ok(CommitOutcome {
        created,
        bytes_written: total,
        hash,
    })
}

/// Generate a 256-bit random staging-file discriminator as lowercase hex.
///
/// Unpredictable by design: the staging file is created inside the vault, so a
/// guessable name would let a local attacker pre-plant a symlink at that path and
/// redirect the write.
fn staging_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Sweep the vault for orphan `.upload-*.tmp` files older than `ttl` and unlink
/// them. These are left behind only if a process is killed mid-stream (the normal
/// failure path always removes its own staging file). Errors are ignored: this is
/// best-effort housekeeping, never a hard failure.
fn sweep_orphan_temp_files_at(vault_path: &Path, now: std::time::SystemTime) {
    sweep_dir(vault_path, now, 0);
}

/// Time-to-live after which a staging file is considered orphaned.
///
/// Kept in step with the server's upload token TTL (`uploads::TOKEN_TTL`): a staging
/// file younger than this may still belong to an in-flight upload, so the sweep must
/// leave it alone. Raising the token TTL without raising this would let the sweep
/// delete a live upload's staging file out from under it.
const STAGING_TTL: std::time::Duration = std::time::Duration::from_secs(300);

fn sweep_dir(dir: &Path, now: std::time::SystemTime, depth: usize) {
    // Bound recursion to avoid pathological deep trees.
    if depth > 24 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            // Skip hidden/system dirs except we still descend normal folders.
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || name == ".obsidian" {
                continue;
            }
            sweep_dir(&path, now, depth + 1);
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(TEMP_PREFIX) || !name.ends_with(".tmp") {
            continue;
        }
        // Only remove staging files older than the TTL, so a concurrent in-flight
        // upload's file is never deleted out from under it.
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| {
                now.duration_since(modified)
                    .map(|age| age > STAGING_TTL)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UploadChunks;
    use std::time::{Duration, SystemTime};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), nanos))
    }

    fn chunks(items: Vec<Result<Vec<u8>, String>>) -> UploadChunks {
        UploadChunks::new(items.into_iter())
    }

    // -- upload commit ------------------------------------------------------

    #[test]
    fn commit_writes_file_and_reports_created() {
        let dir = temp_dir("backend-commit");
        std::fs::create_dir_all(&dir).unwrap();

        let outcome = commit_upload_stream(
            &dir,
            "sub/out.bin",
            None,
            1024,
            chunks(vec![Ok(b"hello ".to_vec()), Ok(b"world".to_vec())]).into_inner(),
        )
        .expect("commit should succeed");
        assert!(outcome.created);
        assert_eq!(outcome.bytes_written, 11);
        assert_eq!(
            outcome.hash,
            deep_obsidian_core::content_hash(b"hello world")
        );
        assert_eq!(
            std::fs::read(dir.join("sub/out.bin")).unwrap(),
            b"hello world"
        );

        // A second commit over the same path reports `created = false`.
        let outcome = commit_upload_stream(
            &dir,
            "sub/out.bin",
            None,
            1024,
            chunks(vec![Ok(b"again".to_vec())]).into_inner(),
        )
        .expect("second commit should succeed");
        assert!(!outcome.created);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_aborts_on_oversize_and_leaves_no_staging_file() {
        let dir = temp_dir("backend-oversize");
        std::fs::create_dir_all(&dir).unwrap();

        let error = commit_upload_stream(
            &dir,
            "big.bin",
            None,
            4,
            chunks(vec![Ok(b"12345".to_vec())]).into_inner(),
        )
        .expect_err("oversize must fail");
        assert!(matches!(error, BackendError::PayloadTooLarge));
        assert_eq!(error.to_string(), "upload exceeds maximum allowed size");
        assert!(!dir.join("big.bin").exists());

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "staging file leaked: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_rejects_hash_conflict_and_preserves_destination() {
        let dir = temp_dir("backend-conflict");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("doc.bin"), b"current").unwrap();

        let error = commit_upload_stream(
            &dir,
            "doc.bin",
            Some("fnv1a64:0000000000000000"),
            1024,
            chunks(vec![Ok(b"replacement".to_vec())]).into_inner(),
        )
        .expect_err("stale expected hash must conflict");
        match &error {
            BackendError::HashConflict { expected, found } => {
                assert_eq!(expected, "fnv1a64:0000000000000000");
                assert_eq!(found, &deep_obsidian_core::content_hash(b"current"));
            }
            other => panic!("expected a hash conflict, got {other:?}"),
        }
        // The destination is untouched.
        assert_eq!(std::fs::read(dir.join("doc.bin")).unwrap(), b"current");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_accepts_matching_expected_hash() {
        let dir = temp_dir("backend-conflict-ok");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("doc.bin"), b"current").unwrap();

        let outcome = commit_upload_stream(
            &dir,
            "doc.bin",
            Some(&deep_obsidian_core::content_hash(b"current")),
            1024,
            chunks(vec![Ok(b"replacement".to_vec())]).into_inner(),
        )
        .expect("matching hash should commit");
        assert!(!outcome.created);
        assert_eq!(std::fs::read(dir.join("doc.bin")).unwrap(), b"replacement");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn commit_rejects_symlink_escape() {
        let outside = temp_dir("backend-escape-outside");
        let vault = temp_dir("backend-escape-vault");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&vault).unwrap();
        let link = vault.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let error = commit_upload_stream(
            &vault,
            "link/escaped.bin",
            None,
            1024,
            chunks(vec![Ok(b"payload".to_vec())]).into_inner(),
        )
        .expect_err("a symlinked destination must be rejected");
        assert!(matches!(error, BackendError::PathEscapesVault));
        assert_eq!(error.to_string(), "destination escapes the vault root");
        assert!(!outside.join("escaped.bin").exists());

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&vault);
    }

    #[test]
    fn sweep_removes_only_stale_staging_files() {
        let vault = temp_dir("backend-sweep");
        std::fs::create_dir_all(vault.join("sub")).unwrap();
        let orphan = vault.join("sub").join(format!("{TEMP_PREFIX}abc.tmp"));
        std::fs::write(&orphan, b"junk").unwrap();
        let keep = vault.join("sub/real.bin");
        std::fs::write(&keep, b"data").unwrap();
        let not_prefixed = vault.join("sub/upload-not-prefixed.tmp");
        std::fs::write(&not_prefixed, b"keep").unwrap();

        // Fresh staging files survive: an upload may still be in flight.
        sweep_orphan_temp_files_at(&vault, SystemTime::now());
        assert!(orphan.exists(), "a fresh staging file must be kept");

        // Sweeping from far in the future makes it stale.
        sweep_orphan_temp_files_at(&vault, SystemTime::now() + Duration::from_secs(3600));
        assert!(!orphan.exists(), "a stale staging file must be removed");
        assert!(keep.exists(), "real files are never touched");
        assert!(not_prefixed.exists(), "only the staging prefix is swept");

        let _ = std::fs::remove_dir_all(&vault);
    }

    // -- grep ---------------------------------------------------------------

    #[tokio::test]
    async fn grep_populates_context_lines() {
        let vault = temp_dir("backend-grep-context");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(
            vault.join("Context.md"),
            "alpha\nbefore\nneedle here\nafter\nomega\n",
        )
        .unwrap();

        let backend = FilesystemVaultBackend::new(&vault);
        if !backend.ripgrep_available {
            let _ = std::fs::remove_dir_all(&vault);
            return;
        }
        let matches = backend
            .execute(BackendRequest::Recall(RecallRequest::Grep {
                query: "needle".to_string(),
                regex: false,
                case_sensitive: true,
                glob: None,
                context_lines: 1,
                limit: 10,
            }))
            .await
            .expect("grep should succeed")
            .into_grep_matches()
            .expect("grep matches");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 3);
        assert_eq!(matches[0].path, "Context.md");
        assert_eq!(matches[0].context_before[0].line_text, "before");
        assert_eq!(matches[0].context_after[0].line_text, "after");

        let _ = std::fs::remove_dir_all(&vault);
    }

    #[tokio::test]
    async fn grep_treats_flaglike_query_as_literal_pattern() {
        // A query beginning with `-`/`--` must be searched as a literal pattern,
        // not parsed by ripgrep as a flag (argv injection guard via `--`).
        let vault = temp_dir("backend-grep-flaglike");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(
            vault.join("Flags.md"),
            "ordinary line\ncontains --pre=/bin/echo here\ntrailing line\n",
        )
        .unwrap();

        let backend = FilesystemVaultBackend::new(&vault);
        if !backend.ripgrep_available {
            let _ = std::fs::remove_dir_all(&vault);
            return;
        }
        let matches = backend
            .execute(BackendRequest::Recall(RecallRequest::Grep {
                query: "--pre=/bin/echo".to_string(),
                regex: false,
                case_sensitive: true,
                glob: None,
                context_lines: 0,
                limit: 10,
            }))
            .await
            .expect("a flag-like query must be a literal search, not an rg flag error")
            .into_grep_matches()
            .expect("grep matches");

        assert_eq!(
            matches.len(),
            1,
            "literal flag-like string should match once"
        );
        assert_eq!(matches[0].line_number, 2);
        assert!(matches[0].line_text.contains("--pre=/bin/echo"));

        let _ = std::fs::remove_dir_all(&vault);
    }

    #[tokio::test]
    async fn grep_excludes_a_custom_index_dir_inside_the_vault() {
        // An index dir configured INSIDE the vault holds the SQLite index and
        // its sidecars. With a caller-supplied glob those files are reachable by
        // rg and would surface as phantom vault paths.
        let vault = temp_dir("backend-grep-index-dir");
        let index_dir = vault.join("Index Cache");
        std::fs::create_dir_all(&index_dir).unwrap();
        std::fs::write(vault.join("Real.md"), "needle in a real note\n").unwrap();
        std::fs::write(index_dir.join("index.sqlite"), "needle in the index\n").unwrap();
        std::fs::write(index_dir.join("cached.md"), "needle in a cache file\n").unwrap();
        // A real note under a SAME-NAMED directory elsewhere must survive: the
        // exclusion is the index dir itself, not every path segment like it.
        let namesake = vault.join("Projets/Index Cache");
        std::fs::create_dir_all(&namesake).unwrap();
        std::fs::write(namesake.join("Notes.md"), "needle in a real note\n").unwrap();

        if !FilesystemVaultBackend::new(&vault).ripgrep_available {
            let _ = std::fs::remove_dir_all(&vault);
            return;
        }

        async fn grep_paths(backend: &FilesystemVaultBackend, glob: Option<&str>) -> Vec<String> {
            let mut paths = backend
                .execute(BackendRequest::Recall(RecallRequest::Grep {
                    query: "needle".to_string(),
                    regex: false,
                    case_sensitive: true,
                    glob: glob.map(str::to_string),
                    context_lines: 0,
                    limit: 10,
                }))
                .await
                .expect("grep should succeed")
                .into_grep_matches()
                .expect("grep matches")
                .into_iter()
                .map(|item| item.path)
                .collect::<Vec<_>>();
            paths.sort();
            paths
        }

        // Without the exclusion the index dir leaks into the results — this is
        // the bug, asserted so the fix below cannot pass vacuously.
        let leaked = grep_paths(&FilesystemVaultBackend::new(&vault), Some("**/*")).await;
        assert_eq!(
            leaked,
            vec![
                "Index Cache/cached.md".to_string(),
                "Index Cache/index.sqlite".to_string(),
                "Projets/Index Cache/Notes.md".to_string(),
                "Real.md".to_string()
            ]
        );

        // With the index dir declared the phantom paths are gone — and only
        // those: the same-named directory deeper in the vault is untouched.
        let filtered = grep_paths(
            &FilesystemVaultBackend::new(&vault).with_index_dir(&index_dir),
            Some("**/*"),
        )
        .await;
        assert_eq!(
            filtered,
            vec![
                "Projets/Index Cache/Notes.md".to_string(),
                "Real.md".to_string()
            ]
        );

        // An index dir outside the vault yields no exclusion and no false
        // negatives.
        let outside_dir = temp_dir("backend-grep-index-dir-outside");
        let outside = grep_paths(
            &FilesystemVaultBackend::new(&vault).with_index_dir(&outside_dir),
            None,
        )
        .await;
        assert_eq!(
            outside,
            vec![
                "Index Cache/cached.md".to_string(),
                "Projets/Index Cache/Notes.md".to_string(),
                "Real.md".to_string()
            ],
            "the default *.md glob still sees every markdown file"
        );

        let _ = std::fs::remove_dir_all(&vault);
    }

    #[tokio::test]
    async fn grep_spawn_not_found_yields_the_clear_message() {
        let vault = temp_dir("backend-grep-missing-rg");
        std::fs::create_dir_all(&vault).unwrap();
        // An absolute path to a real file that is not ripgrep would spawn; instead
        // point at a missing file so availability is false and the guard fires.
        let backend = FilesystemVaultBackend::with_ripgrep(&vault, vault.join("missing-rg"));
        assert!(
            !backend.descriptor().supports(Capability::GrepSearch),
            "a missing rg must not advertise the grep capability"
        );
        let error = backend
            .execute(BackendRequest::Recall(RecallRequest::Grep {
                query: "needle".to_string(),
                regex: false,
                case_sensitive: true,
                glob: None,
                context_lines: 0,
                limit: 10,
            }))
            .await
            .expect_err("grep without rg must fail");
        assert_eq!(error.to_string(), grep::RIPGREP_UNAVAILABLE_MESSAGE);
        assert!(
            !error.to_string().contains("os error 2"),
            "must not surface the raw spawn error: {error}"
        );

        let _ = std::fs::remove_dir_all(&vault);
    }

    // -- changes ------------------------------------------------------------

    #[tokio::test]
    async fn changes_delivers_a_touched_file() {
        let vault = temp_dir("backend-changes");
        std::fs::create_dir_all(&vault).unwrap();
        let backend = FilesystemVaultBackend::new(&vault);
        let mut stream = backend.changes(None);

        // Give the platform watcher a moment to arm before mutating.
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(vault.join("Touched.md"), "# Touched\n").unwrap();

        // FSEvents/inotify latency varies; wait generously but bounded.
        let event = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match stream.recv().await {
                    Some(ChangeEvent::Change(reason)) => return Some(reason),
                    // Watcher errors are not what this test asserts; keep waiting.
                    Some(ChangeEvent::Error(_)) => continue,
                    None => return None,
                }
            }
        })
        .await
        .expect("a change event must arrive within the timeout")
        .expect("the stream must not end before delivering a change");

        assert!(
            event.starts_with("watch:"),
            "reason should carry the watch prefix, got {event}"
        );

        let _ = std::fs::remove_dir_all(&vault);
    }

    #[tokio::test]
    async fn health_overview_reports_a_missing_vault_with_core_wording() {
        let vault = temp_dir("backend-health-missing");
        let backend = FilesystemVaultBackend::new(&vault);
        let error = backend
            .execute(BackendRequest::health_overview())
            .await
            .expect_err("a non-existent vault must fail the health gate");
        assert!(
            error
                .to_string()
                .starts_with("vault path does not exist or is not a directory: "),
            "unexpected wording: {error}"
        );

        std::fs::create_dir_all(&vault).unwrap();
        assert!(matches!(
            backend
                .execute(BackendRequest::health_overview())
                .await
                .expect("an existing vault is reachable"),
            BackendResponse::Health(HealthResponse::Overview { reachable: true })
        ));
        let _ = std::fs::remove_dir_all(&vault);
    }
}
