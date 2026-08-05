//! Two independent participants against a REAL Algolia account.
//!
//! # What this proves that the hermetic suite cannot
//!
//! `algolia_backend.rs` covers the whole guarded-write matrix against the in-process mock,
//! and it is the contract. What it cannot cover is the one guarantee the shared-corpus
//! design actually rests on: **two participants writing the same note must never lose
//! content.** The mock serves requests one at a time from a single task, so a race against
//! it is not a race; and its object store is a `HashMap` whose write ordering is nothing
//! like an eventually-consistent search index with an asynchronous indexing queue.
//!
//! So three properties are asserted here and only here:
//!
//! * a write from a STALE base forks instead of failing, and the version it overtook is
//!   still readable afterwards;
//! * two SIMULTANEOUS writes from the same base leave a head that reassembles to exactly
//!   one participant's content — never a mix, never empty — and the loser's content
//!   survives. This is what the cutover's explicit `versionId:<previous>` delete filter
//!   exists for: a negative filter would have let each writer delete the other's chunks;
//! * search does not surface the loser's ORPHANED chunks. Chunks left in the main index but
//!   unreachable from the head would show a reader text the note no longer contains.
//!
//! Everything goes through [`VaultBackend::execute`], so what is pinned is what the SERVER
//! can observe. The raw client appears only where the boundary has no request for the
//! question — inspecting the history index directly.
//!
//! # Gating
//!
//! `#[ignore]`d, and additionally env-gated so `--include-ignored` on a machine with no
//! credentials skips loudly instead of failing. The hermetic suites are the contract; CI
//! must never require an account.
//!
//! ```sh
//! DEEP_OBSIDIAN_ALGOLIA_APP_ID=... \
//! DEEP_OBSIDIAN_ALGOLIA_API_KEY=<a WRITE key> \
//! DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX=scratch-concurrency \
//!   cargo test -p deep-obsidian-backend --test algolia_concurrency_live \
//!     -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! **`--test-threads=1` is required.** Every test in this file writes to the same index,
//! and two of them deliberately race the same note path; running them concurrently would
//! make one test's cleanup another test's lost head. The paths are distinct per test so a
//! stray parallel run degrades to confusing rather than to corrupting, but serial is the
//! only supported way.
//!
//! The index named by `_TEST_INDEX` is written to and its test notes are retracted at the
//! end of each test. Point it at a scratch index, never at a real corpus.
//!
//! # One thing deliberately changed while porting from PR #40
//!
//! #40's fixture called `std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", ...)` to feed
//! the key to a config-driven constructor. That is a data race by definition — the variable
//! is process-global, `set_var` is `unsafe` as of Rust 2024, and two tests doing it made one
//! derive its key from the other's leftovers (#40's own comment says so). Here every
//! participant is constructed DIRECTLY from an [`AlgoliaCredentials`] value, so the
//! environment is read exactly once, at the start, and never written.

use std::path::PathBuf;

use deep_obsidian_backend::algolia::{
    reads, versioning, AlgoliaCredentials, AlgoliaOptions, AlgoliaVaultBackend,
};
use deep_obsidian_backend::{
    BackendRequest, BackendResponse, BaseVersion, MutationResponse, RecallRequest, RecallResponse,
    SearchRequest, VaultBackend,
};
use secrecy::SecretString;

