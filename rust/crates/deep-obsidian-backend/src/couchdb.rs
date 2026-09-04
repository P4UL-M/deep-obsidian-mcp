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
//!   content rather than a lie. That is also what makes
//!   [`CouchDbVaultBackend::soft_delete`] recoverable at all: the tombstone keeps the
//!   entry's `children`, so its last content survives the delete and writing it back
//!   resurrects the note. There is no revision history behind it — see
//!   [`COUCHDB_VERSION_HISTORY_UNSUPPORTED_MESSAGE`].
//! * **There is no ripgrep.** `grep_search` is served by an IMITATION of ripgrep
//!   ([`crate::virtual_grep`]) running over note text read back through the sidecar:
//!   the manifest supplies the corpus, the caller's glob pre-filters it by path, and
//!   every surviving note is read and matched line by line. It is exhaustive in the
//!   same sense ripgrep is — it looks everywhere — and it costs a full corpus read per
//!   query. See [`CouchDbVaultBackend::grep`].
//! * **Conflicts are served.** The winning revision is returned and
//!   `conflicted: true` comes back with it. [`ContentResponse::Stat`] carries only
//!   `size_bytes`, so the flag cannot be surfaced through the public MCP schema
//!   this slice without changing it; it is logged instead. See
//!   [`CouchDbVaultBackend::stat`].
//! * **Writes need opting in.** On a mount that did not set `writable` every mutation —
//!   including a delete — is refused with [`COUCHDB_READ_ONLY_MESSAGE`], which names the
//!   experimental read-only state and the exact setting that changes it rather than
//!   reporting a generic capability error.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use serde_json::Value;
use tracing::{debug, warn};

use crate::sidecar::{
    ConflictDetail, ConflictsResult, EntryKind, ManifestEntry, ReadPayload, SidecarConfig,
    SidecarCredentials, SidecarError, SidecarErrorKind, SidecarMode, SidecarSupervisor, StatResult,
    WriteGuard, WritePayload, WriteResult,
};
use crate::virtual_grep::{self, GlobFilter, LineMatcher};
use crate::watch::ChangeStream;
use crate::{
    BackendDescriptor, BackendError, BackendKind, BackendRequest, BackendResponse, BaseVersion,
    Capability, ChildListing, ContentRequest, ContentResponse, GrepMatch, GrepOutcome,
    HealthRequest, HealthResponse, ManifestRequest, ManifestResponse, MutationRequest,
    MutationResponse, OpaqueCursor, RecallRequest, RecallResponse, VaultBackend, VaultChildEntry,
    VaultEntryKind,
};

/// Refusal for every write against a READ-ONLY CouchDB mount.
///
/// Deliberately long and specific. A user reaching this has configured a mount and
/// then tried to save a note into it; "unsupported operation" would leave them
/// guessing whether it is a bug, a permission problem, or a missing capability.
///
/// The facts they need are that the backend is experimental, that the refusal comes
/// from THIS MOUNT'S CONFIGURATION rather than from a missing implementation, exactly
/// which setting changes it, and what to do instead if they would rather not.
///
/// Note what this message must not claim. Until writes existed it said they were
/// refused "by construction" and that "no write path exists yet"; both became false
/// the moment `writable` did anything, and a refusal that misstates its own cause
/// sends the reader looking in the wrong place.
pub const COUCHDB_READ_ONLY_MESSAGE: &str = "this mount is an EXPERIMENTAL, READ-ONLY \
CouchDB (Self-hosted LiveSync) vault: it is read-only because its mount configuration does not \
set 'writable', and the sidecar serving it was started read-only as a result, so no write can \
reach your vault. To allow writes, set \"writable\": true on this mount (it additionally requires \
experimental.couchdbVaults, which is already on if this mount loaded) and restart the service; \
guarded writes are then compare-and-swapped against the revision each read observed. Otherwise \
edit the note in Obsidian and let LiveSync replicate it, or write to a filesystem mount instead.";

/// How many notes the virtual grep reads at once.
///
/// The scan is bounded rather than unbounded because the sidecar is ONE child process
/// serving one CouchDB connection: firing a thousand concurrent `read` calls at it
/// would queue a thousand JSON-RPC lines, hold every decrypted body in memory at once,
/// and make a `limit`-satisfied grep pay for notes it never needed. Eight keeps the
/// queue and the peak memory bounded by a constant.
///
/// # What the concurrency actually buys, measured
///
/// The Rust side genuinely pipelines: `SidecarSupervisor::request` releases the stdin
/// lock before awaiting its reply, so N requests are in flight at once. But the sidecar
/// is a single Node process, so ITS dispatch — not this number — bounds the win. Measured
/// against the local mock CouchDB (`a_grep_reads_the_whole_corpus_and_reports_its_throughput`,
/// 40 notes), `1` gives ~880 notes/sec and `8` gives ~955: about 9%, because a loopback
/// read costs almost nothing to overlap. The number is here for the case that measurement
/// cannot show — a REMOTE CouchDB, where per-read latency dominates and eight overlapping
/// round trips are eight times fewer serial waits. Do not read the 9% as the ceiling, and
/// do not raise this expecting the local figure to move.
///
/// Order-preserving (`buffered`, not `buffer_unordered`), because the scan's
/// determinism is load-bearing: see [`crate::virtual_grep`] on output order.
const GREP_READ_CONCURRENCY: usize = 8;

/// Refusal for a ranked search asked of a CouchDB mount.
///
/// Same shape as the filesystem's, and for the same reason: this mount HAS a local
/// search index (the server builds one by walking the sidecar's manifest), so recall is
/// already answered — one layer up. Saying so is what stops a reader concluding that
/// recall is unavailable on a LiveSync vault, which is the opposite of true.
pub const COUCHDB_NATIVE_RECALL_UNSUPPORTED_MESSAGE: &str = "this mount does not perform its own \
ranked search: it is an EXPERIMENTAL CouchDB (Self-hosted LiveSync) vault, and the server builds a \
local search index over its content, so hybrid_search, load_knowledge, related_notes and \
graph_traverse are all answered from that index instead. This request exists for a backend whose \
storage IS a search index; CouchDB is a document store with no relevance ranking. Nothing is \
missing — use the index-backed recall tools.";

/// Refusal for a versioned read or a history listing on a CouchDB mount.
///
/// The one refusal here that is genuinely "not implemented yet" rather than "the
/// storage cannot", and it says so in those words. CouchDB really does keep a revision
/// history, and its sibling revisions are already surfaced as conflicts — so a reader
/// who was told this was impossible would be misinformed. What is missing is a sidecar
/// protocol call to enumerate and fetch a revision, which is a real piece of work and
/// not something a user can turn on.
pub const COUCHDB_VERSION_HISTORY_UNSUPPORTED_MESSAGE: &str = "listing or reading a previous \
version of a note is NOT IMPLEMENTED for this mount yet. It is not impossible: this is an \
EXPERIMENTAL CouchDB (Self-hosted LiveSync) vault and CouchDB does retain a revision history — but \
the sidecar protocol has no call to enumerate or fetch a revision, so the server has nothing to \
ask. No configuration turns this on. Until it exists, recover a previous version through Obsidian's \
own file recovery or a CouchDB backup; vault_info reports which notes have unreconciled sibling \
revisions.";

/// How long a collected manifest may be reused **while we are blind**.
///
/// # This was the cache's whole lifetime, and is now only its fallback
///
/// It used to be the entire invalidation strategy: a two-second window, chosen
/// short precisely because nothing told the cache when it had gone stale. That made
/// every manifest-backed read (`vault_info`, `resources/list`, listings,
/// `conflicted_paths`) pay a full cursor-looped `manifest` walk — N round trips to
/// CouchDB, O(notes) — on the first call after any two-second gap, which for
/// interactive traffic is very nearly every call. Measured at ~47 ms per mount over
/// 300 notes, growing linearly.
///
/// Something DOES tell us now: [`SidecarSupervisor::change_epoch`], moved by every
/// local write and by every change the feed delivers. So the cache is valid until
/// invalidated, and this constant survives for the one case the epoch cannot cover —
/// **the feed is not live** (never armed because auto-reindex is off, or armed and
/// then lost with the child). Then an epoch that has not moved is only evidence that
/// nothing TOLD us, not that nothing changed, and the old conservatism is exactly
/// right: bound the staleness by two seconds and re-walk.
///
/// So the number is unchanged and its justification is unchanged; only its scope
/// shrank. See [`CouchDbVaultBackend::cached_manifest`] for the validity rule and
/// [`CouchDbVaultBackend::fresh_manifest_entries`] for the stricter one the virtual
/// grep needs.
const MANIFEST_REUSE_WINDOW: Duration = Duration::from_secs(2);

