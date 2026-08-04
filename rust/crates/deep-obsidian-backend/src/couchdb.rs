//! The read-only CouchDB / Self-hosted LiveSync vault, behind the boundary.
//!
//! Everything here is a translation between two vocabularies: the
//! [`VaultBackend`] request families, which are shaped by the server's call sites,
//! and the sidecar's protocol, which is shaped by LiveSync's storage. The
//! interesting parts are where the two do not line up:
//!
//! * **There are no directories.** A LiveSync vault is a flat map of paths, so
//!   `ListChildren` and `TopLevelFolders` SYNTHESIZE the folder tree from path
//!   prefixes. Folder entries therefore carry no size (there is nothing to size),
//!   which matches what the filesystem backend reports for a directory.
//! * **Deletes are soft.** A deleted entry is still a readable document with
//!   `deleted: true`. Listings exclude them — a tombstone is not a file — while
//!   `read`/`stat` on one still answers, so a caller holding a stale path gets the
//!   content rather than a lie.
//! * **Conflicts are served.** The winning revision is returned and
//!   `conflicted: true` comes back with it. [`ContentResponse::Stat`] carries only
//!   `size_bytes`, so the flag cannot be surfaced through the public MCP schema
//!   this slice without changing it; it is logged instead. See
//!   [`CouchDbVaultBackend::stat`].
//! * **Nothing can be written.** Every mutation is refused with
//!   [`COUCHDB_READ_ONLY_MESSAGE`], which names the experimental read-only state
//!   explicitly rather than reporting a generic capability error.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::debug;

use crate::sidecar::{
    EntryKind, ManifestEntry, ReadPayload, SidecarConfig, SidecarCredentials, SidecarError,
    SidecarSupervisor,
};
use crate::watch::ChangeStream;
use crate::{
    BackendDescriptor, BackendError, BackendKind, BackendRequest, BackendResponse, Capability,
    ContentRequest, ContentResponse, HealthRequest, HealthResponse, ManifestRequest,
    ManifestResponse, MutationRequest, OpaqueCursor, RecallRequest, VaultBackend, VaultChildEntry,
    VaultEntryKind,
};

/// Refusal for every write against a CouchDB mount.
///
/// Deliberately long and specific. A user reaching this has configured a mount and
/// then tried to save a note into it; "unsupported operation" would leave them
/// guessing whether it is a bug, a permission problem, or a missing capability.
/// The three facts they need are that the backend is experimental, that it is
/// read-only *by construction* rather than by configuration, and where writes DO
/// work.
pub const COUCHDB_READ_ONLY_MESSAGE: &str = "this mount is an EXPERIMENTAL, READ-ONLY \
CouchDB (Self-hosted LiveSync) vault: writes are refused by construction, not by configuration. \
The sidecar that reads it overrides CouchDB's put/delete so it cannot mutate your vault, and no \
write path exists yet. Edit the note in Obsidian and let LiveSync replicate it, or write to a \
filesystem mount instead.";

/// Refusal for `grep_search` against a CouchDB mount.
pub const COUCHDB_GREP_UNSUPPORTED_MESSAGE: &str = "grep_search is unavailable on this mount: it \
is an EXPERIMENTAL, READ-ONLY CouchDB (Self-hosted LiveSync) vault, and line search is served by \
ripgrep over local files, which do not exist for a CouchDB vault. Use hybrid_search or \
bm25_search, which are served by this mount's own index.";

/// How long a collected manifest may be reused.
///
/// A short reuse window, not a cache with an independent lifetime. It exists for
/// one specific shape: the index refresh asks for the note manifest and the
/// artifact manifest back to back, and each ask is a full cursor-looped `manifest`
/// walk over the sidecar. On a filesystem vault a second walk is a cheap
/// `read_dir`; here it is N round trips to CouchDB.
///
/// It is deliberately SHORT rather than invalidated by the change feed: a stale
/// manifest that outlived a change would make the refresh conclude "unchanged" and
/// silently serve stale content. Two seconds is long enough to cover one refresh's
/// paired calls and short enough that no user-visible read can be served from a
/// manifest older than the request that triggered it. `CouchDbSource` additionally
/// pins one manifest per refresh (see the server's `couchdb_source` module), which
/// is what actually collapses 4 walks into 1.
const MANIFEST_REUSE_WINDOW: Duration = Duration::from_secs(2);

