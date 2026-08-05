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

pub mod algolia;
pub mod couchdb;
pub mod filesystem;
pub mod grep;
pub mod router;
pub mod sidecar;
pub mod virtual_grep;
pub mod watch;

#[cfg(test)]
mod memory;

#[cfg(test)]
mod contract;

pub use algolia::{
    AlgoliaCredentials, AlgoliaOptions, AlgoliaVaultBackend, ALGOLIA_API_KEY_ENV,
    ALGOLIA_NO_BINARY_MESSAGE, ALGOLIA_NO_UPLOAD_MESSAGE, ALGOLIA_READ_ONLY_MESSAGE,
};
pub use couchdb::{
    CouchDbVaultBackend, EntryContent, COUCHDB_NATIVE_RECALL_UNSUPPORTED_MESSAGE,
    COUCHDB_READ_ONLY_MESSAGE, COUCHDB_SOFT_DELETE_UNSUPPORTED_MESSAGE,
    COUCHDB_VERSION_HISTORY_UNSUPPORTED_MESSAGE, UPLOAD_COLLECT_ADVISORY_BYTES,
};
pub use deep_obsidian_core::vault::{VaultChildEntry, VaultEntryKind, VaultError};
pub use filesystem::{
    FilesystemVaultBackend, FILESYSTEM_NATIVE_RECALL_UNSUPPORTED_MESSAGE,
    FILESYSTEM_SOFT_DELETE_UNSUPPORTED_MESSAGE, FILESYSTEM_VERSION_HISTORY_UNSUPPORTED_MESSAGE,
};
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
    /// A shared, Markdown-only corpus stored as records in an Algolia index. Has no
    /// local copy of its content and no local search index — see
    /// [`algolia::AlgoliaVaultBackend`].
    Algolia,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Filesystem => "filesystem",
            BackendKind::InMemory => "in-memory",
            BackendKind::Couchdb => "couchdb",
            BackendKind::Algolia => "algolia",
        }
    }
}

/// A discrete thing a backend can do. Absent capability means the server must not
/// advertise or attempt the corresponding operation.
///
/// `GrepSearch` is the capability that today's `rg_available` flag becomes: the
/// `grep_search` tool is advertised if and only if it is present.
///
/// # Declaration order is serialization order
///
/// A [`BackendDescriptor`]'s capabilities live in a `BTreeSet`, whose ordering is this
/// enum's derived `Ord` — i.e. declaration order. That order reaches a client verbatim
/// through `vault_info.mounts[].capabilities`, so a variant inserted in the MIDDLE
/// would reorder an array a test (and a reader) already depends on. New capabilities
/// are therefore APPENDED, and the three below are grouped after the storage-shaped
/// ones because they describe what a backend can *answer*, not what it can store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    GrepSearch,
    BinaryRead,
    BinaryWrite,
    Upload,
    Watch,
    /// The backend answers [`RecallRequest::Search`] itself.
    ///
    /// Present only on a backend whose own storage IS a search index, so there is no
    /// local index above it to rank over. The server's scoped recall tools serve such
    /// a mount through the backend instead of refusing it for having no local index —
    /// see the server's `tools` module.
    ///
    /// It is emphatically NOT a claim of parity with the local index: the hits carry
    /// no semantic/BM25 breakdown, their scores are ordinal (see [`RecallHit::score`]),
    /// and the response says which recall stage produced them
    /// ([`RecallSearchResponse::recall_mode`]).
    NativeRecall,
    /// The backend keeps superseded versions of a note and can enumerate them
    /// ([`ManifestRequest::Versions`]) and read one back
    /// ([`ContentRequest::ReadText`]'s `version`).
    ///
    /// Absent on last-writer-wins storage, where there is exactly one version of a
    /// note by construction and "list its history" has no answer — not an empty one.
    VersionHistory,
    /// The backend can remove a note in a way that is OBSERVABLE and RECOVERABLE
    /// ([`MutationRequest::SoftDelete`]): the removal leaves a tombstone other readers
    /// see, and the content stays reachable through the version history.
    ///
    /// Deliberately not "can delete": a plain unlink is not this capability. The MCP
    /// surface has never exposed destructive local file removal and must not gain it
    /// by a backend advertising this for a `remove_file`.
    SoftDelete,
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
    /// Every retained version of one note, newest first.
    ///
    /// A MANIFEST request rather than a content one: it enumerates what exists and
    /// carries no bodies. Reading one of the versions it names is
    /// [`ContentRequest::ReadText`] with a `version`.
    ///
    /// Answered only by a backend advertising [`Capability::VersionHistory`]. Every
    /// other backend refuses, because "this note has one version" and "this storage
    /// keeps no history" are different facts and an empty-or-single-entry list would
    /// conflate them.
    Versions { path: String },
}