/// How many `path → (rev, content_hash)` pairs the read cache keeps.
///
/// # Why an entry cap and not a byte budget
///
/// The Algolia mount's `NoteCache` bounds itself by BYTES because it caches note
/// bodies, which vary by four orders of magnitude. This caches neither bodies nor
/// anything else of variable size: an entry is a vault path, a CouchDB revision
/// (`<n>-<32 hex>`) and an `fnv1a64:` hash — call it 200 bytes with allocator
/// overhead. Counting entries is therefore counting bytes, with one fewer thing to
/// get wrong, and 4096 entries is roughly a megabyte.
///
/// # Why 4096
///
/// It only has to cover the WORKING SET of one session's conditional re-reads, not
/// the vault: a hit needs the same path read twice with the same revision, and an
/// agent re-reading a note it already has is re-reading something it touched
/// minutes ago. 4096 is comfortably above any plausible such set while staying
/// small enough that the cap is never the interesting part of a bug report. A vault
/// larger than this loses nothing but the optimisation, and loses it on the
/// least-recently-read entries first.
const HASH_CACHE_CAPACITY: usize = 4096;

/// What the last successful read of one path observed.
#[derive(Clone)]
struct CachedHash {
    /// The CouchDB revision the hash was computed from. The whole point: a hash
    /// without the revision it belongs to cannot be shown to be current.
    rev: String,
    /// [`deep_obsidian_core::content_hash`] of that revision's bytes — the same
    /// function, byte for byte, that `read_file` compares `knownHash` against.
    content_hash: String,
    /// The vault's change epoch when the read that produced this was ISSUED.
    ///
    /// PER ENTRY, not one value for the whole cache. A cache-wide epoch would be the
    /// epoch of the most recent insertion, and "nothing has changed since the most
    /// recent insertion" says nothing about an older entry — a change to note B
    /// followed by a read of B would then certify a stale revision for note A. The
    /// epoch has to travel with the observation it dates.
    epoch: u64,
    /// Whether the change feed was live when the read that produced this was issued.
    /// Same hazard, same reason as [`CachedManifest::feed_live_when_collected`]: an
    /// observation made while nothing was listening cannot be certified by a feed that
    /// only started listening afterwards.
    feed_live_when_read: bool,
    /// For eviction. Monotonic tick rather than a clock: eviction only needs an
    /// ORDER, and a `SystemTime` would let a clock adjustment reorder it.
    last_used: u64,
}

/// A bounded `path → (rev, hash)` map. See [`HASH_CACHE_CAPACITY`].
///
/// The revision is stored WITH the hash rather than checked separately, because "I
/// have a hash for this path" and "I have the hash of the revision that is current"
/// are different facts and only the second one can answer `unchanged`. Borrowed from
/// the Algolia mount's `NoteCache`, whose key is `(path, version_id)` for exactly
/// this reason.
///
/// Approximate-LRU by the same construction as `NoteCache`'s: on overflow, sort by
/// last use and drop the oldest. Batched (a quarter of the map at a time) so a
/// steady-state-full cache does not pay a sort per insertion.
#[derive(Default)]
struct HashCache {
    entries: std::collections::HashMap<String, CachedHash>,
    tick: u64,
}

impl HashCache {
    /// The whole entry for `path`, whatever its revision. The caller decides what the
    /// revision and the epoch are worth.
    fn get_entry(&mut self, path: &str) -> Option<CachedHash> {
        self.touch(path).map(|entry| entry.clone())
    }

    fn touch(&mut self, path: &str) -> Option<&mut CachedHash> {
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let entry = self.entries.get_mut(path)?;
        entry.last_used = tick;
        Some(entry)
    }

    fn put(
        &mut self,
        path: &str,
        rev: &str,
        content_hash: &str,
        epoch: u64,
        feed_live_when_read: bool,
    ) {
        // A read that could not name its revision teaches us nothing usable: the entry
        // could never be shown to be current, so it would occupy a slot and always miss.
        if rev.is_empty() {
            return;
        }
        self.tick = self.tick.wrapping_add(1);
        self.entries.insert(
            path.to_string(),
            CachedHash {
                rev: rev.to_string(),
                content_hash: content_hash.to_string(),
                epoch,
                feed_live_when_read,
                last_used: self.tick,
            },
        );
        if self.entries.len() > HASH_CACHE_CAPACITY {
            self.evict();
        }
    }

    /// Forget one path entirely, for a mutation that ended its content rather than
    /// replacing it. See [`CouchDbVaultBackend::soft_delete`].
    fn remove(&mut self, path: &str) {
        self.entries.remove(path);
    }

    fn evict(&mut self) {
        let mut ages: Vec<(u64, String)> = self
            .entries
            .iter()
            .map(|(path, entry)| (entry.last_used, path.clone()))
            .collect();
        ages.sort_unstable();
        for (_, path) in ages.into_iter().take(HASH_CACHE_CAPACITY / 4) {
            self.entries.remove(&path);
        }
    }
}

/// A collected manifest, when it was collected, and what the vault's change epoch was
/// when the collection STARTED.
struct CachedManifest {
    entries: Arc<Vec<ManifestEntry>>,
    collected_at: std::time::Instant,
    /// Read before the walk was issued, never after — see
    /// [`SidecarSupervisor::change_epoch`] for why that direction is the safe one.
    epoch: u64,
    /// Whether the change feed was live when the walk was ISSUED.
    ///
    /// # The hazard this closes
    ///
    /// The feed arms ASYNCHRONOUSLY — the server runtime spawns the subscription, and it
    /// completes some moment after the mount starts serving. Without this flag the
    /// sequence
    ///
    /// 1. collect the manifest at epoch 0, feed not yet armed,
    /// 2. a change lands (nothing is listening, so the epoch does not move),
    /// 3. the feed arms,
    /// 4. read the manifest: epoch is still 0 and the feed is live, so serve the cache
    ///
    /// would serve a manifest that predates a change, for as long as nothing else moved
    /// the epoch. "The epoch has not moved AND the feed is live now" is not sufficient;
    /// the feed has to have been live for the WHOLE interval, and since the epoch pins
    /// the end of that interval, all that is missing is the beginning.
    feed_live_when_collected: bool,
}

/// A LiveSync vault reached through a supervised sidecar. Read-only unless the
/// supervisor was configured `read-write`.
pub struct CouchDbVaultBackend {
    supervisor: Arc<SidecarSupervisor>,
    manifest: std::sync::Mutex<Option<CachedManifest>>,
    /// What each recently-touched path's content hashed to, at which revision.
    /// See [`HashCache`] and [`Self::read_text_conditionally`].
    hashes: std::sync::Mutex<HashCache>,
    /// Derived from the supervisor's mode, never configured separately.
    ///
    /// A second flag could disagree with the sidecar it talks to, and the disagreement
    /// would be silent in one direction: a backend advertising `BinaryWrite` over a
    /// read-only sidecar would advertise a capability, accept the request, and only
    /// then be refused at the far end — a capability lie. Deriving it makes the two
    /// unable to differ.
    writable: bool,
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
    pub fn from_supervisor(supervisor: Arc<SidecarSupervisor>) -> Self {
        Self {
            writable: supervisor.mode().is_writable(),
            supervisor,
            manifest: std::sync::Mutex::new(None),
            hashes: std::sync::Mutex::new(HashCache::default()),
        }
    }

    /// True when this mount accepts writes.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Build a supervisor and a backend over it, resolving the bundle location.
    ///
    /// Construction performs NO IO against CouchDB: the handshake happens on first
    /// use. That is what lets a mount whose remote is down still be constructed, so
    /// the server can report it as degraded instead of refusing to start.
    pub fn spawn(
        sidecar_path: Option<&Path>,
        credentials: SidecarCredentials,
        mode: SidecarMode,
        options: Option<Value>,
        request_timeout: Option<Duration>,
    ) -> Result<(Arc<SidecarSupervisor>, Self), SidecarError> {
        let config =
            SidecarConfig::resolve(sidecar_path, credentials, mode, options, request_timeout)?;
        let supervisor = SidecarSupervisor::new(config);
        Ok((supervisor.clone(), Self::from_supervisor(supervisor)))
    }

    pub fn supervisor(&self) -> &Arc<SidecarSupervisor> {
        &self.supervisor
    }

    /// The whole manifest, from cache when the cache is still known-good.
    ///
    /// The everyday accessor: listings, `WalkMarkdown`, `TopLevelFolders`,
    /// `conflicted_paths` and therefore `vault_info` and `resources/list`. Its
    /// freshness rule is [`Self::cached_manifest`]'s. The virtual grep wants a
    /// stronger one — see [`Self::fresh_manifest_entries`].
    pub async fn manifest_entries(&self) -> Result<Arc<Vec<ManifestEntry>>, BackendError> {
        if let Some(cached) = self.cached_manifest() {
            return Ok(cached);
        }
        self.collect_manifest_entries().await
    }

