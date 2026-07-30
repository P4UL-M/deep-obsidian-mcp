//! Concurrency tests against a REAL Algolia account.
//!
//! The mock is synchronous and single-threaded, so it cannot exercise the one
//! guarantee the whole shared-wiki design rests on: two participants writing
//! the same note must never lose content. These tests therefore talk to a live
//! account and are `#[ignore]`d by default.
//!
//! Run with:
//! ```text
//! DEEP_OBSIDIAN_ALGOLIA_APP_ID=... \
//! DEEP_OBSIDIAN_ALGOLIA_API_KEY=... \
//! DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX=scratch-concurrency \
//!   cargo test -p deep-obsidian-server --test shared_concurrency_live -- --ignored --test-threads=1
//! ```
//! The index is created on first write and emptied at the end of each test.

use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_server::shared::{connect_mount, reads, versioning, SharedMountRuntime};
use deep_obsidian_types::SharedMountConfig;
use std::fs;
use std::path::PathBuf;

fn temp_dir(prefix: &str) -> PathBuf {
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{unique}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Live credentials, or `None` so the test can skip loudly rather than fail.
fn live_env() -> Option<(String, String, String)> {
    Some((
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_APP_ID").ok()?,
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_API_KEY").ok()?,
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX").ok()?,
    ))
}

/// A mount for one participant. Each gets its own runtime (own cache, own
/// provisioning flags) so the two behave like two independent processes.
fn participant(app_id: &str, index: &str, who: &str) -> SharedMountRuntime {
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", std::env::var("DEEP_OBSIDIAN_ALGOLIA_API_KEY").unwrap());
    let config = SharedMountConfig {
        mount_at: "_Shared/Team/".to_string(),
        app_id: app_id.to_string(),
        index_name: index.to_string(),
        key_ref: None,
        base_url: None, // real Algolia
        writable: true,
        participant_id: Some(who.to_string()),
        cache: None,
        retention: None,
    };
    connect_mount(&config, &SecretResolver::new(), &temp_dir("conc-index"))
        .expect("connect live mount")
}

async fn cleanup(mount: &SharedMountRuntime, path: &str) {
    let _ = versioning::retract_note(mount, path).await;
}

fn body(marker: &str) -> String {
    format!(
        "---\ntype: wiki-decision\nproject: Concurrency\n---\n\n\
         # Concurrent note\n\n## Decision\n\n{marker}\n\n## Rationale\n\nWritten by {marker}.\n"
    )
}

/// The semantically important case: a writer whose base version has been
/// overtaken. Its write must SUCCEED, be flagged as a fork, and leave the
/// overtaken content recoverable — nothing silently dropped.
#[tokio::test]
#[ignore = "requires a live Algolia account; see module docs"]
async fn stale_base_write_forks_and_loses_nothing() {
    let Some((app_id, _key, index)) = live_env() else {
        panic!("set DEEP_OBSIDIAN_ALGOLIA_APP_ID / _API_KEY / _TEST_INDEX");
    };
    let path = "_Wiki/Concurrent stale.md";
    let alice = participant(&app_id, &index, "alice@live");
    let bob = participant(&app_id, &index, "bob@live");
    cleanup(&alice, path).await;

    // v0: the shared starting point both participants read.
    let v0 = versioning::push_note_version(&alice, path, &body("v0"), &[], None, false)
        .await
        .expect("v0");
    assert!(v0.forked_from.is_none(), "first write cannot be a fork");

    // Alice advances the head to v1.
    let v1 = versioning::push_note_version(
        &alice,
        path,
        &body("alice-v1"),
        &[],
        Some(&v0.version_id),
        false,
    )
    .await
    .expect("v1");
    assert!(v1.forked_from.is_none(), "head-based write is not a fork");
    assert!(!v1.has_divergence);

    // Bob writes from the STALE base v0 — head is now v1.
    let v2 = versioning::push_note_version(
        &bob,
        path,
        &body("bob-from-stale"),
        &[],
        Some(&v0.version_id),
        false,
    )
    .await
    .expect("stale-based write must still succeed");
    assert_eq!(
        v2.forked_from.as_deref(),
        Some(v1.version_id.as_str()),
        "the fork must name the head it overtook"
    );
    assert!(v2.has_divergence, "divergence must be recorded");

    // Bob's content is the head now...
    let head = reads::read_note(&alice, path).await.expect("read head");
    assert_eq!(head.content, body("bob-from-stale"));
    assert_eq!(head.note.version_id, v2.version_id);

    // ...and ALICE'S OVERTAKEN CONTENT IS STILL RECOVERABLE. This is the
    // guarantee that lets the design skip compare-and-swap entirely.
    let overtaken = reads::fetch_version_chunks(
        &alice,
        &alice.history_index,
        path,
        &v1.version_id,
    )
    .await
    .expect("history chunks for the overtaken version");
    assert!(
        !overtaken.is_empty(),
        "the overtaken version must be in history, not gone"
    );
    assert_eq!(reads::reassemble_chunks(overtaken), body("alice-v1"));

    cleanup(&alice, path).await;
}

/// Two writers firing simultaneously from the same base. Whoever wins, the
/// head must reassemble to exactly ONE participant's content — never a mix,
/// never empty — and the loser's content must survive somewhere.
///
/// This is what the cutover's explicit `versionId:vPrev` delete filter exists
/// for: a negative filter would have let each writer delete the other's chunks.
#[tokio::test]
#[ignore = "requires a live Algolia account; see module docs"]
async fn simultaneous_writes_leave_a_consistent_head() {
    let Some((app_id, _key, index)) = live_env() else {
        panic!("set DEEP_OBSIDIAN_ALGOLIA_APP_ID / _API_KEY / _TEST_INDEX");
    };
    let path = "_Wiki/Concurrent simultaneous.md";
    let alice = participant(&app_id, &index, "alice@live");
    let bob = participant(&app_id, &index, "bob@live");
    cleanup(&alice, path).await;

    let base = versioning::push_note_version(&alice, path, &body("base"), &[], None, false)
        .await
        .expect("base");

    // Fire both writes at once, each based on `base`.
    let alice_body = body("alice-parallel");
    let bob_body = body("bob-parallel");
    let base_id = base.version_id.clone();
    let (left, right) = tokio::join!(
        versioning::push_note_version(&alice, path, &alice_body, &[], Some(&base_id), false),
        versioning::push_note_version(&bob, path, &bob_body, &[], Some(&base_id), false),
    );
    let left = left.expect("alice parallel write");
    let right = right.expect("bob parallel write");

    // Neither write may be rejected — that is the append-only promise.
    assert_ne!(left.version_id, right.version_id);

    // The head must be one of the two, and reassemble to that participant's
    // content exactly. A mixed or empty body would mean the cutover raced
    // destructively.
    let head = reads::read_note(&alice, path).await.expect("read head");
    let candidates = [body("alice-parallel"), body("bob-parallel")];
    assert!(
        candidates.contains(&head.content),
        "head must be exactly one participant's content, got:\n{}",
        head.content
    );
    assert!(
        head.note.version_id == left.version_id || head.note.version_id == right.version_id,
        "head version must be one of the two writes"
    );

    // The loser's content must still exist — in history (superseded) or as the
    // other version's chunks. Losing it outright is the failure this whole
    // design is built to prevent.
    let loser_version = if head.note.version_id == left.version_id {
        right.version_id.clone()
    } else {
        left.version_id.clone()
    };
    let in_history =
        reads::fetch_version_chunks(&alice, &alice.history_index, path, &loser_version)
            .await
            .unwrap_or_default();
    let in_main = reads::fetch_version_chunks(&alice, alice.index(), path, &loser_version)
        .await
        .unwrap_or_default();
    assert!(
        !in_history.is_empty() || !in_main.is_empty(),
        "the losing write's content vanished: version {loser_version} is in neither \
index — this breaks the no-data-loss guarantee"
    );

    // Report where it survived, since that distinction matters: chunks left in
    // the MAIN index are orphans (not reachable from the head) and would show
    // up in search as stale content.
    eprintln!(
        "loser {loser_version}: {} history chunk(s), {} orphaned main chunk(s)",
        in_history.len(),
        in_main.len()
    );

    cleanup(&alice, path).await;
}

/// After a simultaneous race, SEARCH must not surface the losing version's
/// content: those chunks are orphans — unreachable from the head — and a hit on
/// them shows a reader text that is no longer the note.
#[tokio::test]
#[ignore = "requires a live Algolia account; see module docs"]
async fn search_does_not_surface_orphaned_chunks_after_a_race() {
    let Some((app_id, _key, index)) = live_env() else {
        panic!("set DEEP_OBSIDIAN_ALGOLIA_APP_ID / _API_KEY / _TEST_INDEX");
    };
    let path = "_Wiki/Concurrent orphans.md";
    let alice = participant(&app_id, &index, "alice@live");
    let bob = participant(&app_id, &index, "bob@live");
    cleanup(&alice, path).await;

    let base = versioning::push_note_version(&alice, path, &body("base"), &[], None, false)
        .await
        .expect("base");

    let alice_body = body("ALICEMARKERXYZ");
    let bob_body = body("BOBMARKERXYZ");
    let base_id = base.version_id.clone();
    let (left, right) = tokio::join!(
        versioning::push_note_version(&alice, path, &alice_body, &[], Some(&base_id), false),
        versioning::push_note_version(&bob, path, &bob_body, &[], Some(&base_id), false),
    );
    let left = left.expect("alice");
    let right = right.expect("bob");

    let head = reads::read_note(&alice, path).await.expect("head");
    let loser_marker = if head.note.version_id == left.version_id {
        "BOBMARKERXYZ"
    } else {
        "ALICEMARKERXYZ"
    };
    let _ = right;

    let stale = deep_obsidian_server::shared::retrieval::search_mount_with_distinct(
        &alice,
        loser_marker,
        20,
        Some(false),
    )
    .await
    .expect("search for the loser marker");
    let leaked = stale
        .iter()
        .filter(|hit| hit.text.contains(loser_marker))
        .count();

    eprintln!("loser={loser_marker} | stale search hits: {leaked}");
    cleanup(&alice, path).await;

    assert_eq!(
        leaked, 0,
        "search returned {leaked} chunk(s) of the superseded version: orphaned chunks \
from a race stay reachable by search even though the note no longer contains that text"
    );
}
