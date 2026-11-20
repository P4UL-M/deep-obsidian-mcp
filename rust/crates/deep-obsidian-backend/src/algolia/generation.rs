//! The mount's generation sentinel, and the listing cache it validates.
//!
//! # The problem
//!
//! Every whole-corpus listing on an Algolia mount is a `browse` — cursor-followed to
//! exhaustion, because a `search` caps at 1000 hits and a truncated corpus listing is
//! worse than a slow one. `resources/list` performs exactly one such browse per call,
//! and `vault_info` another for divergence, and both are asked repeatedly by an
//! interactive client that is doing nothing in particular. Against the mock that is
//! 4.6x more work than it needs to be; against a real account it is real network round
//! trips, once per call, forever.
//!
//! The obvious fix — cache the listing for N seconds — introduces a staleness class
//! that did not exist: a write through this mount would not be visible to the next
//! listing, and no field on the payload could say so.
//!
//! # The mechanism
//!
//! One record in the main index, [`GENERATION_OBJECT_ID`], holding an opaque token.
//! Every Deep Obsidian write path replaces it with a fresh one. A listing is cached
//! against the token that was current when it was built, and reused only while the
//! token is unchanged.
//!
//! That makes the cache **validated, not merely timed**: an unchanged token is
//! positive evidence that no Deep Obsidian write has landed since, which is a
//! different and much stronger statement than "it has been less than N seconds".
//!
//! The token is a fresh value rather than an incremented counter, so a bump is one
//! write and never a read-modify-write. Two concurrent bumps therefore cannot lose
//! each other's effect: whichever lands second wins, and either value differs from
//! the one every cache is holding, which is all the cache asks of it.
//!
//! # There is no time window, and that is a deliberate reversal
//!
//! The sentinel is read on EVERY listing. The obvious refinement — don't re-read it
//! within N seconds, to bound the lookups as well as the browses — was implemented and
//! then removed, because it quietly reintroduced the staleness the sentinel exists to
//! abolish: for up to N seconds a process would serve a cached listing without checking
//! anything at all, so another participant's write would be invisible even though the
//! sentinel had already moved. `a_tombstoned_note_leaves_reads_and_listings_but_a_write_resurrects_it`
//! caught exactly that, and it was right to.
//!
//! The argument for the window was that Algolia's own asynchronous indexing (~1-3 s from
//! a successful save to the object being searchable) already delays visibility by about
//! as much, so the window hid inside an existing envelope. That is true on average and
//! not true in particular, and "usually no staler than the index" is not a property this
//! codebase states about a read.
//!
//! What it costs to do without: one `getObject` of one small record per listing, in place
//! of a cursor-followed whole-corpus browse. That is the optimisation almost entirely —
//! the browse was never expensive because of the request count.
//!
//! **This mount's own writes never consult the sentinel at all**: [`bump`] drops the
//! local cache synchronously in the write path, so a write-then-list through one server
//! is exact with no wait and no lookup.
//!
//! # Out of contract
//!
//! A writer that mutates the index through Algolia's raw API without bumping the
//! sentinel is never noticed — the token has not moved, so every cache goes on
//! believing itself current, indefinitely. That is stated rather than defended: such a
//! writer already breaks this mount's versioning invariants (it would leave chunk
//! records orphaned from their head, and history records unwritten), so it was never
//! supported. The mount's own CLI, its MCP tools and its index refresh all go through
//! the write paths that bump. A test that stages corpus state with the raw client must
//! move the sentinel itself; `stage_generation_bump` in `algolia_backend.rs` is that,
//! and it exists so the requirement is visible rather than folded into a fixture.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tracing::debug;

use super::{map_algolia, AlgoliaVaultBackend};
use crate::BackendError;

/// The sentinel's object id.
///
/// Namespaced `meta:` so it can never collide with a `note:<path>` or
/// `chunk:<...>` record, and so a human reading the index can see at a glance that it
/// is not content.
pub const GENERATION_OBJECT_ID: &str = "meta:generation";

/// The `recordType` the sentinel carries.
///
/// Set EXPLICITLY rather than omitted. Every read path filters on
/// `recordType:note` or `recordType:chunk`, and a record with no `recordType` at all
/// would be excluded only because a filter on an absent attribute is false — true of
/// Algolia and of the mock, but true by accident. A positive value that is neither
/// makes the exclusion a property of the record rather than of a technicality.
pub const GENERATION_RECORD_TYPE: &str = "meta";

/// The listing kinds this cache holds. `&'static str` keys so a typo is a compile
/// error at the call site rather than a silent second cache entry.
pub(crate) const WALK_MARKDOWN: &str = "walkMarkdown";
pub(crate) const TOP_LEVEL_FOLDERS: &str = "topLevelFolders";
pub(crate) const DIVERGENT_PATHS: &str = "divergentPaths";

