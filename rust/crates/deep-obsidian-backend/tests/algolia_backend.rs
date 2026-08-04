//! The Algolia backend against the in-process mock Algolia.
//!
//! The port of PR #40's `shared_push.rs`, rewritten against the [`VaultBackend`]
//! boundary: everything here goes through `descriptor`/`execute`/`conflicted_paths`
//! rather than calling the runtime's internals, so what is pinned is what the server
//! can actually observe. The raw client is used only to STAGE states the boundary has
//! no request for (a tombstone, an orphaned chunk) and to inspect the history index,
//! which no boundary request exposes.
//!
//! What #40's suite covered and this does not: `seed`, `dump_all` and `retract` are
//! out of scope for this slice (there is no boundary request for any of them), and the
//! live-account suites are a follow-up.

use deep_obsidian_algolia::mock::{spawn_mock, spawn_mock_with, MockAlgolia};
use deep_obsidian_algolia::{AlgoliaClient, SearchRequest};
use deep_obsidian_backend::algolia::{AlgoliaCredentials, AlgoliaOptions, AlgoliaVaultBackend};
use deep_obsidian_backend::{
    BackendRequest, BaseVersion, Capability, RecallRequest, VaultBackend, VaultEntryKind,
};
use secrecy::SecretString;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A unique temp directory. `SystemTime` alone is NOT unique across concurrent tests
/// (microsecond resolution on macOS), so two tests in the same instant would share —
/// and evict from — one cache directory. The counter disambiguates.
fn temp_dir(prefix: &str) -> PathBuf {
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "dob-algolia-it-{prefix}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn options(writable: bool, participant: &str) -> AlgoliaOptions {
    AlgoliaOptions {
        writable,
        participant_id: Some(participant.to_string()),
        ..AlgoliaOptions::default()
    }
}

fn connect(
    base_url: &str,
    index_name: &str,
    options: AlgoliaOptions,
    index_dir: &Path,
) -> AlgoliaVaultBackend {
    AlgoliaVaultBackend::connect(
        AlgoliaCredentials {
            app_id: "TESTAPP".to_string(),
            index_name: index_name.to_string(),
            api_key: SecretString::new("test-key".to_string()),
            base_url: Some(base_url.to_string()),
        },
        options,
        index_dir,
    )
    .expect("connect the algolia backend")
}

/// A writable backend on a fresh index and a fresh cache directory.
fn writable(base_url: &str, index_name: &str) -> AlgoliaVaultBackend {
    connect(
        base_url,
        index_name,
        options(true, "paul@test"),
        &temp_dir(index_name),
    )
}

/// A raw client against the same mock, for staging and inspection.
fn raw_client(base_url: &str) -> AlgoliaClient {
    AlgoliaClient::new("TESTAPP", "test-key", Some(base_url))
}

async fn write(
    backend: &AlgoliaVaultBackend,
    path: &str,
    content: &str,
    base: BaseVersion,
) -> Result<bool, String> {
    match backend
        .execute(BackendRequest::write_text_guarded(path, content, base))
        .await
    {
        Ok(deep_obsidian_backend::BackendResponse::Mutation(
            deep_obsidian_backend::MutationResponse::Written { created },
        )) => Ok(created),
        Ok(other) => panic!("a write answered with {other:?}"),
        Err(error) => Err(error.to_string()),
    }
}

/// `(text, version)` for a note, or the error string.
async fn read(
    backend: &AlgoliaVaultBackend,
    path: &str,
) -> Result<(String, Option<String>), String> {
    backend
        .execute(BackendRequest::read_text(path))
        .await
        .map_err(|error| error.to_string())?
        .into_versioned_text()
        .map_err(|error| error.to_string())
}

async fn markdown_files(backend: &AlgoliaVaultBackend) -> Vec<String> {
    backend
        .execute(BackendRequest::walk_markdown())
        .await
        .expect("walk markdown")
        .into_markdown_files()
        .expect("markdown files")
}

// ---------------------------------------------------------------------------
// Virgin index
// ---------------------------------------------------------------------------