/// Content reads.
///
/// The flavour of error each variant produces is part of the public contract; see
/// [`BackendError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentRequest {
    /// Read a note as UTF-8 text. Errors are [`BackendError::Vault`]-flavoured.
    ///
    /// `version` selects a specific, possibly SUPERSEDED version instead of the
    /// current one. `None` is today's behaviour on every backend, verbatim; `Some` is
    /// refused by every backend that does not advertise
    /// [`Capability::VersionHistory`].
    ///
    /// # A versioned read must never feed a write's precondition
    ///
    /// [`ContentResponse::Text`]'s `version` is the version that was ACTUALLY read, so
    /// for a versioned read it echoes the request rather than naming the head. Feeding
    /// it into [`BaseVersion`] would tell the backend "I observed the destination at
    /// v3" when v3 is history and the head has moved — manufacturing a stale
    /// precondition out of a deliberate historical read. Callers that read in order to
    /// write must use the UNVERSIONED read; the server's `backend_read_note_for_write`
    /// does.
    ReadText {
        path: String,
        version: Option<String>,
    },
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
    ///
    /// `resolve_divergence` is the caller stating that `content` is the RECONCILIATION
    /// of a recorded divergence, so a backend that marks diverged notes may clear the
    /// mark. It is a claim about the content, which only the caller can make — the
    /// storage cannot tell a merge from any other overwrite — and it is honoured only
    /// when this write does not itself fork. A backend with no divergence concept
    /// ignores it, as it ignores `base_version`.
    WriteText {
        path: String,
        content: String,
        base_version: BaseVersion,
        resolve_divergence: bool,
    },
    /// Remove a note OBSERVABLY and RECOVERABLY.
    ///
    /// Not "delete the file". The contract is that after this the note is absent from
    /// every read and listing, that other participants can tell it was removed rather
    /// than merely find it missing, and that its last content is still reachable
    /// through the version history. A backend that cannot promise all three must
    /// refuse rather than approximate it with an unlink — see [`Capability::SoftDelete`].
    SoftDelete { path: String },
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
    /// Ranked relevance search performed BY the backend.
    ///
    /// The one recall request whose absence is the norm: on a filesystem or couchdb
    /// mount the server owns a local SQLite index and ranks over it, so a backend-side
    /// search would be a second, worse answer. This exists for the inverted
    /// arrangement — a backend whose storage already IS a search index shared by
    /// several participants, where building a local copy would rank one participant's
    /// stale snapshot. See [`Capability::NativeRecall`].
    Search(SearchRequest),
}

/// A ranked search over one backend's own index.
///
/// # Why there is no folder filter
///
/// Derived from the call sites, like every other variant. The server's scoped recall
/// tools require `scope` to name a MOUNT ROOT exactly — a narrower scope is refused
/// above this boundary, because these tools truncate to `limit` and honouring a
/// subtree filter after truncation would silently return fewer results than asked for.
/// So by the time a request reaches here the mount IS the scope, and a filter field
/// would have exactly one legal value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub query: String,
    /// How many hits to return. A backend that groups by note (rather than by chunk)
    /// applies it after grouping, so `limit` counts what the caller will see.
    pub limit: usize,
    /// Resume token from a previous [`RecallSearchResponse::next_cursor`]. `None`
    /// starts from the most relevant hit.
    pub cursor: Option<OpaqueCursor>,
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
    Children(ChildListing),
    MarkdownFiles(Vec<String>),
    Folders(Vec<String>),
    Versions(NoteHistory),
}

/// A directory listing, plus whether the SUBFOLDER half of it is complete.
///
/// # Why the flag is on the listing rather than logged
///
/// A backend that synthesizes folders from a facet enumeration has a hard ceiling on
/// how many it can name (Algolia answers 400 above 100 facet values rather than
/// clamping). A listing that quietly stopped at the ceiling would tell a client those
/// folders do not exist, which is the one failure mode a manifest must never have. 5b
/// could only `warn!` it because this variant carried a bare `Vec`; carrying it here is
/// what lets the MCP payload say so.
///
/// Only the FOLDERS can be short. Files come from a paginated record query, so
/// `folders_truncated` never means "some files are missing" — which is what makes the
/// flag actionable rather than a blanket "this might be wrong".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildListing {
    pub entries: Vec<VaultChildEntry>,
    pub folders_truncated: bool,
}