/// The sentinel record to write. `token` is opaque and only ever compared.
fn generation_record(token: &str) -> Value {
    json!({
        "objectID": GENERATION_OBJECT_ID,
        "recordType": GENERATION_RECORD_TYPE,
        "token": token,
    })
}

/// A fresh token, distinct from every token this process has minted.
///
/// Wall clock plus a per-process counter. The counter is what makes it correct: two
/// bumps inside the same millisecond are entirely possible, and a clock that goes
/// backwards (NTP, a suspended laptop) must not be able to mint a token that some
/// cache is already holding.
fn mint_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    format!(
        "{millis}-{}-{:x}",
        COUNTER.fetch_add(1, Ordering::SeqCst),
        std::process::id()
    )
}

/// Cached whole-corpus listings and the sentinel reading that validates them.
#[derive(Default)]
pub(crate) struct ListingCache {
    state: Mutex<ListingState>,
}

#[derive(Default)]
struct ListingState {
    /// The token the entries were built under. `None` means "no reading we can use",
    /// which is the state after a local write and after any failure to read the
    /// sentinel — in both cases nothing may be served from here.
    token: Option<String>,
    entries: HashMap<&'static str, Arc<Vec<String>>>,
}

impl std::fmt::Debug for ListingCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ListingCache { .. }")
    }
}

impl ListingCache {
    /// Drop everything, and forget the sentinel reading with it.
    ///
    /// Called from every local write path. This is what makes a write-then-list exact
    /// with no wait: the cache does not consult the sentinel to discover a write it made
    /// itself, it is told.
    fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.token = None;
            state.entries.clear();
        }
    }

    /// Record a sentinel reading, dropping every entry if the token moved.
    fn observe(&self, token: &str) {
        if let Ok(mut state) = self.state.lock() {
            if state.token.as_deref() != Some(token) {
                state.entries.clear();
                state.token = Some(token.to_string());
            }
        }
    }

    fn get(&self, kind: &'static str, token: &str) -> Option<Arc<Vec<String>>> {
        let state = self.state.lock().ok()?;
        (state.token.as_deref() == Some(token))
            .then(|| state.entries.get(kind).cloned())
            .flatten()
    }

    fn put(&self, kind: &'static str, token: &str, values: Arc<Vec<String>>) {
        if let Ok(mut state) = self.state.lock() {
            // Only if the token has not moved since the browse started. Storing against
            // a token the cache no longer holds would resurrect a listing that a write
            // has already invalidated.
            if state.token.as_deref() == Some(token) {
                state.entries.insert(kind, values);
            }
        }
    }
}

/// Replace the sentinel, then drop this process's cache.
///
/// # Ordering, and why the local clear comes second
///
/// The remote write first: if it fails, this process must not be left believing it has
/// announced a change it has not. Then the local clear, which cannot fail. A caller
/// racing between the two sees the pre-write cache, which is correct — the write it
/// races has not landed either.
///
/// # Awaited
///
/// One small object, and its task is awaited for the same reason the head pointer's is:
/// tasks on one index are processed in order, so a bump that is awaited also guarantees
/// every unawaited write of the same mutation has landed. Awaiting it therefore costs
/// one round trip and buys the whole mutation's visibility.
///
/// # A failed bump is not a failed write
///
/// The error is LOGGED, never propagated. The sentinel is a cache-invalidation hint;
/// the note is already written. Failing the caller's `upsert_note` because a cache hint
/// could not be updated would turn a performance mechanism into a source of write
/// failures. A lost bump costs other processes a stale listing until their re-check
/// window elapses and they browse again — bounded, and strictly better than the
/// alternative.
pub(crate) async fn bump(backend: &AlgoliaVaultBackend) {
    let token = mint_token();
    let outcome = map_algolia(
        backend
            .client()
            .save_objects_awaited(backend.index(), vec![generation_record(&token)])
            .await,
    );
    match outcome {
        Ok(()) => debug!(
            "bumped the generation sentinel on Algolia index '{}' to {token}",
            backend.index()
        ),
        Err(error) => debug!(
            "could not bump the generation sentinel on Algolia index '{}' ({error}); this \
             process's own listings are still exact because its cache is dropped below, but \
             another process may serve a listing that predates this write until its own \
             re-check window elapses",
            backend.index()
        ),
    }
    backend.listings().clear();
}