/// Nothing may 404 against a VIRGIN index. An Algolia index is created by its first
/// write, so before that every read answers `404 Index <name> does not exist` — and
/// the `_history` index stays absent until a note is first superseded. This walks the
/// whole read surface against a brand-new index name, then proves history provisioning
/// happens lazily on the first supersession.
#[tokio::test]
async fn a_virgin_index_never_404s_and_history_is_provisioned_lazily() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "brand-new-wiki");

    // Every read against an index nobody has written: empty, not an error.
    assert!(backend
        .execute(BackendRequest::list_children(None, false, false))
        .await
        .expect("list the root of a virgin index")
        .into_children()
        .expect("children")
        .is_empty());
    assert!(markdown_files(&backend).await.is_empty());
    assert!(backend
        .execute(BackendRequest::top_level_folders())
        .await
        .expect("folders on a virgin index")
        .into_folders()
        .expect("folders")
        .is_empty());
    // `Some(empty)`: this storage CAN record a divergence and records none right now.
    assert_eq!(
        backend
            .conflicted_paths()
            .await
            .expect("conflicted paths on a virgin index"),
        Some(Vec::new())
    );
    // A missing note is `NotFound`, which is what the server reads as "the destination
    // is free" — not a generic failure.
    let error = backend
        .execute(BackendRequest::read_text("Any.md"))
        .await
        .expect_err("a note that is not there");
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotFound));
    assert!(matches!(
        backend
            .execute(BackendRequest::stat("Any.md"))
            .await
            .expect_err("stat of a missing note")
            .io_kind(),
        Some(std::io::ErrorKind::NotFound)
    ));
    // A grep over a virgin index finds nothing rather than failing.
    assert!(grep(&backend, "anything", false, None).await.is_empty());

    // First write: the MAIN index is provisioned, the history index still does not
    // exist at all.
    write(
        &backend,
        "Decisions/Alpha.md",
        "# Alpha\n\nbody\n",
        BaseVersion::Unobserved,
    )
    .await
    .expect("first write");
    let client = raw_client(&base_url);
    let main_settings = client
        .get_settings("brand-new-wiki")
        .await
        .expect("main settings");
    assert!(
        main_settings.get("attributesForFaceting").is_some(),
        "the first write provisions the main index: {main_settings}"
    );
    let history = client
        .browse_all("brand-new-wiki_history", Some("recordType:note"))
        .await;
    assert!(
        history
            .as_ref()
            .err()
            .is_some_and(|error| error.is_index_not_found()),
        "no history index before any supersession, got {history:?}"
    );

    // Second write supersedes: the history index is created AND provisioned.
    write(
        &backend,
        "Decisions/Alpha.md",
        "# Alpha\n\nbody\n\n## More\n\nsecond version\n",
        BaseVersion::Unobserved,
    )
    .await
    .expect("second write");
    let history = client
        .browse_all("brand-new-wiki_history", Some("recordType:note"))
        .await
        .expect("the history index exists now");
    assert_eq!(history.len(), 1, "the superseded version landed in history");
    let history_settings = client
        .get_settings("brand-new-wiki_history")
        .await
        .expect("history settings");
    assert!(
        history_settings.get("attributesForFaceting").is_some(),
        "history settings are provisioned after its first write: {history_settings}"
    );
}

// ---------------------------------------------------------------------------
// Byte-exact round trips
// ---------------------------------------------------------------------------

/// Every note shape must survive the record round trip byte-for-byte, because
/// `contentHash` is over the raw bytes and a lost or invented newline breaks the MCP
/// `expectedHash` guard for that note forever.
///
/// The shapes are chosen for what they exercise in the chunker: exact section tiling,
/// the overlapping heading-less fallback (which needs line-range dedup), a
/// whitespace-only preamble the chunker drops (which needs gap-filling), and the four
/// trailing-newline variants.
#[tokio::test]
async fn writes_round_trip_byte_exactly_for_every_note_shape() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "round-trip-wiki");

    let long_body: String = (1..=90)
        .map(|n| format!("plain line {n} with enough words in it to matter\n"))
        .collect();
    let cases: Vec<(&str, String)> = vec![
        ("Simple.md", "# Simple\n\nOne short body.\n".to_string()),
        (
            "Sections.md",
            "---\ntype: wiki-decision\n---\n\n# Sections\n\n## A\n\nalpha body\n\n## B\n\nbeta body\n"
                .to_string(),
        ),
        // No headings and long enough to trigger the 12-line-overlap fallback: blind
        // concatenation would duplicate twelve lines at every boundary.
        ("Overlapping.md", long_body),
        // The chunker drops a whitespace-only preamble, leaving lines 1-2 uncovered.
        ("Preamble.md", "\n\n# Late heading\n\nbody\n".to_string()),
        ("NoTrailingNewline.md", "# Tight\n\nbody".to_string()),
        ("DoubleTrailing.md", "# Loose\n\nbody\n\n".to_string()),
    ];

    for (path, source) in &cases {
        write(&backend, path, source, BaseVersion::Absent)
            .await
            .unwrap_or_else(|error| panic!("write {path}: {error}"));
    }
    for (path, source) in &cases {
        let (text, version) = read(&backend, path)
            .await
            .unwrap_or_else(|error| panic!("read {path}: {error}"));
        assert_eq!(&text, source, "{path} did not round trip byte-exactly");
        // The version token is what a caller carries into the guarded write.
        assert!(
            version
                .as_deref()
                .is_some_and(|version| !version.is_empty()),
            "{path} must report the head version it was read at"
        );
        // ...and the hash a client would feed back as `expectedHash` matches CORE's
        // hash of the bytes that came out, which is the whole point of byte-exactness.
        assert_eq!(
            deep_obsidian_core::content_hash(text.as_bytes()),
            deep_obsidian_core::content_hash(source.as_bytes())
        );
    }

    // `Stat` reports the size the record carries, without hydrating.
    let size = backend
        .execute(BackendRequest::stat("Simple.md"))
        .await
        .expect("stat a note")
        .into_size_bytes()
        .expect("size");
    assert_eq!(size, "# Simple\n\nOne short body.\n".len() as u64);
}