/// A unique cache directory per participant, so two participants in one process behave
/// like two processes: neither may serve the other's write from a shared cache.
fn temp_dir(prefix: &str) -> PathBuf {
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "dob-algolia-live-{prefix}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The live account's coordinates, read ONCE and passed around as values.
#[derive(Clone)]
struct LiveTarget {
    app_id: String,
    api_key: String,
    index: String,
}

fn live_target() -> Option<LiveTarget> {
    Some(LiveTarget {
        app_id: std::env::var("DEEP_OBSIDIAN_ALGOLIA_APP_ID").ok()?,
        api_key: std::env::var("DEEP_OBSIDIAN_ALGOLIA_API_KEY").ok()?,
        index: std::env::var("DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX").ok()?,
    })
}

macro_rules! require_live {
    () => {
        match live_target() {
            Some(target) => target,
            None => {
                eprintln!(
                    "skipping: set DEEP_OBSIDIAN_ALGOLIA_APP_ID, DEEP_OBSIDIAN_ALGOLIA_API_KEY \
                     and DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX to run the live Algolia tests"
                );
                return;
            }
        }
    };
}

/// One participant: its own backend, its own cache, its own provisioning latches.
///
/// Constructed directly rather than through the config/secret machinery, which is what
/// removes the `set_var` race. Two participants built this way share nothing but the index.
fn participant(target: &LiveTarget, who: &str) -> AlgoliaVaultBackend {
    AlgoliaVaultBackend::connect(
        AlgoliaCredentials {
            app_id: target.app_id.clone(),
            index_name: target.index.clone(),
            api_key: SecretString::new(target.api_key.clone()),
            base_url: None,
        },
        AlgoliaOptions {
            writable: true,
            participant_id: Some(who.to_string()),
            ..AlgoliaOptions::default()
        },
        &temp_dir(who),
    )
    .expect("connect a live Algolia participant")
}

/// Remove a note and its whole history, before and after each test. Best effort: a note
/// that is not there is exactly the state this wants.
async fn cleanup(backend: &AlgoliaVaultBackend, path: &str) {
    let _ = backend.retract_note(path).await;
}

/// A body with several chunkable sections, so a lost or duplicated chunk is detectable
/// rather than hidden inside a single-chunk note.
fn body(marker: &str) -> String {
    format!(
        "---\ntype: wiki-decision\nproject: Concurrency\n---\n\n\
         # Concurrent note\n\n## Decision\n\n{marker}\n\n## Rationale\n\nWritten by {marker}.\n"
    )
}

/// Write through the boundary and return the head version the note now has.
///
/// The version is read back rather than returned by the write, because
/// `MutationResponse::Written` deliberately carries only `created`. That is the server's
/// view, so it is the view this suite works with.
async fn write(
    backend: &AlgoliaVaultBackend,
    path: &str,
    content: &str,
    base: BaseVersion,
) -> Result<String, String> {
    match backend
        .execute(BackendRequest::write_text_guarded(path, content, base))
        .await
    {
        Ok(BackendResponse::Mutation(MutationResponse::Written { .. })) => {}
        Ok(other) => panic!("a write answered with {other:?}"),
        Err(error) => return Err(error.to_string()),
    }
    let head = versioning::fetch_head(backend, path)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("no head after writing {path}"))?;
    Ok(head.version_id)
}

/// `(text, head version)` for a note.
async fn read(backend: &AlgoliaVaultBackend, path: &str) -> (String, String) {
    let hydrated = reads::read_note(backend, path)
        .await
        .unwrap_or_else(|error| panic!("read {path}: {error}"));
    (hydrated.content, hydrated.note.version_id)
}