impl ChildListing {
    /// A listing that named every subfolder. The shape every backend whose directories
    /// are real directories returns, so the flag costs those backends nothing.
    pub fn exhaustive(entries: Vec<VaultChildEntry>) -> Self {
        Self {
            entries,
            folders_truncated: false,
        }
    }
}

/// One note's retained versions, newest first, plus whether the note is diverged.
///
/// `has_divergence` describes the NOTE (it is a property of its head), not any one
/// version, which is why it sits here rather than on [`NoteVersion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteHistory {
    pub has_divergence: bool,
    pub versions: Vec<NoteVersion>,
}

/// One version of a note, as its metadata records it.
///
/// Every field is `Option` except the four a version cannot exist without, because a
/// version's place in the graph is genuinely partial: the first version has no parent,
/// a version nobody has superseded has no successor, and only a fork has a
/// `forked_from`. Rendering an absent link as `null` rather than omitting the key is
/// the caller-visible contract (see the server's `note_history` payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteVersion {
    pub version_id: String,
    /// Who wrote it, in the corpus's own participant vocabulary.
    pub participant_id: String,
    pub updated_at_ms: u64,
    /// The version this one was based on.
    pub parent_version_id: Option<String>,
    /// The head this version DISPLACED, when it landed as a fork. Distinct from
    /// `parent_version_id`: the parent is where the content came from, this is what it
    /// overtook.
    pub forked_from: Option<String>,
    /// The version that superseded this one. `None` on the current version.
    pub superseded_by: Option<String>,
    /// True for exactly one entry: the version a plain read serves.
    pub current: bool,
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
    SoftDeleted {
        /// The tombstone's own version id.
        version_id: String,
        /// True when the note was ALREADY a tombstone, so this call changed nothing.
        /// Reported rather than turned into an error: deleting a deleted note is the
        /// caller's intent already satisfied, and failing it would make the operation
        /// non-idempotent for no gain.
        already_deleted: bool,
        /// The version still holding the content that was just removed. Feed it to a
        /// versioned read to recover it. `None` only when the storage could not name
        /// one, which is what makes the removal unrecoverable and worth surfacing.
        recoverable_from: Option<String>,
    },
    UploadCommitted {
        created: bool,
        bytes_written: usize,
        hash: String,
    },
    Swept,
}

/// `PartialEq` but NOT `Eq`, unlike its sibling response families: a
/// [`RecallHit::score`] is an `f64`, and claiming total equality over a type that can
/// hold a NaN would be a false promise for the sake of a derive nothing needs.
#[derive(Debug, Clone, PartialEq)]
pub enum RecallResponse {
    Grep(GrepOutcome),
    Search(RecallSearchResponse),
}

/// Line matches, plus whether they are ALL of them.
///
/// # Why exhaustiveness is a field and not an assumption
///
/// `grep_search` reads as exhaustive because ripgrep is: it opens every file. A backend
/// whose corpus lives behind a ranked query API cannot be — it prefilters candidates
/// lexically and evaluates the pattern over those — so its short result list looks
/// exactly like "there are no other matches" while meaning "I did not look everywhere".
/// 5b could only `warn!` that; this carries it, and the server emits it into the
/// payload ONLY when it is `false`, so an exhaustive backend's output is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepOutcome {
    pub matches: Vec<GrepMatch>,
    /// True when every line of the searched scope was examined.
    pub exhausted: bool,
    /// How many candidates a non-exhaustive search examined. `None` on an exhaustive
    /// one, where the number would describe nothing a caller can act on — and where
    /// reporting "candidates: 4021" alongside `exhausted: true` would invite the reader
    /// to think a cap was involved.
    pub candidate_count: Option<usize>,
    /// Mounts whose part of the search FAILED, on a federated (whole-vault) grep.
    ///
    /// # Why this is not folded into `exhausted`
    ///
    /// `exhausted: false` already means "I did not look everywhere", and a failed mount is
    /// certainly that — but it says nothing about WHICH lines were never read, so a caller
    /// cannot tell "the backend caps its candidate set" from "a third of your vault is
    /// offline right now". The two have different remedies (raise the bound; fix the mount),
    /// so the mount ids are carried rather than collapsed.
    ///
    /// Always empty for a single-mount vault and for any single-mount routed search, which
    /// is what keeps every existing grep payload byte-identical.
    pub missing_mounts: Vec<String>,
}