/// A second read of an unchanged note is served from the disk cache — and a read after
/// someone else moved the head is NOT, because the version is part of the cache key.
#[tokio::test]
async fn the_cache_serves_an_unchanged_note_and_never_a_superseded_one() {
    let (base_url, _mock) = spawn_mock().await;
    let index_dir = temp_dir("cache-wiki");
    let backend = connect(
        &base_url,
        "cache-wiki",
        options(true, "paul@test"),
        &index_dir,
    );
    write(&backend, "A.md", "# A\n\nfirst\n", BaseVersion::Absent)
        .await
        .expect("write");
    let (first, version) = read(&backend, "A.md").await.expect("read");
    assert_eq!(first, "# A\n\nfirst\n");
    // Served from the cache this time; the content is the same either way, so what is
    // asserted is that the cache exists and holds this body.
    assert_eq!(read(&backend, "A.md").await.expect("cached read").0, first);
    assert!(
        std::fs::read_dir(index_dir.join("algolia-cache").join("cache-wiki"))
            .expect("cache dir")
            .count()
            > 1,
        "the cache directory holds a body file and its state file"
    );

    // Another participant moves the head. The cached body is keyed by the OLD version,
    // so the next read hydrates afresh instead of serving stale content.
    let other = connect(
        &base_url,
        "cache-wiki",
        options(true, "alice@test"),
        &temp_dir("cache-wiki-alice"),
    );
    write(
        &other,
        "A.md",
        "# A\n\nsecond\n",
        BaseVersion::Version(version.clone().unwrap()),
    )
    .await
    .expect("alice writes");
    assert_eq!(
        read(&backend, "A.md").await.expect("re-read").0,
        "# A\n\nsecond\n"
    );
}

// ---------------------------------------------------------------------------
// Guarded writes and divergence
// ---------------------------------------------------------------------------

/// Identical content is a no-op: no new version, and no divergence invented for two
/// participants who happened to arrive at the same text.
#[tokio::test]
async fn rewriting_identical_content_creates_no_new_version() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "idempotent-wiki");
    write(&backend, "A.md", "# A\n\nbody\n", BaseVersion::Absent)
        .await
        .expect("write");
    let (_, first) = read(&backend, "A.md").await.expect("read");

    write(&backend, "A.md", "# A\n\nbody\n", BaseVersion::Unobserved)
        .await
        .expect("rewrite");
    let (_, second) = read(&backend, "A.md").await.expect("re-read");
    assert_eq!(
        first, second,
        "an identical rewrite must not mint a version"
    );
    assert_eq!(
        backend.conflicted_paths().await.expect("divergences"),
        Some(Vec::new()),
        "two writers agreeing on the text have not diverged"
    );
    // ...and nothing was pushed to history either.
    let history = raw_client(&base_url)
        .browse_all("idempotent-wiki_history", Some("recordType:note"))
        .await;
    assert!(
        history.as_ref().map(Vec::len).unwrap_or(0) == 0,
        "an idempotent rewrite supersedes nothing: {history:?}"
    );
}