/// The chunks of one named version in one named index, empty when it holds none.
///
/// The only place the concrete index names are used: "is the loser's content in history or
/// orphaned in main" is a question about STORAGE LAYOUT, and the boundary has no request
/// that distinguishes the two — by design, since nothing the server does should care.
async fn version_chunks(
    backend: &AlgoliaVaultBackend,
    index: &str,
    path: &str,
    version_id: &str,
) -> Vec<(usize, usize, String)> {
    reads::fetch_version_chunks(backend, index, path, version_id)
        .await
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 1. A stale base forks
// ---------------------------------------------------------------------------

/// A writer whose base has been overtaken must SUCCEED, be recorded as a fork, and leave
/// the overtaken content recoverable. Nothing is silently dropped.
///
/// This is what lets the design skip compare-and-swap entirely, and it is the property that
/// most needs a real account: the mock's head pointer moves synchronously, so "the head
/// changed between my read and my write" is a scenario a mock can only simulate.
#[tokio::test]
#[ignore = "requires a live Algolia account; see the module docs"]
async fn a_stale_base_write_forks_and_loses_nothing() {
    let target = require_live!();
    let path = "_Wiki/Live concurrent stale.md";
    let alice = participant(&target, "alice@live");
    let bob = participant(&target, "bob@live");
    cleanup(&alice, path).await;

    // v0: the shared starting point both participants read.
    let v0 = write(&alice, path, &body("v0"), BaseVersion::Absent)
        .await
        .expect("the first write");
    assert!(
        alice
            .conflicted_paths()
            .await
            .expect("divergence list")
            .expect("an algolia mount always answers")
            .iter()
            .all(|divergent| divergent != path),
        "a first write cannot be a fork"
    );

    // Alice advances the head to v1, correctly based on v0.
    let v1 = write(
        &alice,
        path,
        &body("alice-v1"),
        BaseVersion::Version(v0.clone()),
    )
    .await
    .expect("a head-based write");
    assert_ne!(v1, v0);

    // Bob writes from the STALE base v0. The head is v1 by now.
    let v2 = write(
        &bob,
        path,
        &body("bob-from-stale"),
        BaseVersion::Version(v0.clone()),
    )
    .await
    .expect("a stale-based write must still succeed, not fail");
    assert_ne!(v2, v1);

    // The divergence is RECORDED, and reaches the server through the ordinary accessor
    // `vault_info` uses — not through some Algolia-specific field.
    let divergent = bob
        .conflicted_paths()
        .await
        .expect("divergence list")
        .expect("an algolia mount always answers");
    assert!(
        divergent.contains(&path.to_string()),
        "the fork must be reported as a divergence: {divergent:?}"
    );
    let history = bob
        .execute(BackendRequest::note_versions(path))
        .await
        .expect("history")
        .into_note_history()
        .expect("a history response");
    assert!(history.has_divergence, "{history:?}");
    let forked: Vec<&str> = history
        .versions
        .iter()
        .filter_map(|version| version.forked_from.as_deref())
        .collect();
    assert_eq!(
        forked,
        vec![v1.as_str()],
        "exactly one version forked, and it names the head it overtook"
    );

    // Bob's content is the head...
    let (content, head_version) = read(&alice, path).await;
    assert_eq!(content, body("bob-from-stale"));
    assert_eq!(head_version, v2);

    // ...and ALICE'S OVERTAKEN CONTENT IS STILL READABLE. Through the boundary's versioned
    // read, which is the recovery path a user actually has.
    let recovered = alice
        .execute(BackendRequest::read_text_version(path, &v1))
        .await
        .expect("the overtaken version must still be readable")
        .into_text()
        .expect("text");
    assert_eq!(
        recovered,
        body("alice-v1"),
        "the overtaken version must be recoverable byte for byte"
    );

    cleanup(&alice, path).await;
}

// ---------------------------------------------------------------------------
// 2. Simultaneous writes leave a consistent head
// ---------------------------------------------------------------------------

/// Two writers firing at once from the same base. Whoever wins, the head must reassemble to
/// exactly ONE participant's content — never a mix, never empty — and the loser's content
/// must survive somewhere.
#[tokio::test]
#[ignore = "requires a live Algolia account; see the module docs"]
async fn simultaneous_same_base_writes_leave_a_consistent_head() {
    let target = require_live!();
    let path = "_Wiki/Live concurrent simultaneous.md";
    let alice = participant(&target, "alice@live");
    let bob = participant(&target, "bob@live");
    cleanup(&alice, path).await;

    let base = write(&alice, path, &body("base"), BaseVersion::Absent)
        .await
        .expect("the base version");

    // Fire both writes at once, each claiming the same base. `tokio::join!` on one runtime
    // is not true parallelism, but the awaits interleave at every HTTP boundary, which is
    // where the cutover's steps are and therefore where they can race.
    let alice_body = body("alice-parallel");
    let bob_body = body("bob-parallel");
    let (left, right) = tokio::join!(
        alice.execute(BackendRequest::write_text_guarded(
            path,
            &alice_body,
            BaseVersion::Version(base.clone())
        )),
        bob.execute(BackendRequest::write_text_guarded(
            path,
            &bob_body,
            BaseVersion::Version(base.clone())
        )),
    );
    // NEITHER write may be rejected. That is the append-only promise, and it is the whole
    // reason this backend forks instead of returning a version conflict.
    left.expect("alice's parallel write must not be rejected");
    right.expect("bob's parallel write must not be rejected");

    // The head is exactly one participant's content. A mixed body would mean the two
    // cutovers interleaved destructively; an empty one would mean a head pointing at a
    // version whose chunks were deleted by the other writer.
    let (content, head_version) = read(&alice, path).await;
    assert!(
        [alice_body.as_str(), bob_body.as_str()].contains(&content.as_str()),
        "the head must be exactly one participant's content, got:\n{content}"
    );
    assert_ne!(head_version, base, "the head must have moved");

    // The loser's content must still exist. Reported by WHERE it survived, because the
    // distinction matters: chunks left in the MAIN index are orphans, unreachable from the
    // head, and the next test asserts search does not return them.
    let history = alice
        .execute(BackendRequest::note_versions(path))
        .await
        .expect("history")
        .into_note_history()
        .expect("a history response");
    let loser = history
        .versions
        .iter()
        .find(|version| !version.current)
        .map(|version| version.version_id.clone());
    // A history with no superseded entry at all would mean the losing version was never
    // recorded — which is the data loss this whole design exists to prevent.
    let loser = loser.unwrap_or_else(|| {
        panic!("no superseded version was recorded; the losing write vanished: {history:?}")
    });
    let in_history = version_chunks(&alice, alice.history_index(), path, &loser).await;
    let in_main = version_chunks(&alice, alice.index(), path, &loser).await;
    eprintln!(
        "loser {loser}: {} history chunk(s), {} orphaned main chunk(s)",
        in_history.len(),
        in_main.len()
    );
    assert!(
        !in_history.is_empty() || !in_main.is_empty(),
        "the losing write's content vanished: version {loser} is in neither index"
    );
    // And it is readable through the ordinary recovery path, wherever it physically lives.
    let recovered = alice
        .execute(BackendRequest::read_text_version(path, &loser))
        .await
        .expect("the losing version must be readable")
        .into_text()
        .expect("text");
    assert!(
        [alice_body.as_str(), bob_body.as_str()].contains(&recovered.as_str()),
        "the recovered loser must be one participant's content verbatim, got:\n{recovered}"
    );
    assert_ne!(recovered, content, "the loser is not the head");

    cleanup(&alice, path).await;
}

// ---------------------------------------------------------------------------
// 3. Search does not surface orphans
// ---------------------------------------------------------------------------

/// After a race, SEARCH must not return the losing version's chunks.
///
/// Those chunks are orphans — present in the main index but unreachable from the head — and
/// a hit on one shows a reader text the note no longer contains. Only a real index can
/// answer this: the mock's search reads the same `HashMap` the writes just left, with no
/// indexing queue in between, so it cannot exhibit the failure at all.
#[tokio::test]
#[ignore = "requires a live Algolia account; see the module docs"]
async fn search_never_surfaces_orphaned_chunks_after_a_race() {
    let target = require_live!();
    let path = "_Wiki/Live concurrent orphans.md";
    let alice = participant(&target, "alice@live");
    let bob = participant(&target, "bob@live");
    cleanup(&alice, path).await;

    let base = write(&alice, path, &body("base"), BaseVersion::Absent)
        .await
        .expect("the base version");

    // Markers chosen to be single lexical tokens that cannot occur in either the other
    // body or anywhere else in the index, so a hit is unambiguous.
    let alice_body = body("ALICEMARKERXYZ");
    let bob_body = body("BOBMARKERXYZ");
    let (left, right) = tokio::join!(
        alice.execute(BackendRequest::write_text_guarded(
            path,
            &alice_body,
            BaseVersion::Version(base.clone())
        )),
        bob.execute(BackendRequest::write_text_guarded(
            path,
            &bob_body,
            BaseVersion::Version(base.clone())
        )),
    );
    left.expect("alice's write");
    right.expect("bob's write");

    let (content, _) = read(&alice, path).await;
    let loser_marker = if content.contains("ALICEMARKERXYZ") {
        "BOBMARKERXYZ"
    } else {
        "ALICEMARKERXYZ"
    };

    // Through the boundary's own recall request, i.e. exactly what a scoped `hybrid_search`
    // would issue. Searching for a marker the note no longer contains must return nothing
    // that contains it.
    let response = alice
        .execute(BackendRequest::Recall(RecallRequest::Search(
            SearchRequest {
                query: loser_marker.to_string(),
                limit: 20,
                cursor: None,
            },
        )))
        .await
        .expect("search the loser's marker");
    let hits = match response {
        BackendResponse::Recall(RecallResponse::Search(response)) => response.hits,
        other => panic!("a search answered with {other:?}"),
    };
    let leaked: Vec<&str> = hits
        .iter()
        .filter(|hit| hit.snippet.contains(loser_marker))
        .map(|hit| hit.snippet.as_str())
        .collect();
    eprintln!(
        "loser={loser_marker} | {} hit(s), {} containing the marker",
        hits.len(),
        leaked.len()
    );
    // Cleaned up BEFORE the assertion, so a failure does not also leave the scratch note
    // behind for the next run to trip over.
    cleanup(&alice, path).await;
    assert!(
        leaked.is_empty(),
        "search returned {} chunk(s) of the superseded version: an orphaned chunk stays \
         reachable by search even though the note no longer contains that text:\n{leaked:?}",
        leaked.len()
    );
}
