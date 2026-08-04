//! The `VaultBackend` boundary: everything the MCP server needs from a vault,
//! expressed without naming a provider.
//!
//! The server talks to a vault through exactly three entry points — [`VaultBackend::descriptor`],
//! [`VaultBackend::execute`], and [`VaultBackend::changes`]. Provider specifics
//! (absolute paths, revisions, native cursors, HTTP clients, caches) stay private
//! to the implementation; nothing in the request/response vocabulary below is
//! filesystem-shaped except where the *public MCP contract* already froze it.
//!
//! ## Why the request vocabulary looks like this
//!
//! The variants were derived from the server's actual call sites, not from what a
//! generic vault API might want. There is no `WriteBytes`, `Rename`, `RemoveFile`,
//! or `CreateDirAll` because no server call site needs one: the only binary write
//! path is the out-of-band upload, whose temp-file-plus-atomic-rename mechanics are
//! deliberately private to the backend (see [`MutationRequest::CommitUploadStream`]).
//!
//! ## Error fidelity is the hard constraint
//!
//! Every string this crate can surface is already public MCP behaviour, frozen by
//! the server's golden snapshots. See [`BackendError`] for how byte-identical
//! wording is guaranteed.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod couchdb;
pub mod filesystem;
pub mod grep;
pub mod router;
pub mod sidecar;
pub mod watch;

#[cfg(test)]
mod memory;

#[cfg(test)]
mod contract;

pub use couchdb::{
    CouchDbVaultBackend, EntryContent, COUCHDB_GREP_UNSUPPORTED_MESSAGE, COUCHDB_READ_ONLY_MESSAGE,
    UPLOAD_COLLECT_ADVISORY_BYTES,
};
pub use deep_obsidian_core::vault::{VaultChildEntry, VaultEntryKind, VaultError};
pub use filesystem::FilesystemVaultBackend;
pub use grep::{resolve_ripgrep, RIPGREP_UNAVAILABLE_MESSAGE};
pub use router::{Mount, Resolved, RouterError, VaultRouter};
pub use sidecar::{
    CompatibilityStatus, SidecarConfig, SidecarCredentials, SidecarError, SidecarMode,
    SidecarSupervisor, SupervisorHealth,
};
pub use watch::{should_ignore_watch_path, watch_reason, ChangeEvent, ChangeStream};

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

/// Which provider is behind a backend. Informational: the server routes by
/// capability, never by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    Filesystem,
    InMemory,
    /// A read-only Self-hosted LiveSync vault in CouchDB, behind the supervised
    /// Node sidecar.
    Couchdb,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Filesystem => "filesystem",
            BackendKind::InMemory => "in-memory",
            BackendKind::Couchdb => "couchdb",
        }
    }
}

/// A discrete thing a backend can do. Absent capability means the server must not
/// advertise or attempt the corresponding operation.
///
/// `GrepSearch` is the capability that today's `rg_available` flag becomes: the
/// `grep_search` tool is advertised if and only if it is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    GrepSearch,
    BinaryRead,
    BinaryWrite,
    Upload,
    Watch,
}

/// What a backend is and what it can do. Cheap to produce: callers may call
/// [`VaultBackend::descriptor`] on any path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendDescriptor {
    pub kind: BackendKind,
    pub capabilities: BTreeSet<Capability>,
}