/// A stale base does NOT fail: the write lands as a fork and the divergence is
/// recorded. This is the deliberate difference from the CouchDB mount, whose write
/// reports `VersionConflict` and stores nothing — see `push_note_version`'s docs for
/// why a shared corpus cannot afford that.
#[tokio::test]
async fn a_stale_base_forks_and_records_a_divergence_rather_than_failing() {
    let (base_url, _mock) = spawn_mock().await;
    let paul = writable(&base_url, "fork-wiki");
    let alice = connect(
        &base_url,
        "fork-wiki",
        options(true, "alice@test"),
        &temp_dir("fork-wiki-alice"),
    );

    write(&paul, "Shared.md", "# Shared\n\nv1\n", BaseVersion::Absent)
        .await
        .expect("paul creates");
    let (_, v1) = read(&paul, "Shared.md").await.expect("both read v1");

    // Alice lands a version off v1.
    write(
        &alice,
        "Shared.md",
        "# Shared\n\nalice's v2\n",
        BaseVersion::Version(v1.clone().unwrap()),
    )
    .await
    .expect("alice writes");
    assert_eq!(
        paul.conflicted_paths().await.expect("divergences"),
        Some(Vec::new()),
        "a head-based write is a continuation, not a fork"
    );

    // Paul writes off the SAME v1 he read: the head has moved, so this is a fork.
    let created = write(
        &paul,
        "Shared.md",
        "# Shared\n\npaul's v2\n",
        BaseVersion::Version(v1.clone().unwrap()),
    )
    .await
    .expect("a stale base must not fail the write");
    assert!(!created, "the note already existed");

    // The write LANDED — nothing was discarded — and the divergence is visible.
    assert_eq!(
        read(&paul, "Shared.md").await.expect("read back").0,
        "# Shared\n\npaul's v2\n"
    );
    assert_eq!(
        paul.conflicted_paths().await.expect("divergences"),
        Some(vec!["Shared.md".to_string()])
    );
    // Alice's superseded content is recoverable from history, which is what makes
    // recording the divergence honest rather than a shrug.
    let history = raw_client(&base_url)
        .browse_all(
            "fork-wiki_history",
            Some("recordType:chunk AND noteId:\"Shared.md\""),
        )
        .await
        .expect("history chunks");
    let recovered: Vec<&str> = history
        .iter()
        .filter_map(|record| record.get("text").and_then(Value::as_str))
        .collect();
    assert!(
        recovered.iter().any(|text| text.contains("alice's v2")),
        "alice's superseded version must still be readable from history: {recovered:?}"
    );

    // Divergence is STICKY: a later head-based write has still not merged the fork.
    let (_, head) = read(&paul, "Shared.md").await.expect("read the head");
    write(
        &paul,
        "Shared.md",
        "# Shared\n\nv3\n",
        BaseVersion::Version(head.unwrap()),
    )
    .await
    .expect("a head-based write");
    assert_eq!(
        paul.conflicted_paths().await.expect("divergences"),
        Some(vec!["Shared.md".to_string()]),
        "the fork is still unmerged, so the note is still divergent"
    );
}

/// `BaseVersion::Absent` over a head that exists is a divergence too.
///
/// This is the arm PR #40's `Option<&str>` signature could not express: it passed
/// `None` there, the fork check never fired, and a concurrent create was silently
/// overwritten with nothing recorded.
#[tokio::test]
async fn an_absent_base_over_an_existing_head_is_a_divergence() {
    let (base_url, _mock) = spawn_mock().await;
    let paul = writable(&base_url, "absent-base-wiki");
    let alice = connect(
        &base_url,
        "absent-base-wiki",
        options(true, "alice@test"),
        &temp_dir("absent-base-alice"),
    );

    // Alice creates the note. Paul read the path a moment earlier and saw nothing.
    write(
        &alice,
        "Race.md",
        "# Race\n\nalice got there\n",
        BaseVersion::Absent,
    )
    .await
    .expect("alice creates");
    write(
        &paul,
        "Race.md",
        "# Race\n\npaul got there\n",
        BaseVersion::Absent,
    )
    .await
    .expect("paul's create must land, not fail");

    assert_eq!(
        paul.conflicted_paths().await.expect("divergences"),
        Some(vec!["Race.md".to_string()]),
        "a create that landed on top of a concurrent create is a recorded divergence"
    );
}

/// `BaseVersion::Unobserved` asserts no precondition, so it must not invent a
/// divergence — a caller that never read cannot have based its content on anything.
#[tokio::test]
async fn an_unobserved_base_never_records_a_divergence() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "unobserved-wiki");
    write(&backend, "A.md", "# A\n\none\n", BaseVersion::Unobserved)
        .await
        .expect("create");
    write(&backend, "A.md", "# A\n\ntwo\n", BaseVersion::Unobserved)
        .await
        .expect("overwrite");
    assert_eq!(
        backend.conflicted_paths().await.expect("divergences"),
        Some(Vec::new())
    );
}

// ---------------------------------------------------------------------------
// Tombstones
// ---------------------------------------------------------------------------

