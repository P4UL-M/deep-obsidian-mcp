//! Secured-API-key scoping against a REAL Algolia account.
//!
//! # Why this cannot be hermetic
//!
//! This is the actual team-sharing path: an owner mints a filter-restricted search key, a
//! teammate mounts the corpus with it, and the restriction is enforced **by Algolia**, not
//! by anything in this repository. That is the entire point — a scope our own client
//! enforced would be a scope a modified client could lift. The mock does not implement
//! secured keys, ACLs, or per-object authorization, so there is nothing here it could check.
//!
//! Three properties:
//!
//! * a scoped key sees only its prefix, through LISTING, SEARCH and DIRECT READ, and an
//!   out-of-scope path is indistinguishable from one that does not exist — so a teammate
//!   cannot use the difference to enumerate what they may not see;
//! * a scoped key cannot WRITE, and the refusal comes from Algolia even when the mount
//!   config claims `writable`;
//! * a WRITE-CAPABLE parent is refused by the derivation itself. This is the #40 live
//!   finding that motivates the whole check: a secured key INHERITS its parent's ACLs, and
//!   its `filters` restriction constrains SEARCH ONLY, so a key derived from a write key
//!   reads a narrow slice while writing anywhere in the index.
//!
//! # Gating
//!
//! `#[ignore]`d and env-gated. Two DISTINCT keys are needed and the distinction is the
//! subject matter: seeding the fixtures needs write, and the secured key's parent must be
//! search-only or it inherits write access.
//!
//! ```sh
//! DEEP_OBSIDIAN_ALGOLIA_APP_ID=... \
//! DEEP_OBSIDIAN_ALGOLIA_OWNER_KEY=<a WRITE key; seeds the fixtures> \
//! DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY=<a SEARCH-ONLY key; the secured-key parent> \
//! DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX=scratch-securedkey \
//!   cargo test -p deep-obsidian-backend --test algolia_secured_key_live \
//!     -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! **`--test-threads=1` is required**: these tests share one index and seed overlapping
//! fixture paths, so a parallel run would have one test's cleanup delete another's fixture.
//!
//! Optional: set `DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY_HAS_BROWSE=1` when the search-only
//! parent also carries the `browse` ACL. The root-listing assertion flips on it, and that
//! is deliberate — see [`a_scoped_key_sees_only_its_prefix`]'s last section.
//!
//! Like the concurrency suite, every backend here is built from an explicit
//! [`AlgoliaCredentials`] value rather than by mutating `DEEP_OBSIDIAN_ALGOLIA_API_KEY`,
//! which is what lets the owner and the scoped teammate be driven side by side in one
//! process without racing each other's credential.

use std::path::PathBuf;

use deep_obsidian_backend::algolia::{
    reads, versioning, AlgoliaCredentials, AlgoliaOptions, AlgoliaVaultBackend,
};
use deep_obsidian_backend::{
    BackendError, BackendRequest, BackendResponse, RecallRequest, RecallResponse, SearchRequest,
    VaultBackend, VaultEntryKind,
};
use secrecy::SecretString;

/// The in-scope note, and the one outside the scope that must stay invisible.
const PUBLIC: &str = "_Wiki/Decisions/Scoped public.md";
const PRIVATE: &str = "_Agent/Sessions/Scoped private.md";

/// The restriction `algolia key --prefix _Wiki` produces, verbatim. Hard-coded rather than
/// imported so a change to the CLI's encoding shows up HERE as a live-behaviour change
/// rather than being silently tracked.
const WIKI_RESTRICTION: &str = "filters=folders.lvl0%3A%22_Wiki%22";

