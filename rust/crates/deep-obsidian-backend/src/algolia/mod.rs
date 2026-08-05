//! A shared, Markdown-only corpus stored as records in an Algolia index, behind the
//! boundary.
//!
//! Everything here is a translation between two vocabularies: the [`VaultBackend`]
//! request families, shaped by the server's call sites, and a search index, shaped
//! by nothing of the kind. The interesting parts are where the two do not line up:
//!
//! * **The index IS the vault.** There is no local mirror of the corpus and no
//!   intention of building one — that is what lets several participants mount the
//!   same corpus at once. So there is no local search index either: scoped
//!   `hybrid_search` and friends refuse for this mount rather than answering from a
//!   copy that would be one participant's stale snapshot.
//! * **There are no directories.** Folders are synthesized from the hierarchical
//!   `folders.lvlN` facets, so `ListChildren` and `TopLevelFolders` are facet-value
//!   queries. Algolia caps facet enumeration at 100 values, which is a hard 400
//!   rather than a clamp, so a folder with more than 100 direct subfolders cannot be
//!   listed exhaustively; the shortfall is logged rather than silently absorbed.
//! * **Markdown only.** A note is one small `note` record plus one `chunk` record per
//!   chunk of its current version. Binary attachments have no record shape at all, so
//!   every binary path is refused with [`ALGOLIA_NO_BINARY_MESSAGE`] or
//!   [`ALGOLIA_NO_UPLOAD_MESSAGE`] — a fact about the storage, not a missing feature.
//! * **An index does not exist until its first write.** Every read against a corpus
//!   nobody has written answers `404 Index <name> does not exist`, which means "no
//!   records". [`empty_if_missing_index`] wraps every read site accordingly, and the
//!   index settings are applied lazily right after the first write.
//! * **Writes are asynchronous.** A write is queued, not applied, so the head-pointer
//!   push is AWAITED (`save_objects_awaited`): read-after-write is part of the
//!   behaviour contract every capture flow depends on.
//! * **A stale base does not fail — it forks.** See
//!   [`versioning::push_note_version`] for why this differs from the CouchDB mount's
//!   `VersionConflict`, and how it composes with the MCP `expectedHash` guard.
//! * **There is no change feed.** [`AlgoliaVaultBackend::changes`] returns an empty
//!   stream, so nothing advertises `Watch` and no auto-reindex pump waits on one.

pub mod cache;
pub mod grep;
pub mod reads;
pub mod recall;
pub mod records_build;
pub mod versioning;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use deep_obsidian_algolia::{AlgoliaClient, AlgoliaError};
use secrecy::{ExposeSecret, SecretString};
use tracing::warn;

use crate::watch::ChangeStream;
use crate::{
    BackendDescriptor, BackendError, BackendKind, BackendRequest, BackendResponse, BaseVersion,
    Capability, ChildListing, ContentRequest, ContentResponse, GrepOutcome, HealthRequest,
    HealthResponse, ManifestRequest, ManifestResponse, MutationRequest, MutationResponse,
    OpaqueCursor, RecallRequest, RecallResponse, VaultBackend,
};

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Refusal for every BINARY operation against an Algolia mount.
///
/// Long and specific for the same reason [`crate::COUCHDB_READ_ONLY_MESSAGE`] is: a
/// user reaching it has tried to read or store an attachment in a shared wiki and
/// needs to know that this is a property of the storage rather than a bug, a
/// permission problem, or something a setting turns on. Note what it must NOT say —
/// nothing about `writable`, because no configuration change makes an Algolia index
/// hold a PDF.
pub const ALGOLIA_NO_BINARY_MESSAGE: &str = "this mount is an EXPERIMENTAL Algolia-backed shared \
corpus, which stores MARKDOWN ONLY: a note is a small metadata record plus one record per text \
chunk, and there is no record shape for binary content, so no attachment can be read from or \
written to it. This is a property of the storage, not a setting — no configuration makes it hold \
binary files. Keep attachments on a filesystem mount and link to them from the shared note, or \
read them from the mount that actually stores them.";

/// Refusal for minting an out-of-band UPLOAD against an Algolia mount.
///
/// Separate from [`ALGOLIA_NO_BINARY_MESSAGE`] because it is answered at a different
/// moment and has a different remedy. The upload endpoint's whole purpose is landing
/// bytes the MCP transport should not carry, and it asks the backend to validate the
/// destination path before issuing a capability token. Refusing THERE is what makes
/// `request_vault_upload` fail with an explanation instead of handing out a token
/// whose `PUT` would fail minutes later, when the user has already streamed the body.
pub const ALGOLIA_NO_UPLOAD_MESSAGE: &str = "request_vault_upload cannot target this mount: it is \
an EXPERIMENTAL Algolia-backed shared corpus, which stores MARKDOWN ONLY, so there is no \
destination for an uploaded file and no token is issued (refusing at the mint is deliberate — a \
token would only fail after you had already uploaded the body). Upload the file to a filesystem \
mount and link to it from the shared note. Markdown reaches this mount through upsert_note.";

/// Refusal for every write against a READ-ONLY Algolia mount.
///
/// Mirrors [`crate::COUCHDB_READ_ONLY_MESSAGE`]'s shape and discipline: it names the
/// setting that lifts it, attributes itself to THIS MOUNT'S CONFIGURATION rather than
/// to a missing implementation, and points at what does work instead.
pub const ALGOLIA_READ_ONLY_MESSAGE: &str = "this mount is an EXPERIMENTAL, READ-ONLY \
Algolia-backed shared corpus: it is read-only because its mount configuration does not set \
'writable', so no write can reach the shared index. To allow writes, set \"writable\": true on this \
mount (it additionally requires experimental.algoliaVaults, which is already on if this mount \
loaded) and restart the service; writes then append a new version and record a divergence if \
another participant got there first. Otherwise write to a filesystem mount instead.";

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Default byte budget for the hydrated-note cache.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Default retention floor: the N most recent versions of a note are always kept.
pub const DEFAULT_RETENTION_MIN_VERSIONS: usize = 5;
/// Default retention ceiling: anything younger than this is kept regardless of rank.
pub const DEFAULT_RETENTION_MAX_AGE_DAYS: u64 = 90;

/// Environment override for the Algolia API key.
///
/// Kept from PR #40 for compatibility with the container and demo setups that rely on
/// it. It takes PRECEDENCE over the configured `apiKeyRef`, which is the footgun a
/// caller must be warned about — see `crate::algolia::resolve_api_key`'s counterpart
/// in the server's `mounts` module, which logs when the environment shadows a
/// configured reference.
pub const ALGOLIA_API_KEY_ENV: &str = "DEEP_OBSIDIAN_ALGOLIA_API_KEY";