/// A tombstoned note vanishes from reads and listings but stays recoverable, and a
/// write brings it back.
///
/// The tombstone is staged through the raw client because the boundary has no delete
/// request — there is no `MutationRequest::Delete`, so the READ side of tombstone
/// semantics is what this slice can and must pin.
#[tokio::test]
async fn a_tombstoned_note_leaves_reads_and_listings_but_a_write_resurrects_it() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "tombstone-wiki");
    write(
        &backend,
        "Doomed.md",
        "# Doomed\n\nbody\n",
        BaseVersion::Absent,
    )
    .await
    .expect("create");
    write(&backend, "Kept.md", "# Kept\n\nbody\n", BaseVersion::Absent)
        .await
        .expect("create the survivor");
    assert_eq!(markdown_files(&backend).await.len(), 2);

    // Stage the tombstone exactly as a soft delete would leave it: the head record
    // marked deleted with no body, and its chunks gone from the main index.
    let client = raw_client(&base_url);
    let head = client
        .get_objects("tombstone-wiki", &["note:Doomed.md".to_string()])
        .await
        .expect("fetch the head")
        .pop()
        .flatten()
        .expect("a head record");
    let mut tombstone = head.clone();
    tombstone["deleted"] = json!(true);
    tombstone["chunkCount"] = json!(0);
    tombstone["sizeBytes"] = json!(0);
    client
        .save_objects_awaited("tombstone-wiki", vec![tombstone])
        .await
        .expect("write the tombstone");
    client
        .delete_by_query(
            "tombstone-wiki",
            "recordType:chunk AND noteId:\"Doomed.md\"",
        )
        .await
        .expect("remove the chunks");

    // Reads: absent, and absent as `NotFound` so a write over it is a create.
    let error = backend
        .execute(BackendRequest::read_text("Doomed.md"))
        .await
        .expect_err("a tombstone is not a note");
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotFound));
    assert_eq!(
        backend
            .execute(BackendRequest::stat("Doomed.md"))
            .await
            .expect_err("stat of a tombstone")
            .io_kind(),
        Some(std::io::ErrorKind::NotFound)
    );
    // Listings: the tombstone is not a file, so it is not listed.
    assert_eq!(markdown_files(&backend).await, vec!["Kept.md".to_string()]);
    let children = backend
        .execute(BackendRequest::list_children(None, false, false))
        .await
        .expect("list root")
        .into_children()
        .expect("children");
    let listed: Vec<&str> = children.iter().map(|child| child.path.as_str()).collect();
    assert_eq!(listed, vec!["Kept.md"]);
    // Grep: a tombstoned note has no chunks left, so its text cannot surface.
    assert!(grep(&backend, "Doomed", false, None).await.is_empty());

    // A write resurrects it, and reports itself as a CREATE — which is what the tool
    // layer turns into `created: true` for the caller.
    let created = write(
        &backend,
        "Doomed.md",
        "# Doomed\n\nback again\n",
        BaseVersion::Absent,
    )
    .await
    .expect("resurrect");
    assert!(created, "a write over a tombstone is a create");
    assert_eq!(
        read(&backend, "Doomed.md").await.expect("read").0,
        "# Doomed\n\nback again\n"
    );
    assert_eq!(markdown_files(&backend).await.len(), 2);
    // ...and the resurrection is NOT a divergence. A tombstone reads as absent, so the
    // caller's `BaseVersion::Absent` was a true observation; marking every undelete as
    // divergent would fill this list with notes nobody disagreed about.
    assert_eq!(
        backend.conflicted_paths().await.expect("divergences"),
        Some(Vec::new()),
        "resurrecting a soft-deleted note must not record a divergence"
    );
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// The retention floor is honoured and everything beyond it is purged.
///
/// `max_age_days: 0` makes the ceiling keep nothing, so what survives is the floor
/// alone — the only way to observe the floor's effect inside one test run, where every
/// version is milliseconds old.
#[tokio::test]
async fn retention_purges_beyond_the_floor_and_keeps_the_floor() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = connect(
        &base_url,
        "retention-wiki",
        AlgoliaOptions {
            writable: true,
            participant_id: Some("paul@test".to_string()),
            retention_min_versions: Some(2),
            retention_max_age_days: Some(0),
            ..AlgoliaOptions::default()
        },
        &temp_dir("retention-wiki"),
    );

    for revision in 1..=6 {
        write(
            &backend,
            "Churn.md",
            &format!("# Churn\n\nrevision {revision}\n"),
            BaseVersion::Unobserved,
        )
        .await
        .unwrap_or_else(|error| panic!("write {revision}: {error}"));
    }

    let client = raw_client(&base_url);
    let history = client
        .browse_all(
            "retention-wiki_history",
            Some("recordType:note AND noteId:\"Churn.md\""),
        )
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        2,
        "the floor keeps exactly 2 superseded versions, got {:?}",
        history
            .iter()
            .filter_map(|record| record.get("versionId"))
            .collect::<Vec<_>>()
    );
    // The purge removes a version's CHUNKS too, not just its note record — otherwise
    // history would grow without bound while looking pruned.
    let history_chunks = client
        .browse_all(
            "retention-wiki_history",
            Some("recordType:chunk AND noteId:\"Churn.md\""),
        )
        .await
        .expect("history chunks");
    let kept_versions: Vec<&str> = history
        .iter()
        .filter_map(|record| record.get("versionId").and_then(Value::as_str))
        .collect();
    for chunk in &history_chunks {
        let version = chunk
            .get("versionId")
            .and_then(Value::as_str)
            .expect("a chunk has a version");
        assert!(
            kept_versions.contains(&version),
            "chunk of purged version {version} survived: kept {kept_versions:?}"
        );
    }
    // The head itself is untouched by retention: it is not history.
    assert_eq!(
        read(&backend, "Churn.md").await.expect("read the head").0,
        "# Churn\n\nrevision 6\n"
    );
}

