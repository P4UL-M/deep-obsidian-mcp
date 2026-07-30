//! Secured-API-key scoping against a REAL Algolia account.
//!
//! This is the actual team-sharing path: an owner mints a filter-restricted
//! search key, a teammate mounts the index with it, and the restriction is
//! enforced by Algolia rather than by our client. The mock cannot check any of
//! that, so these tests are `#[ignore]`d and env-gated like the concurrency
//! ones.
//!
//! ```text
//! DEEP_OBSIDIAN_ALGOLIA_APP_ID=... \
//! DEEP_OBSIDIAN_ALGOLIA_OWNER_KEY=<write key, seeds the fixtures> \
//! DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY=<SEARCH-ONLY key, the secured-key parent> \
//! DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX=scratch-securedkey \
//!   cargo test -p deep-obsidian-server --test shared_secured_key_live -- --ignored --test-threads=1
//! ```

use deep_obsidian_algolia::generate_secured_api_key;
use deep_obsidian_server::shared::{reads, versioning, SharedMountRuntime};
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

/// (app id, owner write key, search-only parent key, index).
///
/// The two keys are distinct on purpose: seeding needs write, and a secured key
/// must be derived from a search-only parent or it inherits write access.
fn live_env() -> Option<(String, String, String, String)> {
    Some((
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_APP_ID").ok()?,
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_OWNER_KEY").ok()?,
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY").ok()?,
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX").ok()?,
    ))
}

/// Builds a mount whose key is passed explicitly, so the owner and the scoped
/// teammate can be driven side by side in one process.
fn mount_with_key(app_id: &str, index: &str, who: &str, key: &str, writable: bool) -> SharedMountRuntime {
    // Built directly rather than through `connect_mount`, which resolves the key
    // from the environment: mutating a shared env var made one test derive its
    // key from another test's leftovers.
    let config = SharedMountConfig {
        mount_at: "_Shared/Team/".to_string(),
        app_id: app_id.to_string(),
        index_name: index.to_string(),
        key_ref: None,
        base_url: None,
        writable,
        participant_id: Some(who.to_string()),
        cache: None,
        retention: None,
    };
    let dir = temp_dir("key-index");
    SharedMountRuntime {
        client: deep_obsidian_algolia::AlgoliaClient::new(app_id, key, None),
        history_index: deep_obsidian_server::shared::history_index_name(index),
        cache: deep_obsidian_server::shared::cache::NoteCache::open(
            dir.join("cache"),
            64 * 1024 * 1024,
            Vec::new(),
        )
        .expect("cache"),
        config,
        recall_stage: std::sync::Mutex::new("lexical".to_string()),
        history_provisioned: std::sync::atomic::AtomicBool::new(false),
        main_provisioned: std::sync::atomic::AtomicBool::new(false),
    }
}

fn note(marker: &str, note_type: &str) -> String {
    format!(
        "---\ntype: {note_type}\nproject: KeyScope\n---\n\n\
         # {marker}\n\n## Body\n\n{marker} content.\n"
    )
}

const PUBLIC: &str = "_Wiki/Decisions/Scoped public.md";
const PRIVATE: &str = "_Agent/Sessions/Scoped private.md";