/// Subdirectory of a mount's index dir holding the hydrated-note cache.
const CACHE_DIR_SEGMENT: &str = "algolia-cache";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Everything the backend needs to connect, with the key already resolved.
///
/// Mirrors [`crate::sidecar::SidecarCredentials`]: the plaintext secret arrives here
/// as a [`SecretString`] and never leaves as anything else, so a `{:?}` on this
/// struct — or on anything holding one — cannot print it.
pub struct AlgoliaCredentials {
    pub app_id: String,
    pub index_name: String,
    pub api_key: SecretString,
    pub base_url: Option<String>,
}

impl std::fmt::Debug for AlgoliaCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlgoliaCredentials")
            .field("app_id", &self.app_id)
            .field("index_name", &self.index_name)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// The knobs that are not credentials.
#[derive(Debug, Clone, Default)]
pub struct AlgoliaOptions {
    pub writable: bool,
    /// Who this participant is in the corpus's audit trail. `None` defaults to
    /// [`default_participant_id`].
    pub participant_id: Option<String>,
    pub cache_max_bytes: Option<u64>,
    pub cache_pinned_prefixes: Vec<String>,
    pub retention_min_versions: Option<usize>,
    pub retention_max_age_days: Option<u64>,
}

/// The participant id used when the mount declares none.
///
/// `<user>@unknown` rather than just the user name: the id lands in every record this
/// mount writes and is read by OTHER participants, so a bare `paul` would be
/// ambiguous across machines. The `@unknown` suffix says out loud that the host part
/// was never configured, which is exactly the prompt a user needs to set a real one.
pub fn default_participant_id() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "participant".to_string());
    format!("{user}@unknown")
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// A shared Markdown corpus in an Algolia index. Read-only unless configured
/// `writable`.
pub struct AlgoliaVaultBackend {
    client: AlgoliaClient,
    index_name: String,
    history_index: String,
    writable: bool,
    participant_id: String,
    cache: cache::NoteCache,
    retention: (usize, u64),
    /// Set once the MAIN index's settings have been applied. Under mount-only
    /// authorship nothing else provisions it: the index is created by the first mount
    /// write, and without settings its facets, `distinct` and searchable attributes
    /// are all wrong (a facet query fails outright).
    main_provisioned: AtomicBool,
    /// Same, for the history index — which exists only after a note is first
    /// superseded, so provisioning is necessarily lazier still.
    history_provisioned: AtomicBool,
    /// Set once a search has confirmed the index runs Algolia NeuralSearch.
    ///
    /// Deliberately one-way, and deliberately not the inverse: see
    /// [`recall::recall_mode`] for why a confirmed neural stage is cached while a
    /// lexical one is re-detected every time.
    pub(crate) neural_recall_confirmed: AtomicBool,
}

/// Hand-written so no credential can be printed, even transitively.
impl std::fmt::Debug for AlgoliaVaultBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlgoliaVaultBackend")
            .field("app_id", &self.client.app_id())
            .field("index_name", &self.index_name)
            .field("history_index", &self.history_index)
            .field("writable", &self.writable)
            .field("participant_id", &self.participant_id)
            .finish_non_exhaustive()
    }
}

impl AlgoliaVaultBackend {
    /// Connect a backend.
    ///
    /// Performs NO IO against Algolia: the first request is whatever the server asks
    /// for. That is what lets a mount whose index has never been written — or whose
    /// key has been rotated — still be constructed, so the server can report it
    /// rather than refusing to start.
    ///
    /// `index_dir` is the mount's own directory; the hydrated-note cache lives in a
    /// subdirectory of it named after the index, so two mounts sharing an index dir
    /// cannot share a cache.
    pub fn connect(
        credentials: AlgoliaCredentials,
        options: AlgoliaOptions,
        index_dir: &Path,
    ) -> Result<Self, BackendError> {
        let client = AlgoliaClient::new(
            &credentials.app_id,
            credentials.api_key.expose_secret(),
            credentials.base_url.as_deref(),
        );
        let history_index = history_index_name(&credentials.index_name);
        let cache = cache::NoteCache::open(
            cache_dir(index_dir, &credentials.index_name),
            options.cache_max_bytes.unwrap_or(DEFAULT_CACHE_MAX_BYTES),
            options.cache_pinned_prefixes.clone(),
        )
        .map_err(BackendError::io)?;
        Ok(Self {
            client,
            index_name: credentials.index_name,
            history_index,
            writable: options.writable,
            participant_id: options
                .participant_id
                .clone()
                .unwrap_or_else(default_participant_id),
            cache,
            retention: (
                options
                    .retention_min_versions
                    .unwrap_or(DEFAULT_RETENTION_MIN_VERSIONS),
                options
                    .retention_max_age_days
                    .unwrap_or(DEFAULT_RETENTION_MAX_AGE_DAYS),
            ),
            main_provisioned: AtomicBool::new(false),
            history_provisioned: AtomicBool::new(false),
            neural_recall_confirmed: AtomicBool::new(false),
        })
    }

    pub(crate) fn client(&self) -> &AlgoliaClient {
        &self.client
    }

    pub fn index(&self) -> &str {
        &self.index_name
    }

    pub fn history_index(&self) -> &str {
        &self.history_index
    }

    pub(crate) fn cache(&self) -> &cache::NoteCache {
        &self.cache
    }

    pub fn participant_id(&self) -> &str {
        &self.participant_id
    }

    /// `(min_versions, max_age_days)`.
    pub fn retention(&self) -> (usize, u64) {
        self.retention
    }

    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Apply the main index's settings, once per process.
    pub(crate) async fn ensure_main_settings(&self) {
        ensure_index_settings(
            &self.client,
            &self.index_name,
            &self.main_provisioned,
            deep_obsidian_algolia::records::main_index_settings(),
        )
        .await;
    }

    /// Apply the history index's settings, once per process.
    pub(crate) async fn ensure_history_settings(&self) {
        ensure_index_settings(
            &self.client,
            &self.history_index,
            &self.history_provisioned,
            deep_obsidian_algolia::records::history_index_settings(),
        )
        .await;
    }

    /// Refuse every mutation on a read-only mount, with the message that names why.
    fn ensure_writable(&self) -> Result<(), BackendError> {
        if self.writable {
            return Ok(());
        }
        Err(BackendError::Unsupported(
            ALGOLIA_READ_ONLY_MESSAGE.to_string(),
        ))
    }