impl BackendDescriptor {
    pub fn new(kind: BackendKind, capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            kind,
            capabilities: capabilities.into_iter().collect(),
        }
    }

    pub fn supports(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

// ---------------------------------------------------------------------------
// Cursor and change stream
// ---------------------------------------------------------------------------

/// An opaque resume token. Its contents are meaningful only to the backend that
/// minted it; callers persist and replay it without interpretation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OpaqueCursor(String);

impl OpaqueCursor {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A normalized backend failure.
///
/// # Why variants wrap sources instead of carrying pre-rendered strings
///
/// Pre-rendered strings were rejected: two existing server call sites *match on
/// error structure*, not on wording.
///
/// * `resources::read_note_text` pattern-matches [`VaultError::InvalidVaultRelativePath`]
///   to remap it to the legacy `path escapes the vault: {path}` wording;
/// * the streaming upload commit distinguishes `io::ErrorKind::NotFound` (destination
///   absent, so nothing to conflict with) from every other IO failure.
///
/// So the source is kept inspectable, and `Display` is delegated so wording is
/// byte-identical by construction rather than by transcription:
///
/// * [`BackendError::Vault`] renders exactly [`VaultError`]'s `Display`, including
///   the path-and-remediation enrichment on `PermissionDenied`.
/// * [`BackendError::Io`] renders the *bare* `io::Error`, with no path prefix and no
///   enrichment.
///
/// That asymmetry is deliberate and load-bearing. `read_file` goes through core's
/// `read_text_file` and so reports `io error for <abs path>: <cause>`, while
/// `read_artifact` historically called `fs::metadata`/`fs::read` directly and
/// reports the bare `No such file or directory (os error 2)`. Both are frozen
/// public behaviour, so [`ContentRequest::ReadText`] is `Vault`-flavoured while
/// [`ContentRequest::Stat`] and [`ContentRequest::ReadBytes`] are `Io`-flavoured.
/// Do not "fix" the inconsistency here; it is the contract.
#[derive(Debug, Error)]
pub enum BackendError {
    /// A vault-semantics failure. Renders core's enriched wording verbatim.
    #[error("{0}")]
    Vault(#[from] VaultError),
    /// A raw IO failure, rendered bare — no path, no remediation.
    #[error("{source}")]
    Io {
        #[source]
        source: std::io::Error,
    },
    /// An already-rendered message from a subprocess or parser (ripgrep's stderr,
    /// its JSON event stream). These strings were never structured to begin with.
    #[error("{0}")]
    Message(String),
    /// A write exceeded the caller's declared byte budget.
    #[error("upload exceeds maximum allowed size")]
    PayloadTooLarge,
    /// A destination resolved outside the vault root.
    #[error("destination escapes the vault root")]
    PathEscapesVault,
    /// Optimistic-concurrency check failed.
    #[error("hash conflict: expected {expected}, found {found}")]
    HashConflict { expected: String, found: String },
    /// A storage-level compare-and-swap lost: the destination changed between the
    /// caller's precondition check and the write, so NOTHING was written.
    ///
    /// This failure mode cannot arise on the filesystem backend, whose write is a
    /// rename that always wins. It exists because a versioned backend can detect
    /// what the filesystem silently tolerates, and detecting it and then writing
    /// anyway would be the one thing this boundary must never do.
    ///
    /// `Display` opens with `hash conflict for {path}:` so it lands in the same
    /// taxonomy a caller already handles for a stale `expectedHash` — the cause
    /// (the note changed under a concurrent writer) and the remedy (re-read, retry)
    /// are the same. It then says what a hash comparison cannot: that the change
    /// arrived AFTER the caller's own check.
    #[error(
        "hash conflict for {path}: the destination changed between this write's precondition \
         check and the write itself, so nothing was written (precondition: {expected}; the \
         destination is now at {found}). Re-read the note and retry."
    )]
    VersionConflict {
        path: String,
        /// The precondition that was not met, rendered for a human.
        expected: String,
        /// Where the destination actually is now, rendered for a human.
        found: String,
    },
    /// The request is not supported by this backend (capability absent).
    #[error("{0}")]
    Unsupported(String),
}

impl BackendError {
    /// Wrap a raw IO error, preserving its bare rendering.
    pub fn io(source: std::io::Error) -> Self {
        BackendError::Io { source }
    }

    /// The `io::ErrorKind` behind this error, when there is one.
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            BackendError::Io { source } => Some(source.kind()),
            BackendError::Vault(VaultError::Io { source, .. }) => Some(source.kind()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// One unit of work for a backend.
#[derive(Debug)]
pub enum BackendRequest {
    Manifest(ManifestRequest),
    Content(ContentRequest),
    Mutation(MutationRequest),
    Recall(RecallRequest),
    Health(HealthRequest),
}

/// Structure queries: what exists, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestRequest {
    /// Direct children of `path` (the vault root when `None`), directories first,
    /// then files, each group ordered by vault-relative path.
    ListChildren {
        path: Option<String>,
        include_hidden: bool,
        include_ignored: bool,
    },
    /// Every markdown file in the vault, sorted, ignoring hidden and ignored dirs.
    WalkMarkdown,
    /// Visible top-level folders, sorted.
    TopLevelFolders,
}

/// Content reads.
///
/// The flavour of error each variant produces is part of the public contract; see
/// [`BackendError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentRequest {
    /// Read a note as UTF-8 text. Errors are [`BackendError::Vault`]-flavoured.
    ReadText { path: String },
    /// Read raw bytes. IO errors are [`BackendError::Io`]-flavoured (bare).
    ReadBytes { path: String },
    /// Size metadata only. IO errors are [`BackendError::Io`]-flavoured (bare).
    Stat { path: String },
    /// Validate that `path` is an acceptable vault-relative path, without reading
    /// anything. Used by the upload mint, which must reject traversal before it
    /// issues a capability token. Returns no location, so nothing provider-specific
    /// escapes.
    ResolvePath { path: String },
}

/// What the caller observed about a write destination before composing the write.
///
/// # Why this exists
///
/// The MCP layer's `expectedHash` guard is a read, a comparison, and then a write
/// (see the write tools in the server's `tools` module). On a filesystem vault the
/// gap between the comparison and the write is a tolerated race: the write is a
/// `rename` that cannot fail, so a concurrent editor is simply overwritten. That is
/// frozen behaviour and this type does not change it.
///
/// On a backend that versions its documents the gap is closable, and refusing to
/// close it would mean the `expectedHash` contract is weaker than the storage
/// underneath it allows. So a read may hand back an OPAQUE version token, the caller
/// carries it to the write, and the backend turns it into a storage-level
/// precondition. The token is never parsed, compared or logged as content by any
/// caller — it is only ever handed back.
///
/// The three variants are distinct on purpose: `Unobserved` and `Absent` would
/// collapse into one `None` and a caller whose read merely FAILED would silently get
/// create-only semantics.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BaseVersion {
    /// The caller states no precondition — it did not read, or its read failed for a
    /// reason other than "nothing is there". The backend may still guard against its
    /// own most recent observation, but it must not infer that the path is free.
    #[default]
    Unobserved,
    /// The caller read the destination and found NOTHING there. A backend that can
    /// express it should make this a create-only write, so a concurrent create is
    /// reported rather than clobbered.
    Absent,
    /// The caller read the destination at this opaque version.
    Version(String),
}

