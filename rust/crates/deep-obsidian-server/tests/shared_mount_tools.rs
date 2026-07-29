//! Two-participant integration test through the real MCP tool surface:
//! publisher pushes, consumer reads/lists/searches/writes through mounted
//! paths, divergence is recorded and resolved.

use deep_obsidian_algolia::mock::spawn_mock;
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_server::mcp::AppState;
use deep_obsidian_server::runtime::RuntimeState;
use deep_obsidian_server::shared::push::{apply_push, plan_push};
use deep_obsidian_server::shared::{connect_mount, versioning};
use deep_obsidian_server::tools::call_tool;
use deep_obsidian_types::{
    AutoReindexConfig, EmbeddingConfig, HttpConfig, ResolvedServiceConfig, SharedExportConfig,
    SharedMountConfig, StdioMode, TransportMode,
};
use serde_json::json;
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

const DECISION: &str = "---\ntype: wiki-decision\nproject: Deep Obsidian\n---\n\n# Keep retrieval architecture-agnostic\n\n## Decision\n\nRetrieval tools stay generic; workflow rules live in prompts and skills.\n\n## Rationale\n\nA generic retrieval layer is easier to reuse across projects.\n";

fn mount_config(base_url: &str, participant: &str, export: bool) -> SharedMountConfig {
    SharedMountConfig {
        mount_at: "_Shared/Team/".to_string(),
        app_id: "TESTAPP".to_string(),
        index_name: "team-wiki-tools".to_string(),
        key_ref: None,
        base_url: Some(base_url.to_string()),
        writable: true,
        participant_id: Some(participant.to_string()),
        export: export.then(|| SharedExportConfig {
            prefixes: vec!["_Wiki/".to_string()],
            exclude: Vec::new(),
        }),
        cache: None,
        retention: None,
    }
}

fn service_config(vault_path: PathBuf, mount: SharedMountConfig) -> ResolvedServiceConfig {
    ResolvedServiceConfig {
        index_dir: vault_path.join(".index"),
        vault_path,
        transport: TransportMode::Http,
        stdio_mode: StdioMode::Auto,
        http: HttpConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            mcp_path: "/mcp".to_string(),
            health_path: "/healthz".to_string(),
        },
        auto_reindex: AutoReindexConfig {
            enabled: false,
            debounce_ms: 0,
            interval_ms: 0,
        },
        embedding: EmbeddingConfig::default(),
        artifact_embedding: EmbeddingConfig::default(),
        auth: deep_obsidian_types::AuthConfig::default(),
        shared: vec![mount],
        config_file_path: None,
    }
}

async fn consumer_state(base_url: &str, participant: &str) -> AppState {
    let vault = temp_dir("consumer-vault");
    fs::write(vault.join("local-note.md"), "# Local note\n\nStays local.\n").unwrap();
    let config = service_config(vault, mount_config(base_url, participant, false));
    let (runtime, _guard) = RuntimeState::bootstrap(config.clone())
        .await
        .expect("bootstrap");
    AppState::new(config, runtime)
}