    async fn manifest(&self, request: ManifestRequest) -> Result<ManifestResponse, BackendError> {
        match request {
            ManifestRequest::ListChildren {
                path,
                include_hidden,
                include_ignored,
            } => {
                let (entries, folders_truncated) =
                    reads::list_children(self, path.as_deref(), include_hidden, include_ignored)
                        .await?;
                if folders_truncated {
                    // Still logged, and now ALSO carried: the log is for the operator
                    // (it names the index and the cap), the flag is for the caller (it
                    // reaches the `list_children` payload as `foldersTruncated`). Keeping
                    // both is deliberate — an agent acting on the listing needs the flag,
                    // and whoever has to raise the cap needs the log line.
                    warn!(
                        "listing {:?} on Algolia index '{}' hit the {}-value facet-enumeration \
                         cap, so this listing may be missing subfolders; the files listed are \
                         complete",
                        path.as_deref().unwrap_or(""),
                        self.index_name,
                        AlgoliaClient::MAX_FACET_HITS
                    );
                }
                Ok(ManifestResponse::Children(ChildListing {
                    entries,
                    folders_truncated,
                }))
            }
            ManifestRequest::WalkMarkdown => Ok(ManifestResponse::MarkdownFiles(
                reads::walk_markdown(self).await?,
            )),
            ManifestRequest::TopLevelFolders => Ok(ManifestResponse::Folders(
                reads::top_level_folders(self).await?,
            )),
            ManifestRequest::Versions { path } => {
                ensure_vault_relative(&path)?;
                Ok(ManifestResponse::Versions(
                    versioning::note_history(self, &path).await?,
                ))
            }
        }
    }

    async fn content(&self, request: ContentRequest) -> Result<ContentResponse, BackendError> {
        match request {
            // A specific, possibly superseded version. Served from the main index first
            // and the history index second, in that order, because the CURRENT version's
            // chunks live in main — a caller naming the head's version id must not be
            // told the version does not exist.
            ContentRequest::ReadText {
                path,
                version: Some(version),
            } => {
                ensure_vault_relative(&path)?;
                Ok(ContentResponse::Text {
                    text: reads::read_note_version(self, &path, &version).await?,
                    // Echoes the request: this is the version that was READ, which for a
                    // versioned read is not the head. See `ContentRequest::ReadText` for
                    // why that must never become a write's precondition.
                    version: Some(version),
                })
            }
            ContentRequest::ReadText { path, .. } => {
                ensure_vault_relative(&path)?;
                let hydrated = reads::read_note(self, &path).await?;
                // The head's version travels out with the text. That is what lets a
                // caller about to write this note back carry a real precondition into
                // the write instead of a hope. See `BaseVersion`.
                Ok(ContentResponse::Text {
                    text: hydrated.content,
                    version: Some(hydrated.note.version_id),
                })
            }
            // Markdown only. Refused for every path, including a `.md` one: a caller
            // asking for raw bytes wants an attachment, and answering with a
            // reassembled note's UTF-8 would look like the mount stores files.
            ContentRequest::ReadBytes { path } => {
                ensure_vault_relative(&path)?;
                Err(BackendError::Unsupported(
                    ALGOLIA_NO_BINARY_MESSAGE.to_string(),
                ))
            }
            // Discriminated by extension rather than blanket-refused: `Stat` is how
            // `read_artifact` learns an artifact's size, so it must refuse a binary
            // path — but it is also the only way to size a NOTE, and a mount that
            // could not report a note's size would be needlessly crippled.
            ContentRequest::Stat { path } => {
                ensure_vault_relative(&path)?;
                if !reads::is_markdown_path(&path) {
                    return Err(BackendError::Unsupported(
                        ALGOLIA_NO_BINARY_MESSAGE.to_string(),
                    ));
                }
                Ok(ContentResponse::Stat {
                    size_bytes: reads::stat_note(self, &path).await?,
                })
            }
            // The upload mint's gate; see `ALGOLIA_NO_UPLOAD_MESSAGE`. This is the one
            // request whose meaning is narrower than its name on this backend, and it
            // is deliberate: `ResolvePath`'s sole caller is the out-of-band upload
            // mint, and an upload has no destination here.
            ContentRequest::ResolvePath { path } => {
                ensure_vault_relative(&path)?;
                Err(BackendError::Unsupported(
                    ALGOLIA_NO_UPLOAD_MESSAGE.to_string(),
                ))
            }
        }
    }

    /// `WriteText`: append a new version, forking rather than failing on a stale base.
    async fn write_text(
        &self,
        path: &str,
        content: &str,
        base_version: BaseVersion,
        resolve_divergence: bool,
    ) -> Result<MutationResponse, BackendError> {
        self.ensure_writable()?;
        ensure_writable_path(path)?;
        if !reads::is_markdown_path(path) {
            return Err(BackendError::Unsupported(
                ALGOLIA_NO_BINARY_MESSAGE.to_string(),
            ));
        }
        // Links inside a shared note mean notes in the SHARED corpus, so they are
        // resolved against this mount's own note list rather than the writer's private
        // vault — otherwise one record would resolve differently for each participant.
        //
        // A failure here degrades link resolution for THIS version (every wiki link
        // stays raw, so the `links:` backlink filter will not find it) but must not fail
        // the write: the note's content is what the user asked to store. Logged rather
        // than swallowed, because a silently unresolved version is invisible afterwards.
        let known_files = match reads::walk_markdown(self).await {
            Ok(files) => files,
            Err(error) => {
                warn!(
                    "could not list the notes of Algolia index '{}' while writing {path}, so this \
                     version's wiki links are stored UNRESOLVED and backlink filters will not \
                     find it: {error}",
                    self.index_name
                );
                Vec::new()
            }
        };
        let outcome = versioning::push_note_version(
            self,
            path,
            content,
            &known_files,
            &base_version,
            resolve_divergence,
        )
        .await?;
        Ok(MutationResponse::Written {
            created: outcome.created,
        })
    }

    /// `SoftDelete`: replace the head with a tombstone. See
    /// [`versioning::soft_delete_note`].
    ///
    /// Gated on `writable` like every other mutation, and on the markdown-only rule
    /// before that — a caller asking to delete an attachment must be told the mount never
    /// held one, not that the delete failed.
    async fn soft_delete(&self, path: &str) -> Result<MutationResponse, BackendError> {
        self.ensure_writable()?;
        ensure_vault_relative(path)?;
        if !reads::is_markdown_path(path) {
            return Err(BackendError::Unsupported(
                ALGOLIA_NO_BINARY_MESSAGE.to_string(),
            ));
        }
        let outcome = versioning::soft_delete_note(self, path).await?;
        Ok(MutationResponse::SoftDeleted {
            version_id: outcome.version_id,
            already_deleted: outcome.already_deleted,
            recoverable_from: outcome.recoverable_from,
        })
    }