impl BaseVersion {
    /// The observed version, if the caller observed one.
    pub fn as_version(&self) -> Option<&str> {
        match self {
            BaseVersion::Version(version) => Some(version),
            _ => None,
        }
    }

    /// Build from a read's optional version token: an empty or absent token is
    /// `Unobserved`, never `Absent`. A backend that mints no versions therefore
    /// keeps exactly its old semantics.
    pub fn from_read(version: Option<String>) -> Self {
        match version {
            Some(version) if !version.is_empty() => BaseVersion::Version(version),
            _ => BaseVersion::Unobserved,
        }
    }
}

/// Writes.
#[derive(Debug)]
pub enum MutationRequest {
    /// Create or overwrite a text file, creating parent directories as needed and
    /// refusing protected template folders.
    ///
    /// `content` is the COMPLETE new content: composition (section replacement,
    /// manual-note preservation, frontmatter) happens above this boundary, as does
    /// the `expectedHash` check. `base_version` carries what that check observed;
    /// see [`BaseVersion`]. A backend with no version concept ignores it.
    WriteText {
        path: String,
        content: String,
        base_version: BaseVersion,
    },
    /// Land a byte stream at `path` atomically.
    ///
    /// The backend owns the entire mechanic — staging location, incremental hashing,
    /// the byte budget, the optimistic-concurrency re-read, and the atomic swap —
    /// because "write to a sibling temp file, then rename" is precisely the
    /// filesystem-specific detail that must not leak past this boundary. A remote
    /// backend would implement the same request with a multipart session and a
    /// commit call.
    ///
    /// `chunks` is pulled synchronously on a blocking thread; `max_bytes` is
    /// enforced *during* streaming so an oversize body never lands.
    CommitUploadStream {
        path: String,
        expected_hash: Option<String>,
        max_bytes: usize,
        chunks: UploadChunks,
    },
    /// Best-effort removal of staging artifacts orphaned by a killed process.
    /// Never fails: housekeeping, not a contract.
    SweepOrphanStagingFiles,
}