    /// The whole manifest, reused ONLY on positive evidence that nothing has changed.
    ///
    /// # Why the virtual grep needs its own rule
    ///
    /// A LISTING can tolerate a slightly old snapshot: a caller reading a directory
    /// expects eventual consistency, which is what makes [`MANIFEST_REUSE_WINDOW`]'s
    /// two-second fallback acceptable there. A grep cannot, because the manifest is not
    /// metadata to it — it is the definition of what "everywhere" means, and the outcome
    /// CLAIMS to have looked everywhere (`exhausted: true`). A note written a moment ago
    /// and absent from the manifest would be reported as containing no matches, which is
    /// the one thing an exhaustive search must never do.
    ///
    /// That was found by the multi-backend demo, which writes a note through MCP and then
    /// greps for a line in it: the write landed, the read-back was correct, and the grep
    /// returned nothing because a manifest collected during the preceding listing was
    /// still inside the window. The fix at the time was to opt out of the cache entirely.
    ///
    /// # Why it no longer has to opt out
    ///
    /// The two writers that could invalidate a grep's corpus are now both reported,
    /// and this reads the report rather than assuming the worst:
    ///
    /// * **A write through this backend** bumps the epoch on completion, before the
    ///   caller is answered ([`SidecarSupervisor::change_epoch`]). So the demo's
    ///   write-then-grep is exact with zero sleeps, and it is exact whether the feed is
    ///   up or down — which is why that test passes unmodified.
    /// * **A write from anywhere else** bumps the epoch when the feed delivers it, so it
    ///   is covered exactly while [`SidecarSupervisor::change_feed_live`] holds.
    ///
    /// So: reuse iff the epoch has not moved AND the feed is live. **No TTL fallback
    /// here** — with the feed down, an unmoved epoch says nothing about an external
    /// write, and "probably nothing changed in the last two seconds" is not a basis for
    /// claiming to have searched everywhere. Feed down therefore re-walks every time,
    /// which is precisely the behaviour this replaced.
    async fn fresh_manifest_entries(&self) -> Result<Arc<Vec<ManifestEntry>>, BackendError> {
        if self.supervisor.change_feed_live() {
            if let Some(cached) = self.cached_manifest_at_current_epoch() {
                return Ok(cached);
            }
        }
        self.collect_manifest_entries().await
    }

    /// Collect the manifest from the sidecar, ignoring any cached one, and cache the
    /// result against the epoch the walk started at.
    async fn collect_manifest_entries(&self) -> Result<Arc<Vec<ManifestEntry>>, BackendError> {
        // BEFORE the walk, both of them. A change landing while it runs must invalidate
        // the result, and stamping afterwards would instead certify it. See
        // `SidecarSupervisor::change_epoch` and `CachedManifest::feed_live_when_collected`.
        let feed_live_when_collected = self.supervisor.change_feed_live();
        let epoch = self.supervisor.change_epoch();
        let entries = Arc::new(map_sidecar(self.supervisor.collect_manifest().await)?);
        if let Ok(mut slot) = self.manifest.lock() {
            *slot = Some(CachedManifest {
                entries: entries.clone(),
                collected_at: std::time::Instant::now(),
                epoch,
                feed_live_when_collected,
            });
        }
        Ok(entries)
    }

    /// The cached manifest when it is still good enough for a listing.
    ///
    /// Valid when the epoch has not moved AND either the feed is live (so an unmoved
    /// epoch means nothing changed) or the collection is inside
    /// [`MANIFEST_REUSE_WINDOW`] (the blind fallback).
    ///
    /// # Locking
    ///
    /// The supervisor's state is read FIRST and its locks are released before
    /// `self.manifest` is taken, so only one lock is ever held at a time and there is no
    /// order to invert. Nothing here awaits, so the `std::sync::Mutex` is never held
    /// across a suspension point either.
    fn cached_manifest(&self) -> Option<Arc<Vec<ManifestEntry>>> {
        let feed_live = self.supervisor.change_feed_live();
        let epoch = self.supervisor.change_epoch();
        let slot = self.manifest.lock().ok()?;
        let cached = slot.as_ref()?;
        if cached.epoch != epoch {
            return None;
        }
        // The feed must have been live for the WHOLE interval, not merely now — see
        // `CachedManifest::feed_live_when_collected`.
        let feed_vouches = feed_live && cached.feed_live_when_collected;
        (feed_vouches || cached.collected_at.elapsed() < MANIFEST_REUSE_WINDOW)
            .then(|| cached.entries.clone())
    }

    /// The cached manifest only when the epoch proves it current, with no TTL fallback.
    /// The caller is responsible for having checked that the feed is live.
    fn cached_manifest_at_current_epoch(&self) -> Option<Arc<Vec<ManifestEntry>>> {
        let epoch = self.supervisor.change_epoch();
        let slot = self.manifest.lock().ok()?;
        let cached = slot.as_ref()?;
        (cached.epoch == epoch && cached.feed_live_when_collected).then(|| cached.entries.clone())
    }

    async fn manifest_request(
        &self,
        request: ManifestRequest,
    ) -> Result<ManifestResponse, BackendError> {
        // Refused BEFORE the manifest walk: collecting it is N round trips to CouchDB,
        // and a mount whose remote is down would then answer with an unreachability
        // error instead of the honest "not implemented" — sending the reader to debug
        // their connection over a call that could never have succeeded.
        if matches!(request, ManifestRequest::Versions { .. }) {
            return Err(BackendError::Unsupported(
                COUCHDB_VERSION_HISTORY_UNSUPPORTED_MESSAGE.to_string(),
            ));
        }
        let entries = self.manifest_entries().await?;
        match request {
            ManifestRequest::ListChildren {
                path,
                include_hidden,
                include_ignored,
            } => Ok(ManifestResponse::Children(ChildListing::exhaustive(
                list_children(&entries, path.as_deref(), include_hidden, include_ignored),
            ))),
            ManifestRequest::WalkMarkdown => {
                Ok(ManifestResponse::MarkdownFiles(walk_markdown(&entries)))
            }
            ManifestRequest::TopLevelFolders => {
                Ok(ManifestResponse::Folders(top_level_folders(&entries)))
            }
            // Refused above, before the manifest walk.
            ManifestRequest::Versions { .. } => Err(BackendError::Unsupported(
                COUCHDB_VERSION_HISTORY_UNSUPPORTED_MESSAGE.to_string(),
            )),
        }
    }