    async fn health(&self, request: HealthRequest) -> Result<HealthResponse, BackendError> {
        match request {
            // NOT a hard startup gate, for the same reason the couchdb mount's is not:
            // an unreachable shared corpus must leave the server serving its
            // filesystem root. So this reports `reachable: false` rather than erroring.
            HealthRequest::Overview => Ok(HealthResponse::Overview {
                reachable: reads::probe_reachable(self).await,
            }),
        }
    }
}

#[async_trait::async_trait]
impl VaultBackend for AlgoliaVaultBackend {
    /// # Capability rationale
    ///
    /// * `GrepSearch` — yes, but **candidate-bounded, not exhaustive**. A filesystem
    ///   mount's `grep_search` reads every file; this one runs a lexical prefilter
    ///   over the index and then evaluates the caller's pattern locally over the
    ///   candidates it returned, so a match in a chunk the index ranked below the cap
    ///   is not reported. The capability is advertised anyway because a bounded line
    ///   search over a shared wiki is genuinely useful and the alternative — refusing
    ///   `grep_search` for the whole vault whenever one mount is Algolia-backed —
    ///   would be a worse answer for every other mount. The bound is reported at
    ///   `warn` on every call, and a pattern with no literal anchor is refused rather
    ///   than answered misleadingly. See [`grep::ALGOLIA_GREP_NO_ANCHOR_MESSAGE`].
    /// * `NativeRecall` — yes, and it is the whole reason the capability exists. This
    ///   mount has no LOCAL index (the remote index is the corpus), so a scoped
    ///   `hybrid_search` here is served by the index itself rather than refused. The
    ///   hits carry ordinal scores and name their recall stage, so nothing claims parity
    ///   with the local hybrid ranker. See [`recall`].
    /// * `VersionHistory` — yes, unconditionally, including on a READ-ONLY mount:
    ///   listing versions and reading one back are reads, and a participant who may read
    ///   the corpus may read its history. Gating it on `writable` would hide the recovery
    ///   path from exactly the mounts most likely to need it.
    /// * `SoftDelete` — only when `writable`. This is the ONE capability `writable`
    ///   gates, and it must be gated: a delete is a write, and the server registers the
    ///   `delete_note` tool from this capability, so advertising it on a read-only mount
    ///   would put a tool on the surface that could only ever refuse.
    /// * NO `BinaryRead`, NO `BinaryWrite`, NO `Upload` — Markdown only, by storage
    ///   design. See [`ALGOLIA_NO_BINARY_MESSAGE`].
    /// * NO `Watch` — there is no change feed. Algolia has no "tell me what changed
    ///   since" primitive, and polling the whole corpus is not one.
    fn descriptor(&self) -> BackendDescriptor {
        let mut capabilities = vec![
            Capability::GrepSearch,
            Capability::NativeRecall,
            Capability::VersionHistory,
        ];
        if self.writable {
            capabilities.push(Capability::SoftDelete);
        }
        BackendDescriptor::new(BackendKind::Algolia, capabilities)
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, BackendError> {
        match request {
            BackendRequest::Manifest(request) => {
                self.manifest(request).await.map(BackendResponse::Manifest)
            }
            BackendRequest::Content(request) => {
                self.content(request).await.map(BackendResponse::Content)
            }
            BackendRequest::Mutation(MutationRequest::WriteText {
                path,
                content,
                base_version,
                resolve_divergence,
            }) => self
                .write_text(&path, &content, base_version, resolve_divergence)
                .await
                .map(BackendResponse::Mutation),
            BackendRequest::Mutation(MutationRequest::SoftDelete { path }) => {
                self.soft_delete(&path).await.map(BackendResponse::Mutation)
            }
            // Refused for being BINARY rather than for the mount being read-only,
            // even on a read-only mount: `writable` can be turned on, and the reader
            // of this error must not be sent off to do that in the belief it will
            // help. Nothing makes an Algolia index hold an attachment.
            BackendRequest::Mutation(MutationRequest::CommitUploadStream { chunks, .. }) => {
                // The body is dropped without being pulled. The caller's pump sees the
                // failure through the response, and reading a body only to discard it
                // would make a refusal cost the whole upload.
                drop(chunks);
                Err(BackendError::Unsupported(
                    ALGOLIA_NO_BINARY_MESSAGE.to_string(),
                ))
            }
            // Housekeeping, and a no-op: documented as best-effort and never-failing,
            // and there is no staging area here to sweep — a note's records are pushed
            // chunks-first, head-last, so a killed process leaves records that the
            // head simply does not point at, not a partial artifact on disk.
            BackendRequest::Mutation(MutationRequest::SweepOrphanStagingFiles) => {
                Ok(BackendResponse::Mutation(MutationResponse::Swept))
            }
            BackendRequest::Recall(RecallRequest::Grep {
                query,
                regex,
                case_sensitive,
                glob,
                context_lines,
                limit,
            }) => grep::grep(
                self,
                &query,
                regex,
                case_sensitive,
                glob.as_deref(),
                context_lines,
                limit,
            )
            .await
            .map(|(matches, candidate_count)| {
                // NEVER exhaustive, and the response says so rather than leaving the
                // caller to assume ripgrep semantics. `candidate_count` is what the
                // lexical prefilter actually examined; see `grep::grep`.
                BackendResponse::Recall(RecallResponse::Grep(GrepOutcome {
                    matches,
                    exhausted: false,
                    candidate_count: Some(candidate_count),
                }))
            }),
            BackendRequest::Recall(RecallRequest::Search(request)) => {
                recall::search(self, &request)
                    .await
                    .map(|response| BackendResponse::Recall(RecallResponse::Search(response)))
            }
            BackendRequest::Health(request) => {
                self.health(request).await.map(BackendResponse::Health)
            }
        }
    }

    /// `Some`, always — and it means something different from CouchDB's.
    ///
    /// A CouchDB conflict is an UNRECONCILED pair of sibling revisions: the storage
    /// itself has not decided. Nothing is unresolved here — the head pointer is a
    /// single record and a read serves it unambiguously. What `Some` reports is
    /// RECORDED DIVERGENCE: a version was pushed whose base was not the head at push
    /// time, so the content it forked away from is sitting in the history index and
    /// has never been merged into the line the head belongs to.
    ///
    /// It is still the right thing to answer here. Both facts have the same shape and
    /// the same consequence for a reader — the content you were served is not the only
    /// content anyone wrote — and `vault_info` is where a caller looks for it. An
    /// empty list is a real answer rather than an inapplicable one, which is exactly
    /// what distinguishes `Some(vec![])` from `None`.
    async fn conflicted_paths(&self) -> Result<Option<Vec<String>>, BackendError> {
        reads::divergent_paths(self).await.map(Some)
    }

    /// An empty stream: there is no change feed.
    ///
    /// `after` is ignored because nothing could be resumed from. A mount whose corpus
    /// another participant edits is refreshed by the reader's next read (the head
    /// lookup is the freshness check), not by a notification.
    fn changes(&self, _after: Option<OpaqueCursor>) -> ChangeStream {
        ChangeStream::empty()
    }

    fn as_algolia(&self) -> Option<&AlgoliaVaultBackend> {
        Some(self)
    }
}

// ---------------------------------------------------------------------------
// The CLI's window
// ---------------------------------------------------------------------------

/// What `algolia status` reports about one mount.
///
/// Every field is OBSERVED against the account when the report is built. That rules out
/// the tempting shortcut of reading the backend's own `main_provisioned` /
/// `history_provisioned` flags: those are per-process latches that say "this process has
/// already applied the settings", so in a freshly-started CLI they are always `false` and
/// reporting them would tell an operator their index is unprovisioned when it is fine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlgoliaMountStatus {
    /// The main index answered a request. False for an unreachable account, a rejected
    /// key, and an index that does not exist yet — the three are distinguished by
    /// `main_provisioned` and `notes` below rather than collapsed here.
    pub reachable: bool,
    /// The main index exists and carries faceting settings, i.e. the corpus has been
    /// written to at least once and provisioning succeeded. An index with records but no
    /// settings is a real state (a failed settings call is non-fatal by design), and it
    /// is worth naming because facet queries — every folder listing — fail in it.
    pub main_provisioned: bool,
    /// Same for the `_history` index, which does not exist until a note is first
    /// superseded. `false` on a young corpus is normal, not a fault.
    pub history_provisioned: bool,
    /// Live (non-tombstoned) notes in the main index.
    pub notes: usize,
    /// Superseded versions in the history index. Not a total: the head of each note is
    /// in the main index and is not counted here.
    pub superseded_versions: usize,
    /// Notes whose head records a divergence, sorted. Same list `vault_info` reports.
    pub divergent_paths: Vec<String>,
    /// `(min_versions, max_age_days)` — the retention keep-set rule this mount applies
    /// when it purges history.
    pub retention: (usize, u64),
    /// `(entries, bytes)` in the local hydrated-note cache.
    pub cache: (usize, u64),
}