/// A collected manifest and when it was collected.
struct CachedManifest {
    entries: Arc<Vec<ManifestEntry>>,
    collected_at: std::time::Instant,
}

/// A read-only LiveSync vault reached through a supervised sidecar.
pub struct CouchDbVaultBackend {
    supervisor: Arc<SidecarSupervisor>,
    manifest: std::sync::Mutex<Option<CachedManifest>>,
}

impl std::fmt::Debug for CouchDbVaultBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CouchDbVaultBackend")
            .field("supervisor", &self.supervisor)
            .finish()
    }
}

impl CouchDbVaultBackend {
    /// Build a backend over an existing supervisor.
    ///
    /// Takes the supervisor rather than constructing one so the backend and the
    /// index source SHARE a single child process. Two supervisors for one mount
    /// would mean two CouchDB connections, two handshakes, two change feeds, and
    /// two irreconcilable health answers.
    pub fn new(supervisor: Arc<SidecarSupervisor>) -> Self {
        Self {
            supervisor,
            manifest: std::sync::Mutex::new(None),
        }
    }

    /// Build a supervisor and a backend over it, resolving the bundle location.
    ///
    /// Construction performs NO IO against CouchDB: the handshake happens on first
    /// use. That is what lets a mount whose remote is down still be constructed, so
    /// the server can report it as degraded instead of refusing to start.
    pub fn spawn(
        sidecar_path: Option<&Path>,
        credentials: SidecarCredentials,
        options: Option<Value>,
        request_timeout: Option<Duration>,
    ) -> Result<(Arc<SidecarSupervisor>, Self), SidecarError> {
        let config = SidecarConfig::resolve(sidecar_path, credentials, options, request_timeout)?;
        let supervisor = SidecarSupervisor::new(config);
        Ok((supervisor.clone(), Self::new(supervisor)))
    }

    pub fn supervisor(&self) -> &Arc<SidecarSupervisor> {
        &self.supervisor
    }

    /// The whole manifest, reusing a very recent collection.
    ///
    /// See [`MANIFEST_REUSE_WINDOW`] for why the window is short rather than
    /// change-feed-invalidated.
    pub async fn manifest_entries(&self) -> Result<Arc<Vec<ManifestEntry>>, BackendError> {
        if let Some(cached) = self.cached_manifest() {
            return Ok(cached);
        }
        let entries = Arc::new(map_sidecar(self.supervisor.collect_manifest().await)?);
        if let Ok(mut slot) = self.manifest.lock() {
            *slot = Some(CachedManifest {
                entries: entries.clone(),
                collected_at: std::time::Instant::now(),
            });
        }
        Ok(entries)
    }

    fn cached_manifest(&self) -> Option<Arc<Vec<ManifestEntry>>> {
        let slot = self.manifest.lock().ok()?;
        let cached = slot.as_ref()?;
        (cached.collected_at.elapsed() < MANIFEST_REUSE_WINDOW).then(|| cached.entries.clone())
    }

    async fn manifest_request(
        &self,
        request: ManifestRequest,
    ) -> Result<ManifestResponse, BackendError> {
        let entries = self.manifest_entries().await?;
        match request {
            ManifestRequest::ListChildren {
                path,
                include_hidden,
                include_ignored,
            } => Ok(ManifestResponse::Children(list_children(
                &entries,
                path.as_deref(),
                include_hidden,
                include_ignored,
            ))),
            ManifestRequest::WalkMarkdown => {
                Ok(ManifestResponse::MarkdownFiles(walk_markdown(&entries)))
            }
            ManifestRequest::TopLevelFolders => {
                Ok(ManifestResponse::Folders(top_level_folders(&entries)))
            }
        }
    }

