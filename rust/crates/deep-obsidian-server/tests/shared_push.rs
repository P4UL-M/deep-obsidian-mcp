//! End-to-end export/push tests against the in-process mock Algolia.

use deep_obsidian_algolia::mock::spawn_mock;
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_server::shared::push::{apply_push, plan_push, PushAction};
use deep_obsidian_server::shared::{connect_mount, reads, versioning};
use deep_obsidian_types::{SharedExportConfig, SharedMountConfig};
use std::fs;
use std::path::PathBuf;

fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
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

fn mount_config(base_url: &str, participant: &str) -> SharedMountConfig {
    SharedMountConfig {
        mount_at: "_Shared/Team/".to_string(),
        app_id: "TESTAPP".to_string(),
        index_name: "team-wiki".to_string(),
        key_ref: None,
        base_url: Some(base_url.to_string()),
        writable: true,
        participant_id: Some(participant.to_string()),
        export: Some(SharedExportConfig {
            prefixes: vec!["_Wiki/".to_string()],
            exclude: Vec::new(),
        }),
        cache: None,
        retention: None,
    }
}

#[tokio::test]
async fn push_publishes_reconciles_and_retracts() {
    // Env var supplies the key so no keyring is involved.
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", "test-key");
    let (base_url, _mock) = spawn_mock().await;
    let vault = seed_vault();
    let index_dir = temp_dir("shared-push-index");
    let secrets = SecretResolver::new();
    let mount = connect_mount(&mount_config(&base_url, "paul@test"), &secrets, &index_dir)
        .expect("connect mount");

    // First push: plan flags it, share:false note excluded, _Agent/ untouched.
    let plan = plan_push(&vault, &mount).await.expect("plan");
    assert!(plan.first_push);
    let planned_paths: Vec<&str> = plan.items.iter().map(|item| item.path.as_str()).collect();
    assert_eq!(planned_paths.len(), 2, "share:false and _Agent/ excluded");
    assert!(planned_paths
        .iter()
        .all(|path| path.starts_with("_Wiki/") && !path.contains("Drafts")));
    assert!(plan
        .items
        .iter()
        .all(|item| item.action == PushAction::Create));

    let report = apply_push(&vault, &mount, &plan).await.expect("apply");
    assert_eq!(report.pushed, 2);

    // The pushed note hydrates back byte-identical through the read path.
    let hydrated = reads::read_note(
        &mount,
        "_Wiki/Decisions/Keep retrieval architecture-agnostic.md",
    )
    .await
    .expect("hydrate");
    assert_eq!(hydrated.content, DECISION);
    // Wiki-link resolved against the exporter's file list.
    assert_eq!(
        hydrated.note.links,
        vec!["_Wiki/Decisions/Keep retrieval architecture-agnostic.md".to_string()]
            .into_iter()
            .filter(|_| false)
            .collect::<Vec<_>>(),
        "decision note has no outgoing links"
    );

    // Second push with no changes: everything unchanged.
    let plan = plan_push(&vault, &mount).await.expect("plan 2");
    assert!(!plan.first_push);
    assert_eq!(plan.changed_count(), 0);

    // Edit a note -> update + history version.
    let decision_path = vault.join("_Wiki/Decisions/Keep retrieval architecture-agnostic.md");
    let edited = format!("{DECISION}\n## Consequences\n\nNew section added.\n");
    fs::write(&decision_path, &edited).unwrap();
    let plan = plan_push(&vault, &mount).await.expect("plan 3");
    assert_eq!(plan.changed_count(), 1);
    apply_push(&vault, &mount, &plan).await.expect("apply 3");

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

    // Retraction: delete the synthesis locally -> tombstoned remotely,
    // including its history.
    fs::remove_file(vault.join("_Wiki/Syntheses/Product narrative.md")).unwrap();
    let plan = plan_push(&vault, &mount).await.expect("plan 4");
    assert_eq!(plan.retract, vec!["_Wiki/Syntheses/Product narrative.md"]);
    let report = apply_push(&vault, &mount, &plan).await.expect("apply 4");
    assert_eq!(report.retracted, 1);
    assert!(versioning::fetch_head(&mount, "_Wiki/Syntheses/Product narrative.md")
        .await
        .expect("head lookup")
        .is_none());
    let leftover = mount
        .client
        .browse_all(
            mount.index(),
            Some("noteId:\"_Wiki/Syntheses/Product narrative.md\""),
        )
        .await
        .expect("browse main");
    assert!(leftover.is_empty(), "retraction removes note + chunks");
}

#[tokio::test]
async fn foreign_notes_are_never_retracted() {
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", "test-key");
    let (base_url, _mock) = spawn_mock().await;
    let vault = seed_vault();
    let index_dir = temp_dir("shared-push-index-b");
    let secrets = SecretResolver::new();
    let paul = connect_mount(&mount_config(&base_url, "paul@test"), &secrets, &index_dir)
        .expect("connect paul");

    let plan = plan_push(&vault, &paul).await.expect("plan");
    apply_push(&vault, &paul, &plan).await.expect("apply");

    // Alice writes a NEW note directly to the shared index (not in Paul's vault).
    let alice = connect_mount(&mount_config(&base_url, "alice@test"), &secrets, &index_dir)
        .expect("connect alice");
    versioning::push_note_version(
        &alice,
        "_Wiki/Decisions/Alice own note.md",
        "# Alice's decision\n\nWritten remotely.\n",
        &[],
        None,
        false,
    )
    .await
    .expect("alice push");

    // Paul's next reconcile sees the foreign note but does NOT retract it.
    let plan = plan_push(&vault, &paul).await.expect("plan 2");
    assert!(plan.retract.is_empty());
    assert_eq!(
        plan.foreign_orphans,
        vec!["_Wiki/Decisions/Alice own note.md"]
    );
}