/// The token to validate a listing against, or `None` when there is no usable reading.
///
/// `None` is the safe answer and it is returned generously: a missing sentinel (a
/// corpus nothing has written yet), an index that does not exist, and a secured API key
/// scoped so that `meta:generation` cannot be addressed at all all land here. In every
/// one of those cases the caller browses, exactly as it did before this existed.
async fn current_token(backend: &AlgoliaVaultBackend) -> Option<String> {
    let fetched = backend
        .client()
        .get_objects(backend.index(), &[GENERATION_OBJECT_ID.to_string()])
        .await;
    let token = match fetched {
        Ok(objects) => objects.into_iter().next().flatten().and_then(|record| {
            record
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        Err(error) => {
            // Not an error the caller can act on, and not one it should fail for. A
            // secured key that forbids addressing this object is a legitimate
            // configuration; so is an index that has never been written.
            debug!(
                "could not read the generation sentinel on Algolia index '{}' ({error}); \
                 listings will browse rather than be cached",
                backend.index()
            );
            None
        }
    };
    let token = token?;
    backend.listings().observe(&token);
    Some(token)
}

/// Serve `kind` from the cache when the sentinel says nothing has changed, otherwise
/// run `collect` and cache what it returns.
///
/// The listing is `Arc`-shared rather than cloned per call: `walk_markdown` on a large
/// corpus is tens of thousands of strings and the callers only read them.
pub(crate) async fn cached<Collect, Fut>(
    backend: &AlgoliaVaultBackend,
    kind: &'static str,
    collect: Collect,
) -> Result<Arc<Vec<String>>, BackendError>
where
    Collect: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, BackendError>>,
{
    // Read BEFORE the browse, never after: a token sampled afterwards could have been
    // minted by a write that the browse missed, and the listing would then be cached as
    // if it included one. Same rule, and same reason, as the CouchDB manifest's epoch.
    let token = current_token(backend).await;
    if let Some(token) = &token {
        if let Some(cached) = backend.listings().get(kind, token) {
            debug!(
                "served the {kind} listing for Algolia index '{}' from cache: the generation \
                 sentinel is unchanged at {token}",
                backend.index()
            );
            return Ok(cached);
        }
    }
    let values = Arc::new(collect().await?);
    if let Some(token) = &token {
        backend.listings().put(kind, token, values.clone());
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentinel_record_is_neither_a_note_nor_a_chunk() {
        let record = generation_record("t1");
        assert_eq!(record["objectID"], json!(GENERATION_OBJECT_ID));
        // The two positive facts every read path's filter depends on.
        assert_eq!(record["recordType"], json!("meta"));
        assert_ne!(record["recordType"], json!("note"));
        assert_ne!(record["recordType"], json!("chunk"));
        // And the three absences: `path` and `dir` keep it out of the listings that
        // filter on those locally, `noteId` keeps it out of every delete-by-query.
        for absent in ["path", "dir", "noteId", "folders"] {
            assert!(
                record.get(absent).is_none(),
                "the sentinel must carry no {absent}"
            );
        }
    }

    #[test]
    fn minted_tokens_never_repeat_even_within_one_millisecond() {
        let tokens: std::collections::BTreeSet<String> = (0..1000).map(|_| mint_token()).collect();
        assert_eq!(tokens.len(), 1000, "a repeated token would freeze a cache");
    }

    #[test]
    fn a_moved_token_drops_every_entry() {
        let cache = ListingCache::default();
        cache.observe("t1");
        cache.put(WALK_MARKDOWN, "t1", Arc::new(vec!["A.md".to_string()]));
        assert!(cache.get(WALK_MARKDOWN, "t1").is_some());

        cache.observe("t2");
        assert!(
            cache.get(WALK_MARKDOWN, "t1").is_none(),
            "a listing must never be served against a superseded token"
        );
        assert!(cache.get(WALK_MARKDOWN, "t2").is_none(), "nor a fresh one");
    }

    #[test]
    fn a_local_clear_forgets_the_reading_as_well_as_the_entries() {
        let cache = ListingCache::default();
        cache.observe("t1");
        cache.put(WALK_MARKDOWN, "t1", Arc::new(vec!["A.md".to_string()]));

        cache.clear();
        assert!(cache.get(WALK_MARKDOWN, "t1").is_none());
    }

    #[test]
    fn a_listing_collected_across_an_invalidation_is_not_cached() {
        let cache = ListingCache::default();
        cache.observe("t1");
        // The browse started under t1; a write landed while it ran.
        cache.clear();
        cache.put(WALK_MARKDOWN, "t1", Arc::new(vec!["Stale.md".to_string()]));
        assert!(
            cache.get(WALK_MARKDOWN, "t1").is_none(),
            "a listing whose collection overlapped a write must be discarded"
        );
    }
}