    async fn content(&self, request: ContentRequest) -> Result<ContentResponse, BackendError> {
        match request {
            ContentRequest::ReadText { path } => {
                ensure_vault_relative(&path)?;
                let result = map_sidecar(self.supervisor.read(&path).await)?;
                note_conflict(&path, result.conflicted);
                match result.payload {
                    ReadPayload::Text(text) => Ok(ContentResponse::Text { text }),
                    // A `newnote` entry read as text. Refused rather than
                    // lossily decoded: the caller asked for a note.
                    ReadPayload::Bytes(_) => Err(BackendError::Message(format!(
                        "{path} is stored as a binary attachment in this CouchDB vault, not as \
                         text; read it with read_artifact instead"
                    ))),
                }
            }
            ContentRequest::ReadBytes { path } => {
                ensure_vault_relative(&path)?;
                let result = map_sidecar(self.supervisor.read(&path).await)?;
                note_conflict(&path, result.conflicted);
                Ok(ContentResponse::Bytes(match result.payload {
                    ReadPayload::Bytes(bytes) => bytes,
                    // A text entry read as bytes: its UTF-8 encoding IS its bytes.
                    ReadPayload::Text(text) => text.into_bytes(),
                }))
            }
            ContentRequest::Stat { path } => {
                ensure_vault_relative(&path)?;
                let stat = map_sidecar(self.supervisor.stat(&path).await)?;
                // `ContentResponse::Stat` carries only `size_bytes`, and widening it
                // would change a frozen MCP payload. So `conflicted` is INTERNAL-ONLY
                // this slice: logged here, and reported per-mount through health.
                note_conflict(&path, stat.conflicted);
                Ok(ContentResponse::Stat {
                    size_bytes: stat.size,
                })
            }
            // Pure validation, so it must not touch the sidecar: the upload mint
            // calls it to reject traversal before issuing a token, and a mount whose
            // remote is down must still reject `../escape`.
            ContentRequest::ResolvePath { path } => {
                ensure_vault_relative(&path)?;
                Ok(ContentResponse::PathAccepted)
            }
        }
    }

    async fn health(&self, request: HealthRequest) -> Result<HealthResponse, BackendError> {
        match request {
            // NOT a hard startup gate: a CouchDB mount whose remote is unreachable
            // must leave the server serving its filesystem root. So this reports
            // `reachable: false` rather than erroring, and the mount's readiness
            // (which does fail closed) is what marks it degraded.
            HealthRequest::Overview => {
                let health = self.supervisor.probe_health().await;
                Ok(HealthResponse::Overview {
                    reachable: health.is_ready(),
                })
            }
        }
    }
}