/// A `folders.lvl0:_Wiki` secured key must expose the wiki and hide everything
/// else — through search, listing AND direct reads — and an out-of-scope path
/// must be indistinguishable from a path that does not exist, so a teammate
/// cannot enumerate what they may not see.
#[tokio::test]
#[ignore = "requires a live Algolia account; see module docs"]
async fn secured_key_scopes_reads_and_hides_out_of_scope_paths() {
    let Some((app_id, owner_key, search_key, index)) = live_env() else {
        panic!("set DEEP_OBSIDIAN_ALGOLIA_APP_ID / _OWNER_KEY / _SEARCH_KEY / _TEST_INDEX");
    };

    // Owner seeds one note inside the shared scope and one outside it.
    let owner = mount_with_key(&app_id, &index, "owner@keyscope", &owner_key, true);
    let _ = versioning::retract_note(&owner, PUBLIC).await;
    let _ = versioning::retract_note(&owner, PRIVATE).await;
    versioning::push_note_version(&owner, PUBLIC, &note("PUBLICMARK", "wiki-decision"), &[], None, false)
        .await
        .expect("seed public");
    versioning::push_note_version(&owner, PRIVATE, &note("PRIVATEMARK", "agent-session"), &[], None, false)
        .await
        .expect("seed private");

    // Mint the teammate key. The restriction string is url-encoded exactly as
    // `share key` builds it.
    let secured = generate_secured_api_key(&search_key, "filters=folders.lvl0%3A_Wiki");
    let teammate = mount_with_key(&app_id, &index, "teammate@keyscope", &secured, false);

    // Listing a NAMED folder uses facet + search only, so it works with a
    // search-only key. (The mount ROOT additionally needs the `browse` ACL —
    // asserted separately below.)
    let (entries, _truncated) = reads::list_children(&teammate, "_Wiki")
        .await
        .expect("list _Wiki");
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert!(
        names.contains(&"Decisions"),
        "the in-scope subfolder must be visible: {names:?}"
    );

    // Nothing out of scope may surface anywhere in the wiki listing.
    let (all_wiki, _) = reads::list_children(&teammate, "_Wiki/Decisions")
        .await
        .expect("list _Wiki/Decisions");
    assert!(
        all_wiki.iter().any(|entry| entry.name == "Scoped public.md"),
        "the in-scope note must be listed"
    );
    assert!(
        !all_wiki.iter().any(|entry| entry.name.contains("private")),
        "out-of-scope note leaked into a listing"
    );

    // The in-scope note reads normally.
    let public = reads::read_note(&teammate, PUBLIC).await.expect("read public");
    assert!(public.content.contains("PUBLICMARK"));

    // Search cannot reach the out-of-scope note.
    let hits = deep_obsidian_server::shared::retrieval::search_mount(&teammate, "PRIVATEMARK", 20)
        .await
        .expect("search out-of-scope marker");
    assert!(
        hits.iter().all(|hit| !hit.text.contains("PRIVATEMARK")),
        "search leaked out-of-scope content"
    );

    // And a direct read of the out-of-scope note must look EXACTLY like a read
    // of a path that does not exist. Algolia answers 403 "objectID not allowed"
    // for the former; surfacing that verbatim would let a teammate probe which
    // paths exist outside their scope.
    let out_of_scope = reads::read_note(&teammate, PRIVATE)
        .await
        .expect_err("out-of-scope read must fail");
    let nonexistent = reads::read_note(&teammate, "_Wiki/Decisions/No such note at all.md")
        .await
        .expect_err("nonexistent read must fail");
    assert!(
        matches!(
            out_of_scope,
            deep_obsidian_server::shared::SharedError::NoteNotFound(_)
        ),
        "out-of-scope read must report NoteNotFound, got: {out_of_scope}"
    );
    assert_eq!(
        std::mem::discriminant(&out_of_scope),
        std::mem::discriminant(&nonexistent),
        "out-of-scope and nonexistent must be indistinguishable"
    );

    // Documented limitation, pinned so it cannot regress silently: enumerating
    // the mount ROOT uses `browse`, a distinct ACL from `search`. A key without
    // it fails there while every scoped read above still works — which is why
    // `share key` warns when the parent lacks `browse`.
    let root = reads::list_children(&teammate, "").await;
    let has_browse = std::env::var("DEEP_OBSIDIAN_ALGOLIA_SEARCH_KEY_HAS_BROWSE").is_ok();
    if has_browse {
        let (entries, _) = root.expect("root listing with a browse-capable key");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"_Wiki"), "wiki must be visible at root: {names:?}");
        assert!(
            !names.contains(&"_Agent"),
            "out-of-scope folder leaked into the root listing: {names:?}"
        );
    } else {
        assert!(
            root.is_err(),
            "a key without `browse` is expected to fail the root listing; if this now \
succeeds, drop the browse warning from `share key`"
        );
    }

    // Cleanup with the owner key.
    let owner = mount_with_key(&app_id, &index, "owner@keyscope", &owner_key, true);
    let _ = versioning::retract_note(&owner, PUBLIC).await;
    let _ = versioning::retract_note(&owner, PRIVATE).await;
}

/// A secured key is search-only: writes must be refused. Our own
/// `writable: false` guard stops them before the network, and Algolia refuses
/// them even if a config claims otherwise.
#[tokio::test]
#[ignore = "requires a live Algolia account; see module docs"]
async fn secured_key_cannot_write() {
    let Some((app_id, owner_key, search_key, index)) = live_env() else {
        panic!("set DEEP_OBSIDIAN_ALGOLIA_APP_ID / _OWNER_KEY / _SEARCH_KEY / _TEST_INDEX");
    };
    let secured = generate_secured_api_key(&search_key, "filters=folders.lvl0%3A_Wiki");
    // Deliberately claim writable so the request actually reaches Algolia.
    let teammate = mount_with_key(&app_id, &index, "teammate@keyscope", &secured, true);
    let path = "_Wiki/Decisions/Teammate attempt.md";

    let error = versioning::push_note_version(
        &teammate,
        path,
        &note("TEAMMATEWRITE", "wiki-decision"),
        &[],
        None,
        false,
    )
    .await
    .expect_err("a secured (search-only) key must not be able to write");
    let rendered = error.to_string();
    let lowered = rendered.to_lowercase();
    assert!(
        lowered.contains("not enough rights") || lowered.contains("not allowed"),
        "expected an authorization failure, got: {rendered}"
    );

    // Nothing landed.
    let owner = mount_with_key(&app_id, &index, "owner@keyscope", &owner_key, true);
    assert!(
        versioning::fetch_head(&owner, path)
            .await
            .expect("head lookup")
            .is_none(),
        "the refused write must not have created a note"
    );
}