#[tokio::test]
async fn mounted_tools_read_search_write_and_resolve_divergence() {
    std::env::set_var("DEEP_OBSIDIAN_ALGOLIA_API_KEY", "test-key");
    let (base_url, _mock) = spawn_mock().await;

    // Publisher: seed + push _Wiki/.
    let publisher_vault = temp_dir("publisher-vault");
    fs::create_dir_all(publisher_vault.join("_Wiki/Decisions")).unwrap();
    fs::write(
        publisher_vault.join("_Wiki/Decisions/Keep retrieval architecture-agnostic.md"),
        DECISION,
    )
    .unwrap();
    let secrets = SecretResolver::new();
    let publisher = connect_mount(
        &mount_config(&base_url, "paul@test", true),
        &secrets,
        &temp_dir("publisher-index"),
    )
    .expect("publisher mount");
    let plan = plan_push(&publisher_vault, &publisher).await.expect("plan");
    apply_push(&publisher_vault, &publisher, &plan)
        .await
        .expect("apply");

    // Consumer: an AppState whose vault does NOT contain the wiki.
    let state = consumer_state(&base_url, "alice@test").await;
    let mounted = "_Shared/Team/_Wiki/Decisions/Keep retrieval architecture-agnostic.md";

    // read_file hydrates the exact content through the mount.
    let result = call_tool(&state, "read_file", &json!({ "path": mounted }))
        .await
        .expect("read_file");
    assert_eq!(result.structured_content["shared"], json!(true));
    assert_eq!(result.structured_content["text"], json!(DECISION));

    // list_children walks the virtual namespace: root -> synthetic _Shared.
    let result = call_tool(&state, "list_children", &json!({}))
        .await
        .expect("list root");
    let children = result.structured_content["children"]
        .as_array()
        .expect("children");
    assert!(children
        .iter()
        .any(|entry| entry["path"] == json!("_Shared") && entry["shared"] == json!(true)));

    // ... and inside the mount, folders come from facets.
    let result = call_tool(&state, "list_children", &json!({ "path": "_Shared/Team" }))
        .await
        .expect("list mount root");
    let entries = result.structured_content["entries"].as_array().expect("entries");
    assert!(entries.iter().any(|entry| entry["name"] == json!("_Wiki")));

    // hybrid_search federates the shared corpus into the ranking.
    let result = call_tool(
        &state,
        "hybrid_search",
        &json!({ "query": "retrieval architecture agnostic" }),
    )
    .await
    .expect("hybrid_search");
    let matches = result.structured_content["matches"].as_array().expect("matches");
    assert!(
        matches.iter().any(|item| item["shared"] == json!(true)
            && item["path"].as_str().unwrap_or("").starts_with("_Shared/Team/")),
        "shared hit expected in fused results: {matches:?}"
    );
    let shared_meta = &result.structured_content["shared"]["mounts"][0];
    assert_eq!(shared_meta["recallStage"], json!("lexical"));

    // graph_traverse incoming on the mounted path (backlinks via filters).
    let result = call_tool(
        &state,
        "graph_traverse",
        &json!({ "path": mounted, "direction": "incoming", "depth": 1 }),
    )
    .await
    .expect("graph_traverse");
    assert_eq!(result.structured_content["shared"], json!(true));

    // Versioned write through upsert_note (read-modify-write on the mount).
    let alice_edit = format!("{DECISION}\n## Consequences\n\nAlice adds a section.\n");
    let result = call_tool(
        &state,
        "upsert_note",
        &json!({ "path": mounted, "content": alice_edit }),
    )
    .await
    .expect("upsert_note on mount");
    assert_eq!(result.structured_content["shared"], json!(true));
    let alice_version = result.structured_content["versionId"]
        .as_str()
        .expect("versionId")
        .to_string();
    assert_eq!(result.structured_content["hasDivergence"], json!(false));

    // note_history shows both versions.
    let result = call_tool(&state, "note_history", &json!({ "path": mounted }))
        .await
        .expect("note_history");
    assert_eq!(result.structured_content["count"], json!(2));

    // Divergence: paul pushes on top, then alice writes from her stale base.
    versioning::push_note_version(
        &publisher,
        "_Wiki/Decisions/Keep retrieval architecture-agnostic.md",
        &format!("{DECISION}\n## Consequences\n\nPaul adds a different section.\n"),
        &[],
        Some(&alice_version),
        false,
    )
    .await
    .expect("paul concurrent push");

    let stale_write = format!("{DECISION}\n## Consequences\n\nAlice, based on stale head.\n");
    let paul_head = versioning::fetch_head(
        &publisher,
        "_Wiki/Decisions/Keep retrieval architecture-agnostic.md",
    )
    .await
    .expect("head")
    .expect("head exists");
    // Alice pushes with her OLD version as base -> fork recorded, not blocked.
    let outcome = versioning::push_note_version(
        &publisher, // same index; participant identity irrelevant to the mechanics
        "_Wiki/Decisions/Keep retrieval architecture-agnostic.md",
        &stale_write,
        &[],
        Some(&alice_version),
        false,
    )
    .await
    .expect("stale-based push succeeds");
    assert_eq!(outcome.forked_from.as_deref(), Some(paul_head.version_id.as_str()));
    assert!(outcome.has_divergence);

    // resolve_divergence returns head + overtaken + common ancestor.
    let result = call_tool(&state, "resolve_divergence", &json!({ "path": mounted }))
        .await
        .expect("resolve_divergence");
    assert_eq!(result.structured_content["hasDivergence"], json!(true));
    assert!(result.structured_content["head"]["text"]
        .as_str()
        .expect("head text")
        .contains("based on stale head"));
    assert!(result.structured_content["overtaken"]["text"]
        .as_str()
        .expect("overtaken text")
        .contains("Paul adds a different section"));

    // read_version can still read the overtaken content.
    let overtaken_id = result.structured_content["overtaken"]["versionId"]
        .as_str()
        .expect("overtaken id")
        .to_string();
    let result = call_tool(
        &state,
        "read_version",
        &json!({ "path": mounted, "versionId": overtaken_id }),
    )
    .await
    .expect("read_version");
    assert!(result.structured_content["text"]
        .as_str()
        .unwrap()
        .contains("Paul adds a different section"));

    // Merged write with resolveDivergence clears the flag.
    let merged = format!(
        "{DECISION}\n## Consequences\n\nAlice, based on stale head.\n\nPaul adds a different section.\n"
    );
    let result = call_tool(
        &state,
        "upsert_note",
        &json!({ "path": mounted, "content": merged, "resolveDivergence": true }),
    )
    .await
    .expect("merged write");
    assert_eq!(result.structured_content["hasDivergence"], json!(false));

    // grep_search: shared scope is candidate-bounded and says so.
    if state.rg_available {
        let result = call_tool(
            &state,
            "grep_search",
            &json!({ "query": "architecture-agnostic", "regex": false }),
        )
        .await
        .expect("grep_search");
        let scope = result.structured_content["sharedScope"]
            .as_array()
            .expect("shared scope");
        assert_eq!(scope[0]["exhaustive"], json!(false));
        assert!(scope[0]["candidateCount"].as_u64().unwrap() >= 1);
        assert!(
            scope[0]["matchCount"].as_u64().unwrap() >= 1,
            "shared grep must surface the matching lines: {:?}",
            result.structured_content
        );

        // Anchor-less pattern: remote refused with a reason, not silently empty.
        let result = call_tool(
            &state,
            "grep_search",
            &json!({ "query": "^\\s*$", "regex": true }),
        )
        .await
        .expect("grep_search unanchored");
        let scope = result.structured_content["sharedScope"]
            .as_array()
            .expect("shared scope");
        assert_eq!(scope[0]["searched"], json!(false));
        assert!(scope[0]["reason"].as_str().unwrap().contains("anchor"));
    }

    // vault_info reports the mount.
    let result = call_tool(&state, "vault_info", &json!({}))
        .await
        .expect("vault_info");
    let mounts = result.structured_content["sharedMounts"]
        .as_array()
        .expect("mounts");
    assert_eq!(mounts[0]["mountAt"], json!("_Shared/Team/"));
    assert_eq!(mounts[0]["recallStage"], json!("lexical"));
}