impl AlgoliaVaultBackend {
    /// Observe everything `algolia status` prints.
    ///
    /// One method rather than a handful of accessors because the fields are only
    /// meaningful together — "0 notes" means something different on an unreachable mount
    /// than on a provisioned one — and because it keeps [`AlgoliaVaultBackend::client`]
    /// `pub(crate)`. A CLI that could reach the raw client could also reach the API key.
    ///
    /// Never fails: every probe degrades to a reported absence, because the whole point
    /// of a status command is to describe a broken mount rather than to fail like one.
    pub async fn status(&self) -> AlgoliaMountStatus {
        let reachable = reads::probe_reachable(self).await;
        let notes = reads::walk_markdown(self)
            .await
            .map(|files| files.len())
            .unwrap_or(0);
        let superseded_versions = empty_if_missing_index(
            // `recordType:note` only: a history record set also holds one CHUNK record
            // per chunk of every superseded version, and counting those would report a
            // number an order of magnitude larger than "versions kept".
            self.client
                .browse_all(&self.history_index, Some("recordType:note"))
                .await,
            Vec::new(),
        )
        .map(|records| records.len())
        .unwrap_or(0);
        AlgoliaMountStatus {
            reachable,
            main_provisioned: index_has_faceting(&self.client, &self.index_name).await,
            history_provisioned: index_has_faceting(&self.client, &self.history_index).await,
            notes,
            superseded_versions,
            divergent_paths: reads::divergent_paths(self).await.unwrap_or_default(),
            retention: self.retention,
            cache: self.cache.stats(),
        }
    }

    /// Permanently remove a note: its head, its chunks, and its ENTIRE history.
    ///
    /// # Why this is not a `BackendRequest`
    ///
    /// Deliberately unreachable through [`VaultBackend::execute`], which is what keeps it
    /// off the MCP surface. Every other removal on this mount is a soft delete: the
    /// content survives in history and a versioned read can bring it back. This one
    /// destroys it, and no amount of tool-description wording makes that safe to hand an
    /// agent — the agent cannot judge whether the human wanted the history gone. PR #40
    /// drew the same line and it is worth restating: `retract` exists because a mistaken
    /// push into a SHARED corpus must be withdrawable by a person, and only by a person.
    ///
    /// Gated on `writable` like every other mutation. Two delete-by-query calls, main
    /// then history, both keyed on `noteId` so the note record and every chunk record of
    /// every version go together — a purge that removed the head and left the chunks
    /// would leave orphaned text that search still matches.
    pub async fn retract_note(&self, remote_path: &str) -> Result<(), BackendError> {
        self.ensure_writable()?;
        ensure_vault_relative(remote_path)?;
        let filter = format!("noteId:{}", quote_filter_value(remote_path));
        // The main index first. If the second call fails, what is left behind is history
        // for a note that no longer exists — inert, and reported. The other order would
        // leave a live head whose history had been destroyed, which is worse.
        empty_if_missing_index(
            self.client.delete_by_query(&self.index_name, &filter).await,
            serde_json::Value::Null,
        )?;
        empty_if_missing_index(
            self.client
                .delete_by_query(&self.history_index, &filter)
                .await,
            serde_json::Value::Null,
        )?;
        self.cache.remove(remote_path);
        Ok(())
    }
}