/// A synchronous, `Send` source of upload chunks.
///
/// Boxed rather than generic so [`MutationRequest`] stays a plain data enum that an
/// object-safe `execute` can accept.
pub struct UploadChunks(Box<dyn Iterator<Item = Result<Vec<u8>, String>> + Send>);

impl UploadChunks {
    pub fn new<I>(chunks: I) -> Self
    where
        I: Iterator<Item = Result<Vec<u8>, String>> + Send + 'static,
    {
        Self(Box::new(chunks))
    }

    /// Consume the source, yielding the raw iterator.
    pub fn into_inner(self) -> Box<dyn Iterator<Item = Result<Vec<u8>, String>> + Send> {
        self.0
    }
}

impl std::fmt::Debug for UploadChunks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UploadChunks(..)")
    }
}

/// Search that the backend performs itself rather than the server's index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallRequest {
    /// Literal or regex line search.
    ///
    /// Context lines are produced *inside* this request, not by follow-up content
    /// reads: one round trip returns matches already carrying their surrounding
    /// lines. A backend whose search is remote can then satisfy `context_lines`
    /// however it likes without the server issuing N extra reads.
    Grep {
        query: String,
        regex: bool,
        case_sensitive: bool,
        glob: Option<String>,
        context_lines: usize,
        limit: usize,
    },
}

