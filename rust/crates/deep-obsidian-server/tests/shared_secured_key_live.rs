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
//! DEEP_OBSIDIAN_ALGOLIA_API_KEY=<a key with search+write on the test index> \
//! DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX=scratch-securedkey \
//!   cargo test -p deep-obsidian-server --test shared_secured_key_live -- --ignored --test-threads=1
//! ```

use deep_obsidian_algolia::generate_secured_api_key;
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

fn live_env() -> Option<(String, String, String)> {
    Some((
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_APP_ID").ok()?,
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_API_KEY").ok()?,
        std::env::var("DEEP_OBSIDIAN_ALGOLIA_TEST_INDEX").ok()?,
    ))
}

/// Builds a mount whose key is passed explicitly, so the owner and the scoped
/// teammate can be driven side by side in one process.
fn mount_with_key(app_id: &str, index: &str, who: &str, key: &str, writable: bool) -> SharedMountRuntime {
    // `resolve_api_key` prefers the env var, which is how a teammate supplies a
    // secured key without it ever touching a config file.
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", key);
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
    connect_mount(&config, &SecretResolver::new(), &temp_dir("key-index"))
        .expect("connect live mount")
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
    let Some((app_id, owner_key, index)) = live_env() else {
        panic!("set DEEP_OBSIDIAN_ALGOLIA_APP_ID / _API_KEY / _TEST_INDEX");
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
    let secured = generate_secured_api_key(&owner_key, "filters=folders.lvl0%3A_Wiki");
    let teammate = mount_with_key(&app_id, &index, "teammate@keyscope", &secured, false);

    // Listing shows only the in-scope folder.
    let (entries, _truncated) = reads::list_children(&teammate, "").await.expect("list root");
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert!(names.contains(&"_Wiki"), "wiki must be visible: {names:?}");
    assert!(
        !names.contains(&"_Agent"),
        "out-of-scope folder leaked into the listing: {names:?}"
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
    let Some((app_id, owner_key, index)) = live_env() else {
        panic!("set DEEP_OBSIDIAN_ALGOLIA_APP_ID / _API_KEY / _TEST_INDEX");
    };
    let secured = generate_secured_api_key(&owner_key, "filters=folders.lvl0%3A_Wiki");
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
    assert!(
        rendered.contains("403") || rendered.to_lowercase().contains("not allowed"),
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
