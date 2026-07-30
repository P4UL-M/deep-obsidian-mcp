//! End-to-end seed / dump / retract tests against the in-process mock Algolia.

use deep_obsidian_algolia::mock::spawn_mock;
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_server::shared::seed::{apply_seed, plan_seed, remove_seeded_local_files, SeedAction};
use deep_obsidian_server::shared::{connect_mount, reads, versioning};
use deep_obsidian_types::SharedMountConfig;
use std::fs;
use std::path::PathBuf;

fn temp_dir(prefix: &str) -> PathBuf {
    // SystemTime alone is NOT unique across concurrent tests (µs resolution on
    // macOS): two tests in the same instant would share — and mutate — one
    // directory. The atomic counter disambiguates.
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

const DECISION: &str = "---\ntype: wiki-decision\nproject: Deep Obsidian\nstatus: active\n---\n\n# Keep retrieval architecture-agnostic\n\n## Decision\n\nRetrieval tools stay generic; workflow rules live in prompts and skills.\n\n## Rationale\n\nA generic retrieval layer is easier to reuse across projects and vault layouts.\n";

const SYNTHESIS: &str = "---\ntype: wiki-synthesis\nproject: Deep Obsidian\n---\n\n# Product narrative\n\n## Summary\n\nDeep Obsidian bridges human notes and agent memory. See [[Keep retrieval architecture-agnostic]].\n";

const PRIVATE: &str = "---\nshare: false\n---\n\n# Secret draft\n\nNot for the team.\n";

fn seed_vault() -> PathBuf {
    let vault = temp_dir("shared-push-vault");
    fs::create_dir_all(vault.join("_Wiki/Decisions")).unwrap();
    fs::create_dir_all(vault.join("_Wiki/Syntheses")).unwrap();
    fs::create_dir_all(vault.join("_Agent/Sessions")).unwrap();
    fs::write(
        vault.join("_Wiki/Decisions/Keep retrieval architecture-agnostic.md"),
        DECISION,
    )
    .unwrap();
    fs::write(vault.join("_Wiki/Syntheses/Product narrative.md"), SYNTHESIS).unwrap();
    fs::write(vault.join("_Wiki/Drafts.md"), PRIVATE).unwrap();
    fs::write(
        vault.join("_Agent/Sessions/session.md"),
        "# Session\nlocal only\n",
    )
    .unwrap();
    vault
}

const SEED_PREFIXES: [&str; 1] = ["_Wiki/"];

fn prefixes() -> Vec<String> {
    SEED_PREFIXES.iter().map(|p| p.to_string()).collect()
}

fn mount_config(base_url: &str, participant: &str) -> SharedMountConfig {
    SharedMountConfig {
        mount_at: "_Shared/Team/".to_string(),
        app_id: "TESTAPP".to_string(),
        index_name: "team-wiki".to_string(),
        key_ref: None,
        base_url: Some(base_url.to_string()),
        writable: true,
        participant_id: Some(participant.to_string()),
        cache: None,
        retention: None,
    }
}

#[tokio::test]
async fn seed_imports_excludes_and_versions() {
    // Env var supplies the key so no keyring is involved.
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", "test-key");
    let (base_url, _mock) = spawn_mock().await;
    let vault = seed_vault();
    let index_dir = temp_dir("seed-index");
    let secrets = SecretResolver::new();
    let mount = connect_mount(&mount_config(&base_url, "paul@test"), &secrets, &index_dir)
        .expect("connect mount");

    // First import: flagged, share:false note excluded, _Agent/ untouched.
    let plan = plan_seed(&vault, &mount, &prefixes()).await.expect("plan");
    assert!(plan.first_push);
    let planned_paths: Vec<&str> = plan.items.iter().map(|item| item.path.as_str()).collect();
    assert_eq!(planned_paths.len(), 2, "share:false and _Agent/ excluded");
    assert!(planned_paths
        .iter()
        .all(|path| path.starts_with("_Wiki/") && !path.contains("Drafts")));
    assert!(plan
        .items
        .iter()
        .all(|item| item.action == SeedAction::Create));

    let report = apply_seed(&vault, &mount, &prefixes(), &plan)
        .await
        .expect("apply");
    assert_eq!(report.seeded, 2);

    // The imported note hydrates back byte-identical through the read path.
    let hydrated = reads::read_note(
        &mount,
        "_Wiki/Decisions/Keep retrieval architecture-agnostic.md",
    )
    .await
    .expect("hydrate");
    assert_eq!(hydrated.content, DECISION);

    // Re-seeding with no changes: everything already up to date.
    let plan = plan_seed(&vault, &mount, &prefixes()).await.expect("plan 2");
    assert!(!plan.first_push);
    assert_eq!(plan.changed_count(), 0);

    // Edit a note -> update + history version.
    let decision_path = vault.join("_Wiki/Decisions/Keep retrieval architecture-agnostic.md");
    let edited = format!("{DECISION}\n## Consequences\n\nNew section added.\n");
    fs::write(&decision_path, &edited).unwrap();
    let plan = plan_seed(&vault, &mount, &prefixes()).await.expect("plan 3");
    assert_eq!(plan.changed_count(), 1);
    apply_seed(&vault, &mount, &prefixes(), &plan)
        .await
        .expect("apply 3");

    let hydrated = reads::read_note(
        &mount,
        "_Wiki/Decisions/Keep retrieval architecture-agnostic.md",
    )
    .await
    .expect("hydrate edited");
    assert_eq!(hydrated.content, edited);

    // The superseded version is in history with full chunks.
    let history = mount
        .client
        .browse_all(
            &mount.history_index,
            Some("recordType:note AND noteId:\"_Wiki/Decisions/Keep retrieval architecture-agnostic.md\""),
        )
        .await
        .expect("browse history");
    assert_eq!(history.len(), 1, "one superseded version");
    let superseded = &history[0];
    assert!(superseded.get("supersededBy").is_some());
    let old_version = superseded
        .get("versionId")
        .and_then(serde_json::Value::as_str)
        .unwrap();
    let old_chunks = reads::fetch_version_chunks(
        &mount,
        &mount.history_index,
        "_Wiki/Decisions/Keep retrieval architecture-agnostic.md",
        old_version,
    )
    .await
    .expect("history chunks");
    assert_eq!(reads::reassemble_chunks(old_chunks), DECISION);
}

/// Deleting a note locally must NOT remove it from the index: seed never
/// reconciles deletions (that is `share retract`, tested below). This is the
/// property that replaced the old push-time reconciliation.
#[tokio::test]
async fn seed_never_removes_remote_notes() {
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", "test-key");
    let (base_url, _mock) = spawn_mock().await;
    let vault = seed_vault();
    let index_dir = temp_dir("seed-no-retract-index");
    let secrets = SecretResolver::new();
    let mut config = mount_config(&base_url, "paul@test");
    config.index_name = "no-retract-wiki".to_string();
    let mount = connect_mount(&config, &secrets, &index_dir).expect("connect");

    let plan = plan_seed(&vault, &mount, &prefixes()).await.expect("plan");
    apply_seed(&vault, &mount, &prefixes(), &plan)
        .await
        .expect("apply");

    fs::remove_file(vault.join("_Wiki/Syntheses/Product narrative.md")).unwrap();
    let plan = plan_seed(&vault, &mount, &prefixes()).await.expect("plan 2");
    assert_eq!(plan.items.len(), 1, "only the surviving local note is planned");
    apply_seed(&vault, &mount, &prefixes(), &plan)
        .await
        .expect("apply 2");
    assert!(
        versioning::fetch_head(&mount, "_Wiki/Syntheses/Product narrative.md")
            .await
            .expect("head lookup")
            .is_some(),
        "a local deletion must not silently remove the shared note"
    );

    // Explicit retraction is what removes it — note, chunks, and history.
    versioning::retract_note(&mount, "_Wiki/Syntheses/Product narrative.md")
        .await
        .expect("retract");
    assert!(
        versioning::fetch_head(&mount, "_Wiki/Syntheses/Product narrative.md")
            .await
            .expect("head lookup")
            .is_none()
    );
    let leftover = mount
        .client
        .browse_all(
            mount.index(),
            Some("noteId:\"_Wiki/Syntheses/Product narrative.md\""),
        )
        .await
        .expect("browse main");
    assert!(leftover.is_empty(), "retraction removes note + chunks");
    // This note was never superseded, so the history index may not exist at
    // all — which is itself "no history". Tolerated the same way production
    // reads do.
    let leftover_history = deep_obsidian_server::shared::empty_if_missing_index(
        mount
            .client
            .browse_all(
                &mount.history_index,
                Some("noteId:\"_Wiki/Syntheses/Product narrative.md\""),
            )
            .await,
        Vec::new(),
    )
    .expect("browse history");
    assert!(leftover_history.is_empty(), "retraction purges history too");
}

/// A colleague's note that was never in this participant's vault stays put:
/// seed only touches the paths it imports.
#[tokio::test]
async fn seed_leaves_foreign_notes_alone() {
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", "test-key");
    let (base_url, _mock) = spawn_mock().await;
    let vault = seed_vault();
    let index_dir = temp_dir("seed-foreign-index");
    let secrets = SecretResolver::new();
    let mut config = mount_config(&base_url, "paul@test");
    config.index_name = "foreign-wiki".to_string();
    let paul = connect_mount(&config, &secrets, &index_dir).expect("connect paul");

    let plan = plan_seed(&vault, &paul, &prefixes()).await.expect("plan");
    apply_seed(&vault, &paul, &prefixes(), &plan)
        .await
        .expect("apply");

    // Alice authors a NEW note directly through the mount.
    let mut alice_config = config.clone();
    alice_config.participant_id = Some("alice@test".to_string());
    let alice = connect_mount(&alice_config, &secrets, &index_dir).expect("connect alice");
    versioning::push_note_version(
        &alice,
        "_Wiki/Decisions/Alice own note.md",
        "# Alice's decision\n\nAuthored through the mount.\n",
        &[],
        None,
        false,
    )
    .await
    .expect("alice write");

    // Paul re-seeds: Alice's note is untouched and still present.
    let plan = plan_seed(&vault, &paul, &prefixes()).await.expect("plan 2");
    assert!(plan
        .items
        .iter()
        .all(|item| item.path != "_Wiki/Decisions/Alice own note.md"));
    apply_seed(&vault, &paul, &prefixes(), &plan)
        .await
        .expect("apply 2");
    assert!(
        versioning::fetch_head(&paul, "_Wiki/Decisions/Alice own note.md")
            .await
            .expect("head")
            .is_some(),
        "a re-seed must not disturb notes authored through the mount"
    );
}

#[tokio::test]
async fn seed_move_and_dump_round_trip() {
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", "test-key");
    let (base_url, _mock) = spawn_mock().await;
    let vault = seed_vault();
    let index_dir = temp_dir("seed-move-index");
    let secrets = SecretResolver::new();
    // Seed = push with an ephemeral export rule and no reconciliation.
    let mut config = mount_config(&base_url, "paul@test");
    config.index_name = "seed-wiki".to_string();
    let mount = connect_mount(&config, &secrets, &index_dir).expect("connect");

    let plan = plan_seed(&vault, &mount, &prefixes()).await.expect("plan");
    apply_seed(&vault, &mount, &prefixes(), &plan)
        .await
        .expect("apply");

    // --move: local copies removed only when the index verifiably holds them.
    let (deleted, skipped) = remove_seeded_local_files(&vault, &mount, &prefixes())
        .await
        .expect("move");
    assert_eq!(deleted.len(), 2, "both seeded notes removed: {deleted:?}");
    assert!(skipped.is_empty(), "nothing drifted: {skipped:?}");
    assert!(!vault
        .join("_Wiki/Decisions/Keep retrieval architecture-agnostic.md")
        .exists());
    // Excluded content untouched: share:false note and _Agent/ stay local.
    assert!(vault.join("_Wiki/Drafts.md").exists());
    assert!(vault.join("_Agent/Sessions/session.md").exists());
    // Empty seeded folders pruned.
    assert!(!vault.join("_Wiki/Decisions").exists());

    // Dump materializes the index back out, byte-identical.
    let target = temp_dir("dump-target");
    let report = reads::dump_all(&mount, &target).await.expect("dump");
    assert_eq!(report.notes, 2);
    assert!(report.hash_mismatches.is_empty(), "{:?}", report.hash_mismatches);
    let restored = std::fs::read_to_string(
        target.join("_Wiki/Decisions/Keep retrieval architecture-agnostic.md"),
    )
    .expect("restored note");
    assert_eq!(restored, DECISION);
    assert!(target.join("deep-obsidian-dump.json").exists());
}

/// Regression: nothing may 404 against a VIRGIN Algolia app. An index is
/// created by its first write, so before that every read answers
/// `404 Index <name> does not exist` — and the `_history` index stays absent
/// until a note is first superseded. This walks the whole surface against a
/// brand-new index name, then proves history provisioning happens lazily on
/// the first supersession.
#[tokio::test]
async fn virgin_index_never_404s_and_history_is_provisioned_lazily() {
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", "test-key");
    let (base_url, _mock) = spawn_mock().await;
    let vault = seed_vault();
    let index_dir = temp_dir("virgin-index");
    let secrets = SecretResolver::new();
    let mut config = mount_config(&base_url, "paul@test");
    config.index_name = "brand-new-wiki".to_string();
    let mount = connect_mount(&config, &secrets, &index_dir).expect("connect");
    let prefixes = vec!["_Wiki/".to_string()];

    // Reads against an index that has never been written to: empty, not 404.
    assert!(reads::list_children(&mount, "").await.expect("list root").is_empty());
    assert!(reads::list_folders(&mount, 3).await.expect("folders").is_empty());
    assert!(reads::find_paths(&mount, "wiki", 10).await.expect("find").is_empty());
    assert!(reads::backlinks(&mount, "_Wiki/Any.md").await.expect("backlinks").is_empty());
    assert!(versioning::fetch_head(&mount, "_Wiki/Any.md")
        .await
        .expect("head on virgin index")
        .is_none());
    let dump_target = temp_dir("virgin-dump");
    assert_eq!(
        reads::dump_all(&mount, &dump_target).await.expect("dump virgin").notes,
        0
    );

    // First seed: plan sees an empty index as first_push, apply provisions the
    // MAIN index only — the history index still does not exist.
    let plan = plan_seed(&vault, &mount, &prefixes).await.expect("plan virgin");
    assert!(plan.first_push);
    apply_seed(&vault, &mount, &prefixes, &plan).await.expect("apply virgin");

    // History reads still fine (no index yet) — this is the exact call that
    // produced the reported `Index shared_history does not exist`.
    let seeded = "_Wiki/Decisions/Keep retrieval architecture-agnostic.md";
    let history = deep_obsidian_server::shared::empty_if_missing_index(
        mount
            .client
            .browse_all(&mount.history_index, Some("recordType:note"))
            .await,
        Vec::new(),
    )
    .expect("history browse pre-supersession");
    assert!(history.is_empty(), "no history before any supersession");

    // Second write supersedes -> history index is created AND provisioned.
    versioning::push_note_version(
        &mount,
        seeded,
        &format!("{DECISION}\n## Consequences\n\nSecond version.\n"),
        &[],
        None,
        false,
    )
    .await
    .expect("supersede");

    let history = mount
        .client
        .browse_all(&mount.history_index, Some("recordType:note"))
        .await
        .expect("history exists now");
    assert_eq!(history.len(), 1, "the superseded version landed in history");
    let settings = mount
        .client
        .get_settings(&mount.history_index)
        .await
        .expect("history settings applied lazily");
    assert!(
        settings.get("attributesForFaceting").is_some(),
        "history index settings were provisioned after its first write: {settings}"
    );
}