// ---------------------------------------------------------------------------
// Anti-enumeration
// ---------------------------------------------------------------------------

/// A 403 `objectID not allowed` — a SECURED key whose restriction excludes the object
/// — must read as the SAME not-found a genuinely missing note produces.
///
/// Surfacing the difference would let a scoped participant tell "exists but hidden"
/// from "does not exist" and walk the difference to enumerate paths outside their
/// scope. Verified live in PR #40; pinned here against a mock that answers the real
/// 403 message.
#[tokio::test]
async fn a_forbidden_object_id_reads_as_a_missing_note() {
    let (base_url, _mock) = spawn_mock_with(MockAlgolia::with_forbidden_object_ids(vec![
        "note:Secret.md".to_string(),
    ]))
    .await;
    let backend = writable(&base_url, "scoped-wiki");
    // Something must exist, so the failure cannot be confused with a virgin index.
    write(
        &backend,
        "Public.md",
        "# Public\n\nbody\n",
        BaseVersion::Absent,
    )
    .await
    .expect("create the visible note");

    let forbidden = backend
        .execute(BackendRequest::read_text("Secret.md"))
        .await
        .expect_err("a forbidden object reads as absent");
    let missing = backend
        .execute(BackendRequest::read_text("NeverExisted.md"))
        .await
        .expect_err("a missing note");
    assert_eq!(forbidden.io_kind(), Some(std::io::ErrorKind::NotFound));
    // Indistinguishable apart from the path each names: normalizing the path away must
    // leave two identical messages, or the difference is a side channel.
    assert_eq!(
        forbidden.to_string().replace("Secret.md", "<path>"),
        missing.to_string().replace("NeverExisted.md", "<path>"),
    );
    // The error must not leak WHY: no mention of a key, a scope or a permission.
    let rendered = forbidden.to_string().to_lowercase();
    for leak in ["forbidden", "not allowed", "api key", "403"] {
        assert!(!rendered.contains(leak), "{rendered} leaks {leak:?}");
    }
}

// ---------------------------------------------------------------------------
// Listings
// ---------------------------------------------------------------------------

/// Folders are synthesized from facets, and the ordering is CORE's — directories
/// first, then files, each group by path — so a caller cannot tell which backend
/// answered from the shape of a listing.
#[tokio::test]
async fn listings_synthesize_folders_and_keep_cores_ordering() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "listing-wiki");
    for path in [
        "Alpha.md",
        "Notes/Beta.md",
        "Notes/Deep/Gamma.md",
        "NotesArchive/Old.md",
        "Zeta.md",
    ] {
        write(
            &backend,
            path,
            &format!("# {path}\n\nbody\n"),
            BaseVersion::Absent,
        )
        .await
        .unwrap_or_else(|error| panic!("write {path}: {error}"));
    }

    let children = backend
        .execute(BackendRequest::list_children(None, false, false))
        .await
        .expect("list root")
        .into_children()
        .expect("children");
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
            ("Notes", true),
            ("NotesArchive", true),
            ("Alpha.md", false),
            ("Zeta.md", false),
        ]
    );
    // A synthesized folder has no size, exactly as a real directory reports.
    assert!(children[0].size_bytes.is_none());
    let alpha = children
        .iter()
        .find(|child| child.path == "Alpha.md")
        .expect("a file child");
    assert_eq!(alpha.size_bytes, Some("# Alpha.md\n\nbody\n".len() as u64));
    assert!(alpha.is_markdown);
    assert_eq!(alpha.name, "Alpha.md");

    // The prefix match is segment-aware: `Notes` must not swallow `NotesArchive`.
    let nested = backend
        .execute(BackendRequest::list_children(
            Some("Notes".to_string()),
            false,
            false,
        ))
        .await
        .expect("list Notes")
        .into_children()
        .expect("children");
    let rendered: Vec<&str> = nested.iter().map(|child| child.path.as_str()).collect();
    assert_eq!(rendered, vec!["Notes/Deep", "Notes/Beta.md"]);

    // ...and a leading/trailing slash is tolerated, as on a filesystem mount.
    assert_eq!(
        backend
            .execute(BackendRequest::list_children(
                Some("/Notes/".to_string()),
                false,
                false
            ))
            .await
            .expect("list /Notes/")
            .into_children()
            .expect("children")
            .len(),
        2
    );

    // WalkMarkdown is sorted, which is what fixes note and chunk ids downstream.
    assert_eq!(
        markdown_files(&backend).await,
        vec![
            "Alpha.md".to_string(),
            "Notes/Beta.md".to_string(),
            "Notes/Deep/Gamma.md".to_string(),
            "NotesArchive/Old.md".to_string(),
            "Zeta.md".to_string(),
        ]
    );
    assert_eq!(
        backend
            .execute(BackendRequest::top_level_folders())
            .await
            .expect("folders")
            .into_folders()
            .expect("folders"),
        vec!["Notes".to_string(), "NotesArchive".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Grep
// ---------------------------------------------------------------------------

async fn grep(
    backend: &AlgoliaVaultBackend,
    query: &str,
    regex: bool,
    glob: Option<&str>,
) -> Vec<deep_obsidian_backend::GrepMatch> {
    backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: query.to_string(),
            regex,
            case_sensitive: false,
            glob: glob.map(str::to_string),
            context_lines: 1,
            limit: 50,
        }))
        .await
        .expect("grep")
        .into_grep_matches()
        .expect("matches")
}