#[async_trait::async_trait]
impl VaultBackend for CouchDbVaultBackend {
    /// # Capability rationale
    ///
    /// * `BinaryRead` — attachments are read through the sidecar's `read` with
    ///   `kind: "binary"`.
    /// * `Watch` — the sidecar's live change feed.
    /// * NO `GrepSearch` — ripgrep needs files on disk. See
    ///   [`COUCHDB_GREP_UNSUPPORTED_MESSAGE`].
    /// * NO `BinaryWrite`, NO `Upload` — read-only. See
    ///   [`COUCHDB_READ_ONLY_MESSAGE`].
    fn descriptor(&self) -> BackendDescriptor {
        BackendDescriptor::new(
            BackendKind::Couchdb,
            [Capability::BinaryRead, Capability::Watch],
        )
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, BackendError> {
        match request {
            BackendRequest::Manifest(request) => self
                .manifest_request(request)
                .await
                .map(BackendResponse::Manifest),
            BackendRequest::Content(request) => {
                self.content(request).await.map(BackendResponse::Content)
            }
            // Every write, refused with the same explicit message. `SweepOrphanStagingFiles`
            // is the one exception: it is documented as best-effort housekeeping that
            // never fails, and there is no staging area here, so it is a no-op rather
            // than a refusal — failing it would make a caller's cleanup pass report a
            // spurious error.
            BackendRequest::Mutation(MutationRequest::SweepOrphanStagingFiles) => {
                Ok(BackendResponse::Mutation(crate::MutationResponse::Swept))
            }
            BackendRequest::Mutation(_) => Err(BackendError::Unsupported(
                COUCHDB_READ_ONLY_MESSAGE.to_string(),
            )),
            BackendRequest::Recall(RecallRequest::Grep { .. }) => Err(BackendError::Unsupported(
                COUCHDB_GREP_UNSUPPORTED_MESSAGE.to_string(),
            )),
            BackendRequest::Health(request) => {
                self.health(request).await.map(BackendResponse::Health)
            }
        }
    }

    /// Bridge the sidecar's change notifications onto a [`ChangeStream`].
    ///
    /// `after` is the sidecar's own opaque cursor, wrapped in [`OpaqueCursor`] and
    /// handed back verbatim. The supervisor replays `changesSince` from it before
    /// arming the live feed, so a resumed subscription does not miss the edits made
    /// while nothing was subscribed.
    fn changes(&self, after: Option<OpaqueCursor>) -> ChangeStream {
        let receiver = self
            .supervisor
            .changes(after.map(|cursor| cursor.as_str().to_string()));
        // The stream owns nothing that stops the child: several subscribers share one
        // feed, and dropping one must not silence the others (unlike the filesystem
        // backend, whose stream owns its `notify` watcher).
        ChangeStream::new(receiver, ())
    }
}

/// Log a conflicted read at debug.
///
/// The winning revision is what was served, which is correct but worth a trace:
/// two devices disagreed about this note and the loser's edit is not in the
/// content that was just handed out.
fn note_conflict(path: &str, conflicted: bool) {
    if conflicted {
        debug!(
            "livesync entry {path} has conflicting revisions; served the winning revision \
             (conflict revisions are not exposed in protocol v1)"
        );
    }
}

/// Map a sidecar failure onto a backend failure.
///
/// `not-found` becomes a bare [`std::io::ErrorKind::NotFound`] IO error rather than
/// a message, because the server distinguishes "destination absent" from every
/// other failure by `io_kind()` — see [`BackendError`]'s own docs. Everything else
/// keeps the sidecar's already-redacted wording, prefixed with the mount kind so a
/// user can tell a CouchDB failure from a filesystem one.
fn map_sidecar<T>(result: Result<T, SidecarError>) -> Result<T, BackendError> {
    result.map_err(|error| match &error {
        SidecarError::Rpc {
            kind: crate::sidecar::SidecarErrorKind::NotFound,
            detail,
            ..
        } => BackendError::io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            detail.clone(),
        )),
        _ => BackendError::Message(error.to_string()),
    })
}

/// Reject a path that is not usable as a vault-relative path.
///
/// The sidecar hides paths containing `:` and paths starting with `.` (mirroring
/// commonlib's own `isTargetFile`), so those can never be served and are refused
/// here rather than turned into a confusing `not-found`. Traversal is refused for
/// the obvious reason, and because [`ContentRequest::ResolvePath`]'s contract is
/// exactly this check.
fn ensure_vault_relative(path: &str) -> Result<(), BackendError> {
    let refuse = || {
        Err(BackendError::Vault(
            deep_obsidian_core::vault::VaultError::InvalidVaultRelativePath(path.to_string()),
        ))
    };
    if path.trim().is_empty() {
        return refuse();
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return refuse();
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.starts_with('.') {
            return refuse();
        }
    }
    Ok(())
}

/// True when a manifest entry should appear in a listing.
///
/// Soft-deleted entries are excluded: a tombstone is not a file, and listing one
/// would advertise a path whose content is a deleted document. `internal` entries
/// are excluded too — the sidecar already omits them from `manifest`, so this is
/// belt and braces against a future sidecar that stops doing so.
fn is_listable(entry: &ManifestEntry) -> bool {
    !entry.deleted && !matches!(entry.kind, EntryKind::Internal)
}