fn temp_dir(prefix: &str) -> PathBuf {
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "dob-algolia-key-{prefix}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[derive(Clone)]
struct LiveTarget {
    app_id: String,
    /// A write key. Seeds the fixtures and cleans them up.
    owner_key: String,
    /// A SEARCH-ONLY key. The secured key's parent.
    search_key: String,
    index: String,
}

fn live_target() -> Option<LiveTarget> {
    Some(LiveTarget {
        app_id: std::env::var("DEEP_OBSIDIAN_ALGOLIA_APP_ID").ok()?,
        owner_key: std::env::var("DEEP_OBSIDIAN_ALGOLIA_OWNER_KEY").ok()?,
        search_key: std::env::var("DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY").ok()?,
        index: std::env::var("DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX").ok()?,
    })
}

macro_rules! require_live {
    () => {
        match live_target() {
            Some(target) => target,
            None => {
                eprintln!(
                    "skipping: set DEEP_OBSIDIAN_ALGOLIA_APP_ID, DEEP_OBSIDIAN_ALGOLIA_OWNER_KEY \
                     (a write key), DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY (a search-only key) and \
                     DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX to run the live secured-key tests"
                );
                return;
            }
        }
    };
}

/// A backend on the live index with an explicitly supplied key.
fn mount_with_key(
    target: &LiveTarget,
    who: &str,
    key: &str,
    writable: bool,
) -> AlgoliaVaultBackend {
    AlgoliaVaultBackend::connect(
        AlgoliaCredentials {
            app_id: target.app_id.clone(),
            index_name: target.index.clone(),
            api_key: SecretString::new(key.to_string()),
            base_url: None,
        },
        AlgoliaOptions {
            writable,
            participant_id: Some(who.to_string()),
            ..AlgoliaOptions::default()
        },
        &temp_dir(who),
    )
    .expect("connect a live Algolia mount")
}

fn note(marker: &str, note_type: &str) -> String {
    format!(
        "---\ntype: {note_type}\nproject: KeyScope\n---\n\n\
         # {marker}\n\n## Body\n\n{marker} content.\n"
    )
}

/// Seed the two fixture notes with the OWNER key, from a clean slate.
async fn seed_fixtures(target: &LiveTarget) -> AlgoliaVaultBackend {
    let owner = mount_with_key(target, "owner@keyscope", &target.owner_key, true);
    let _ = owner.retract_note(PUBLIC).await;
    let _ = owner.retract_note(PRIVATE).await;
    for (path, marker, note_type) in [
        (PUBLIC, "PUBLICMARK", "wiki-decision"),
        (PRIVATE, "PRIVATEMARK", "agent-session"),
    ] {
        owner
            .execute(BackendRequest::write_text(path, note(marker, note_type)))
            .await
            .unwrap_or_else(|error| panic!("seed {path}: {error}"));
    }
    owner
}

async fn cleanup(owner: &AlgoliaVaultBackend) {
    let _ = owner.retract_note(PUBLIC).await;
    let _ = owner.retract_note(PRIVATE).await;
}

async fn child_names(backend: &AlgoliaVaultBackend, path: &str) -> Result<Vec<String>, String> {
    backend
        .execute(BackendRequest::list_children(
            Some(path.to_string()),
            false,
            false,
        ))
        .await
        .map_err(|error| error.to_string())?
        .into_children()
        .map(|entries| entries.into_iter().map(|entry| entry.name).collect())
        .map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// 1. Scope is enforced through every read
// ---------------------------------------------------------------------------

/// A `folders.lvl0:"_Wiki"` secured key exposes the wiki and hides everything else — through
/// listing, search AND direct reads — and an out-of-scope path is indistinguishable from a
/// path that does not exist.
///
/// The indistinguishability is the security property, not a nicety. Algolia answers a 403
/// `objectID not allowed` for an out-of-scope object; surfacing that verbatim would let a
/// teammate probe which paths exist outside their scope, one path at a time.
#[tokio::test]
#[ignore = "requires a live Algolia account; see the module docs"]
async fn a_scoped_key_sees_only_its_prefix() {
    let target = require_live!();
    let owner = seed_fixtures(&target).await;

    let secured =
        deep_obsidian_algolia::generate_secured_api_key(&target.search_key, WIKI_RESTRICTION);
    let teammate = mount_with_key(&target, "teammate@keyscope", &secured, false);

    // LISTING a named folder uses facet + search only, so a search-only key serves it. (The
    // mount ROOT additionally needs `browse`; asserted at the end.)
    let wiki = child_names(&teammate, "_Wiki")
        .await
        .expect("list the in-scope folder");
    assert!(
        wiki.iter().any(|name| name == "Decisions"),
        "the in-scope subfolder must be visible: {wiki:?}"
    );
    let decisions = child_names(&teammate, "_Wiki/Decisions")
        .await
        .expect("list the in-scope subfolder");
    assert!(
        decisions.iter().any(|name| name == "Scoped public.md"),
        "the in-scope note must be listed: {decisions:?}"
    );
    assert!(
        !decisions
            .iter()
            .any(|name| name.to_lowercase().contains("private")),
        "an out-of-scope note leaked into a listing: {decisions:?}"
    );

    // The in-scope note READS normally.
    let public = teammate
        .execute(BackendRequest::read_text(PUBLIC))
        .await
        .expect("the in-scope note reads")
        .into_text()
        .expect("text");
    assert!(public.contains("PUBLICMARK"), "{public}");

    // SEARCH cannot reach the out-of-scope note.
    let response = teammate
        .execute(BackendRequest::Recall(RecallRequest::Search(
            SearchRequest {
                query: "PRIVATEMARK".to_string(),
                limit: 20,
                cursor: None,
            },
        )))
        .await
        .expect("search the out-of-scope marker");
    let hits = match response {
        BackendResponse::Recall(RecallResponse::Search(response)) => response.hits,
        other => panic!("a search answered with {other:?}"),
    };
    assert!(
        hits.iter().all(|hit| !hit.snippet.contains("PRIVATEMARK")),
        "search leaked out-of-scope content: {hits:?}"
    );
    assert!(
        hits.iter().all(|hit| hit.path != PRIVATE),
        "search leaked an out-of-scope PATH even without its text: {hits:?}"
    );

    // A DIRECT READ of the out-of-scope note must look exactly like a read of a path that
    // does not exist: same error kind, so the two cannot be told apart.
    let out_of_scope = teammate
        .execute(BackendRequest::read_text(PRIVATE))
        .await
        .expect_err("an out-of-scope read must fail");
    let nonexistent = teammate
        .execute(BackendRequest::read_text(
            "_Wiki/Decisions/No such note at all.md",
        ))
        .await
        .expect_err("a nonexistent read must fail");
    assert_eq!(
        out_of_scope.io_kind(),
        Some(std::io::ErrorKind::NotFound),
        "an out-of-scope read must report absence, got: {out_of_scope}"
    );
    assert_eq!(
        out_of_scope.io_kind(),
        nonexistent.io_kind(),
        "out-of-scope and nonexistent must be indistinguishable: {out_of_scope} vs {nonexistent}"
    );
    assert!(
        matches!(out_of_scope, BackendError::Io { .. }),
        "an out-of-scope read must not surface a transport or authorization error: \
         {out_of_scope:?}"
    );

    // A documented LIMITATION, pinned so it cannot regress silently: enumerating the mount
    // ROOT uses `browse`, a distinct ACL from `search`. A parent without it fails there
    // while every scoped read above still works — which is exactly why `algolia key` warns
    // when the parent lacks `browse` instead of refusing.
    let root = teammate
        .execute(BackendRequest::list_children(None, false, false))
        .await;
    if std::env::var("DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY_HAS_BROWSE").is_ok() {
        let entries = root
            .expect("a browse-capable parent lists the root")
            .into_children()
            .expect("children");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(
            names.contains(&"_Wiki"),
            "the in-scope folder must be visible at the root: {names:?}"
        );
        assert!(
            !names.contains(&"_Agent"),
            "an out-of-scope folder leaked into the root listing: {names:?}"
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry.kind == VaultEntryKind::Directory || entry.name.ends_with(".md")),
            "the root listing must hold folders and notes only: {entries:?}"
        );
    } else {
        assert!(
            root.is_err(),
            "a parent key without `browse` is expected to fail the ROOT listing; if this now \
             succeeds, the browse warning in `algolia key` is obsolete and must be dropped"
        );
    }

    cleanup(&owner).await;
}

// ---------------------------------------------------------------------------
// 2. A scoped key cannot write
// ---------------------------------------------------------------------------

/// A secured key derived from a search-only parent cannot write, and Algolia says so even
/// when the mount config claims `writable`.
///
/// Deliberately configured `writable: true` so the request actually reaches the network:
/// this asserts the REMOTE's refusal, not our own read-only guard (which
/// `algolia_backend.rs` already pins hermetically).
#[tokio::test]
#[ignore = "requires a live Algolia account; see the module docs"]
async fn a_scoped_key_cannot_write() {
    let target = require_live!();
    let secured =
        deep_obsidian_algolia::generate_secured_api_key(&target.search_key, WIKI_RESTRICTION);
    let teammate = mount_with_key(&target, "teammate@keyscope", &secured, true);
    let path = "_Wiki/Decisions/Teammate attempt.md";
    let owner = mount_with_key(&target, "owner@keyscope", &target.owner_key, true);
    let _ = owner.retract_note(path).await;

    let error = teammate
        .execute(BackendRequest::write_text(
            path,
            note("TEAMMATEWRITE", "wiki-decision"),
        ))
        .await
        .expect_err("a search-only secured key must not be able to write");
    let rendered = error.to_string().to_lowercase();
    assert!(
        rendered.contains("not enough rights")
            || rendered.contains("not allowed")
            || rendered.contains("403"),
        "expected an authorization failure from Algolia, got: {error}"
    );

    // Nothing landed. Checked with the OWNER key, which can see everything.
    assert!(
        versioning::fetch_head(&owner, path)
            .await
            .expect("head lookup")
            .is_none(),
        "the refused write must not have created a note"
    );
    // ...and no orphaned chunk was left behind either: the cutover pushes chunks first, so
    // a write that failed at the head-pointer step would leave text search could match.
    assert!(
        reads::fetch_version_chunks(&owner, owner.index(), path, "any")
            .await
            .unwrap_or_default()
            .is_empty(),
        "the refused write left chunk records behind"
    );
}

// ---------------------------------------------------------------------------
// 3. A write-capable parent is refused by the derivation
// ---------------------------------------------------------------------------

/// The #40 finding, asserted against the account that produced it: the owner's WRITE key
/// really does report write ACLs, and the classification really does refuse it.
///
/// The refusal logic itself is unit-tested hermetically in `algolia_cmd.rs`; what needs an
/// account is the premise — that `key_acls` on a live write key returns an ACL in
/// `WRITE_ACLS`, and that a search-only key does not. If Algolia ever changed either, the
/// unit test would keep passing while the guard silently stopped guarding.
#[tokio::test]
#[ignore = "requires a live Algolia account; see the module docs"]
async fn a_write_capable_parent_is_refused_and_a_search_only_one_is_not() {
    let target = require_live!();

    let owner_client =
        deep_obsidian_algolia::AlgoliaClient::new(&target.app_id, &target.owner_key, None);
    let owner_acls = owner_client
        .key_acls(&target.owner_key)
        .await
        .expect("the owner key's ACLs are readable");
    let write: Vec<&String> = owner_acls
        .iter()
        .filter(|acl| deep_obsidian_algolia::WRITE_ACLS.contains(&acl.as_str()))
        .collect();
    assert!(
        !write.is_empty(),
        "DEEP_OBSIDIAN_ALGOLIA_OWNER_KEY must be a WRITE key for this suite to mean anything; \
         its ACLs are {owner_acls:?}"
    );

    let search_client =
        deep_obsidian_algolia::AlgoliaClient::new(&target.app_id, &target.search_key, None);
    let search_acls = search_client
        .key_acls(&target.search_key)
        .await
        .expect("the search key's ACLs are readable");
    assert!(
        !search_acls
            .iter()
            .any(|acl| deep_obsidian_algolia::WRITE_ACLS.contains(&acl.as_str())),
        "DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY must be SEARCH-ONLY; its ACLs are {search_acls:?}"
    );
    assert!(
        search_acls.iter().any(|acl| acl == "search"),
        "a search-only parent must actually be able to search: {search_acls:?}"
    );

    // And the reason the refusal exists at all: a key derived from the WRITE parent would
    // still be able to write, because the filter restriction constrains search only. Proven
    // rather than asserted — this is the finding.
    let derived_from_write =
        deep_obsidian_algolia::generate_secured_api_key(&target.owner_key, WIKI_RESTRICTION);
    let dangerous = mount_with_key(&target, "dangerous@keyscope", &derived_from_write, true);
    let path = "_Wiki/Decisions/Inherited write.md";
    let owner = mount_with_key(&target, "owner@keyscope", &target.owner_key, true);
    let _ = owner.retract_note(path).await;
    let outcome = dangerous
        .execute(BackendRequest::write_text(
            path,
            note("INHERITEDWRITE", "wiki-decision"),
        ))
        .await;
    let _ = owner.retract_note(path).await;
    assert!(
        outcome.is_ok(),
        "the whole reason `algolia key` refuses a write-capable parent is that the derived key \
         CAN still write. If this now fails, Algolia has changed its secured-key semantics and \
         the refusal in classify_parent_key should be revisited: {outcome:?}"
    );
}