/// Liveness probes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthRequest {
    /// Is the vault reachable right now? Errors with the backend's own
    /// unreachability wording, so a caller can use this as a startup gate.
    Overview,
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// The result of a [`BackendRequest`], mirroring its shape.
#[derive(Debug)]
pub enum BackendResponse {
    Manifest(ManifestResponse),
    Content(ContentResponse),
    Mutation(MutationResponse),
    Recall(RecallResponse),
    Health(HealthResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestResponse {
    Children(Vec<VaultChildEntry>),
    MarkdownFiles(Vec<String>),
    Folders(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentResponse {
    Text {
        text: String,
        /// The opaque version this text was read at, for a backend that versions
        /// its documents. `None` on a backend that does not — which is what keeps
        /// the filesystem's read-then-write race exactly as it was.
        ///
        /// A caller that is about to write the note back should carry this into
        /// [`MutationRequest::WriteText`]; see [`BaseVersion`].
        version: Option<String>,
    },
    Bytes(Vec<u8>),
    Stat {
        size_bytes: u64,
    },
    PathAccepted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationResponse {
    Written {
        created: bool,
    },
    UploadCommitted {
        created: bool,
        bytes_written: usize,
        hash: String,
    },
    Swept,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallResponse {
    Grep(Vec<GrepMatch>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthResponse {
    Overview { reachable: bool },
}

/// One line-level search hit, with its context already attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line_number: usize,
    pub submatches: Vec<GrepSubmatch>,
    pub line_text: String,
    pub context_before: Vec<GrepContextLine>,
    pub context_after: Vec<GrepContextLine>,
}

/// Byte offsets of one match inside its line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepSubmatch {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// A line adjacent to a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepContextLine {
    pub line_number: usize,
    pub line_text: String,
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// A vault, behind three entry points.
#[async_trait::async_trait]
pub trait VaultBackend: Send + Sync {
    /// What this backend is and what it can do.
    fn descriptor(&self) -> BackendDescriptor;

    /// Perform one unit of work.
    ///
    /// The response variant always mirrors the request family; a caller that asked
    /// for `Content` receives `Content`. Unwrap with the `BackendResponse::into_*`
    /// helpers, and treat a family mismatch as a bug rather than a runtime condition.
    async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, BackendError>;

    /// Subscribe to change notifications.
    ///
    /// `after` resumes from a previously minted cursor when the backend supports
    /// replay; backends without replay ignore it and deliver only changes observed
    /// from subscription onward.
    fn changes(&self, after: Option<OpaqueCursor>) -> ChangeStream;

    /// Vault-relative paths whose stored content has unreconciled sibling versions,
    /// sorted; or `None` when this backend's storage has no such notion.
    ///
    /// # Why `Option` rather than an empty list
    ///
    /// `None` and `Some(vec![])` are different facts and a caller needs to tell them
    /// apart. `Some(vec![])` means "this vault CAN hold conflicting versions and holds
    /// none right now" — worth reporting. `None` means the question does not apply: a
    /// filesystem has exactly one version of a file by construction, so answering
    /// "zero conflicts" would invite a reader to conclude a check was performed when
    /// none was possible.
    ///
    /// The default is therefore `None`, and it is a statement about the storage model
    /// rather than an unimplemented stub. A backend whose writes are last-writer-wins
    /// never has a losing version to report.
    async fn conflicted_paths(&self) -> Result<Option<Vec<String>>, BackendError> {
        Ok(None)
    }

    /// This backend as a CouchDB vault, when it is one.
    ///
    /// # Why this exists rather than more trait methods
    ///
    /// The `couchdb export` / `couchdb restore` commands are provider-specific by
    /// definition — they exist because a LiveSync vault is the one vault a user cannot
    /// back up with `cp -r`, and they speak in revisions and entry kinds. Expressing
    /// them as `VaultBackend` methods would put "export yourself to a directory" on the
    /// filesystem backend, where it means nothing.
    ///
    /// A narrow, explicitly-named accessor is the honest shape: everything the SERVER
    /// does stays provider-agnostic through `execute`, and exactly one CLI command pair
    /// admits that it is talking to CouchDB. The default is `None`, so a new backend
    /// gets the right answer without writing anything.
    fn as_couchdb(&self) -> Option<&couchdb::CouchDbVaultBackend> {
        None
    }
}

// ---------------------------------------------------------------------------
// Response unwrapping
// ---------------------------------------------------------------------------

/// Message used when a backend answers with the wrong response family. This is a
/// backend bug, not a user-facing condition, so it is never expected to surface.
fn mismatch(expected: &str, got: &BackendResponse) -> BackendError {
    BackendError::Message(format!(
        "backend returned a {} response for a {expected} request",
        match got {
            BackendResponse::Manifest(_) => "manifest",
            BackendResponse::Content(_) => "content",
            BackendResponse::Mutation(_) => "mutation",
            BackendResponse::Recall(_) => "recall",
            BackendResponse::Health(_) => "health",
        }
    ))
}

impl BackendResponse {
    /// Text from a [`ContentRequest::ReadText`], discarding any version token.
    pub fn into_text(self) -> Result<String, BackendError> {
        self.into_versioned_text().map(|(text, _)| text)
    }

    /// Text from a [`ContentRequest::ReadText`] together with the opaque version it
    /// was read at. The pair a caller needs to write the note back safely.
    pub fn into_versioned_text(self) -> Result<(String, Option<String>), BackendError> {
        match self {
            BackendResponse::Content(ContentResponse::Text { text, version }) => {
                Ok((text, version))
            }
            other => Err(mismatch("read-text", &other)),
        }
    }

    /// Bytes from a [`ContentRequest::ReadBytes`].
    pub fn into_bytes(self) -> Result<Vec<u8>, BackendError> {
        match self {
            BackendResponse::Content(ContentResponse::Bytes(bytes)) => Ok(bytes),
            other => Err(mismatch("read-bytes", &other)),
        }
    }

    /// Size from a [`ContentRequest::Stat`].
    pub fn into_size_bytes(self) -> Result<u64, BackendError> {
        match self {
            BackendResponse::Content(ContentResponse::Stat { size_bytes }) => Ok(size_bytes),
            other => Err(mismatch("stat", &other)),
        }
    }

    /// Entries from a [`ManifestRequest::ListChildren`].
    pub fn into_children(self) -> Result<Vec<VaultChildEntry>, BackendError> {
        match self {
            BackendResponse::Manifest(ManifestResponse::Children(entries)) => Ok(entries),
            other => Err(mismatch("list-children", &other)),
        }
    }

    /// Paths from a [`ManifestRequest::WalkMarkdown`].
    pub fn into_markdown_files(self) -> Result<Vec<String>, BackendError> {
        match self {
            BackendResponse::Manifest(ManifestResponse::MarkdownFiles(files)) => Ok(files),
            other => Err(mismatch("walk-markdown", &other)),
        }
    }

    /// Folders from a [`ManifestRequest::TopLevelFolders`].
    pub fn into_folders(self) -> Result<Vec<String>, BackendError> {
        match self {
            BackendResponse::Manifest(ManifestResponse::Folders(folders)) => Ok(folders),
            other => Err(mismatch("top-level-folders", &other)),
        }
    }

    /// Matches from a [`RecallRequest::Grep`].
    pub fn into_grep_matches(self) -> Result<Vec<GrepMatch>, BackendError> {
        match self {
            BackendResponse::Recall(RecallResponse::Grep(matches)) => Ok(matches),
            other => Err(mismatch("grep", &other)),
        }
    }

    /// Outcome of a [`MutationRequest::CommitUploadStream`].
    pub fn into_upload_outcome(self) -> Result<UploadOutcome, BackendError> {
        match self {
            BackendResponse::Mutation(MutationResponse::UploadCommitted {
                created,
                bytes_written,
                hash,
            }) => Ok(UploadOutcome {
                created,
                bytes_written,
                hash,
            }),
            other => Err(mismatch("commit-upload-stream", &other)),
        }
    }
}

/// What a committed upload landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadOutcome {
    pub created: bool,
    pub bytes_written: usize,
    pub hash: String,
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

impl BackendRequest {
    pub fn read_text(path: impl Into<String>) -> Self {
        BackendRequest::Content(ContentRequest::ReadText { path: path.into() })
    }

    pub fn read_bytes(path: impl Into<String>) -> Self {
        BackendRequest::Content(ContentRequest::ReadBytes { path: path.into() })
    }

    pub fn stat(path: impl Into<String>) -> Self {
        BackendRequest::Content(ContentRequest::Stat { path: path.into() })
    }

    pub fn resolve_path(path: impl Into<String>) -> Self {
        BackendRequest::Content(ContentRequest::ResolvePath { path: path.into() })
    }

    /// A write with no observed precondition. The shape every pre-existing call site
    /// wants, and the shape a filesystem-only caller will always want.
    pub fn write_text(path: impl Into<String>, content: impl Into<String>) -> Self {
        BackendRequest::write_text_guarded(path, content, BaseVersion::Unobserved)
    }

    /// A write carrying what the caller observed at the destination. See
    /// [`BaseVersion`].
    pub fn write_text_guarded(
        path: impl Into<String>,
        content: impl Into<String>,
        base_version: BaseVersion,
    ) -> Self {
        BackendRequest::Mutation(MutationRequest::WriteText {
            path: path.into(),
            content: content.into(),
            base_version,
        })
    }

    pub fn walk_markdown() -> Self {
        BackendRequest::Manifest(ManifestRequest::WalkMarkdown)
    }

    pub fn top_level_folders() -> Self {
        BackendRequest::Manifest(ManifestRequest::TopLevelFolders)
    }

    pub fn list_children(
        path: Option<String>,
        include_hidden: bool,
        include_ignored: bool,
    ) -> Self {
        BackendRequest::Manifest(ManifestRequest::ListChildren {
            path,
            include_hidden,
            include_ignored,
        })
    }

    pub fn health_overview() -> Self {
        BackendRequest::Health(HealthRequest::Overview)
    }

    pub fn sweep_orphan_staging_files() -> Self {
        BackendRequest::Mutation(MutationRequest::SweepOrphanStagingFiles)
    }
}