    async fn content(&self, request: ContentRequest) -> Result<ContentResponse, BackendError> {
        match request {
            // Refused before the sidecar is asked: there is no protocol call that could
            // serve it, so a round trip could only produce a misleading error.
            ContentRequest::ReadText {
                version: Some(_), ..
            } => Err(BackendError::Unsupported(
                COUCHDB_VERSION_HISTORY_UNSUPPORTED_MESSAGE.to_string(),
            )),
            ContentRequest::ReadText {
                path, known_hash, ..
            } => {
                ensure_vault_relative(&path)?;
                if let Some(known_hash) = &known_hash {
                    if let Some(response) = self.read_text_conditionally(&path, known_hash).await? {
                        return Ok(response);
                    }
                }
                // Both sampled before the read is issued; see `remember_hash`.
                let feed_live_when_read = self.supervisor.change_feed_live();
                let epoch = self.supervisor.change_epoch();
                let result = map_sidecar(self.supervisor.read(&path).await)?;
                note_conflict(&path, result.conflicted);
                let version = (!result.rev.is_empty()).then(|| result.rev.clone());
                match result.payload {
                    // The revision travels out with the text. That is what lets a
                    // caller that is about to write this note back turn its own
                    // `expectedHash` check into a storage-level precondition instead
                    // of a hope. See `BaseVersion`.
                    ReadPayload::Text(text) => {
                        // Populate the cache from the one place that has BOTH the bytes
                        // and the revision that produced them. Computing the hash here
                        // costs one pass over text we are already holding, and the layer
                        // above is about to compute the identical hash with the identical
                        // function anyway — so this is not extra work, it is the same
                        // work done where its result can be kept.
                        self.remember_hash(&path, &result.rev, &text, epoch, feed_live_when_read);
                        Ok(ContentResponse::Text { text, version })
                    }
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

    /// Record what one revision of one path hashed to. See [`HashCache`].
    ///
    /// `epoch` must be the value read BEFORE the read that produced `text` was issued —
    /// same rule, and same reason, as [`Self::collect_manifest_entries`]'s. Stamping the
    /// epoch afterwards would date the observation later than it happened, and a change
    /// that landed mid-read would then be certified away.
    fn remember_hash(
        &self,
        path: &str,
        rev: &str,
        text: &str,
        epoch: u64,
        feed_live_when_read: bool,
    ) {
        if let Ok(mut cache) = self.hashes.lock() {
            cache.put(
                path,
                rev,
                &deep_obsidian_core::content_hash(text.as_bytes()),
                epoch,
                feed_live_when_read,
            );
        }
    }

    /// Try to answer a conditional read WITHOUT hydrating the note.
    ///
    /// `Ok(Some(Unchanged))` when the caller's `known_hash` is provably the current
    /// content's; `Ok(None)` when that could not be established and the caller must fall
    /// through to a full read. An error is a real failure (an unreachable remote), not a
    /// miss — a miss is `Ok(None)`.
    ///
    /// # What a full read costs here, and why this is worth a method
    ///
    /// On a filesystem mount `read_file` with a matching `knownHash` saves only the
    /// response body: the file is read and hashed regardless, because `fnv1a64` cannot be
    /// computed any other way. On this mount a read is a JSON-RPC round trip that fetches
    /// the entry root, fetches every chunk it names, reassembles them, and decrypts the
    /// lot on an E2EE vault — so the body was never the expensive part. Measured through
    /// the mount, `knownHash` used to save nothing at all (2.64 ms against 2.60 ms) for
    /// precisely that reason: the saving was on the only cheap step.
    ///
    /// # The chain that makes an answer legitimate
    ///
    /// The invariant is absolute: **never answer `unchanged` unless the revision the
    /// cached hash belongs to is shown to be the CURRENT one.** Two ways to show it, and
    /// the cheaper one is tried first:
    ///
    /// 1. **The change feed is live and the epoch has not moved since the entry was
    ///    cached.** Then nothing in this vault has changed since we read this note —
    ///    every local write bumps the epoch before its caller is answered, and every
    ///    remote write bumps it when the feed delivers it — so the revision we recorded
    ///    is still the head, and NO round trip is needed at all. This is the case the
    ///    manifest cache's own invalidation (see [`Self::cached_manifest`]) buys for
    ///    free; it is the same signal read for a different question.
    /// 2. **Otherwise, `stat`.** One metadata round trip yields the current revision. If
    ///    it equals the revision the cached hash belongs to, the hash is current. The
    ///    chunk fetches and the reassembly are skipped; on a note of any size that is the
    ///    entire cost of the read.
    ///
    /// A cache entry for a DIFFERENT revision is not evidence of anything and is not
    /// treated as such — `HashCache::get` takes the revision as part of the lookup, so a
    /// stale entry misses rather than answering. That is also why an external write can
    /// never poison this: the write moves the revision, so either the feed reports it
    /// (case 1 fails) or the `stat` observes it (case 2 fails), and both fall through to
    /// a full read.
    async fn read_text_conditionally(
        &self,
        path: &str,
        known_hash: &str,
    ) -> Result<Option<ContentResponse>, BackendError> {
        // Nothing is asked of the remote until the cache says an answer is POSSIBLE.
        //
        // This ordering is the whole difference between an optimisation and a
        // pessimisation. Probing with a `stat` first would add a round trip to precisely
        // the cases it cannot help: a path this process has never read (an agent resuming
        // with hashes from an earlier session — every read), and a `known_hash` that does
        // not match what we hold (the note really did change — also every read). Both then
        // pay stat + read where they used to pay read. So the cache is consulted first,
        // and a miss costs nothing at all.
        let Some(entry) = self.cached_hash_entry(path) else {
            return Ok(None);
        };
        // A hash that is not the caller's cannot become `Unchanged` however current it
        // turns out to be, so there is nothing worth confirming.
        if entry.content_hash != known_hash {
            return Ok(None);
        }

        // Case 1: no round trip at all. The feed is checked FIRST and the epoch sampled
        // after it, so a change arriving between any two of these steps can only make the
        // test fail, never pass. `feed_live_when_read` covers the other end of the
        // interval — see `CachedManifest::feed_live_when_collected` for the hazard.
        if entry.feed_live_when_read
            && self.supervisor.change_feed_live()
            && entry.epoch == self.supervisor.change_epoch()
        {
            debug!(
                "read of {path} answered unchanged from the change feed alone: no revision \
                 has moved in this vault since it was last read"
            );
            return Ok(Some(ContentResponse::Unchanged {
                hash: known_hash.to_string(),
                version: Some(entry.rev),
            }));
        }

        // Case 2: one `stat`, worth paying now that the hash is known to match — it can
        // only come back "yes, that revision is current" or "no", and the first answer
        // saves the entry fetch, every chunk fetch, the reassembly and the decryption.
        let Ok(stat) = self.supervisor.stat(path).await else {
            // Not a miss to report as an error: the caller asked to read a note, and the
            // full read below produces the right error for whatever is wrong with it.
            return Ok(None);
        };
        if stat.rev.is_empty() || stat.rev != entry.rev {
            return Ok(None);
        }
        note_conflict(path, stat.conflicted);
        debug!(
            "read of {path} answered unchanged after a metadata-only stat: revision {} is the \
             one the cached hash belongs to, so no chunk was fetched",
            stat.rev
        );
        Ok(Some(ContentResponse::Unchanged {
            hash: known_hash.to_string(),
            version: Some(stat.rev),
        }))
    }

    /// Everything recorded for `path`, or `None` when nothing is.
    fn cached_hash_entry(&self, path: &str) -> Option<CachedHash> {
        let mut cache = self.hashes.lock().ok()?;
        cache.get_entry(path)
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    /// Refuse every mutation on a read-only mount, with the message that names why.
    fn ensure_writable(&self) -> Result<(), BackendError> {
        if self.writable {
            return Ok(());
        }
        Err(BackendError::Unsupported(
            COUCHDB_READ_ONLY_MESSAGE.to_string(),
        ))
    }

    /// `WriteText`, guarded by whatever the caller observed.
    ///
    /// # The guard, and why it is not a retry
    ///
    /// The MCP layer above has already read this note, hashed it, compared the hash
    /// to the caller's `expectedHash`, and composed `content`. `base_version` is the
    /// revision that read saw. Handing it to the sidecar as `baseRev` makes CouchDB
    /// itself adjudicate the window between that comparison and this write — the
    /// window a filesystem mount cannot close and therefore silently loses.
    ///
    /// So a `conflict` here means something specific: the note changed AFTER the
    /// caller's precondition was checked. Retrying with the fresh revision would
    /// write anyway, which is precisely the last-writer-wins behaviour the caller
    /// asked to be protected from. It is reported instead. The single exception is
    /// spelled out in [`Self::resolve_write_conflict`].
    async fn write_text(
        &self,
        path: &str,
        content: &str,
        base_version: BaseVersion,
    ) -> Result<MutationResponse, BackendError> {
        self.ensure_writable()?;
        ensure_writable_path(path)?;
        let guard = write_guard_for(&base_version);
        let result = self
            .guarded_write(
                path,
                WritePayload::Text(content.to_string()),
                guard,
                content.as_bytes(),
            )
            .await?;
        Ok(MutationResponse::Written {
            created: result.created,
        })
    }

    /// `SoftDelete`: turn the entry into a LiveSync tombstone, guarded by its own revision.
    ///
    /// # What "soft" means here, and how it differs from the Algolia mount's
    ///
    /// The sidecar sets `deleted: true` on the entry document and leaves `children`
    /// alone — it never sends `_deleted`, which would take the document out of
    /// `_all_docs` entirely. So three things are true afterwards, and the tool payload
    /// above depends on all three:
    ///
    /// * the note is GONE from every listing, from the manifest, from the virtual grep and
    ///   from the local index built over that manifest ([`is_listable`]);
    /// * other devices SEE the removal — LiveSync replicates the tombstone, which is what
    ///   makes this a delete rather than a local hide;
    /// * the last content is still THERE, because the chunks the tombstone still names are
    ///   untouched. `read`/`stat` answer on a tombstone, so the content can be read back
    ///   and written again, and the write resurrects the note.
    ///
    /// What is NOT true is Algolia's story: there is no readable revision history behind
    /// the tombstone, so `recoverable_from` is `None` — not because the removal is
    /// unrecoverable, but because no `read_version` exists here to name a version for. See
    /// [`COUCHDB_VERSION_HISTORY_UNSUPPORTED_MESSAGE`]. CouchDB's own older revisions are
    /// not an answer either: they die on compaction, so naming one would promise a read
    /// that may already be impossible.
    ///
    /// # The guard, and the delete that is not issued
    ///
    /// ONE `stat` serves both questions — is this already a tombstone, and what revision
    /// must the delete be guarded on — because taking two reads would open a window
    /// between them. A `conflict` from the delete therefore means the entry moved after
    /// that `stat`, and it is REPORTED rather than retried, exactly as a lost write is:
    /// re-deleting at the fresh revision would discard whatever that concurrent writer
    /// stored. See [`Self::resolve_write_conflict`] for the one exception a WRITE has and
    /// this deliberately does not.
    ///
    /// An entry that is already a tombstone is answered from the `stat` alone, with
    /// `already_deleted: true` and no request issued. The sidecar would happily accept the
    /// delete — verified against the real one in
    /// `couchdb_sidecar.rs::deleting_a_tombstone_changes_nothing_and_says_so`, where it
    /// produces a fresh `deleted: true` revision — and that is precisely why the request is
    /// not made: the new revision would replicate to every device that syncs this vault
    /// and mean nothing that the old one did not.
    async fn soft_delete(&self, path: &str) -> Result<MutationResponse, BackendError> {
        self.ensure_writable()?;
        // The protected-path rule, and not just `ensure_vault_relative`: `write_text` and
        // `commit_upload` both refuse `Templates/`, and a surface where a template cannot
        // be overwritten but can be tombstoned would protect nothing.
        ensure_writable_path(path)?;
        // A missing entry arrives as an IO `NotFound`, which is the taxonomy the tool layer
        // already branches on for a read of a missing note.
        let stat = map_sidecar(self.supervisor.stat(path).await)?;
        if stat.deleted {
            debug!(
                "delete of {path} was asked of an entry that is already a livesync tombstone at \
                 revision {}; answered from the stat, with no request issued",
                stat.rev
            );
            return Ok(MutationResponse::SoftDeleted {
                version_id: stat.rev,
                already_deleted: true,
                recoverable_from: None,
            });
        }
        // Empty only if a sidecar omitted it, which is "no observation" rather than a
        // revision — so the delete goes out unguarded rather than guarded on nothing.
        let base_rev = (!stat.rev.is_empty()).then(|| stat.rev.clone());
        let result = match self.supervisor.delete(path, base_rev.as_deref()).await {
            Ok(result) => result,
            Err(error) if error.rpc_kind() == Some(SidecarErrorKind::Conflict) => {
                let detail = error.conflict().cloned().unwrap_or_default();
                let expected = match &base_rev {
                    Some(rev) => WriteGuard::Revision(rev.clone()),
                    None => WriteGuard::Unguarded,
                };
                warn!(
                    "livesync delete of {path} lost its compare-and-swap ({}); nothing was \
                     deleted",
                    describe_conflict(&detail)
                );
                return Err(BackendError::VersionConflict {
                    path: path.to_string(),
                    expected: expected.describe(),
                    found: describe_conflict(&detail),
                });
            }
            Err(error) => return Err(map_sidecar_error(error)),
        };
        // The hash cache is a cache of LIVE content keyed by revision, and the revision has
        // just moved, so nothing it holds for this path can answer `unchanged` any more.
        // The rev-keyed lookup and the epoch stamp would already refuse it — this is the
        // same belt-and-braces the Algolia mount's `cache().remove()` is on a delete, and
        // it costs one map removal.
        self.forget_hash(path);
        Ok(MutationResponse::SoftDeleted {
            version_id: result.rev,
            already_deleted: false,
            // Deliberately `None`. See the "What 'soft' means here" note above: the
            // content is recoverable, but not through a version a versioned read could
            // serve, and naming one that cannot be read would be worse than naming none.
            recoverable_from: None,
        })
    }

    /// Drop whatever the cache holds for `path`. See [`HashCache`].
    fn forget_hash(&self, path: &str) {
        if let Ok(mut cache) = self.hashes.lock() {
            cache.remove(path);
        }
    }

    /// `CommitUploadStream`: collect the body, verify `expected_hash`, write binary.
    ///
    /// The filesystem backend's staging file and atomic rename have no analogue here
    /// and need none: the sidecar's write is already all-or-nothing at the entry root
    /// (chunks first, root last), so there is nothing to stage. What IS shared is the
    /// contract: `max_bytes` enforced *during* collection so an oversize body never
    /// reaches the remote, `expected_hash` verified against the destination as it is
    /// at commit time, and the canonical content hash reported back.
    async fn commit_upload(
        &self,
        path: &str,
        expected_hash: Option<&str>,
        max_bytes: usize,
        chunks: crate::UploadChunks,
    ) -> Result<MutationResponse, BackendError> {
        self.ensure_writable()?;
        ensure_writable_path(path)?;

        // The chunk iterator is fed by the caller's async body pump, so pulling it on
        // a reactor thread would deadlock against that pump — the same reason the
        // filesystem backend spawns here.
        let collected = tokio::task::spawn_blocking(move || collect_upload(max_bytes, chunks))
            .await
            .map_err(|error| BackendError::Message(error.to_string()))??;

        // One read serves both the precondition and the guard: re-reading for the
        // revision separately would open a second window between them.
        let existing = self.read_for_write(path).await?;
        if let Some(expected) = expected_hash {
            let found = existing
                .as_ref()
                .map(|existing| deep_obsidian_core::content_hash(&existing.bytes));
            if found.as_deref() != Some(expected) {
                // Byte-identical to the filesystem backend's own wording and shape,
                // because the upload endpoint's 409 body is frozen public behaviour.
                return Err(BackendError::HashConflict {
                    expected: expected.to_string(),
                    found: found.unwrap_or_else(|| "null".to_string()),
                });
            }
        }
        let guard = match &existing {
            Some(existing) => WriteGuard::Revision(existing.rev.clone()),
            None => WriteGuard::CreateOnly,
        };

        let result = self
            .guarded_write(
                path,
                WritePayload::Base64(encode_base64(&collected.bytes)),
                guard,
                &collected.bytes,
            )
            .await?;
        Ok(MutationResponse::UploadCommitted {
            created: result.created,
            bytes_written: collected.bytes.len(),
            hash: collected.hash,
        })
    }

    /// Issue one guarded write and decide what a lost compare-and-swap means.
    ///
    /// `desired` is the exact bytes this write is trying to land. It is needed only on
    /// the conflict path, and only for the ambiguous case; see
    /// [`Self::resolve_write_conflict`].
    async fn guarded_write(
        &self,
        path: &str,
        payload: WritePayload,
        guard: WriteGuard,
        desired: &[u8],
    ) -> Result<WriteResult, BackendError> {
        // Sampled before the write, for the usual reason: `remember_hash` requires an
        // epoch that PRE-dates the observation it dates. Note the consequence — the write
        // itself bumps the epoch, so a read of this note will not take the no-round-trip
        // path (case 1 in `read_text_conditionally`); it takes the `stat` path, which
        // still skips the entry fetch, every chunk fetch and the reassembly. Stamping the
        // post-write epoch would make case 1 fire and would be WRONG: an external change
        // landing between this write landing and its own bump would be ordered before the
        // stamp, and this revision would be certified current after it had been
        // superseded.
        let epoch = self.supervisor.change_epoch();
        let feed_live_when_read = self.supervisor.change_feed_live();
        let attempt = self.supervisor.write(path, &payload, &guard).await;
        match attempt.outcome {
            Ok(result) => {
                // The write knows the new revision AND the bytes that are now at it, so
                // it can record the pair the conditional read needs — which is what makes
                // the documented client loop cheap: `upsert_note` hands back `newHash`,
                // the agent feeds it straight back as `knownHash`, and that read no longer
                // hydrates the note it just wrote.
                //
                // Only for a TEXT write. A binary entry is not readable as text, so a hash
                // recorded for one could only ever serve a read that is refused anyway.
                if let WritePayload::Text(_) = &payload {
                    if let Ok(text) = std::str::from_utf8(desired) {
                        self.remember_hash(path, &result.rev, text, epoch, feed_live_when_read);
                    }
                }
                if result.conflicted {
                    warn!(
                        "wrote {path} on a livesync entry that has conflicting revisions; the \
                         write extended the WINNING revision only and neither created nor \
                         resolved a conflict branch"
                    );
                }
                if result.resurrected {
                    debug!("write of {path} brought a soft-deleted livesync entry back");
                }
                Ok(result)
            }
            Err(error) if error.rpc_kind() == Some(SidecarErrorKind::Conflict) => {
                self.resolve_write_conflict(path, &guard, &error, desired, attempt.outcome_unknown)
                    .await
            }
            Err(error) => Err(map_sidecar_error(error)),
        }
    }

    /// What to do about a `conflict`.
    ///
    /// The answer is almost always "report it". A compare-and-swap that lost means
    /// another writer got there first, and the whole point of threading the revision
    /// was to find that out instead of overwriting them.
    ///
    /// There is exactly one exception, and it is not a conflict being mapped to
    /// success. When an earlier attempt of THIS call was issued and its outcome was
    /// never observed (`outcome_unknown`), the winning revision may be that attempt's
    /// own. A revision cannot distinguish the two — but the content can: if the
    /// destination already holds exactly the bytes this write was asked to land, then
    /// the requested state is the state, and reporting a failure would send the caller
    /// off to retry a write that already happened. The check is byte-equality, never a
    /// merge, and it is gated strictly on the ambiguity actually having arisen.
    async fn resolve_write_conflict(
        &self,
        path: &str,
        guard: &WriteGuard,
        error: &SidecarError,
        desired: &[u8],
        outcome_unknown: bool,
    ) -> Result<WriteResult, BackendError> {
        let detail = error.conflict().cloned().unwrap_or_default();
        if outcome_unknown {
            if let Ok(Some(current)) = self.read_for_write(path).await {
                if current.bytes == desired {
                    warn!(
                        "livesync write of {path} was retried after an unobserved outcome and then \
                         lost the compare-and-swap, but the entry already holds exactly the \
                         requested content at revision {}; treating the write as the no-op it is \
                         rather than reporting a failure for a write that landed",
                        current.rev
                    );
                    return Ok(WriteResult {
                        path: path.to_string(),
                        rev: current.rev,
                        conflicted: current.conflicted,
                        size: current.bytes.len() as u64,
                        mtime_ms: 0,
                        ctime_ms: 0,
                        // Unknowable on this path: the write that landed was not
                        // observed. Reported as false rather than guessed. No MCP
                        // payload is served from this flag — the tool layer derives
                        // `created` from its own read.
                        created: false,
                        resurrected: false,
                    });
                }
            }
        }
        warn!(
            "livesync write of {path} lost its compare-and-swap ({}); nothing was written",
            describe_conflict(&detail)
        );
        Err(BackendError::VersionConflict {
            path: path.to_string(),
            expected: guard.describe(),
            found: describe_conflict(&detail),
        })
    }

    /// Read a destination for the purpose of writing it: its bytes and its revision.
    ///
    /// `Ok(None)` means nothing is there. Every other failure is propagated, which is
    /// what keeps an unreachable remote or an undecryptable entry from being mistaken
    /// for a free path.
    async fn read_for_write(&self, path: &str) -> Result<Option<ExistingEntry>, BackendError> {
        match self.supervisor.read(path).await {
            Ok(result) => Ok(Some(ExistingEntry {
                bytes: match result.payload {
                    ReadPayload::Bytes(bytes) => bytes,
                    ReadPayload::Text(text) => text.into_bytes(),
                },
                rev: result.rev,
                conflicted: result.conflicted,
            })),
            Err(SidecarError::Rpc {
                kind: SidecarErrorKind::NotFound,
                ..
            }) => Ok(None),
            Err(error) => Err(map_sidecar_error(error)),
        }
    }

    /// Every conflicted path in the vault, off the cached manifest.
    ///
    /// Free: `conflicted` is already on every manifest entry and the manifest is
    /// already collected for listings, so this costs no extra round trip. Sorted, so a
    /// report is stable.
    pub async fn collect_conflicted_paths(&self) -> Result<Vec<String>, BackendError> {
        let entries = self.manifest_entries().await?;
        let mut paths: Vec<String> = entries
            .iter()
            .filter(|entry| is_listable(entry) && entry.conflicted)
            .map(|entry| entry.path.clone())
            .collect();
        paths.sort();
        Ok(paths)
    }

    /// The winning revision and every sibling revision for one path.
    ///
    /// Available on a read-only mount too, which is the point: that is exactly where a
    /// caller most needs to know the content it was served has a losing sibling.
    pub async fn conflicts(&self, path: &str) -> Result<ConflictsResult, BackendError> {
        ensure_vault_relative(path)?;
        map_sidecar(self.supervisor.conflicts(path).await)
    }

    /// One entry's full metadata, including its revision and conflicted flag.
    ///
    /// The boundary's `Stat` carries only `size_bytes` (widening it would change a
    /// frozen MCP payload), so this exists for the export path, which needs the
    /// revision it is recording to be the one that produced the bytes it wrote.
    pub async fn stat_entry(&self, path: &str) -> Result<StatResult, BackendError> {
        ensure_vault_relative(path)?;
        map_sidecar(self.supervisor.stat(path).await)
    }

    /// An entry's raw bytes and revision, or `None` when nothing is there.
    ///
    /// The pair a restore needs: the bytes to compare against the snapshot, and the
    /// revision to guard the write with — from ONE read, so no window opens between the
    /// comparison and the write.
    pub async fn read_bytes_and_version(
        &self,
        path: &str,
    ) -> Result<Option<(Vec<u8>, String)>, BackendError> {
        ensure_vault_relative(path)?;
        Ok(self
            .read_for_write(path)
            .await?
            .map(|existing| (existing.bytes, existing.rev)))
    }

    /// Write one entry's exact content, choosing its storage kind explicitly.
    ///
    /// Used by `couchdb restore`. The kind is a PARAMETER rather than inferred from the
    /// bytes because it decides whether the entry becomes a LiveSync `plain` or
    /// `newnote` document, and a wrong choice is not visible afterwards. The caller
    /// establishes it from the export manifest; see the CLI's `resolve_kind`.
    ///
    /// Goes through the same guarded write every other caller uses, so a restore cannot
    /// overwrite an edit that landed between its own read and its write either.
    pub async fn write_entry(
        &self,
        path: &str,
        content: EntryContent<'_>,
        base_version: BaseVersion,
    ) -> Result<WriteResult, BackendError> {
        self.ensure_writable()?;
        ensure_writable_path(path)?;
        let (payload, desired) = match content {
            EntryContent::Text(text) => (WritePayload::Text(text.to_string()), text.as_bytes()),
            EntryContent::Binary(bytes) => (WritePayload::Base64(encode_base64(bytes)), bytes),
        };
        self.guarded_write(path, payload, write_guard_for(&base_version), desired)
            .await
    }

    /// Line search: an imitation of ripgrep over the whole corpus.
    ///
    /// # Cost, stated rather than hidden
    ///
    /// Every call is a manifest walk plus a `read` of EVERY note the glob admits, each
    /// one a JSON-RPC round trip to the sidecar and from there to CouchDB (decrypting
    /// on the way, on an E2EE vault). There is no content cache: a grep that answered
    /// from stale text would be a worse failure than a slow one, and the manifest it
    /// scopes itself by is reused only on positive evidence that it is current
    /// ([`Self::fresh_manifest_entries`]) for the same reason. The reads
    /// run [`GREP_READ_CONCURRENCY`] at a time and stop as soon as `limit` is reached,
    /// so a narrow glob or a small `limit` is genuinely cheaper — but an unfiltered
    /// grep over a large vault reads the large vault.
    ///
    /// # Why this is `exhausted: true`
    ///
    /// Because it looked everywhere. `exhausted` distinguishes a search that examined
    /// the whole scope from one that was CANDIDATE-BOUNDED (the Algolia mount's, which
    /// evaluates the pattern over the top-N chunks a lexical prefilter returned and can
    /// therefore miss a match it never fetched). This scan has no such shortlist, so it
    /// reports what the ripgrep path reports — including under a `limit`, which
    /// truncates the OUTPUT on both paths without either of them claiming to have
    /// stopped looking. `candidate_count` stays `None` for the same reason: there was no
    /// candidate set, and a number here would invite the reader to think there was.
    ///
    /// A read that FAILS propagates. A note the manifest named and the sidecar could not
    /// serve (a remote outage, or a tombstone that landed between the walk and the read)
    /// means some lines were never examined, and there is no field on a successful
    /// outcome that can say so — `exhausted: false` would say "bounded", which is a
    /// different fact. The router turns the error into
    /// [`GrepOutcome::missing_mounts`] naming this mount on a federated grep, and a
    /// scoped grep surfaces it, which is the honest report in both shapes.
    async fn grep(
        &self,
        query: String,
        regex: bool,
        case_sensitive: bool,
        glob: Option<String>,
        context_lines: usize,
        limit: usize,
    ) -> Result<GrepOutcome, BackendError> {
        // Both compiled BEFORE the manifest walk: a bad pattern or an uninterpretable
        // glob must cost the caller a message, not N round trips to CouchDB.
        let matcher = LineMatcher::new(&query, regex, case_sensitive)?;
        let filter = GlobFilter::new(glob.as_deref())?;
        let limit = limit.max(1);

        // A manifest that is PROVABLY current, which is not the same as always freshly
        // walked: see `fresh_manifest_entries` for why an exhaustive search cannot be
        // scoped by a corpus snapshot that might predate it, and what now constitutes
        // proof that one does not.
        let entries = self.fresh_manifest_entries().await?;
        let candidates = grep_corpus(&entries, &filter);
        debug!(
            "virtual grep over {} candidate note{} on a couchdb mount",
            candidates.len(),
            if candidates.len() == 1 { "" } else { "s" }
        );

        let mut matches: Vec<GrepMatch> = Vec::new();
        // `buffered` keeps the results in candidate order, so the scan is deterministic
        // and `limit` truncates the same set on every run.
        let mut reads = stream::iter(candidates.into_iter().map(|path| async move {
            let result = self.supervisor.read(&path).await;
            (path, result)
        }))
        .buffered(GREP_READ_CONCURRENCY);
        while let Some((path, result)) = reads.next().await {
            let result = map_sidecar(result)?;
            note_conflict(&path, result.conflicted);
            let text = match result.payload {
                ReadPayload::Text(text) => text,
                // A manifest entry the sidecar classified as text but read back as
                // bytes. Skipped rather than lossily decoded: ripgrep would have
                // detected the file as binary and reported no line matches for it
                // either, so skipping is the faithful answer as well as the safe one.
                ReadPayload::Bytes(_) => continue,
            };
            if virtual_grep::collect_note_matches(
                &path,
                &text,
                &matcher,
                context_lines,
                limit,
                &mut matches,
            ) {
                break;
            }
        }
        Ok(GrepOutcome::exhaustive(matches))
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
    /// * `GrepSearch` — served by [`Self::grep`], an imitation of ripgrep over note
    ///   text rather than files. Advertised UNCONDITIONALLY, and in particular
    ///   independently of `writable`: line search is a read, and a read-only mount can
    ///   do it perfectly well. (It does not depend on a local `rg` binary either — the
    ///   pattern is evaluated in-process — so this mount can serve grep on a host where
    ///   a filesystem mount could not.)
    /// * `BinaryRead` — attachments are read through the sidecar's `read` with
    ///   `kind: "binary"`.
    /// * `Watch` — the sidecar's live change feed.
    /// * `BinaryWrite`, `Upload` — only on a `writable` mount, i.e. only when the
    ///   sidecar behind it was initialized `read-write`. A read-only mount advertises
    ///   neither and refuses both with [`COUCHDB_READ_ONLY_MESSAGE`], exactly as
    ///   before.
    /// * `SoftDelete` — only when `writable`, gated identically to the two above and for
    ///   the reason the Algolia mount states: a delete is a write, the server registers
    ///   the `delete_note` tool from this capability, and advertising it on a read-only
    ///   mount would put a tool on the surface that could only ever refuse. A LiveSync
    ///   tombstone is exactly what this capability means — see [`Self::soft_delete`].
    /// * NO `VersionHistory`, even though this mount HAS a soft delete. The two come
    ///   apart in the direction the Algolia mount does not: CouchDB retains revisions but
    ///   the sidecar protocol cannot enumerate or fetch one, and compaction may have
    ///   removed them anyway, so there is nothing `note_history` could honestly list. The
    ///   consequence for a caller is that a delete here reports no `recoverableFrom`; the
    ///   tool payload says what to do instead. See
    ///   [`COUCHDB_VERSION_HISTORY_UNSUPPORTED_MESSAGE`].
    fn descriptor(&self) -> BackendDescriptor {
        let mut capabilities = vec![
            Capability::GrepSearch,
            Capability::BinaryRead,
            Capability::Watch,
        ];
        if self.writable {
            capabilities.push(Capability::BinaryWrite);
            capabilities.push(Capability::Upload);
            capabilities.push(Capability::SoftDelete);
            // Two steps, not one — the sidecar reaches a document through `path2id`, so a
            // move is a write at the new path and a removal at the old. Advertised anyway
            // and reported `atomic: false`; see `rename_by_write_then_remove`.
            capabilities.push(Capability::Rename);
        }
        BackendDescriptor::new(BackendKind::Couchdb, capabilities)
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
            // Housekeeping, and a no-op in BOTH modes: it is documented as
            // best-effort and never-failing, and there is no staging area here to
            // sweep — the sidecar's write is all-or-nothing at the entry root, so no
            // partial artifact ever exists for a killed process to leave behind.
            // Failing it would make a caller's cleanup pass report a spurious error.
            BackendRequest::Mutation(MutationRequest::SweepOrphanStagingFiles) => {
                Ok(BackendResponse::Mutation(crate::MutationResponse::Swept))
            }
            // `resolve_divergence` is ignored: a CouchDB conflict is unreconciled at the
            // STORAGE level, so there is no server-side flag a caller could clear by
            // asserting a merge — reconciliation means writing the merged revision, which
            // is the ordinary guarded write this already is.
            BackendRequest::Mutation(MutationRequest::WriteText {
                path,
                content,
                base_version,
                ..
            }) => self
                .write_text(&path, &content, base_version)
                .await
                .map(BackendResponse::Mutation),
            // Refused rather than emulated. LiveSync has no move: doing it as
            // write-then-soft-delete is two replications, and a crash between them leaves
            // the note visible at both paths to every other participant with no way to
            // tell that from success. `Capability::Rename` is therefore not advertised —
            // see its doc, which requires atomicity to claim it.
            BackendRequest::Mutation(MutationRequest::Rename { from, to, .. }) => {
                crate::rename_by_write_then_remove(self, &from, &to)
                    .await
                    .map(BackendResponse::Mutation)
            }
            BackendRequest::Mutation(MutationRequest::SoftDelete { path }) => {
                self.soft_delete(&path).await.map(BackendResponse::Mutation)
            }
            BackendRequest::Mutation(MutationRequest::CommitUploadStream {
                path,
                expected_hash,
                max_bytes,
                chunks,
            }) => self
                .commit_upload(&path, expected_hash.as_deref(), max_bytes, chunks)
                .await
                .map(BackendResponse::Mutation),
            BackendRequest::Recall(RecallRequest::Grep {
                query,
                regex,
                case_sensitive,
                glob,
                context_lines,
                limit,
            }) => self
                .grep(query, regex, case_sensitive, glob, context_lines, limit)
                .await
                .map(|outcome| BackendResponse::Recall(RecallResponse::Grep(outcome))),
            BackendRequest::Recall(RecallRequest::Search(_)) => Err(BackendError::Unsupported(
                COUCHDB_NATIVE_RECALL_UNSUPPORTED_MESSAGE.to_string(),
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
    /// `Some`, always: a LiveSync vault genuinely can hold sibling revisions, so even
    /// an empty list is a real answer here rather than an inapplicable one.
    async fn conflicted_paths(&self) -> Result<Option<Vec<String>>, BackendError> {
        self.collect_conflicted_paths().await.map(Some)
    }

    fn as_couchdb(&self) -> Option<&CouchDbVaultBackend> {
        Some(self)
    }

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

/// Content to store, with its storage kind stated rather than inferred.
///
/// `Text` becomes a LiveSync `plain` entry and `Binary` a `newnote`; the distinction is
/// permanent once written and invisible afterwards, which is why it is a type the caller
/// must choose rather than something derived from the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryContent<'a> {
    Text(&'a str),
    Binary(&'a [u8]),
}

/// A destination as it exists right now, for a write's precondition.
struct ExistingEntry {
    bytes: Vec<u8>,
    rev: String,
    conflicted: bool,
}

/// A fully collected upload body.
struct CollectedUpload {
    bytes: Vec<u8>,
    hash: String,
}

/// The largest upload this backend has actually been exercised with, end to end.
///
/// **Advisory, not a cap.** The enforced limit is the caller's `max_bytes` (the upload
/// endpoint's own 100 MiB budget), and lowering it here would silently change a
/// documented contract for one mount kind. But a CouchDB upload is not a stream — the
/// sidecar needs whole content to chunk it, so the bytes are held once by the collector
/// and again as base64 in a single JSON-RPC line, roughly 2.3x the payload across two
/// processes. So the real ceiling is memory and Node's line handling rather than the
/// configured number, and a body above this one is served on a path no test has walked.
/// Crossing it is logged rather than refused, and this constant is what the round-trip
/// test uses, so the documented figure is a measured one.
pub const UPLOAD_COLLECT_ADVISORY_BYTES: usize = 4 * 1024 * 1024;

/// Pull the whole upload body into memory, enforcing the byte budget as it arrives.
///
/// # Why the body is buffered here, unlike on the filesystem
///
/// A LiveSync write is not a stream: the sidecar takes the complete content, runs
/// upstream's content-defined chunker over it, and publishes the chunks. There is no
/// partial-write representation to hand it, so the bytes must be whole before the
/// write starts. `max_bytes` is checked DURING collection anyway, so an oversize body
/// is refused at exactly the same byte as it would be on a filesystem mount and with
/// the same `PayloadTooLarge` (413) taxonomy — it just never reaches the remote.
///
/// The practical ceiling is therefore memory, not the configured cap: the bytes are
/// held once here and again as base64 in the request line. See the module docs.
fn collect_upload(
    max_bytes: usize,
    chunks: crate::UploadChunks,
) -> Result<CollectedUpload, BackendError> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut hasher = deep_obsidian_core::ContentHasher::new();
    for chunk in chunks.into_inner() {
        let chunk = chunk.map_err(BackendError::Message)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(BackendError::PayloadTooLarge);
        }
        hasher.update(&chunk);
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() > UPLOAD_COLLECT_ADVISORY_BYTES {
        warn!(
            "collecting a {} byte upload for a CouchDB mount, above the {} byte size this path \
             has been tested to; it is held in memory here and again as base64 in the sidecar \
             request, so expect roughly 2.3x that in peak memory across the two processes",
            bytes.len(),
            UPLOAD_COLLECT_ADVISORY_BYTES
        );
    }
    Ok(CollectedUpload {
        hash: hasher.finish(),
        bytes,
    })
}

/// The compare-and-swap precondition a caller's observation implies.
///
/// The mapping is the whole design in four lines:
///
/// * observed a revision → guard on it, so an edit that arrived after the caller's
///   `expectedHash` check loses instead of being overwritten;
/// * observed nothing → create-only, so a concurrent CREATE is reported rather than
///   clobbered (and a soft-deleted entry occupying the path is reported too, which is
///   information the caller genuinely wants);
/// * observed nothing reliably → unguarded. The sidecar still guards against the
///   revision IT read a moment earlier, so this can never fork the revision tree; it
///   just does not carry a precondition the caller never established.
fn write_guard_for(base_version: &BaseVersion) -> WriteGuard {
    match base_version {
        BaseVersion::Version(rev) => WriteGuard::Revision(rev.clone()),
        BaseVersion::Absent => WriteGuard::CreateOnly,
        BaseVersion::Unobserved => WriteGuard::Unguarded,
    }
}

/// Reject a path no write may target.
///
/// Two rules, both borrowed rather than reinvented: the sidecar's own path rules (via
/// [`ensure_vault_relative`]), and core's protected-template policy — reported with
/// [`deep_obsidian_core::vault::VaultError::ProtectedWritePath`] so the wording is
/// byte-identical to a filesystem mount's refusal. A mount kind must not decide
/// whether `Templates/` is writable.
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

/// Render a conflict detail for a human, without inventing certainty.
fn describe_conflict(detail: &ConflictDetail) -> String {
    let mut rendered = match &detail.current_rev {
        Some(rev) => format!("revision {rev}"),
        // The guarded entry is gone entirely, which a rev cannot express.
        None => "no revision at all (the entry does not exist)".to_string(),
    };
    if detail.deleted {
        rendered.push_str(", soft-deleted");
    }
    if detail.conflicted {
        rendered.push_str(", itself conflicted");
    }
    rendered
}

/// Encode bytes as standard base64.
///
/// The mirror of the decoder in `sidecar.rs`, and here for the same reason: this crate
/// has no base64 dependency and these two call sites are the only ones that need one.
fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for group in input.chunks(3) {
        let bits = (u32::from(group[0]) << 16)
            | (group.get(1).map_or(0, |byte| u32::from(*byte)) << 8)
            | group.get(2).map_or(0, |byte| u32::from(*byte));
        out.push(ALPHABET[(bits >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(bits >> 12) as usize & 0x3f] as char);
        // Padding is length-driven, so a 1- or 2-byte tail emits exactly the `=`
        // count CouchDB (and the sidecar's own decoder) expects.
        out.push(if group.len() > 1 {
            ALPHABET[(bits >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[bits as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
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
    result.map_err(map_sidecar_error)
}

/// [`map_sidecar`] for one error, where the caller has already unwrapped the result.
fn map_sidecar_error(error: SidecarError) -> BackendError {
    match &error {
        SidecarError::Rpc {
            kind: SidecarErrorKind::NotFound,
            detail,
            ..
        } => BackendError::io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            detail.clone(),
        )),
        _ => BackendError::Message(error.to_string()),
    }
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

/// The notes a virtual grep will read, sorted and glob-filtered.
///
/// # Why this is not [`walk_markdown`]
///
/// `walk_markdown` additionally requires an `.md` extension, because it feeds the
/// INDEX, which is about notes. Grep is about the caller's glob: the ripgrep path
/// searches whatever the glob admits, and `--glob '*.txt'` really does return matches
/// from `.txt` files. Reusing `walk_markdown` here would silently answer that request
/// with nothing. So the extension decision belongs to `filter`, and this only decides
/// what is READABLE AS TEXT:
///
/// * [`is_listable`] drops tombstones (a deleted entry is still a readable document, and
///   grepping one would return lines from a note the vault no longer has) and `i:`
///   internal entries;
/// * [`EntryKind::Markdown`] means "stored as text" in LiveSync's vocabulary — not only
///   `.md`. A `Binary` entry is excluded, which is also what ripgrep effectively does:
///   it detects a binary file and reports no line matches for it;
/// * [`path_is_filtered`] drops hidden and ignored subtrees, keeping grep's corpus equal
///   to the corpus every other tool on this mount sees. See [`crate::virtual_grep`] for
///   why this deliberately differs from ripgrep's `--hidden`.
///
/// The glob is applied HERE rather than after reading, so a note the caller excluded
/// costs nothing at all. Sorted and deduplicated so the scan order — and therefore a
/// `limit`-truncated result — is deterministic.
///
/// # Why `size` is not used to skip anything
///
/// Skipping `size == 0` entries was tried and REMOVED. It looks free — an empty note has
/// no lines, so it can hold no match, and ripgrep was confirmed to report nothing for an
/// empty file — but it is the only decision in the imitation that would trust METADATA
/// instead of bytes, under an outcome that claims to have looked everywhere. `size` is
/// written by whichever LiveSync client created the entry; a writer that left it at `0`
/// for a note with content would make this grep silently skip that note while reporting
/// `exhausted: true`. Ripgrep opens the file. So does this, and the saving it gives up is
/// one round trip per empty note.
fn grep_corpus(entries: &[ManifestEntry], filter: &GlobFilter) -> Vec<String> {
    let mut paths: Vec<String> = entries
        .iter()
        .filter(|entry| is_listable(entry))
        .filter(|entry| matches!(entry.kind, EntryKind::Markdown))
        .filter(|entry| !path_is_filtered(&entry.path))
        .filter(|entry| filter.is_match(&entry.path))
        .map(|entry| entry.path.clone())
        .collect();
    paths.sort();
    paths.dedup();
    paths
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
            conflict: None,
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
            conflict: None,
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
    ///
    /// `grep_search` is deliberately absent from this list: it is no longer refused at
    /// all, and the constant that used to carry its refusal is gone rather than left
    /// behind as documentation of a state the backend is not in.
    #[test]
    fn refusal_strings_say_experimental_and_read_only() {
        assert!(COUCHDB_READ_ONLY_MESSAGE.contains("EXPERIMENTAL"));
        assert!(COUCHDB_READ_ONLY_MESSAGE.contains("READ-ONLY"));
        // ...and it points at what DOES work.
        assert!(COUCHDB_READ_ONLY_MESSAGE.contains("filesystem mount"));
    }

    /// The grep corpus: tombstones, binaries, hidden and ignored subtrees are all out,
    /// the glob decides the extension, and an empty entry is not worth a round trip.
    #[test]
    fn the_grep_corpus_is_glob_filtered_text_entries_only() {
        let filter = GlobFilter::new(None).expect("glob");
        assert_eq!(
            grep_corpus(&vault(), &filter),
            vec![
                "Alpha.md".to_string(),
                "Notes/Beta.md".to_string(),
                "Notes/Deep/Gamma.md".to_string(),
                "NotesArchive/Old.md".to_string(),
            ]
        );
        // The binary attachment is never a grep candidate, whatever the glob says.
        let everything = GlobFilter::new(Some("**/*")).expect("glob");
        let corpus = grep_corpus(&vault(), &everything);
        assert!(!corpus.contains(&"Assets/logo.png".to_string()));
        // The tombstone and the ignored subtree stay out under a permissive glob too.
        assert!(!corpus.contains(&"Removed.md".to_string()));
        assert!(!corpus.contains(&"node_modules/pkg/index.md".to_string()));
    }

    /// Unlike the index's walk, the corpus does NOT require an `.md` extension: the
    /// caller's glob decides, exactly as ripgrep's does.
    #[test]
    fn a_non_markdown_glob_reaches_text_entries_the_index_walk_skips() {
        let mut entries = vault();
        entries.push(entry("Notes/Plain.txt", EntryKind::Markdown, false));
        assert!(!walk_markdown(&entries).contains(&"Notes/Plain.txt".to_string()));
        let filter = GlobFilter::new(Some("*.txt")).expect("glob");
        assert_eq!(
            grep_corpus(&entries, &filter),
            vec!["Notes/Plain.txt".to_string()]
        );
    }

    /// A zero-`size` entry is STILL read. It cannot hold a match, so reading it is
    /// wasted work — but `size` is metadata a foreign writer produced, and an exhaustive
    /// search does not decide what to skip from metadata. See `grep_corpus`.
    #[test]
    fn a_zero_size_entry_is_still_a_candidate() {
        let mut empty = entry("Empty.md", EntryKind::Markdown, false);
        empty.size = 0;
        let filter = GlobFilter::new(None).expect("glob");
        assert_eq!(
            grep_corpus(&[empty], &filter),
            vec!["Empty.md".to_string()],
            "a grep that claims to look everywhere must not trust `size`"
        );
    }

    /// The write refusal must name its ACTUAL cause and the setting that changes it.
    ///
    /// This exists because the previous wording — "refused by construction, not by
    /// configuration" and "no write path exists yet" — became false the moment
    /// `writable` did anything, and the test above could not tell: it only greps for
    /// EXPERIMENTAL and READ-ONLY, which a wrong-but-alarming message also contains.
    /// A refusal that misstates its own cause is worse than a generic one, because it
    /// actively sends the reader somewhere there is nothing to find.
    #[test]
    fn the_write_refusal_names_the_setting_that_lifts_it() {
        assert!(
            COUCHDB_READ_ONLY_MESSAGE.contains("\"writable\": true"),
            "the refusal must name the exact setting: {COUCHDB_READ_ONLY_MESSAGE}"
        );
        assert!(
            COUCHDB_READ_ONLY_MESSAGE.contains("mount configuration"),
            "the refusal must attribute itself to configuration: {COUCHDB_READ_ONLY_MESSAGE}"
        );
        // And must NOT claim the capability is unimplemented, which it no longer is.
        for false_claim in ["by construction", "no write path exists"] {
            assert!(
                !COUCHDB_READ_ONLY_MESSAGE.contains(false_claim),
                "the refusal must not claim {false_claim:?}: {COUCHDB_READ_ONLY_MESSAGE}"
            );
        }
    }
}