/// The bounded grep finds matches with the NOTE's line numbers, honours a glob, and
/// refuses a pattern it cannot prefilter rather than answering it misleadingly.
#[tokio::test]
async fn grep_finds_matches_honours_globs_and_refuses_an_anchorless_pattern() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "grep-wiki");
    write(
        &backend,
        "Decisions/Retention.md",
        "# Retention\n\n## Policy\n\nThe retention policy keeps five versions.\n",
        BaseVersion::Absent,
    )
    .await
    .expect("write");
    write(
        &backend,
        "Syntheses/Narrative.md",
        "# Narrative\n\nThe retention story is a different one.\n",
        BaseVersion::Absent,
    )
    .await
    .expect("write");

    // A literal query.
    let matches = grep(&backend, "retention policy", false, None).await;
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].path, "Decisions/Retention.md");
    assert_eq!(
        matches[0].line_number, 5,
        "line numbers are the NOTE's, not the chunk's: {:?}",
        matches[0]
    );
    assert!(matches[0].line_text.contains("retention policy"));
    assert_eq!(matches[0].submatches[0].text, "retention policy");

    // A regex with a usable literal anchor reaches both notes.
    let matches = grep(&backend, r"retention\s+\w+", true, None).await;
    let paths: Vec<&str> = matches.iter().map(|item| item.path.as_str()).collect();
    assert!(paths.contains(&"Decisions/Retention.md"), "{paths:?}");
    assert!(paths.contains(&"Syntheses/Narrative.md"), "{paths:?}");

    // A glob narrows to one folder, and does so EXACTLY rather than by prefix.
    let matches = grep(&backend, "retention", false, Some("Decisions/*.md")).await;
    let mut paths: Vec<&str> = matches.iter().map(|item| item.path.as_str()).collect();
    paths.dedup();
    assert_eq!(
        paths,
        vec!["Decisions/Retention.md"],
        "the glob must exclude Syntheses/, which also mentions retention"
    );
    assert!(grep(&backend, "retention", false, Some("Nowhere/*.md"))
        .await
        .is_empty());

    // A pattern with no literal anchor is REFUSED, with a message that says why.
    let error = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: "[a-z]+".to_string(),
            regex: true,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
            limit: 10,
        }))
        .await
        .expect_err("an anchorless regex must be refused");
    assert_eq!(
        error.to_string(),
        deep_obsidian_backend::algolia::grep::ALGOLIA_GREP_NO_ANCHOR_MESSAGE
    );
}

/// Orphaned chunks — a losing concurrent writer's, still in the main index because the
/// delete filter is deliberately narrow — must never reach a reader.
#[tokio::test]
async fn orphaned_chunks_of_a_superseded_version_never_surface_in_grep() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "orphan-wiki");
    write(
        &backend,
        "A.md",
        "# A\n\nthe surviving sentence about kangaroos\n",
        BaseVersion::Absent,
    )
    .await
    .expect("write");

    // Stage a chunk record from a version that is not the head — exactly what a losing
    // concurrent writer leaves behind.
    raw_client(&base_url)
        .save_objects_awaited(
            "orphan-wiki",
            vec![json!({
                "objectID": "chunk:A.md@v-orphan#0",
                "recordType": "chunk",
                "noteId": "A.md",
                "versionId": "v-orphan",
                "path": "A.md",
                "dir": "",
                "folders": {},
                "title": "A",
                "headings": [],
                "chunkIndex": 0,
                "startLine": 1,
                "endLine": 1,
                "text": "an orphaned sentence about kangaroos",
                "updatedAtMs": 1,
                "participantId": "ghost@test",
            })],
        )
        .await
        .expect("stage the orphan");

    let matches = grep(&backend, "kangaroos", false, None).await;
    let texts: Vec<&str> = matches.iter().map(|item| item.line_text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["the surviving sentence about kangaroos"],
        "an orphaned version's text must not be reported as the note's content"
    );
}