/// Whether an index exists AND carries faceting settings.
///
/// The same test [`ensure_index_settings`] uses to decide an index is already configured,
/// so `status` cannot disagree with the code that does the provisioning. A missing index
/// and an unreachable account both answer `false`; the caller distinguishes them with
/// `reachable`.
async fn index_has_faceting(client: &AlgoliaClient, index: &str) -> bool {
    client
        .get_settings(index)
        .await
        .map(|current| {
            current
                .get("attributesForFaceting")
                .and_then(|value| value.as_array())
                .map(|list| !list.is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The history index's name, derived rather than configured.
///
/// Derived so the two cannot disagree: a separately-named history index that drifted
/// from its main index would be a corpus whose history silently belonged to something
/// else.
pub fn history_index_name(index_name: &str) -> String {
    format!("{index_name}_history")
}

/// Where a mount's hydrated-note cache lives.
///
/// The index name is sanitized into one safe path segment. Algolia index names permit
/// characters a path does not (and a name with a `/` in it would silently nest the
/// cache somewhere else), so anything outside `[A-Za-z0-9._-]` becomes `_`.
fn cache_dir(index_dir: &Path, index_name: &str) -> PathBuf {
    let safe: String = index_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || character == '.'
                || character == '_'
                || character == '-'
            {
                character
            } else {
                '_'
            }
        })
        .collect();
    index_dir.join(CACHE_DIR_SEGMENT).join(safe)
}

/// Map Algolia's "index does not exist" 404 onto an empty result.
///
/// An index is created by its FIRST WRITE, so a corpus nobody has written — and the
/// `_history` index until a note is first superseded — answers 404 to every read.
/// Semantically that is "no records", not a failure, and treating it as one is what
/// made a virgin index 404 through the whole surface in PR #40. Every other error
/// still propagates.
pub fn empty_if_missing_index<T>(
    result: Result<T, AlgoliaError>,
    empty: T,
) -> Result<T, BackendError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.is_index_not_found() => Ok(empty),
        Err(error) => Err(map_algolia_error(error)),
    }
}

/// An empty `SearchResponse`, for [`empty_if_missing_index`] on search calls.
pub fn empty_search_response() -> deep_obsidian_algolia::SearchResponse {
    deep_obsidian_algolia::SearchResponse {
        hits: Vec::new(),
        nb_hits: 0,
        facets: None,
    }
}

/// Map an Algolia failure onto a backend failure.
pub(crate) fn map_algolia<T>(result: Result<T, AlgoliaError>) -> Result<T, BackendError> {
    result.map_err(map_algolia_error)
}

/// The one place an [`AlgoliaError`] becomes a [`BackendError`].
///
/// Everything is a `Message`, deliberately: unlike the sidecar's `not-found`, an
/// Algolia error never means "this note is absent" (absence is an empty result, not
/// an error), so nothing here may become an `io::ErrorKind::NotFound` — which the
/// server reads as "the destination is free" on the write path. Absence is minted by
/// [`note_not_found`] instead, at the one site that actually establishes it.
fn map_algolia_error(error: AlgoliaError) -> BackendError {
    BackendError::Message(format!("algolia mount error: {error}"))
}

/// The error for a note that is not in the corpus.
///
/// An `io::ErrorKind::NotFound` because the server distinguishes "destination absent"
/// from every other failure by `io_kind()` — that is what turns a failed pre-write
/// read into [`BaseVersion::Absent`] rather than into `Unobserved`, and so what lets
/// a concurrent create be reported instead of clobbered.
///
/// Reached for a genuinely missing note, for a TOMBSTONED one (the record exists so
/// the removal is observable, but the note does not), and for a 403 `objectID not
/// allowed` from a scoped secured key — the last so a scoped participant cannot tell
/// "exists but hidden" from "does not exist" and use the difference to enumerate
/// paths outside their scope.
fn note_not_found(path: &str) -> BackendError {
    BackendError::io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("no note at {path} on this Algolia mount"),
    ))
}