/// True when a path segment is hidden or in an ignored directory, mirroring core's
/// `should_ignore_entry` so a CouchDB listing filters what a filesystem listing
/// filters.
fn segment_is_filtered(segment: &str, include_hidden: bool, include_ignored: bool) -> bool {
    if !include_hidden && segment.starts_with('.') {
        return true;
    }
    if !include_ignored && deep_obsidian_core::vault::DEFAULT_IGNORED_DIRS.contains(&segment) {
        return true;
    }
    false
}

fn is_markdown_path(path: &str) -> bool {
    path.rsplit_once('.')
        .map(|(_, extension)| extension.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

/// Direct children of `prefix`, with folders synthesized from path prefixes.
///
/// Ordering matches core's `list_children` exactly — directories first, then files,
/// each group by vault-relative path — because the MCP `list_children` payload is
/// frozen on that order and a caller must not be able to tell which backend
/// answered from the shape of the result.
fn list_children(
    entries: &[ManifestEntry],
    prefix: Option<&str>,
    include_hidden: bool,
    include_ignored: bool,
) -> Vec<VaultChildEntry> {
    let prefix = prefix.map(|prefix| prefix.trim_matches('/')).unwrap_or("");
    let mut directories: BTreeSet<String> = BTreeSet::new();
    let mut files: Vec<VaultChildEntry> = Vec::new();

    for entry in entries.iter().filter(|entry| is_listable(entry)) {
        let Some(remainder) = strip_prefix_segments(&entry.path, prefix) else {
            continue;
        };
        let mut segments = remainder.splitn(2, '/');
        let head = segments.next().unwrap_or_default();
        if head.is_empty() || segment_is_filtered(head, include_hidden, include_ignored) {
            continue;
        }
        let child_path = if prefix.is_empty() {
            head.to_string()
        } else {
            format!("{prefix}/{head}")
        };
        match segments.next() {
            // A deeper path: `head` is a synthesized folder.
            Some(_) => {
                directories.insert(child_path);
            }
            None => files.push(VaultChildEntry {
                name: head.to_string(),
                path: child_path,
                kind: VaultEntryKind::File,
                is_markdown: is_markdown_path(head),
                size_bytes: Some(entry.size),
            }),
        }
    }

    let mut children: Vec<VaultChildEntry> = directories
        .into_iter()
        .map(|path| VaultChildEntry {
            name: path.rsplit('/').next().unwrap_or(&path).to_string(),
            path,
            kind: VaultEntryKind::Directory,
            is_markdown: false,
            // A synthesized folder has no size, exactly as a real directory reports
            // `None` from the filesystem backend.
            size_bytes: None,
        })
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    children.extend(files);
    children
}

/// The part of `path` below `prefix`, or `None` when `path` is not under it.
///
/// Segment-aware: `Notes` must not match `NotesArchive/x.md`.
fn strip_prefix_segments<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    let remainder = path.strip_prefix(prefix)?;
    remainder.strip_prefix('/')
}

/// Every markdown entry, sorted, hidden and ignored paths dropped.
///
/// Sorted by vault-relative path string, which is what
/// `NoteSource::note_snapshots` requires: the order fixes note and chunk ids and
/// therefore retrieval scores.
fn walk_markdown(entries: &[ManifestEntry]) -> Vec<String> {
    let mut files: Vec<String> = entries
        .iter()
        .filter(|entry| is_listable(entry))
        .filter(|entry| is_markdown_path(&entry.path))
        .filter(|entry| !path_is_filtered(&entry.path))
        .map(|entry| entry.path.clone())
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Visible top-level folders, sorted.
fn top_level_folders(entries: &[ManifestEntry]) -> Vec<String> {
    let mut folders: BTreeSet<String> = BTreeSet::new();
    for entry in entries.iter().filter(|entry| is_listable(entry)) {
        let Some((head, _)) = entry.path.split_once('/') else {
            continue;
        };
        if segment_is_filtered(head, false, false) {
            continue;
        }
        folders.insert(head.to_string());
    }
    folders.into_iter().collect()
}

/// True when any segment of `path` is hidden or an ignored directory.
///
/// Mirrors core's `ensure_markdown_dir_ignored`, which drops a whole subtree rather
/// than just the leaf.
fn path_is_filtered(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment_is_filtered(segment, false, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::EntryKind;

    fn entry(path: &str, kind: EntryKind, deleted: bool) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            size: path.len() as u64,
            mtime_ms: 1,
            ctime_ms: 1,
            deleted,
            conflicted: false,
            kind,
        }
    }

    /// The fixture shape: a nested folder, a hidden folder, an ignored folder, a
    /// soft-deleted note and a binary attachment.
    fn vault() -> Vec<ManifestEntry> {
        vec![
            entry("Alpha.md", EntryKind::Markdown, false),
            entry("Notes/Beta.md", EntryKind::Markdown, false),
            entry("Notes/Deep/Gamma.md", EntryKind::Markdown, false),
            entry("NotesArchive/Old.md", EntryKind::Markdown, false),
            entry("Assets/logo.png", EntryKind::Binary, false),
            entry("Removed.md", EntryKind::Markdown, true),
            entry(".obsidian/workspace.json", EntryKind::Markdown, false),
            entry("node_modules/pkg/index.md", EntryKind::Markdown, false),
        ]
    }

    /// Directories first, then files, each group by path — core's exact ordering, so
    /// a caller cannot tell which backend answered.
    #[test]
    fn list_children_synthesizes_folders_and_keeps_cores_ordering() {
        let children = list_children(&vault(), None, false, false);
        let rendered: Vec<(&str, bool)> = children
            .iter()
            .map(|child| {
                (
                    child.path.as_str(),
                    matches!(child.kind, VaultEntryKind::Directory),
                )
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                ("Assets", true),
                ("Notes", true),
                ("NotesArchive", true),
                ("Alpha.md", false),
            ]
        );
        // A synthesized folder has no size, exactly as a real directory reports.
        assert!(children[0].size_bytes.is_none());
        // A file carries the manifest size, and markdown is flagged.
        let alpha = children.last().expect("a file child");
        assert_eq!(alpha.size_bytes, Some("Alpha.md".len() as u64));
        assert!(alpha.is_markdown);
        assert_eq!(alpha.name, "Alpha.md");
    }

    /// A soft delete is not a file: it must not be listed. The hidden and ignored
    /// folders are dropped too.
    #[test]
    fn list_children_excludes_tombstones_hidden_and_ignored() {
        let children = list_children(&vault(), None, false, false);
        let paths: Vec<&str> = children.iter().map(|child| child.path.as_str()).collect();
        assert!(!paths.contains(&"Removed.md"), "{paths:?}");
        assert!(!paths.contains(&".obsidian"), "{paths:?}");
        assert!(!paths.contains(&"node_modules"), "{paths:?}");
    }

    /// Hidden entries appear when asked for, matching core's `include_hidden`.
    #[test]
    fn list_children_honours_include_hidden_and_include_ignored() {
        let children = list_children(&vault(), None, true, true);
        let paths: Vec<&str> = children.iter().map(|child| child.path.as_str()).collect();
        assert!(paths.contains(&".obsidian"), "{paths:?}");
        assert!(paths.contains(&"node_modules"), "{paths:?}");
    }

    /// The prefix match is segment-aware: `Notes` must not swallow `NotesArchive`.
    #[test]
    fn list_children_matches_whole_segments_only() {
        let children = list_children(&vault(), Some("Notes"), false, false);
        let rendered: Vec<&str> = children.iter().map(|child| child.path.as_str()).collect();
        assert_eq!(rendered, vec!["Notes/Deep", "Notes/Beta.md"]);

        let nested = list_children(&vault(), Some("Notes/Deep"), false, false);
        let rendered: Vec<&str> = nested.iter().map(|child| child.path.as_str()).collect();
        assert_eq!(rendered, vec!["Notes/Deep/Gamma.md"]);

        // A leading/trailing slash is tolerated, as it is for a filesystem mount.
        assert_eq!(
            list_children(&vault(), Some("/Notes/"), false, false).len(),
            2
        );
    }

    /// Sorted by vault-relative path string: the ordering `NoteSource` requires,
    /// because it fixes note and chunk ids.
    #[test]
    fn walk_markdown_is_sorted_and_filters_the_same_paths_as_core() {
        assert_eq!(
            walk_markdown(&vault()),
            vec![
                "Alpha.md".to_string(),
                "Notes/Beta.md".to_string(),
                "Notes/Deep/Gamma.md".to_string(),
                "NotesArchive/Old.md".to_string(),
            ]
        );
        // Binary entries are not markdown; tombstones, hidden and ignored subtrees
        // are all dropped.
        assert!(!walk_markdown(&vault()).contains(&"Assets/logo.png".to_string()));
        assert!(!walk_markdown(&vault()).contains(&"Removed.md".to_string()));
        assert!(!walk_markdown(&vault()).contains(&"node_modules/pkg/index.md".to_string()));
    }

    #[test]
    fn top_level_folders_are_visible_and_sorted() {
        assert_eq!(
            top_level_folders(&vault()),
            vec![
                "Assets".to_string(),
                "Notes".to_string(),
                "NotesArchive".to_string(),
            ]
        );
    }

    /// The paths the sidecar can never serve are refused here, so a caller gets a
    /// path error rather than a confusing `not-found`.
    #[test]
    fn rejects_paths_the_sidecar_cannot_serve() {
        for path in [
            "",
            "   ",
            "/absolute.md",
            "../escape.md",
            "Notes/../../escape.md",
            "has:colon.md",
            ".hidden/note.md",
            "back\\slash.md",
        ] {
            assert!(
                ensure_vault_relative(path).is_err(),
                "{path:?} must be refused"
            );
        }
        for path in ["Alpha.md", "Notes/Beta.md", "Assets/logo.png"] {
            assert!(ensure_vault_relative(path).is_ok(), "{path:?} must be ok");
        }
    }

    /// `not-found` must arrive as an IO-kind error, because the server branches on
    /// `io_kind()` to tell "destination absent" from every other failure.
    #[test]
    fn not_found_maps_to_an_io_not_found_error() {
        let error = map_sidecar::<()>(Err(SidecarError::Rpc {
            kind: crate::sidecar::SidecarErrorKind::NotFound,
            detail: "no entry at that path".to_string(),
            status: None,
        }))
        .expect_err("not-found must map to an error");
        assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotFound));
    }

    /// Every other kind keeps the sidecar's redacted wording.
    #[test]
    fn other_kinds_keep_the_sidecars_wording() {
        let error = map_sidecar::<()>(Err(SidecarError::Rpc {
            kind: crate::sidecar::SidecarErrorKind::DecryptFailed,
            detail: "chunk could not be decrypted".to_string(),
            status: None,
        }))
        .expect_err("decrypt-failed must map to an error");
        let message = error.to_string();
        assert!(message.contains("decrypt-failed"), "{message}");
        assert!(
            message.contains("chunk could not be decrypted"),
            "{message}"
        );
        assert!(error.io_kind().is_none());
    }

    /// The refusal strings name the experimental read-only state explicitly, which
    /// is the whole point of not reusing a generic capability error.
    #[test]
    fn refusal_strings_say_experimental_and_read_only() {
        for message in [COUCHDB_READ_ONLY_MESSAGE, COUCHDB_GREP_UNSUPPORTED_MESSAGE] {
            assert!(message.contains("EXPERIMENTAL"), "{message}");
            assert!(message.contains("READ-ONLY"), "{message}");
        }
        // ...and each points at what DOES work.
        assert!(COUCHDB_READ_ONLY_MESSAGE.contains("filesystem mount"));
        assert!(COUCHDB_GREP_UNSUPPORTED_MESSAGE.contains("hybrid_search"));
    }
}