impl GrepOutcome {
    /// The outcome of a search that read everything. What ripgrep returns.
    pub fn exhaustive(matches: Vec<GrepMatch>) -> Self {
        Self {
            matches,
            exhausted: true,
            candidate_count: None,
            missing_mounts: Vec::new(),
        }
    }
}

/// Ranked hits from a backend's own index.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallSearchResponse {
    pub hits: Vec<RecallHit>,
    /// Resume token for the next page, when there is one.
    pub next_cursor: Option<OpaqueCursor>,
    /// True when this page is the last one. `false` together with a `next_cursor` is
    /// the ordinary "there is more"; `false` with no cursor means the backend knows it
    /// truncated but cannot offer a resume point, which is still worth saying.
    pub exhausted: bool,
    /// Which retrieval stage produced these hits. Reported rather than assumed: the
    /// same request against the same backend answers differently depending on the
    /// index's own configuration, and a caller weighing these hits against local ones
    /// needs to know which.
    pub recall_mode: RecallMode,
}

/// The retrieval stage a backend's index actually used.
///
/// Named for what the PROVIDER did, and biased towards under-claiming: a backend that
/// cannot determine its own mode reports [`RecallMode::Lexical`], because reporting a
/// weaker stage than was used is harmless while claiming a stronger one misleads a
/// caller into trusting the ranking more than it should.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecallMode {
    /// Token/keyword matching. The default claim.
    Lexical,
    /// A vector or hybrid neural stage, enabled on the index itself.
    Neural,
}

impl RecallMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RecallMode::Lexical => "lexical",
            RecallMode::Neural => "neural",
        }
    }
}