/// Quote a value for an Algolia `filters` expression.
///
/// Values reaching a filter are note paths, version ids and folder names — arbitrary
/// vault content. An unescaped `"` in one would terminate the quoted string early and
/// the rest of the path would be parsed as filter syntax, which at best is a 400 and
/// at worst is a filter that matches the wrong records. Backslashes are escaped
/// first, then quotes, so the escape itself cannot be smuggled.
fn quote_filter_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Apply index settings once per process, and only when they look absent — so a
/// hand-tuned index (NeuralSearch, custom ranking) is never clobbered.
///
/// Settings cannot be applied to an index that does not exist, and an index only
/// exists after its first write, so every caller invokes this AFTER writing.
///
/// A failure is NON-FATAL: the index still works with Algolia's defaults, only
/// faceting and ranking degrade, and a settings call that failed must not fail a
/// user's write — the note is already in the index by the time this runs.
pub async fn ensure_index_settings(
    client: &AlgoliaClient,
    index: &str,
    flag: &AtomicBool,
    settings: serde_json::Value,
) {
    if flag.swap(true, Ordering::Relaxed) {
        return;
    }
    let already_configured = client
        .get_settings(index)
        .await
        .map(|current| {
            current
                .get("attributesForFaceting")
                .and_then(|value| value.as_array())
                .map(|list| !list.is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if already_configured {
        return;
    }
    if let Err(error) = client.set_settings_awaited(index, settings).await {
        warn!(index = %index, "algolia index settings not applied: {error}");
    }
}

/// The retention keep-set: the `min_versions` most recent versions UNIONED with
/// everything younger than `max_age_days`.
///
/// A union, never an intersection. The floor alone would purge a busy note's history
/// down to five versions regardless of age; the ceiling alone would purge a note
/// nobody has touched in a year down to NOTHING, which is the case that actually
/// loses information. `versions` is `(version_id, updated_at_ms)` in any order.
pub fn retention_keep_set(
    versions: &[(String, u64)],
    min_versions: usize,
    max_age_days: u64,
    now_ms: u64,
) -> HashSet<String> {
    let mut sorted: Vec<&(String, u64)> = versions.iter().collect();
    // Most recent first, so `rank < min_versions` is the floor.
    sorted.sort_by_key(|(_, updated_at)| std::cmp::Reverse(*updated_at));
    let age_floor_ms = now_ms.saturating_sub(max_age_days.saturating_mul(24 * 60 * 60 * 1000));
    sorted
        .iter()
        .enumerate()
        .filter(|(rank, (_, updated_at))| *rank < min_versions || *updated_at > age_floor_ms)
        .map(|(_, (version_id, _))| version_id.clone())
        .collect()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// A new version id.
///
/// Sorts naturally by timestamp, with a participant-derived component so two people's
/// ids are distinguishable and a random salt so two writes in the same millisecond
/// are too.
pub fn new_version_id(participant_id: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in participant_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let salt: u16 = rand::random();
    format!("v{}-{:04x}{:04x}", now_ms(), (hash & 0xffff) as u16, salt)
}

/// Reject a path that is not usable as a vault-relative note path.
///
/// The same rules the couchdb backend applies, and for the same reason: traversal is
/// refused outright, and a path a listing could never produce is refused here rather
/// than turned into a confusing not-found.
fn ensure_vault_relative(path: &str) -> Result<(), BackendError> {
    let refuse = || {
        Err(BackendError::Vault(
            deep_obsidian_core::vault::VaultError::InvalidVaultRelativePath(path.to_string()),
        ))
    };
    if path.trim().is_empty() {
        return refuse();
    }
    if path.starts_with('/') || path.contains('\\') {
        return refuse();
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.starts_with('.') {
            return refuse();
        }
    }
    Ok(())
}

/// Reject a path no write may target.
///
/// Two rules, both borrowed rather than reinvented: [`ensure_vault_relative`], and
/// core's protected-template policy reported through
/// [`deep_obsidian_core::vault::VaultError::ProtectedWritePath`] so the wording is
/// byte-identical to a filesystem mount's refusal. A mount kind must not get to
/// decide whether `Templates/` is writable.
fn ensure_writable_path(path: &str) -> Result<(), BackendError> {
    ensure_vault_relative(path)?;
    if path.split('/').any(|segment| {
        segment.eq_ignore_ascii_case("template") || segment.eq_ignore_ascii_case("templates")
    }) {
        return Err(BackendError::Vault(
            deep_obsidian_core::vault::VaultError::ProtectedWritePath(path.to_string()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(writable: bool) -> AlgoliaVaultBackend {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("dob-algolia-unit-{}-{unique}", std::process::id()));
        AlgoliaVaultBackend::connect(
            AlgoliaCredentials {
                app_id: "TESTAPP".to_string(),
                index_name: "unit-index".to_string(),
                api_key: SecretString::new("super-secret-key".to_string()),
                base_url: Some("http://127.0.0.1:1/".to_string()),
            },
            AlgoliaOptions {
                writable,
                participant_id: Some("tester@unit".to_string()),
                ..AlgoliaOptions::default()
            },
            &dir,
        )
        .expect("connect")
    }

    /// The hard constraint: no `{:?}` anywhere may print the key. The client's own
    /// `Debug` is hand-written for the same reason, so this holds transitively.
    #[test]
    fn debug_never_prints_the_api_key() {
        let backend = backend(true);
        let rendered = format!("{backend:?}");
        assert!(!rendered.contains("super-secret-key"), "{rendered}");
        assert!(rendered.contains("TESTAPP"), "{rendered}");

        let credentials = AlgoliaCredentials {
            app_id: "TESTAPP".to_string(),
            index_name: "unit-index".to_string(),
            api_key: SecretString::new("super-secret-key".to_string()),
            base_url: None,
        };
        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains("super-secret-key"), "{rendered}");
        assert!(format!("{:?}", backend.client()).contains("<redacted>"));
    }

    /// The capability set is exactly what this storage can do: a bounded grep, its own
    /// ranked search, and a version history. Every BINARY capability is absent, so
    /// nothing advertises what the storage cannot hold.
    #[test]
    fn the_descriptor_advertises_recall_history_and_the_bounded_grep() {
        let descriptor = backend(true).descriptor();
        assert_eq!(descriptor.kind, BackendKind::Algolia);
        for present in [
            Capability::GrepSearch,
            Capability::NativeRecall,
            Capability::VersionHistory,
        ] {
            assert!(
                descriptor.supports(present),
                "{present:?} must be advertised"
            );
        }
        for absent in [
            Capability::BinaryRead,
            Capability::BinaryWrite,
            Capability::Upload,
            Capability::Watch,
        ] {
            assert!(
                !descriptor.supports(absent),
                "{absent:?} must not be advertised"
            );
        }
    }

    /// `writable` gates EXACTLY one capability, and it has to be that one: the server
    /// registers `delete_note` from `SoftDelete`, so advertising it on a read-only mount
    /// would put a tool on the surface whose every call is refused. Reading the history
    /// stays available, because recovering content is a read.
    #[test]
    fn only_soft_delete_is_gated_on_writable() {
        let read_only = backend(false).descriptor();
        let writable = backend(true).descriptor();
        assert!(!read_only.supports(Capability::SoftDelete));
        assert!(writable.supports(Capability::SoftDelete));
        assert!(
            read_only.supports(Capability::VersionHistory)
                && read_only.supports(Capability::NativeRecall),
            "reads do not depend on `writable`: {read_only:?}"
        );
        let difference: Vec<Capability> = writable
            .capabilities
            .difference(&read_only.capabilities)
            .copied()
            .collect();
        assert_eq!(difference, vec![Capability::SoftDelete]);
    }

    /// A read-only mount refuses a DELETE by naming the setting, exactly as it refuses a
    /// write — and before anything reaches Algolia.
    #[tokio::test]
    async fn a_read_only_mount_refuses_a_delete_by_naming_the_setting() {
        let error = backend(false)
            .execute(BackendRequest::soft_delete("A.md"))
            .await
            .expect_err("a read-only mount refuses a delete");
        assert_eq!(error.to_string(), ALGOLIA_READ_ONLY_MESSAGE);
    }

    /// Deleting an ATTACHMENT path is refused for being binary rather than for being
    /// undeletable: this mount never held one, and a "delete failed" would suggest it did.
    #[tokio::test]
    async fn deleting_a_binary_path_is_refused_as_markdown_only() {
        let error = backend(true)
            .execute(BackendRequest::soft_delete("Assets/logo.png"))
            .await
            .expect_err("a binary path has nothing to delete");
        assert_eq!(error.to_string(), ALGOLIA_NO_BINARY_MESSAGE);
    }

    /// A read-only mount refuses a write with the message that names the setting, and
    /// does so BEFORE any request reaches Algolia (the base url here is unroutable,
    /// so a message about the setting proves nothing was attempted).
    #[tokio::test]
    async fn a_read_only_mount_refuses_writes_by_naming_the_setting() {
        let error = backend(false)
            .execute(BackendRequest::write_text("A.md", "# A\n"))
            .await
            .expect_err("a read-only mount refuses");
        assert_eq!(error.to_string(), ALGOLIA_READ_ONLY_MESSAGE);
        assert!(ALGOLIA_READ_ONLY_MESSAGE.contains("\"writable\": true"));
        assert!(ALGOLIA_READ_ONLY_MESSAGE.contains("mount configuration"));
    }

    /// Binary is refused for being binary, on a WRITABLE mount too — the refusal must
    /// not blame a setting that cannot change it.
    #[tokio::test]
    async fn every_binary_path_is_refused_with_the_markdown_only_message() {
        let backend = backend(true);
        for request in [
            BackendRequest::read_bytes("Assets/logo.png"),
            BackendRequest::stat("Assets/logo.png"),
            BackendRequest::write_text("Assets/logo.png", "not markdown"),
        ] {
            let error = backend
                .execute(request)
                .await
                .expect_err("a binary path is refused");
            assert_eq!(error.to_string(), ALGOLIA_NO_BINARY_MESSAGE);
        }
        // A markdown path is refused for READ_BYTES too: raw bytes mean an attachment.
        let error = backend
            .execute(BackendRequest::read_bytes("A.md"))
            .await
            .expect_err("read_bytes is refused for markdown too");
        assert_eq!(error.to_string(), ALGOLIA_NO_BINARY_MESSAGE);

        // The upload mint has its own message, answered at the mint.
        let error = backend
            .execute(BackendRequest::resolve_path("Assets/logo.png"))
            .await
            .expect_err("the upload mint is refused");
        assert_eq!(error.to_string(), ALGOLIA_NO_UPLOAD_MESSAGE);

        let error = backend
            .execute(BackendRequest::Mutation(
                MutationRequest::CommitUploadStream {
                    path: "Assets/logo.png".to_string(),
                    expected_hash: None,
                    max_bytes: 16,
                    chunks: crate::UploadChunks::new(std::iter::once(Ok(b"bytes".to_vec()))),
                },
            ))
            .await
            .expect_err("an upload commit is refused");
        assert_eq!(error.to_string(), ALGOLIA_NO_BINARY_MESSAGE);
    }

    /// Every refusal must say EXPERIMENTAL, must say what the mount actually is, and
    /// must point at something that does work — and the binary ones must NOT blame
    /// `writable`, which would send the reader to change a setting that cannot help.
    #[test]
    fn the_refusals_are_honest_about_their_own_cause() {
        for message in [
            ALGOLIA_NO_BINARY_MESSAGE,
            ALGOLIA_NO_UPLOAD_MESSAGE,
            ALGOLIA_READ_ONLY_MESSAGE,
        ] {
            assert!(message.contains("EXPERIMENTAL"), "{message}");
            assert!(message.contains("Algolia"), "{message}");
        }
        for message in [ALGOLIA_NO_BINARY_MESSAGE, ALGOLIA_NO_UPLOAD_MESSAGE] {
            assert!(message.contains("MARKDOWN ONLY"), "{message}");
            assert!(
                !message.contains("writable"),
                "a binary refusal must not blame a setting that cannot lift it: {message}"
            );
            assert!(message.contains("filesystem mount"), "{message}");
        }
        assert!(ALGOLIA_READ_ONLY_MESSAGE.contains("READ-ONLY"));
    }

    /// Traversal and unusable paths are refused before anything is fetched, and with
    /// core's own wording so a caller cannot tell which backend answered.
    #[tokio::test]
    async fn unusable_paths_are_refused_with_cores_wording() {
        let backend = backend(true);
        for path in ["", "   ", "/absolute.md", "../escape.md", ".hidden/note.md"] {
            let error = backend
                .execute(BackendRequest::read_text(path))
                .await
                .expect_err("{path} must be refused");
            assert!(
                matches!(error, BackendError::Vault(_)),
                "{path:?} produced {error:?}"
            );
        }
        // A protected template folder is refused for writes with CORE's wording, so a
        // caller cannot tell a shared mount's refusal from a filesystem mount's.
        let error = backend
            .execute(BackendRequest::write_text("Templates/T.md", "x"))
            .await
            .expect_err("a protected path is refused");
        assert_eq!(
            error.to_string(),
            deep_obsidian_core::vault::VaultError::ProtectedWritePath("Templates/T.md".to_string())
                .to_string()
        );
    }

    #[test]
    fn retention_keeps_the_floor_unioned_with_recency() {
        let day_ms = 24 * 60 * 60 * 1000_u64;
        let now = 200 * day_ms;
        let versions: Vec<(String, u64)> = (0..10)
            .map(|age_days| (format!("v{age_days}"), now - age_days as u64 * day_ms))
            .collect();
        // min 2, max age 5 days: the 2 most recent PLUS anything younger than 5 days.
        let keep = retention_keep_set(&versions, 2, 5, now);
        assert!(keep.contains("v0") && keep.contains("v1"), "the floor");
        assert!(keep.contains("v4"), "4 days old, inside the window");
        assert!(!keep.contains("v5"), "5 days old and outside the floor");
        assert!(!keep.contains("v9"));

        // A note nobody has touched in ages: the floor is what saves its history, and
        // it is why the rule is a union rather than an intersection.
        let stale: Vec<(String, u64)> = (0..3)
            .map(|index| (format!("s{index}"), day_ms * (index as u64 + 1)))
            .collect();
        assert_eq!(retention_keep_set(&stale, 5, 90, now).len(), 3);
        assert!(retention_keep_set(&stale, 0, 90, now).is_empty());
    }

    #[test]
    fn filter_values_are_quoted_and_escaped() {
        assert_eq!(quote_filter_value("A/B.md"), "\"A/B.md\"");
        // A quote in a path must not terminate the filter string early.
        assert_eq!(
            quote_filter_value("Odd \"name\".md"),
            "\"Odd \\\"name\\\".md\""
        );
        // ...and the escape itself cannot be smuggled in.
        assert_eq!(quote_filter_value("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn the_history_index_and_cache_dir_are_derived_not_configured() {
        assert_eq!(history_index_name("team-wiki"), "team-wiki_history");
        assert_eq!(
            cache_dir(Path::new("/idx"), "team/wiki"),
            Path::new("/idx").join("algolia-cache").join("team_wiki"),
            "an index name is sanitized into ONE path segment"
        );
    }

    /// The default participant id says out loud that the host part was never
    /// configured, rather than quietly claiming an identity.
    #[test]
    fn the_default_participant_id_admits_it_is_a_default() {
        assert!(default_participant_id().ends_with("@unknown"));
    }

    /// There is no change feed, and the stream says so by ending immediately rather
    /// than hanging a consumer forever.
    #[tokio::test]
    async fn the_change_stream_is_empty() {
        let mut stream = backend(true).changes(None);
        assert!(stream.recv().await.is_none());
    }

    /// Housekeeping must never fail: a caller's cleanup pass would report a spurious
    /// error for a mount that has nothing to clean.
    #[tokio::test]
    async fn sweeping_staging_files_is_a_successful_no_op() {
        let response = backend(false)
            .execute(BackendRequest::sweep_orphan_staging_files())
            .await
            .expect("the sweep never fails");
        assert!(matches!(
            response,
            BackendResponse::Mutation(MutationResponse::Swept)
        ));
    }
}