// ---------------------------------------------------------------------------
// Capabilities, health and read-only mounts
// ---------------------------------------------------------------------------

/// A read-only mount serves reads and refuses writes, and its descriptor is identical
/// to a writable one's (nothing writes-only is advertised either way).
#[tokio::test]
async fn a_read_only_mount_reads_but_refuses_writes() {
    let (base_url, _mock) = spawn_mock().await;
    let author = writable(&base_url, "read-only-wiki");
    write(&author, "A.md", "# A\n\nbody\n", BaseVersion::Absent)
        .await
        .expect("seed through a writable handle");

    let reader = connect(
        &base_url,
        "read-only-wiki",
        options(false, "reader@test"),
        &temp_dir("read-only-reader"),
    );
    assert_eq!(
        read(&reader, "A.md").await.expect("read").0,
        "# A\n\nbody\n"
    );
    assert_eq!(
        write(&reader, "A.md", "# A\n\nedited\n", BaseVersion::Unobserved)
            .await
            .expect_err("a read-only mount refuses"),
        deep_obsidian_backend::ALGOLIA_READ_ONLY_MESSAGE
    );
    // ...and the refusal left the note untouched.
    assert_eq!(
        read(&reader, "A.md").await.expect("re-read").0,
        "# A\n\nbody\n"
    );
    assert!(reader.descriptor().supports(Capability::GrepSearch));
}

/// Health reports an index nobody has written as REACHABLE: a virgin corpus is empty,
/// not down. An unreachable endpoint reports false.
#[tokio::test]
async fn health_distinguishes_a_virgin_index_from_an_unreachable_one() {
    let (base_url, _mock) = spawn_mock().await;
    let reachable = writable(&base_url, "never-written-wiki");
    assert!(matches!(
        reachable
            .execute(BackendRequest::health_overview())
            .await
            .expect("health"),
        deep_obsidian_backend::BackendResponse::Health(
            deep_obsidian_backend::HealthResponse::Overview { reachable: true }
        )
    ));

    // Port 1 on loopback refuses connections, so this is a transport failure rather
    // than an API answer.
    let unreachable = connect(
        "http://127.0.0.1:1",
        "unreachable-wiki",
        options(true, "paul@test"),
        &temp_dir("unreachable"),
    );
    assert!(matches!(
        unreachable
            .execute(BackendRequest::health_overview())
            .await
            .expect("health never errors"),
        deep_obsidian_backend::BackendResponse::Health(
            deep_obsidian_backend::HealthResponse::Overview { reachable: false }
        )
    ));
}

/// The chunk-fetch that hydrates a note runs with `distinct` OFF. With the index-level
/// `distinct` on `path` left enabled, a multi-chunk note would come back as ONE chunk
/// and the body would silently be a fragment — so this pins the failure the flag
/// prevents, by asking the same question both ways.
#[tokio::test]
async fn hydration_needs_distinct_off_to_see_every_chunk() {
    let (base_url, _mock) = spawn_mock().await;
    let backend = writable(&base_url, "distinct-wiki");
    // Long enough sections that the chunker keeps them apart: a short note merges into
    // a single chunk, and a single-chunk note cannot demonstrate anything about
    // `distinct`.
    let section = |name: &str| {
        let body: String = (1..=60)
            .map(|line| format!("{name} body line {line} with several words in it\n"))
            .collect();
        format!("## {name}\n\n{body}\n")
    };
    let source = format!(
        "# Multi\n\n{}{}{}",
        section("Alpha"),
        section("Beta"),
        section("Gamma")
    );
    write(&backend, "Multi.md", &source, BaseVersion::Absent)
        .await
        .expect("write");
    assert_eq!(read(&backend, "Multi.md").await.expect("read").0, source);

    let client = raw_client(&base_url);
    let filters = "recordType:chunk AND noteId:\"Multi.md\"".to_string();
    let with_distinct = client
        .search(
            "distinct-wiki",
            &SearchRequest {
                filters: Some(filters.clone()),
                hits_per_page: Some(1000),
                distinct: Some(true),
                ..SearchRequest::default()
            },
        )
        .await
        .expect("search with distinct");
    let without_distinct = client
        .search(
            "distinct-wiki",
            &SearchRequest {
                filters: Some(filters),
                hits_per_page: Some(1000),
                distinct: Some(false),
                ..SearchRequest::default()
            },
        )
        .await
        .expect("search without distinct");
    assert!(
        without_distinct.hits.len() > with_distinct.hits.len(),
        "distinct collapses a multi-chunk note to {} of {} chunks, which is why the read \
         path disables it",
        with_distinct.hits.len(),
        without_distinct.hits.len()
    );
}