/// One hit from a backend's own index.
///
/// The fields are exactly what the server's `hybrid_search` payload can render for a
/// hit it did not produce locally — no more. In particular there is no
/// `semantic_score`/`bm25_score` pair: those are the local hybrid ranker's two input
/// signals, and inventing values for them would be the clearest possible lie about
/// where a hit came from.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    /// MOUNT-relative path. The router re-prefixes it into the logical namespace, the
    /// same way it does for a local index's paths.
    pub path: String,
    pub title: String,
    /// Relevance, descending, comparable only WITHIN this response.
    ///
    /// # Why this is ordinal
    ///
    /// A ranked search API returns an order, not a calibrated score — Algolia's ranking
    /// is a tie-break cascade with no numeric relevance in the default response. So this
    /// is derived from the hit's RANK (`1/(rank+1)`), which is honest about being
    /// ordinal: it preserves the order the provider chose and makes no claim of
    /// comparability against a cosine similarity or a BM25 value.
    /// [`RecallSearchResponse::recall_mode`] tells a caller what produced the order.
    pub score: f64,
    /// The matching passage. Named `snippet` rather than `text` because it is a fragment
    /// of the note by construction, not the note.
    pub snippet: String,
    pub chunk_index: usize,
    pub start_line: usize,
    pub end_line: usize,
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

    /// This backend as an Algolia vault, when it is one.
    ///
    /// The same bargain as [`VaultBackend::as_couchdb`], for the same reason. The
    /// `algolia` CLI family is provider-specific by definition: `seed` imports a local
    /// folder into an index, `dump`/`restore` are the backup-and-exit story for a corpus
    /// whose only copy lives in a search index, `retract` purges a note's history, and
    /// `key` derives a secured API key. Every one of those speaks Algolia — index names,
    /// version records, ACLs — and none of them means anything on a filesystem mount, so
    /// none of them belongs on the trait as a request family.
    ///
    /// What is NOT behind this accessor matters as much: everything the SERVER does with
    /// an Algolia mount still goes through `execute`, so the boundary stays the only way
    /// the MCP surface reaches any storage. This is the CLI's admission that it is
    /// talking to one specific provider, and the `None` default means a new backend gets
    /// the right answer for free.
    fn as_algolia(&self) -> Option<&algolia::AlgoliaVaultBackend> {
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

    /// Entries from a [`ManifestRequest::ListChildren`], discarding the completeness
    /// flag. The shape every caller that only renders entries wants.
    pub fn into_children(self) -> Result<Vec<VaultChildEntry>, BackendError> {
        self.into_child_listing().map(|listing| listing.entries)
    }

    /// Entries from a [`ManifestRequest::ListChildren`] together with whether the
    /// subfolder half is complete. See [`ChildListing`].
    pub fn into_child_listing(self) -> Result<ChildListing, BackendError> {
        match self {
            BackendResponse::Manifest(ManifestResponse::Children(listing)) => Ok(listing),
            other => Err(mismatch("list-children", &other)),
        }
    }

    /// History from a [`ManifestRequest::Versions`].
    pub fn into_note_history(self) -> Result<NoteHistory, BackendError> {
        match self {
            BackendResponse::Manifest(ManifestResponse::Versions(history)) => Ok(history),
            other => Err(mismatch("versions", &other)),
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

    /// Matches from a [`RecallRequest::Grep`], discarding the exhaustiveness report.
    pub fn into_grep_matches(self) -> Result<Vec<GrepMatch>, BackendError> {
        self.into_grep_outcome().map(|outcome| outcome.matches)
    }

    /// Matches from a [`RecallRequest::Grep`] together with whether they are all of
    /// them. See [`GrepOutcome`].
    pub fn into_grep_outcome(self) -> Result<GrepOutcome, BackendError> {
        match self {
            BackendResponse::Recall(RecallResponse::Grep(outcome)) => Ok(outcome),
            other => Err(mismatch("grep", &other)),
        }
    }

    /// Hits from a [`RecallRequest::Search`].
    pub fn into_recall_search(self) -> Result<RecallSearchResponse, BackendError> {
        match self {
            BackendResponse::Recall(RecallResponse::Search(response)) => Ok(response),
            other => Err(mismatch("recall-search", &other)),
        }
    }

    /// Outcome of a [`MutationRequest::SoftDelete`].
    pub fn into_soft_delete(self) -> Result<SoftDeleteOutcome, BackendError> {
        match self {
            BackendResponse::Mutation(MutationResponse::SoftDeleted {
                version_id,
                already_deleted,
                recoverable_from,
            }) => Ok(SoftDeleteOutcome {
                version_id,
                already_deleted,
                recoverable_from,
            }),
            other => Err(mismatch("soft-delete", &other)),
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

/// What a soft delete left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftDeleteOutcome {
    pub version_id: String,
    pub already_deleted: bool,
    pub recoverable_from: Option<String>,
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
    /// Read the CURRENT text of a note. The shape every pre-existing call site wants,
    /// and the only shape a caller that is about to write the note back may use — see
    /// [`ContentRequest::ReadText`].
    pub fn read_text(path: impl Into<String>) -> Self {
        BackendRequest::Content(ContentRequest::ReadText {
            path: path.into(),
            version: None,
        })
    }

    /// Read one specific, possibly superseded version of a note.
    pub fn read_text_version(path: impl Into<String>, version: impl Into<String>) -> Self {
        BackendRequest::Content(ContentRequest::ReadText {
            path: path.into(),
            version: Some(version.into()),
        })
    }

    /// Enumerate a note's retained versions.
    pub fn note_versions(path: impl Into<String>) -> Self {
        BackendRequest::Manifest(ManifestRequest::Versions { path: path.into() })
    }

    /// Remove a note observably and recoverably.
    pub fn soft_delete(path: impl Into<String>) -> Self {
        BackendRequest::Mutation(MutationRequest::SoftDelete { path: path.into() })
    }

    /// A ranked search served by the backend itself, from the most relevant hit.
    pub fn recall_search(query: impl Into<String>, limit: usize) -> Self {
        BackendRequest::recall_search_page(query, limit, None)
    }

    /// One PAGE of a backend-served ranked search, resuming from `cursor`.
    ///
    /// The paginated form exists for federated recall's deepening loop, which asks a
    /// native-recall mount for another page when its next unseen candidate could still
    /// enter the fused top-`limit`. A caller that only wants the best hits uses
    /// [`Self::recall_search`], which is this with no cursor.
    pub fn recall_search_page(
        query: impl Into<String>,
        limit: usize,
        cursor: Option<OpaqueCursor>,
    ) -> Self {
        BackendRequest::Recall(RecallRequest::Search(SearchRequest {
            query: query.into(),
            limit,
            cursor,
        }))
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
        BackendRequest::write_text_full(path, content, base_version, false)
    }

    /// A guarded write that additionally claims to RECONCILE a recorded divergence.
    /// See [`MutationRequest::WriteText`]'s `resolve_divergence`.
    pub fn write_text_full(
        path: impl Into<String>,
        content: impl Into<String>,
        base_version: BaseVersion,
        resolve_divergence: bool,
    ) -> Self {
        BackendRequest::Mutation(MutationRequest::WriteText {
            path: path.into(),
            content: content.into(),
            base_version,
            resolve_divergence,
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
