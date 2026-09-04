//! Black-box multi-mount behaviour, driven through `mcp::handle_request`.
//!
//! The companion to `mcp_contract.rs`: that suite freezes what a client sees for a
//! SINGLE-mount vault, this one asserts what a client sees for a two-mount one.
//! Both drive the same JSON-RPC entry point, so nothing here reaches past the
//! public surface into a tool handler.
//!
//! Deliberately not snapshot-based. These payloads are new, still moving, and the
//! properties worth pinning are structural (which mount served a read, whether a
//! folder was synthesized, whether a not-yet-federated tool refused) rather than
//! byte-exact. Adding goldens here would freeze incidental shape and put pressure
//! on `UPDATE_GOLDEN`, which the single-mount goldens must never see.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_server::health::{
    build_readiness_payload, insert_mount_index_detail, readiness_status_code,
};
use deep_obsidian_server::mcp::{handle_request, AppState};
use deep_obsidian_server::mounts::MountBackends;
use deep_obsidian_server::protocol::JsonRpcRequest;
use deep_obsidian_server::runtime::MountRuntimes;
use deep_obsidian_types::{
    AuthConfig, AutoReindexConfig, EmbeddingConfig, ExperimentalConfig, HttpConfig,
    MountBackendConfig, MountConfig, ResolvedServiceConfig, SecretRef, StdioMode, TransportMode,
};
use serde_json::{json, Value};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Fixture: two real filesystem vaults behind two mounts
// ---------------------------------------------------------------------------

/// Two temp vaults plus a sibling index dir.
///
/// `root` is mounted at the vault root and is the one the search index covers;
/// `team` is mounted at `Team`. The index dir lives outside both so it can never
/// appear in a listing.
struct Fixture {
    root_vault: PathBuf,
    team_vault: PathBuf,
    index_dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "deep-obsidian-multivault-{name}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let fixture = Self {
            root_vault: base.join("root-vault"),
            team_vault: base.join("team-vault"),
            index_dir: base.join("index"),
        };
        fs::create_dir_all(fixture.root_vault.join("Notes")).expect("create root vault");
        fs::create_dir_all(&fixture.team_vault).expect("create team vault");
        fs::create_dir_all(&fixture.index_dir).expect("create index dir");
        fs::write(
            fixture.root_vault.join("Root.md"),
            "# Root\n\nRoot note in the root mount.\n",
        )
        .expect("write Root.md");
        fs::write(
            fixture.root_vault.join("Notes/Deep.md"),
            "# Deep\n\nA nested root-mount note.\n",
        )
        .expect("write Deep.md");
        fs::write(
            fixture.team_vault.join("Charter.md"),
            // The wiki link gives the team mount a graph edge of its own, so
            // `graph_traverse` on this mount has something to find that the ROOT
            // mount's index could not possibly know about.
            "# Charter\n\nThe team charter lives on the team mount.\n\nSee [[Roster]].\n",
        )
        .expect("write Charter.md");
        fs::write(
            fixture.team_vault.join("Roster.md"),
            "# Roster\n\nThe team roster, and who signs the charter.\n",
        )
        .expect("write Roster.md");
        fixture
    }

    /// Where the `team` mount's index must land, given no explicit `indexDir`:
    /// keyed by MOUNT ID under the root's index dir.
    fn team_index_dir(&self) -> PathBuf {
        self.index_dir.join("mounts").join("team")
    }

    fn config(&self) -> ResolvedServiceConfig {
        self.config_with_team_vault(self.team_vault.clone())
    }

    /// The same two-mount config with the team mount pointed somewhere else, so a
    /// broken mount can be built by naming a directory that does not exist.
    fn config_with_team_vault(&self, team_vault: PathBuf) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            federated_rerank: true,
            // Still the ROOT mount's path: it is what `vaultPath` has always meant,
            // and the root mount's own runtime is built from this config verbatim.
            vault_path: Some(self.root_vault.clone()),
            mounts: vec![
                MountConfig {
                    unknown: Default::default(),
                    recall_weight: None,
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: self.root_vault.clone(),
                        index_dir: None,
                    },
                },
                MountConfig {
                    unknown: Default::default(),
                    recall_weight: None,
                    id: "team".to_string(),
                    mount_at: "Team".to_string(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: team_vault,
                        // Deliberately unset: this pins the DERIVED default
                        // (`<root index dir>/mounts/team`), which is the thing that
                        // must not collide with the root's index.
                        index_dir: None,
                    },
                },
            ],
            experimental: ExperimentalConfig {
                multi_vault: true,
                ..ExperimentalConfig::default()
            },
            index_dir: self.index_dir.clone(),
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
            auth: AuthConfig::default(),
            config_file_path: None,
        }
    }

    async fn state(&self) -> AppState {
        self.state_for(self.config()).await
    }

    async fn state_for(&self, config: ResolvedServiceConfig) -> AppState {
        let backends = MountBackends::build(&config);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("bootstrap runtime");
        // `with_backends`, not `new`: `new` would build a SECOND MountBackends, and
        // for a couchdb mount the router's backend would then not be the one the index
        // reads through -- two child processes for one mount.
        AppState::with_backends(config, runtimes, &backends)
    }

    /// State with an upload base, so `request_vault_upload` can mint.
    async fn state_with_uploads(&self) -> AppState {
        self.state()
            .await
            .with_upload_base("http://127.0.0.1:4100".to_string())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.index_dir.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC driver
// ---------------------------------------------------------------------------

/// One JSON-RPC round trip through the public entry point.
async fn request(state: &AppState, method: &str, params: Value) -> Value {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let parsed: JsonRpcRequest =
        serde_json::from_value(payload).expect("request payload to deserialize");
    match handle_request(state.clone(), parsed).await {
        Ok(Some(response)) => response,
        Ok(None) => json!({"__notification_no_response__": true}),
        Err(error) => serde_json::to_value(&error).expect("error response to serialize"),
    }
}

async fn tool_call(state: &AppState, name: &str, arguments: Value) -> Value {
    request(
        state,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
    .await
}

/// The tool's `structuredContent`, or a panic naming the error it returned.
fn structured(response: &Value) -> &Value {
    response
        .get("result")
        .and_then(|result| result.get("structuredContent"))
        .unwrap_or_else(|| panic!("expected a successful tool call, got {response}"))
}

/// The JSON-RPC error message of a failed tool call.
fn error_message(response: &Value) -> &str {
    response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected an error response, got {response}"))
}

// ---------------------------------------------------------------------------
// Routed single-path operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reads_route_to_the_mount_owning_the_path() {
    let fixture = Fixture::new("read");
    let state = fixture.state().await;

    let root = tool_call(&state, "read_file", json!({"path": "Root.md"})).await;
    assert!(structured(&root)["text"]
        .as_str()
        .expect("text")
        .contains("Root note in the root mount"));

    // Served by the team mount, addressed by its logical path.
    let team = tool_call(&state, "read_file", json!({"path": "Team/Charter.md"})).await;
    assert!(structured(&team)["text"]
        .as_str()
        .expect("text")
        .contains("team charter lives on the team mount"));

    // The same file is NOT reachable at the team mount's own relative path: the
    // logical namespace is the only addressing scheme a client sees.
    let unrouted = tool_call(&state, "read_file", json!({"path": "Charter.md"})).await;
    assert!(error_message(&unrouted).contains("Charter.md"));
}

#[tokio::test]
async fn writes_land_in_the_mount_owning_the_path() {
    let fixture = Fixture::new("write");
    let state = fixture.state().await;

    let created = tool_call(
        &state,
        "upsert_note",
        json!({"path": "Team/New Note.md", "content": "Team body"}),
    )
    .await;
    assert_eq!(structured(&created)["created"], json!(true));
    // Physically on the TEAM vault, at the mount-relative path -- and nowhere in
    // the root vault.
    assert!(fixture.team_vault.join("New Note.md").exists());
    assert!(!fixture.root_vault.join("Team").exists());

    let root_created = tool_call(
        &state,
        "upsert_note",
        json!({"path": "Root Note.md", "content": "Root body"}),
    )
    .await;
    assert_eq!(structured(&root_created)["created"], json!(true));
    assert!(fixture.root_vault.join("Root Note.md").exists());
    assert!(!fixture.team_vault.join("Root Note.md").exists());

    // A routed read sees the routed write.
    let read_back = tool_call(&state, "read_file", json!({"path": "Team/New Note.md"})).await;
    assert!(structured(&read_back)["text"]
        .as_str()
        .expect("text")
        .contains("Team body"));
}

#[tokio::test]
async fn a_section_update_routes_to_the_owning_mount() {
    let fixture = Fixture::new("section");
    let state = fixture.state().await;

    let updated = tool_call(
        &state,
        "update_note_section",
        json!({
            "path": "Team/Charter.md",
            "heading": "Scope",
            "content": "Scoped to the team mount.",
        }),
    )
    .await;
    assert_eq!(structured(&updated)["path"], json!("Team/Charter.md"));
    let on_disk = fs::read_to_string(fixture.team_vault.join("Charter.md")).expect("read charter");
    assert!(on_disk.contains("Scoped to the team mount."));
}

// ---------------------------------------------------------------------------
// Listing synthesis
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_root_listing_merges_the_root_mount_with_a_synthesized_mount_folder() {
    let fixture = Fixture::new("listing");
    let state = fixture.state().await;

    let response = tool_call(&state, "list_children", json!({})).await;
    let children = structured(&response)["children"]
        .as_array()
        .expect("children")
        .clone();
    let paths: Vec<&str> = children
        .iter()
        .map(|entry| entry["path"].as_str().expect("path"))
        .collect();
    // Directories first, then files -- core's ordering, across the merged set.
    assert_eq!(paths, vec!["Notes", "Team", "Root.md"]);

    // The synthesized mount folder is shaped like a physical directory, so a
    // client cannot tell a mount point from a folder.
    let team = children
        .iter()
        .find(|entry| entry["path"] == json!("Team"))
        .expect("Team entry");
    assert_eq!(team["name"], json!("Team"));
    assert_eq!(team["kind"], json!("directory"));

    // Descending into the mount lists the mount's own content, at logical paths.
    let inside = tool_call(&state, "list_children", json!({"path": "Team"})).await;
    let paths: Vec<&str> = structured(&inside)["children"]
        .as_array()
        .expect("children")
        .iter()
        .map(|entry| entry["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, vec!["Team/Charter.md", "Team/Roster.md"]);

    // `foldersOnly` sees the synthesized folder too.
    let folders = tool_call(&state, "list_children", json!({"foldersOnly": true})).await;
    let folders = structured(&folders)["folders"]
        .as_array()
        .expect("folders")
        .clone();
    assert!(folders.contains(&json!("Team")));
    assert!(folders.contains(&json!("Notes")));
}

// ---------------------------------------------------------------------------
// Honest refusals: what this slice does not federate
// ---------------------------------------------------------------------------

/// An unscoped `grep_search` searches EVERY mount and concatenates.
///
/// This replaced a refusal. Grep produces MATCHES, not a ranking, so appending each mount's
/// matches is the same set one vault would have produced -- there is no score to make
/// comparable. Answering from the root mount alone (the thing the refusal existed to prevent)
/// would have reported zero matches for text that is in the vault.
#[tokio::test]
async fn an_unscoped_grep_federates_across_every_mount() {
    let fixture = Fixture::new("grep");
    let state = fixture.state().await;
    if !state.rg_available {
        return;
    }

    let response = tool_call(&state, "grep_search", json!({"query": "charter"})).await;
    let payload = structured(&response);
    let paths: Vec<&str> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect();
    // "charter" appears only on the TEAM mount, and the search finds it without the caller
    // naming a mount.
    assert!(paths.contains(&"Team/Charter.md"), "{paths:?}");
    // Every mount was read, so the answer keeps grep's frozen "exhaustive by omission"
    // shape: no `exhaustive` key, no degradation.
    assert!(payload.get("exhaustive").is_none(), "{payload}");
    assert!(payload.get("missingBackends").is_none(), "{payload}");

    // Scoping to a single mount works, and reports logical paths.
    let scoped = tool_call(
        &state,
        "grep_search",
        json!({"query": "charter", "glob": "Team/**/*.md"}),
    )
    .await;
    let matches = structured(&scoped)["matches"].as_array().expect("matches");
    assert!(!matches.is_empty());
    // Membership, not `matches[0]`: BOTH team notes contain "charter" (Roster.md says
    // "who signs the charter"), the search is case-insensitive by default, and
    // ripgrep walks the tree in parallel — so which hit lands first is genuinely
    // nondeterministic. This assertion was observed to flake on the ordering. What the
    // test is actually about is that scoping reaches the team mount and reports
    // LOGICAL paths, and both of those are checked here.
    let paths: Vec<&str> = matches
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect();
    assert!(paths.contains(&"Team/Charter.md"), "{paths:?}");
    // Every path is mount-prefixed, i.e. logical rather than mount-relative.
    assert!(
        paths.iter().all(|path| path.starts_with("Team/")),
        "{paths:?}"
    );
}

/// An unscoped recall on a multi-mount vault is FEDERATED, not refused.
///
/// This replaced a refusal. The refusal was correct while there was no way to merge two
/// mounts' orderings -- answering from one mount would have reported "no matches" for text
/// that exists in the vault -- but it was never the answer a caller wanted. What the payload
/// must now carry instead of an error is the provenance: which mounts were searched, how many
/// candidates each contributed, and which mount every hit came from.
#[tokio::test]
async fn unscoped_recall_federates_every_mount_and_reports_which_answered() {
    let fixture = Fixture::new("recall-unscoped");
    let state = fixture.state().await;

    // `search_artifacts` is federated too, but this fixture configures no ARTIFACT embedding
    // backend, so it cannot rank on any mount -- see the assertion at the end.
    for (tool, arguments, collection) in [
        ("hybrid_search", json!({"query": "charter"}), "matches"),
        ("load_knowledge", json!({"subject": "charter"}), "chunks"),
    ] {
        let response = tool_call(&state, tool, arguments).await;
        let payload = structured(&response);
        assert_eq!(payload["federated"], json!(true), "{tool}: {payload}");
        // Both mounts are named, whether or not they had anything to contribute: a mount
        // missing from this list would be a mount a caller cannot tell was searched.
        let mount_ids: Vec<&str> = payload["mounts"]
            .as_array()
            .unwrap_or_else(|| panic!("{tool} must report a mounts summary: {payload}"))
            .iter()
            .filter_map(|mount| mount["id"].as_str())
            .collect();
        assert_eq!(mount_ids, vec!["team", "vault"], "{tool}: {payload}");
        // Nothing failed, so nothing is missing and the answer is not degraded.
        assert_eq!(payload["degraded"], json!(false), "{tool}: {payload}");
        assert!(
            payload.get("missingBackends").is_none(),
            "{tool}: {payload}"
        );
        for mount in payload["mounts"].as_array().expect("mounts") {
            assert_eq!(mount["source"], json!("local-index"), "{tool}: {mount}");
            assert!(mount["candidateCount"].is_u64(), "{tool}: {mount}");
            assert!(mount["exhausted"].is_boolean(), "{tool}: {mount}");
            assert_eq!(mount["recallWeight"], json!(1.0), "{tool}: {mount}");
        }
        // Every hit says which mount answered it.
        for hit in payload[collection].as_array().expect("a hit collection") {
            assert!(hit["mountId"].is_string(), "{tool}: {hit}");
        }
    }

    // The whole point: "roster" exists ONLY on the team mount, and an unscoped search finds
    // it without the caller knowing the vault's mount layout.
    let response = tool_call(&state, "hybrid_search", json!({"query": "roster"})).await;
    let payload = structured(&response);
    let paths: Vec<&str> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(paths.contains(&"Team/Roster.md"), "{paths:?}");

    // A federated artifact search whose every index-backed mount failed is an ERROR, not an
    // empty `matches[]`. Artifacts have no lexical fallback, so "no ranking was produced" and
    // "there are no matching artifacts" are different facts and only one of them is true.
    let response = tool_call(&state, "search_artifacts", json!({"query": "charter"})).await;
    let message = error_message(&response);
    assert!(
        message.contains("artifact embedding") || message.contains("embedding configuration"),
        "the failure must name the artifact embedding backend, got: {message}"
    );
}

/// `find_files` enumerates every mount, in the logical namespace.
///
/// It federates while `recommend_folder` refuses because it is an ENUMERATION filtered by a
/// path match, not a ranking: the matcher is a substring or regex test over paths and the
/// result is the first `limit` matches in walk order, so merging the mounts' walks gives the
/// same answer one vault would.
#[tokio::test]
async fn find_files_enumerates_every_mount() {
    let fixture = Fixture::new("find-files");
    let state = fixture.state().await;

    let response = tool_call(&state, "find_files", json!({"query": ".md"})).await;
    let payload = structured(&response);
    let paths: Vec<&str> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect();
    // Both mounts' notes, at LOGICAL paths, sorted -- the index stores `Charter.md` and the
    // client must never see that spelling.
    assert_eq!(
        paths,
        vec![
            "Notes/Deep.md",
            "Root.md",
            "Team/Charter.md",
            "Team/Roster.md"
        ],
        "{payload}"
    );
    // Nothing was cut, so no truncation claim.
    assert!(payload.get("truncated").is_none(), "{payload}");

    // A mount-specific query reaches the mount that owns it.
    let response = tool_call(&state, "find_files", json!({"query": "Roster"})).await;
    let payload = structured(&response);
    assert_eq!(payload["matches"][0]["path"], json!("Team/Roster.md"));

    // Truncation is REPORTED on a multi-mount vault, because the merged walk is ordered by
    // logical path: a full result set is the alphabetically first `limit` matches, and a whole
    // mount can sit past the cut.
    let response = tool_call(&state, "find_files", json!({"query": ".md", "limit": 2})).await;
    let payload = structured(&response);
    assert_eq!(payload["count"], json!(2), "{payload}");
    assert_eq!(payload["truncated"], json!(true), "{payload}");
    assert!(
        payload["truncationNote"]
            .as_str()
            .expect("a truncation note")
            .contains("alphabetically first"),
        "{payload}"
    );
    // And the notes that survived are the alphabetically first ones -- the team mount is
    // entirely absent, which is exactly what the note warns about.
    let paths: Vec<&str> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, vec!["Notes/Deep.md", "Root.md"]);
}

/// `recommend_folder` is the one whole-vault tool that still refuses, and on purpose.
///
/// It scores each candidate folder by how much of the query's evidence lives under it, and
/// those counts are only comparable within ONE index. Its output is also the single folder a
/// note gets written to, so a plausible-looking arbitrary answer silently misfiles work.
/// `find_files` is federated instead, because it is an enumeration filtered by a path match
/// rather than a ranking -- see `find_files_enumerates_every_mount`.
#[tokio::test]
async fn recommend_folder_still_refuses_a_multi_mount_vault_and_says_why() {
    let fixture = Fixture::new("recall-unscopable");
    let state = fixture.state().await;

    let response = tool_call(&state, "recommend_folder", json!({"topic": "charter"})).await;
    let message = error_message(&response);
    assert!(
        message.starts_with("recommend_folder") && message.contains("multi-mount"),
        "the refusal must be explicit, got: {message}"
    );
    assert!(
        message.contains("comparable"),
        "it must name the reason -- incomparable per-index evidence -- got: {message}"
    );
    assert!(
        message.contains("list_children"),
        "it must name what the caller can do instead, got: {message}"
    );
}

#[tokio::test]
async fn a_scoped_hybrid_search_is_served_by_that_mounts_index_with_logical_paths() {
    let fixture = Fixture::new("recall-scoped");
    let state = fixture.state().await;

    // "roster" exists ONLY on the team mount. Before per-mount indexing this query
    // could not be answered at all: the root mount's index has never seen the file.
    let team = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "roster charter", "scope": "Team"}),
    )
    .await;
    let matches = structured(&team)["matches"].as_array().expect("matches");
    assert!(!matches.is_empty(), "team mount must serve its own notes");
    let paths: Vec<&str> = matches
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect();
    // LOGICAL paths, every one of them: the index stores `Roster.md`, the client
    // must never see that spelling.
    assert!(
        paths.iter().all(|path| path.starts_with("Team/")),
        "scoped results must be logical paths under the mount: {paths:?}"
    );
    assert!(paths.contains(&"Team/Roster.md"), "{paths:?}");
    // The resource URI moves with the path, or a follow-up read would 404.
    let roster = matches
        .iter()
        .find(|item| item["path"] == json!("Team/Roster.md"))
        .expect("Roster match");
    assert_eq!(
        roster["resourceUri"],
        json!("obsidian://note?path=Team%2FRoster.md")
    );

    // The root mount, addressed as "/", answers from its OWN index and cannot see
    // the team mount's notes -- each index is self-contained.
    let root = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "roster charter", "scope": "/"}),
    )
    .await;
    let root_paths: Vec<&str> = structured(&root)["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect();
    assert!(
        root_paths.iter().all(|path| !path.starts_with("Team/")),
        "the root mount's index must not contain another mount's notes: {root_paths:?}"
    );
}

/// A scope must name a mount root. These tools truncate to `limit`, so a narrower
/// scope could only be honoured by filtering an already-truncated list -- silently
/// returning fewer results than asked for. The refusal is the exact answer.
#[tokio::test]
async fn a_scope_that_does_not_name_a_mount_root_is_refused() {
    let fixture = Fixture::new("recall-deep-scope");
    let state = fixture.state().await;

    let response = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "deep", "scope": "Notes"}),
    )
    .await;
    let message = error_message(&response);
    assert!(
        message.contains("Notes") && message.contains("'vault'"),
        "the refusal must name the scope and its mount: {message}"
    );
    assert!(
        message.contains("'/'") && message.contains("'Team'"),
        "the refusal must name the usable scopes: {message}"
    );
}

#[tokio::test]
async fn path_taking_recall_routes_to_the_mount_owning_the_path() {
    let fixture = Fixture::new("path-recall");
    let state = fixture.state().await;

    // Inside the root mount: unchanged behaviour.
    for tool in ["related_notes", "graph_traverse"] {
        let response = tool_call(&state, tool, json!({"path": "Root.md"})).await;
        assert_eq!(structured(&response)["path"], json!("Root.md"));
    }

    // On the team mount: served by the TEAM index, which is the only one that has
    // ever seen this note. Previously an explicit refusal.
    let related = tool_call(
        &state,
        "related_notes",
        json!({"path": "Team/Charter.md", "limit": 5}),
    )
    .await;
    let payload = structured(&related);
    assert_eq!(payload["path"], json!("Team/Charter.md"));
    for item in payload["matches"].as_array().expect("matches") {
        let path = item["path"].as_str().expect("path");
        assert!(
            path.starts_with("Team/"),
            "related notes must be logical paths: {path}"
        );
    }

    // The wiki link inside the team vault is an edge of the TEAM mount's graph, and
    // both endpoints are reported logically.
    let graph = tool_call(
        &state,
        "graph_traverse",
        json!({"path": "Team/Charter.md", "direction": "outgoing", "depth": 1}),
    )
    .await;
    let graph = structured(&graph);
    assert_eq!(graph["path"], json!("Team/Charter.md"));
    let node_paths: Vec<&str> = graph["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|node| node["path"].as_str().expect("path"))
        .collect();
    assert!(
        node_paths.contains(&"Team/Charter.md") && node_paths.contains(&"Team/Roster.md"),
        "the mount's own graph must be traversable at logical paths: {node_paths:?}"
    );
    let edges = graph["edges"].as_array().expect("edges");
    let edge = edges
        .iter()
        .find(|edge| edge["source"] == json!("Team/Charter.md"))
        .expect("the charter's outgoing edge");
    assert_eq!(edge["target"], json!("Team/Roster.md"));
    // `rawLink` is the literal wiki-link text, not an address, so it is NOT
    // rewritten -- it must still read back as what the note actually says.
    assert_eq!(edge["rawLink"], json!("Roster"));
}

// ---------------------------------------------------------------------------
// Per-mount indexes
// ---------------------------------------------------------------------------

/// The collision guarantee, on disk: each mount's SQLite index is its own file.
/// Two runtimes sharing one index file would corrupt each other's writes.
#[tokio::test]
async fn every_mount_indexes_into_its_own_directory() {
    let fixture = Fixture::new("index-dirs");
    let state = fixture.state().await;
    tool_call(&state, "build_index", json!({})).await;

    let root_index = fixture.index_dir.join("index.sqlite");
    let team_index = fixture.team_index_dir().join("index.sqlite");
    assert!(root_index.is_file(), "root index at {root_index:?}");
    assert!(team_index.is_file(), "team index at {team_index:?}");
    assert_ne!(root_index, team_index);
    // Derived from the ROOT index dir, so a packaged install (whose indexDir points
    // outside every vault) keeps every mount's index outside every vault too.
    assert!(team_index.starts_with(&fixture.index_dir));
    // And neither vault got an index dir of its own by accident.
    assert!(!fixture.team_vault.join(".deep-obsidian-mcp").exists());
}

#[tokio::test]
async fn build_index_rebuilds_every_mount_and_reports_each_one() {
    let fixture = Fixture::new("build-index");
    let state = fixture.state().await;

    let response = tool_call(&state, "build_index", json!({})).await;
    let payload = structured(&response);
    assert_eq!(payload["rebuilt"], json!(true));

    let mounts = payload["mounts"].as_array().expect("mounts array");
    assert_eq!(mounts.len(), 2);
    for mount in mounts {
        assert_eq!(mount["rebuilt"], json!(true), "{mount}");
        assert!(
            mount["noteCount"].as_u64().expect("noteCount") > 0,
            "{mount}"
        );
    }
    assert_eq!(mounts[0]["id"], json!("vault"));
    assert_eq!(mounts[1]["id"], json!("team"));

    // The top-level counts cover the whole logical vault, not just the root mount.
    let total: u64 = mounts
        .iter()
        .map(|mount| mount["noteCount"].as_u64().expect("noteCount"))
        .sum();
    assert_eq!(payload["noteCount"], json!(total));
    // 2 root notes + 2 team notes.
    assert_eq!(total, 4);

    // The rebuild is visible to a scoped search immediately.
    let scoped = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "roster", "scope": "Team"}),
    )
    .await;
    assert!(!structured(&scoped)["matches"]
        .as_array()
        .expect("matches")
        .is_empty());
}

// ---------------------------------------------------------------------------
// Resources: enumeration federates, because it does not rank
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resources_enumerate_every_mount_at_logical_paths() {
    let fixture = Fixture::new("resources");
    let state = fixture.state().await;

    let listed = request(&state, "resources/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .filter_map(|resource| resource["name"].as_str())
        .collect();
    for expected in [
        "Root.md",
        "Notes/Deep.md",
        "Team/Charter.md",
        "Team/Roster.md",
    ] {
        assert!(
            names.contains(&expected),
            "resources/list must enumerate {expected}: {names:?}"
        );
    }
    assert_eq!(listed["result"]["_meta"]["noteResourceTotal"], json!(4));

    // Globally lexicographic over LOGICAL paths, not grouped by mount: the same
    // kind of list a single-mount client already receives.
    let note_names: Vec<&str> = names
        .iter()
        .filter(|name| name.ends_with(".md"))
        .copied()
        .collect();
    let mut sorted = note_names.clone();
    sorted.sort_unstable();
    assert_eq!(note_names, sorted, "note resources must be globally sorted");

    let index = request(
        &state,
        "resources/read",
        json!({"uri": "obsidian://vault/notes-index"}),
    )
    .await;
    let text = index["result"]["contents"][0]["text"]
        .as_str()
        .expect("notes-index text");
    let manifest: Value = serde_json::from_str(text).expect("notes-index is JSON");
    assert_eq!(manifest["noteCount"], json!(4));
    let paths: Vec<&str> = manifest["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .map(|note| note["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        vec![
            "Notes/Deep.md",
            "Root.md",
            "Team/Charter.md",
            "Team/Roster.md"
        ]
    );
}

#[tokio::test]
async fn the_vault_overview_resource_sums_counts_and_details_each_mount() {
    let fixture = Fixture::new("vault-overview");
    let state = fixture.state().await;

    let overview = request(
        &state,
        "resources/read",
        json!({"uri": "obsidian://vault/info"}),
    )
    .await;
    let text = overview["result"]["contents"][0]["text"]
        .as_str()
        .expect("overview text");
    let payload: Value = serde_json::from_str(text).expect("overview is JSON");

    // `vaultPath` still means the root mount's path: unchanged meaning.
    assert_eq!(
        payload["vaultPath"],
        json!(fixture.root_vault.to_string_lossy())
    );
    // The whole logical vault, not the root mount's share of it.
    assert_eq!(payload["markdownFileCount"], json!(4));
    let mounts = payload["mounts"].as_array().expect("mounts");
    assert_eq!(mounts.len(), 2);
    assert_eq!(mounts[1]["id"], json!("team"));
    assert_eq!(mounts[1]["mountAt"], json!("Team"));
    assert_eq!(mounts[1]["indexStatus"], json!("ready"));
    assert_eq!(mounts[1]["markdownFileCount"], json!(2));
}

// ---------------------------------------------------------------------------
// Failure isolation
// ---------------------------------------------------------------------------

/// A non-root mount whose vault cannot be read must not take the server down --
/// but readiness must not claim everything is fine either.
#[tokio::test]
async fn a_broken_mount_degrades_readiness_by_name_while_the_root_keeps_serving() {
    let fixture = Fixture::new("broken-mount");
    // A directory that is deliberately never created: the mount's index refresh
    // fails, exactly as an unreadable vault folder would.
    let missing = fixture.index_dir.parent().expect("base").join("gone");
    let config = fixture.config_with_team_vault(missing);
    // Bootstrap SUCCEEDS: a failing non-root mount is degradation, not a fatal
    // startup error.
    let state = fixture.state_for(config).await;

    // The root mount still serves reads and its own recall.
    let root = tool_call(&state, "read_file", json!({"path": "Root.md"})).await;
    assert!(structured(&root)["text"]
        .as_str()
        .expect("text")
        .contains("Root note"));
    let scoped = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "root note", "scope": "/"}),
    )
    .await;
    assert!(!structured(&scoped)["matches"]
        .as_array()
        .expect("matches")
        .is_empty());

    // Readiness is degraded, 503, and NAMES the mount. The top-level wording is
    // frozen and says nothing about mounts, so the mount id appears in the additive
    // per-mount detail rather than being laundered into `lastError`.
    let diagnostics = state.runtimes.aggregate_diagnostics();
    assert_eq!(diagnostics.status.as_str(), "degraded");
    assert_eq!(
        readiness_status_code(&diagnostics),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let mut payload = build_readiness_payload(&state.config, &diagnostics);
    insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
    assert_eq!(payload["status"], json!("degraded"));
    assert_eq!(payload["ready"], json!(false));
    assert_eq!(payload["degradedMounts"], json!(["team"]));
    let mounts = payload["mounts"].as_array().expect("mounts");
    assert_eq!(mounts[0]["indexStatus"], json!("ready"));
    assert_eq!(mounts[1]["id"], json!("team"));
    assert_eq!(mounts[1]["indexStatus"], json!("degraded"));
    assert!(!mounts[1]["lastError"]["message"]
        .as_str()
        .expect("the failure message")
        .is_empty());

    // And enumeration refuses rather than silently dropping the broken mount's
    // notes: a listing that omits them would assert they do not exist.
    let listed = request(&state, "resources/list", json!({})).await;
    let message = listed["error"]["message"]
        .as_str()
        .expect("resources/list must fail");
    assert!(
        message.contains("mount 'team'"),
        "the failure must name the mount: {message}"
    );
}

// ---------------------------------------------------------------------------
// tools/list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_scope_argument_is_declared_only_on_a_multi_mount_vault() {
    let fixture = Fixture::new("tools-list");
    let state = fixture.state().await;

    let listed = request(&state, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().expect("tools");
    for name in ["hybrid_search", "search_artifacts", "load_knowledge"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} tool definition"));
        assert!(
            tool["inputSchema"]["properties"]["scope"].is_object(),
            "{name} must declare 'scope' on a multi-mount vault"
        );
        // OPTIONAL, not required: omitting it now asks for the FEDERATED answer over every
        // mount. Declaring it required would tell a client the whole-vault search does not
        // exist, which is the opposite of true.
        assert!(
            !tool["inputSchema"]["required"]
                .as_array()
                .expect("required")
                .contains(&json!("scope")),
            "{name} must not require 'scope': an unscoped call federates every mount"
        );
        // The description has to say what omitting it does, or a client reading only the
        // schema cannot discover the federated answer at all.
        let description = tool["inputSchema"]["properties"]["scope"]["description"]
            .as_str()
            .expect("scope description");
        assert!(
            description.contains("Omit"),
            "{name}'s scope description must say what omitting it does: {description}"
        );
    }
    // The tools that cannot be scoped do not grow a meaningless argument.
    for name in ["find_files", "recommend_folder"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} tool definition"));
        assert!(tool["inputSchema"]["properties"]["scope"].is_null());
    }
    // The single-mount half of this contract is frozen by the `tools_list` golden
    // in `mcp_contract.rs`, which must never contain `scope`.
}

// ---------------------------------------------------------------------------
// vault_info
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vault_info_reports_one_descriptor_per_mount() {
    let fixture = Fixture::new("vault-info");
    let state = fixture.state().await;

    let response = tool_call(&state, "vault_info", json!({})).await;
    let payload = structured(&response);
    // `vaultPath` still means the root mount's path, unchanged.
    assert_eq!(
        payload["vaultPath"],
        json!(fixture.root_vault.to_string_lossy())
    );

    let mounts = payload["mounts"].as_array().expect("mounts array");
    assert_eq!(mounts.len(), 2);
    assert_eq!(mounts[0]["id"], json!("vault"));
    assert_eq!(mounts[0]["mountAt"], json!(""));
    assert_eq!(mounts[0]["backendKind"], json!("filesystem"));
    assert_eq!(mounts[1]["id"], json!("team"));
    assert_eq!(mounts[1]["mountAt"], json!("Team"));
    assert!(mounts[1]["capabilities"]
        .as_array()
        .expect("capabilities")
        .contains(&json!("binary-read")));

    // The text block is the pretty-printed form of the same payload, so the new
    // field appears in both halves or in neither.
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text block");
    let reparsed: Value = serde_json::from_str(text).expect("text block is JSON");
    assert_eq!(&reparsed, payload);
}

// ---------------------------------------------------------------------------
// Uploads and write protection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upload_can_be_minted_for_a_path_on_a_non_root_mount() {
    let fixture = Fixture::new("upload-mint");
    let state = fixture.state_with_uploads().await;

    // The mint validates the destination through the router (`ResolvePath`), so a
    // non-root path must be accepted rather than rejected as escaping the vault.
    let minted = tool_call(
        &state,
        "request_vault_upload",
        json!({"path": "Team/Assets/logo.png"}),
    )
    .await;
    let payload = structured(&minted);
    assert_eq!(payload["path"], json!("Team/Assets/logo.png"));
    assert!(payload["uploadUrl"]
        .as_str()
        .expect("uploadUrl")
        .contains("/upload/"));

    // A path under no mount is still refused at mint, before any capability token
    // is issued.
    let traversal = tool_call(
        &state,
        "request_vault_upload",
        json!({"path": "../escape.png"}),
    )
    .await;
    assert!(!error_message(&traversal).is_empty());
}

#[tokio::test]
async fn template_protection_still_applies_on_a_non_root_mount() {
    let fixture = Fixture::new("templates");
    let state = fixture.state_with_uploads().await;

    // `is_protected_write_path` inspects the LOGICAL path while the backend checks
    // the MOUNT-RELATIVE one. Both spellings contain the protected segment here, so
    // the two agree -- this pins that they do.
    let minted = tool_call(
        &state,
        "request_vault_upload",
        json!({"path": "Team/Templates/logo.png"}),
    )
    .await;
    assert!(
        error_message(&minted).contains("protected write path"),
        "mint must refuse a protected destination on any mount: {}",
        error_message(&minted)
    );

    // And a text write to the same folder is refused by the routed backend itself.
    let written = tool_call(
        &state,
        "upsert_note",
        json!({"path": "Team/Templates/Note.md", "content": "body"}),
    )
    .await;
    assert!(!error_message(&written).is_empty());
    assert!(!fixture.team_vault.join("Templates/Note.md").exists());
}

// ---------------------------------------------------------------------------
// A couchdb mount, end to end through the MCP surface
// ---------------------------------------------------------------------------
//
// The couchdb topologies below are all multi-mount (a filesystem root plus the couchdb
// prefix), so they belong in this suite rather than in `mcp_contract.rs`, whose goldens
// describe a single-mount vault and must not move. A couchdb mount CAN now be the root —
// see `a_couchdb_root_serves_the_whole_vault` further down, which is deliberately kept in
// this suite for the same reason.
//
// A stub sidecar stands in for the real one. That is the right level here: the real
// sidecar against the real fixture CouchDB is already covered end to end in
// `deep-obsidian-backend`'s `couchdb_sidecar.rs`, and what this suite adds is the
// part that suite cannot see -- that the ROUTER sends a path on the couchdb prefix to
// the couchdb backend, that a read comes back through the MCP tool surface, and that
// a write on that prefix is refused with the experimental read-only message while the
// filesystem root stays writable.

/// A node script that speaks protocol v1 and serves two notes from memory.
///
/// Echoes the exact `supported` triple the supervisor enforces, so a drift in that
/// triple fails this test too.
///
/// It also models the parts of the write surface this suite needs to observe through
/// the MCP tools: `initialize.mode` (refusing `write` and `delete` with
/// `read-only`/-32009 unless `read-write` was asked for), a real revision-guarded
/// compare-and-swap on both (-32008 with `data.conflict.currentRev`), and LiveSync's
/// SOFT delete -- a tombstone that keeps the entry readable at its path, so writing it
/// back resurrects the note. Revisions are a counter rather than CouchDB hashes -- what
/// this suite asserts is that the REVISION THREADING is wired end to end, not how CouchDB
/// derives a rev, which `couchdb_sidecar.rs` covers against the real thing.
const STUB_SIDECAR: &str = r##"
import { createInterface } from "node:readline";
import { existsSync } from "node:fs";
/**
 * Where this child's compatibility verdict comes from.
 *
 * `null` -- the value every test but the readiness-recovery one uses -- means "always
 * ok". A PATH makes the verdict depend on whether that file existed when THIS CHILD
 * STARTED, which is exactly the mechanism a readiness-recovery test needs: the sidecar
 * protocol refuses a second `initialize` on one connection, so the supervisor can only
 * obtain a fresh verdict by starting a fresh child, and a fresh child re-reads the flag.
 * Rewritten by `gated_stub_sidecar`.
 */
const READY_FLAG = null;
const COMPATIBILITY =
    READY_FLAG === null || existsSync(READY_FLAG)
        ? { status: "ok" }
        : { status: "unreachable", detail: "the fixture remote is refusing connections" };
const NOTES = {
    "Charter.md": "# LiveSync Charter\n\nServed from the CouchDB mount.\n",
    "Deep/Nested.md": "# Nested\n\nA nested LiveSync note.\n",
};
/** Path -> revision. Bumped on every accepted write, as CouchDB would. */
const REVS = { "Charter.md": "1-stub", "Deep/Nested.md": "1-stub" };
/**
 * Path -> entry kind. Modelled rather than assumed: LiveSync stores a `plain` entry as
 * text and a `newnote` as base64 chunks, and `read` reports which it was. A stub that
 * always answered `text` would silently corrupt every byte above 0x7f on the way back
 * out, and the upload round-trip test would be asserting against a fiction.
 */
const KINDS = { "Charter.md": "markdown", "Deep/Nested.md": "markdown" };
/** Binary bodies, kept as base64 exactly as they arrived. */
const BINARIES = {};
/**
 * Paths that are LiveSync TOMBSTONES.
 *
 * A set rather than a deletion from `NOTES`, because that is what the storage does: a
 * soft-deleted entry is still a document, its chunks are still there, and `read`/`stat`
 * still answer for it. Modelling it as a removal would make the stub agree with the Rust
 * side about listings while silently disagreeing about everything else — and the
 * resurrection path, which writes over a tombstone, would have nothing to write over.
 */
const DELETED = new Set();
let revSeed = 1;
let mode = "read-only";
const rl = createInterface({ input: process.stdin });
rl.on("line", (line) => {
    const text = line.trim();
    if (!text) return;
    const message = JSON.parse(text);
    const reply = (result) =>
        process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: message.id, result }) + "\n");
    const fail = (code, kind, detail) =>
        process.stdout.write(
            JSON.stringify({ jsonrpc: "2.0", id: message.id, error: { code, message: detail, data: { kind, detail } } }) + "\n"
        );
    switch (message.method) {
        case "initialize":
            mode = message.params.mode ?? "read-only";
            return reply({
                protocolVersion: 1,
                mode,
                sidecarVersion: "0.1.0",
                commonlibVersion: "0.1.2",
                supportedSchemaVersion: 12,
                supported: {
                    protocolVersion: 1,
                    commonlibVersion: "0.1.2",
                    maxSchemaVersion: 12,
                    pluginVersionTested: "1.0.3",
                },
                compatibility: COMPATIBILITY,
                remote: { schemaVersion: 12, encrypted: false, pathObfuscation: false },
            });
        case "manifest":
            return reply({
                entries: [
                    ...Object.entries(NOTES).map(([path, body]) => ({
                        path,
                        size: Buffer.byteLength(body),
                        kind: "markdown",
                    })),
                    ...Object.entries(BINARIES).map(([path, base64]) => ({
                        path,
                        size: Buffer.from(base64, "base64").length,
                        kind: "binary",
                    })),
                ].map((entry) => ({
                    ...entry,
                    mtimeMs: 1700000000000,
                    ctimeMs: 1700000000000,
                    // A tombstone IS listed by the manifest, carrying the flag. Excluding
                    // it is the Rust side's job (`is_listable`), and a stub that hid it
                    // here would make that filter untestable.
                    deleted: DELETED.has(entry.path),
                    conflicted: false,
                })),
                exhausted: true,
            });
        case "read": {
            if (KINDS[message.params.path] === "binary") {
                const base64 = BINARIES[message.params.path];
                return reply({
                    kind: "binary",
                    base64,
                    path: message.params.path,
                    size: Buffer.from(base64, "base64").length,
                    mtimeMs: 1700000000000,
                    ctimeMs: 1700000000000,
                    deleted: DELETED.has(message.params.path),
                    conflicted: false,
                    rev: REVS[message.params.path],
                });
            }
            const body = NOTES[message.params.path];
            if (body === undefined) return fail(-32004, "not-found", "no entry at that path");
            return reply({
                kind: "text",
                text: body,
                path: message.params.path,
                size: Buffer.byteLength(body),
                mtimeMs: 1700000000000,
                ctimeMs: 1700000000000,
                deleted: DELETED.has(message.params.path),
                conflicted: false,
                rev: REVS[message.params.path],
            });
        }
        case "stat": {
            if (KINDS[message.params.path] === "binary") {
                const base64 = BINARIES[message.params.path];
                return reply({
                    kind: "binary",
                    path: message.params.path,
                    size: Buffer.from(base64, "base64").length,
                    mtimeMs: 1700000000000,
                    ctimeMs: 1700000000000,
                    deleted: DELETED.has(message.params.path),
                    conflicted: false,
                    rev: REVS[message.params.path],
                });
            }
            const body = NOTES[message.params.path];
            if (body === undefined) return fail(-32004, "not-found", "no entry at that path");
            return reply({
                kind: "markdown",
                path: message.params.path,
                size: Buffer.byteLength(body),
                mtimeMs: 1700000000000,
                ctimeMs: 1700000000000,
                deleted: DELETED.has(message.params.path),
                conflicted: false,
                rev: REVS[message.params.path],
            });
        }
        case "write": {
            // A config-level refusal, before anything reaches a remote.
            if (mode !== "read-write") {
                return fail(-32009, "read-only", "this sidecar was initialized read-only");
            }
            const path = message.params.path;
            const current = REVS[path];
            const hasBaseRev = Object.prototype.hasOwnProperty.call(message.params, "baseRev");
            const baseRev = message.params.baseRev;
            const conflict = (expected) =>
                process.stdout.write(
                    JSON.stringify({
                        jsonrpc: "2.0",
                        id: message.id,
                        error: {
                            code: -32008,
                            message: "conflict",
                            data: {
                                kind: "conflict",
                                detail: "the revision guard did not hold",
                                conflict: {
                                    ...(current !== undefined ? { currentRev: current } : {}),
                                    expected,
                                    deleted: false,
                                    conflicted: false,
                                },
                            },
                        },
                    }) + "\n"
                );
            // The three CAS modes, exactly as the protocol defines them.
            if (hasBaseRev && baseRev === null) {
                // create-only
                if (current !== undefined) return conflict(null);
            } else if (hasBaseRev) {
                // guarded update
                if (current !== baseRev) return conflict(baseRev);
            }
            const created = current === undefined;
            // Writing over a tombstone brings the entry back, and the flag is simply not
            // carried over -- resurrection is structural, exactly as upstream's is.
            const resurrected = DELETED.delete(path);
            revSeed += 1;
            REVS[path] = `${revSeed}-stub`;
            const isText = message.params.content.kind === "text";
            KINDS[path] = isText ? "markdown" : "binary";
            let size;
            if (isText) {
                NOTES[path] = message.params.content.text;
                delete BINARIES[path];
                size = Buffer.byteLength(NOTES[path]);
            } else {
                // Kept as base64 verbatim, so the bytes survive: decoding to a JS string
                // would mangle everything above 0x7f.
                BINARIES[path] = message.params.content.base64;
                delete NOTES[path];
                size = Buffer.from(BINARIES[path], "base64").length;
            }
            return reply({
                path,
                rev: REVS[path],
                conflicted: false,
                size,
                mtimeMs: 1700000000000,
                ctimeMs: 1700000000000,
                kind: message.params.content.kind === "text" ? "markdown" : "binary",
                created,
                resurrected,
            });
        }
        case "delete": {
            // The same config-level refusal `write` gets, with the same code: a delete is
            // a write, and the sidecar that owns the mode is what enforces it.
            if (mode !== "read-write") {
                return fail(-32009, "read-only", "this sidecar was initialized read-only");
            }
            const path = message.params.path;
            const current = REVS[path];
            // A path with no document at all. NOT the same as a tombstone, which is a
            // document and is deleted again quite happily -- see below.
            if (current === undefined) return fail(-32004, "not-found", "no entry at this path");
            const baseRev = message.params.baseRev;
            // `null` and absent both mean unguarded; only a string is a precondition.
            // Create-only is meaningless for a delete, so there is no third case.
            if (typeof baseRev === "string" && current !== baseRev) {
                return process.stdout.write(
                    JSON.stringify({
                        jsonrpc: "2.0",
                        id: message.id,
                        error: {
                            code: -32008,
                            message: "conflict",
                            data: {
                                kind: "conflict",
                                detail: "guarded delete refused: the remote revision moved",
                                conflict: {
                                    currentRev: current,
                                    expected: baseRev,
                                    deleted: DELETED.has(path),
                                    conflicted: false,
                                },
                            },
                        },
                    }) + "\n"
                );
            }
            // Deleting a tombstone is accepted and produces a FRESH revision, which is
            // what the real sidecar does (it re-sets `deleted` and puts the document). The
            // Rust side is what makes a repeated delete cost nothing, by answering from
            // its `stat` instead of asking -- so this arm must stay permissive, or that
            // short-circuit would be untestable and the stub would be asserting a fiction.
            revSeed += 1;
            REVS[path] = `${revSeed}-stub`;
            DELETED.add(path);
            // The content and the chunks stay: the entry is still readable at this path.
            return reply({ path, rev: REVS[path], deleted: true });
        }
        case "changesSince":
            return reply({ changes: [], nextCursor: "c1", exhausted: true });
        case "watch":
            return reply({ watching: true, cursor: "c1" });
        case "unwatch":
            return reply({ watching: false });
        case "health":
            return reply({ status: "ok", mode, compatibility: COMPATIBILITY, watching: false, uptimeMs: 1 });
        case "shutdown":
            reply({ ok: true });
            return process.exit(0);
        default:
            return fail(-32601, "method-not-found", message.method);
    }
});
"##;

/// True when `node` can run. The stub is a node script, so this suite's couchdb tests
/// gate on the same prerequisite the real backend does.
fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A filesystem root mount plus a couchdb mount at `LiveSync`, backed by the stub.
struct CouchdbFixture {
    inner: Fixture,
    stub: PathBuf,
    secrets: PathBuf,
}

impl CouchdbFixture {
    fn new(name: &str) -> Self {
        let inner = Fixture::new(name);
        let base = inner
            .index_dir
            .parent()
            .expect("fixture base")
            .to_path_buf();
        let stub = base.join("stub-sidecar.mjs");
        fs::write(&stub, STUB_SIDECAR).expect("write the stub sidecar");
        Self {
            inner,
            stub,
            secrets: base.join("secrets.json"),
        }
    }

    /// The two-mount config, with the couchdb mount pointed at the stub. READ-ONLY,
    /// which is what `writable` defaults to in the config schema.
    fn config(&self) -> ResolvedServiceConfig {
        self.config_writable(false)
    }

    /// The same table with the couchdb mount opted in to writes.
    fn config_writable(&self, writable: bool) -> ResolvedServiceConfig {
        let mut config = self.inner.config();
        config.experimental = ExperimentalConfig {
            multi_vault: true,
            couchdb_vaults: true,
            ..ExperimentalConfig::default()
        };
        // Replace the `team` filesystem mount with a couchdb one at the same prefix,
        // so the routing assertions below are about the BACKEND KIND rather than about
        // a different prefix.
        config.mounts[1] = MountConfig {
            unknown: Default::default(),
            recall_weight: None,
            id: "live".to_string(),
            mount_at: "LiveSync".to_string(),
            backend: MountBackendConfig::Couchdb {
                url: "http://couch.invalid".to_string(),
                database: "vault".to_string(),
                username: Some("vaultuser".to_string()),
                password_ref: SecretRef::EncryptedFile {
                    id: "livesync-password".to_string(),
                },
                e2ee: None,
                sidecar_path: Some(self.stub.clone()),
                index_dir: None,
                options: None,
                writable,
            },
        };
        config
    }

    /// State over the couchdb config, with the password stored in a TEMP secrets file.
    ///
    /// A temp store rather than `XDG_CONFIG_HOME`: that variable is process-global and
    /// mutating it races every other test that reads the default secrets path.
    async fn state(&self) -> AppState {
        self.state_writable(false).await
    }

    /// The same state with the couchdb mount opted in to writes.
    async fn state_writable(&self, writable: bool) -> AppState {
        let resolver = SecretResolver::with_encrypted_file_path(self.secrets.clone());
        resolver
            .put(
                &SecretRef::EncryptedFile {
                    id: "livesync-password".to_string(),
                },
                secrecy::SecretString::new("s3cr3t-password-value".to_string()),
            )
            .expect("store the fixture password");

        let config = self.config_writable(writable);
        let backends = MountBackends::build_with_resolver(&config, &resolver);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("a couchdb mount must not fail the bootstrap");
        AppState::with_backends(config, runtimes, &backends)
    }
}

/// The config the gate rejects, and the one it accepts.
#[test]
fn a_couchdb_mount_requires_the_couchdb_vaults_flag() {
    let fixture = CouchdbFixture::new("gate");
    let config = fixture.config();

    // The resolved config used above already has both flags; assert the gate through
    // the normalizer, which is what a real config file goes through.
    let input = deep_obsidian_types::ServiceConfigInput {
        mounts: Some(config.mounts.clone()),
        experimental: Some(ExperimentalConfig {
            multi_vault: true,
            couchdb_vaults: false,
            ..ExperimentalConfig::default()
        }),
        ..Default::default()
    };
    let error = deep_obsidian_server::normalize_service_config(input)
        .expect_err("an ungated couchdb mount must be refused");
    assert!(error.to_string().contains("couchdbVaults"), "{error}");

    let input = deep_obsidian_types::ServiceConfigInput {
        mounts: Some(config.mounts.clone()),
        experimental: Some(ExperimentalConfig {
            multi_vault: true,
            couchdb_vaults: true,
            ..ExperimentalConfig::default()
        }),
        ..Default::default()
    };
    assert!(deep_obsidian_server::normalize_service_config(input).is_ok());
}

/// A read on the couchdb prefix is served BY THE COUCHDB BACKEND, and a read on the
/// root is still served by the filesystem one.
#[tokio::test]
async fn reads_on_a_couchdb_prefix_are_served_by_the_couchdb_backend() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-read");
    let state = fixture.state().await;

    // The stub's content, reached through the MCP tool surface at the MOUNTED path.
    let live = tool_call(&state, "read_file", json!({"path": "LiveSync/Charter.md"})).await;
    let text = structured(&live)["text"]
        .as_str()
        .expect("text payload")
        .to_string();
    assert!(text.contains("Served from the CouchDB mount"), "{text}");

    // ...and the filesystem root is untouched by any of this.
    let root = tool_call(&state, "read_file", json!({"path": "Root.md"})).await;
    let root_text = structured(&root)["text"].as_str().expect("text payload");
    assert!(
        root_text.contains("Root note in the root mount"),
        "{root_text}"
    );

    // The couchdb mount's index lands in ITS OWN directory, keyed by mount id under
    // the root's. A collision here would be two `RuntimeState`s writing one SQLite
    // file, and `IndexTarget::from_factory` derives the path from `index_dir` alone
    // (a couchdb mount has no vault path to derive it from), so this is worth pinning
    // -- it is what `every_mount_indexes_into_its_own_directory` covers for
    // filesystem mounts.
    let live_index = fixture.inner.index_dir.join("mounts").join("live");
    assert!(
        live_index.join("index.sqlite").is_file(),
        "the couchdb mount's index must land at {}",
        live_index.display()
    );
    // ...and it is a DIFFERENT file from the root mount's.
    assert!(fixture.inner.index_dir.join("index.sqlite").is_file());

    // `vault_info` names the backend kind per mount, which is how an operator sees
    // which mount is the experimental one.
    let info = tool_call(&state, "vault_info", json!({})).await;
    let mounts = structured(&info)["mounts"]
        .as_array()
        .expect("per-mount detail");
    let live_mount = mounts
        .iter()
        .find(|mount| mount["id"] == json!("live"))
        .expect("the couchdb mount is reported");
    assert_eq!(live_mount["backendKind"], json!("couchdb"));
}

/// A listing on the couchdb prefix synthesizes folders from path prefixes: a LiveSync
/// vault is a flat map with no directories of its own.
#[tokio::test]
async fn listings_on_a_couchdb_prefix_synthesize_folders() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-list");
    let state = fixture.state().await;

    let listing = tool_call(&state, "list_children", json!({"path": "LiveSync"})).await;
    let entries = structured(&listing)["children"]
        .as_array()
        .expect("children")
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().unwrap_or_default().to_string(),
                entry["kind"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<Vec<_>>();

    // `Deep` exists only as a path prefix of `Deep/Nested.md`, and is reported as a
    // directory; directories come first, exactly as for a filesystem mount.
    assert_eq!(
        entries,
        vec![
            ("LiveSync/Deep".to_string(), "directory".to_string()),
            ("LiveSync/Charter.md".to_string(), "file".to_string()),
        ],
        "{entries:?}"
    );
}

/// A write on the couchdb prefix is refused with the experimental read-only message,
/// while the SAME tool still writes to the filesystem root.
#[tokio::test]
async fn writes_on_a_couchdb_prefix_are_refused_but_the_root_stays_writable() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-write");
    let state = fixture.state().await;

    let refused = tool_call(
        &state,
        "upsert_note",
        json!({"path": "LiveSync/New.md", "content": "# New\n\nbody\n"}),
    )
    .await;
    let message = error_message(&refused);
    assert!(message.contains("EXPERIMENTAL"), "{message}");
    assert!(message.contains("READ-ONLY"), "{message}");
    // The refusal points at what DOES work rather than stopping at "unsupported".
    assert!(message.contains("filesystem mount"), "{message}");

    // Nothing was written anywhere.
    let missing = tool_call(&state, "read_file", json!({"path": "LiveSync/New.md"})).await;
    assert!(missing.get("error").is_some(), "{missing}");

    // ...and the root mount is unaffected: the refusal is per-mount, not global.
    let written = tool_call(
        &state,
        "upsert_note",
        json!({"path": "RootWritten.md", "content": "# Root Written\n\nbody\n"}),
    )
    .await;
    assert!(
        written.get("result").is_some(),
        "the filesystem root must stay writable: {written}"
    );
}

/// The write tools work end to end on a WRITABLE couchdb mount: create, overwrite,
/// section update, and session note.
///
/// The point is not that a write returns success — it is that every one of these tools
/// composes its content above the boundary and then lands it through the couchdb write
/// path, and that a read afterwards serves the composed content back.
#[tokio::test]
async fn the_write_tools_work_end_to_end_on_a_writable_couchdb_mount() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-writable");
    let state = fixture.state_writable(true).await;

    // A create on a path the stub does not have.
    let created = tool_call(
        &state,
        "upsert_note",
        json!({"path": "LiveSync/Fresh.md", "content": "# Fresh\n\nWritten by the agent.\n"}),
    )
    .await;
    let payload = structured(&created);
    assert_eq!(payload["created"], json!(true), "{payload}");
    assert_eq!(payload["action"], json!("created"), "{payload}");

    let read_back = tool_call(&state, "read_file", json!({"path": "LiveSync/Fresh.md"})).await;
    assert!(
        structured(&read_back)["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Written by the agent."),
        "{read_back}"
    );

    // An overwrite of an existing note, which is where the revision threading matters:
    // the tool read the note (getting its rev), and the write must be accepted under
    // exactly that rev.
    let updated = tool_call(
        &state,
        "upsert_note",
        json!({"path": "LiveSync/Charter.md", "content": "# Charter\n\nRevised.\n"}),
    )
    .await;
    let payload = structured(&updated);
    assert_eq!(payload["created"], json!(false), "{payload}");
    assert_eq!(payload["action"], json!("updated"), "{payload}");

    // A section update, which reads-composes-writes in one tool call.
    let sectioned = tool_call(
        &state,
        "update_note_section",
        json!({
            "path": "LiveSync/Charter.md",
            "heading": "Status",
            "content": "Green.",
            "createIfMissing": true
        }),
    )
    .await;
    assert!(
        sectioned.get("result").is_some(),
        "a section update must land on a writable couchdb mount: {sectioned}"
    );
    let read_back = tool_call(&state, "read_file", json!({"path": "LiveSync/Charter.md"})).await;
    let text = structured(&read_back)["text"].as_str().unwrap_or_default();
    assert!(text.contains("## Status"), "{text}");
    assert!(text.contains("Green."), "{text}");

    // A session note, whose path is derived rather than given.
    let session = tool_call(
        &state,
        "upsert_session_note",
        json!({"path": "LiveSync/Sessions/Today.md", "content": "Session body.\n"}),
    )
    .await;
    assert!(
        session.get("result").is_some(),
        "a session note must land on a writable couchdb mount: {session}"
    );

    // The filesystem root is unaffected by any of this.
    let root = tool_call(
        &state,
        "upsert_note",
        json!({"path": "RootStillWritable.md", "content": "# Root\n\nbody\n"}),
    )
    .await;
    assert!(root.get("result").is_some(), "{root}");
}

/// Every path an unscoped-by-mount `grep_search` reports for `needle`.
async fn grepped_paths(state: &AppState, needle: &str) -> Vec<String> {
    let response = tool_call(
        state,
        "grep_search",
        json!({"query": needle, "scope": "LiveSync"}),
    )
    .await;
    structured(&response)["matches"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry["path"].as_str().map(str::to_string))
        .collect()
}

/// `delete_note` on a WRITABLE couchdb mount, end to end: the tombstone leaves every
/// enumeration, the payload says how to undo it WITHOUT naming the history tools, a second
/// delete is a no-op, and `upsert_note` brings the note back.
///
/// # What this adds over the backend suite
///
/// `couchdb_sidecar.rs` proves the storage semantics against the real sidecar. What only
/// this level can show is the SURFACE: that the tool is registered at all for a vault whose
/// only capable mount is couchdb, that the payload's `howToRecover` is built for a mount
/// with no version history rather than assuming one, and that the recovery it describes is
/// something a caller can actually perform with the tools this vault advertises — which for
/// a couchdb mount means `read_file` and `upsert_note`, because `read_version` is not even
/// registered.
#[tokio::test]
async fn delete_note_tombstones_a_couchdb_note_and_says_how_to_undo_it() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-delete");
    let state = fixture.state_writable(true).await;
    let path = "LiveSync/Deep/Nested.md";

    // The tool exists BECAUSE the couchdb mount advertises `soft-delete`, and the three
    // history tools do not exist, because no mount advertises `version-history`. This
    // combination is the one the Algolia mount never produces.
    let names = tool_names(&request(&state, "tools/list", json!({})).await);
    assert!(
        names.contains(&"delete_note".to_string()),
        "a writable couchdb mount must advertise delete_note: {names:?}"
    );
    for absent in ["note_history", "read_version", "resolve_divergence"] {
        assert!(
            !names.contains(&absent.to_string()),
            "{absent} must stay absent: CouchDB retains revisions but nothing here can \
             fetch one: {names:?}"
        );
    }

    let original = structured(&tool_call(&state, "read_file", json!({"path": path})).await)["text"]
        .as_str()
        .expect("the note's text")
        .to_string();

    // The needle the negative assertions below are about, checked POSITIVE first. Without
    // this every "the tombstone is gone" assertion would also pass against a grep that
    // never matched anything.
    let needle = "nested LiveSync note";
    let before = grepped_paths(&state, needle).await;
    assert_eq!(
        before,
        vec![path.to_string()],
        "precondition: exactly this note carries the needle"
    );

    let deleted =
        structured(&tool_call(&state, "delete_note", json!({"path": path})).await).clone();
    assert_eq!(deleted["deleted"], json!(true), "{deleted}");
    assert_eq!(deleted["alreadyDeleted"], json!(false), "{deleted}");
    assert!(
        deleted["versionId"]
            .as_str()
            .is_some_and(|rev| !rev.is_empty()),
        "the tombstone's revision is reported: {deleted}"
    );
    // NO `recoverableFrom`: there is no versionId a versioned read could serve here, and
    // inventing one would name a tool this vault does not even advertise.
    assert!(
        deleted.get("recoverableFrom").is_none(),
        "a mount with no version history must not name a recoverable version: {deleted}"
    );
    let recovery = deleted["howToRecover"]
        .as_str()
        .expect("recovery guidance is present on every delete, recoverable version or not");
    assert!(
        !recovery.contains("read_version"),
        "the guidance must not point at a tool that is not registered for this vault: \
         {recovery}"
    );
    assert!(
        recovery.contains("read_file") && recovery.contains("upsert_note"),
        "it must name the two tools that DO recover the note here: {recovery}"
    );
    assert!(
        recovery.contains("no version history"),
        "and say why the history route is unavailable: {recovery}"
    );

    // The tombstone is gone from every enumeration, with no rebuild and no wait: all three
    // of these are answered from the mount's manifest.
    let listed = tool_call(&state, "list_children", json!({"path": "LiveSync/Deep"})).await;
    let names: Vec<String> = structured(&listed)["children"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.contains(&"Nested.md".to_string()),
        "a tombstone is not a file: {names:?}"
    );
    let found = tool_call(&state, "find_files", json!({"query": "Nested"})).await;
    let paths: Vec<String> = structured(&found)["matches"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry["path"].as_str().map(str::to_string))
        .collect();
    assert!(
        !paths.contains(&path.to_string()),
        "find_files walks the manifest, so it must not enumerate a tombstone: {paths:?}"
    );
    assert!(
        grepped_paths(&state, needle).await.is_empty(),
        "an exhaustive grep must not report a line from a tombstone"
    );

    // Recall is the one enumeration a delete does NOT reach synchronously, and that is the
    // shape of this mount rather than a bug in the delete: a couchdb mount has a LOCAL
    // index built by walking the manifest, so the tombstone leaves recall when the index is
    // next built — which the manifest walk, now excluding it, is what feeds. Asserted after
    // an explicit rebuild rather than assumed, because the alternative (the index keeps
    // serving a deleted note forever) would be the real defect.
    tool_call(&state, "build_index", json!({})).await;
    let recall = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "LiveSync", "scope": "LiveSync"}),
    )
    .await;
    let recall_paths: Vec<String> = structured(&recall)["matches"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry["path"].as_str().map(str::to_string))
        .collect();
    assert!(
        !recall_paths.contains(&path.to_string()),
        "a rebuilt index must not still rank a tombstoned note: {recall_paths:?}"
    );
    assert!(
        recall_paths.contains(&"LiveSync/Charter.md".to_string()),
        "...while the mount's surviving notes are still ranked, so the assertion above is \
         about the tombstone rather than about an empty answer: {recall_paths:?}"
    );

    // `read_file` still serves it, which is NOT an oversight: it is pre-existing behaviour
    // of this mount (a tombstone keeps its stored content, so a caller holding a stale path
    // gets the content rather than a lie) and it is the recovery route the payload just
    // described. The Algolia mount differs here — its tombstone carries no body and a read
    // of one fails — and the difference is real rather than a bug on either side.
    let still = tool_call(&state, "read_file", json!({"path": path})).await;
    assert_eq!(
        structured(&still)["text"],
        json!(original),
        "the tombstone's content is what `howToRecover` sends the caller to read: {still}"
    );

    // A second delete is a successful no-op.
    let again = structured(&tool_call(&state, "delete_note", json!({"path": path})).await).clone();
    assert_eq!(again["alreadyDeleted"], json!(true), "{again}");
    assert_eq!(
        again["versionId"], deleted["versionId"],
        "an unchanged entry reports the revision it already had, so a repeated delete \
         replicates nothing: {again}"
    );

    // Recovery, exactly as `howToRecover` describes it: write the content back.
    let restored = tool_call(
        &state,
        "upsert_note",
        json!({"path": path, "content": original}),
    )
    .await;
    assert!(
        restored.get("result").is_some(),
        "writing over a tombstone must land: {restored}"
    );
    let listed = tool_call(&state, "list_children", json!({"path": "LiveSync/Deep"})).await;
    let names: Vec<String> = structured(&listed)["children"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.contains(&"Nested.md".to_string()),
        "the resurrected note is listed again: {names:?}"
    );

    // The one thing this must not have granted: deletion of a LOCAL file. The tool now
    // EXISTS in this vault, which is exactly when that guarantee is worth re-checking.
    let refused =
        error_message(&tool_call(&state, "delete_note", json!({"path": "Root.md"})).await)
            .to_string();
    assert!(
        refused.contains("mount 'vault'") && refused.contains("filesystem"),
        "the refusal names the mount and its backend: {refused}"
    );
    assert!(
        refused.contains("no deletion of local vault files"),
        "and says the omission is deliberate: {refused}"
    );
    assert!(
        refused.contains("'LiveSync/'"),
        "and names the mount that does support it: {refused}"
    );
    assert!(
        fixture.inner.root_vault.join("Root.md").exists(),
        "a refused delete must not have removed anything"
    );
}

/// On a READ-ONLY couchdb mount `delete_note` is not advertised, and calling it anyway is
/// refused by naming `writable` — not by claiming the backend cannot delete.
///
/// The refusal's wording is the whole point. This backend CAN soft-delete; the reason this
/// mount cannot is a setting in the mount table. The generic "removing a note here would be
/// an ordinary file deletion" sentence is true of a filesystem mount and false of this one,
/// and a refusal that misstates its own cause sends the reader looking in the wrong place —
/// the lesson the read-only write refusal already carries.
#[tokio::test]
async fn a_read_only_couchdb_mount_refuses_a_delete_by_naming_writable() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-delete-read-only");
    let state = fixture.state().await;

    let names = tool_names(&request(&state, "tools/list", json!({})).await);
    assert!(
        !names.contains(&"delete_note".to_string()),
        "a read-only vault must not advertise delete_note at all: {names:?}"
    );

    let message = error_message(
        &tool_call(
            &state,
            "delete_note",
            json!({"path": "LiveSync/Charter.md"}),
        )
        .await,
    )
    .to_string();
    assert!(
        message.contains("mount 'live'") && message.contains("couchdb"),
        "the refusal names the mount and its backend: {message}"
    );
    assert!(
        message.contains("\"writable\": true"),
        "and the exact setting that changes it: {message}"
    );
    assert!(
        !message.contains("ordinary file deletion"),
        "and must NOT claim this removal would be a local unlink: {message}"
    );
    assert!(
        message.contains("No mount in this vault supports it."),
        "with no capable mount there is nothing to suggest: {message}"
    );

    // The note is untouched, and still readable.
    let read = tool_call(&state, "read_file", json!({"path": "LiveSync/Charter.md"})).await;
    assert!(read.get("result").is_some(), "{read}");

    // And the history tools refuse a couchdb path with ITS reason, not the filesystem's.
    // "One content per note by construction" is false of CouchDB — it retains revisions —
    // and the whole point of the per-backend refusals is that neither of them says something
    // untrue about the other's storage.
    let message = error_message(
        &tool_call(
            &state,
            "note_history",
            json!({"path": "LiveSync/Charter.md"}),
        )
        .await,
    )
    .to_string();
    assert!(
        message.contains("does retain revisions"),
        "the refusal must not claim this storage keeps one content per note: {message}"
    );
    assert!(
        message.contains("No configuration turns this on."),
        "and must separate this from `writable`, which cannot help here: {message}"
    );
}

/// A stale `expectedHash` on a couchdb mount is refused with the SAME error taxonomy a
/// filesystem mount produces, and nothing is written.
///
/// The check itself happens above the boundary, which is exactly why the wording is
/// identical: there is one implementation of it, not one per backend.
#[tokio::test]
async fn an_expected_hash_conflict_on_a_couchdb_mount_matches_the_filesystem_taxonomy() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-hash-conflict");
    let state = fixture.state_writable(true).await;

    let stale = json!("fnv1a64:0000000000000000");
    let couchdb = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "LiveSync/Charter.md",
            "content": "# Charter\n\nclobbered\n",
            "expectedHash": stale
        }),
    )
    .await;
    let filesystem = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "Root.md",
            "content": "# Root\n\nclobbered\n",
            "expectedHash": stale
        }),
    )
    .await;

    for (label, response) in [("couchdb", &couchdb), ("filesystem", &filesystem)] {
        let message = error_message(response);
        assert!(
            message.starts_with("hash conflict for "),
            "[{label}] {message}"
        );
        assert!(
            message.contains("expected fnv1a64:0000000000000000"),
            "[{label}] {message}"
        );
    }

    // Neither note was touched.
    let charter = tool_call(&state, "read_file", json!({"path": "LiveSync/Charter.md"})).await;
    assert!(
        !structured(&charter)["text"]
            .as_str()
            .unwrap_or_default()
            .contains("clobbered"),
        "{charter}"
    );
    assert!(
        !fs::read_to_string(fixture.inner.root_vault.join("Root.md"))
            .expect("read Root.md")
            .contains("clobbered")
    );
}

/// A dry run on a couchdb mount never reaches the write path.
///
/// Composition and the hash comparison both happen above the boundary, so a dry run is
/// structurally incapable of touching the remote — this pins that, because a future
/// refactor that moved composition down would break it silently.
#[tokio::test]
async fn a_dry_run_on_a_couchdb_mount_never_writes() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-dry-run");
    let state = fixture.state_writable(true).await;

    let before = tool_call(&state, "read_file", json!({"path": "LiveSync/Charter.md"})).await;
    let before = structured(&before)["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let dry = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "LiveSync/Charter.md",
            "content": "# Charter\n\nDRY RUN CONTENT\n",
            "dryRun": true
        }),
    )
    .await;
    let payload = structured(&dry);
    assert_eq!(payload["dryRun"], json!(true), "{payload}");
    // It still reports what it WOULD write, which is the whole value of a dry run.
    assert!(payload["newHash"].is_string(), "{payload}");

    let after = tool_call(&state, "read_file", json!({"path": "LiveSync/Charter.md"})).await;
    assert_eq!(
        structured(&after)["text"].as_str().unwrap_or_default(),
        before,
        "a dry run must leave the remote untouched"
    );

    // A dry run against a path that does not exist must also not create it.
    let dry_create = tool_call(
        &state,
        "upsert_note",
        json!({"path": "LiveSync/NeverWritten.md", "content": "body", "dryRun": true}),
    )
    .await;
    assert!(dry_create.get("result").is_some(), "{dry_create}");
    let missing = tool_call(
        &state,
        "read_file",
        json!({"path": "LiveSync/NeverWritten.md"}),
    )
    .await;
    assert!(missing.get("error").is_some(), "{missing}");
}

/// `request_vault_upload` → commit → `read_artifact`, all on a writable couchdb prefix.
///
/// The backend suite covers `CommitUploadStream` against the real sidecar. What this adds
/// is the surface above it, which is couchdb-specific in ways the backend tests cannot
/// see: the mint validates the destination through the ROUTER's `ResolvePath` (and
/// couchdb's path rules are stricter than the filesystem's), enforces the protected-path
/// policy, and fans the staging sweep across every mount including the couchdb no-op —
/// and then `read_artifact` has to serve the bytes back from the same logical path the
/// token was bound to.
///
/// Driven through the upload store and `commit_stream_via_backend`, which is the exact
/// path `upload_handler` drives; the HTTP layer itself is not reachable from this harness
/// and adds no couchdb-specific behaviour.
#[tokio::test]
async fn an_upload_round_trips_through_the_mcp_surface_on_a_couchdb_mount() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-upload");
    let state = fixture
        .state_writable(true)
        .await
        .with_upload_base("http://127.0.0.1:4100".to_string());

    // `.png`, not `.bin`: `read_artifact` is gated on a supported artifact extension
    // (`is_supported_artifact_path`), which is a pre-existing, backend-independent policy
    // and not something a couchdb mount changes.
    let logical = "LiveSync/assets/uploaded.png";

    // 1. Mint. The destination is validated through the router, so a couchdb path has to
    //    be accepted here rather than refused as escaping the vault.
    let minted = tool_call(&state, "request_vault_upload", json!({"path": logical})).await;
    let payload = structured(&minted);
    assert_eq!(payload["path"], json!(logical), "{payload}");
    let upload_url = payload["uploadUrl"]
        .as_str()
        .expect("uploadUrl")
        .to_string();
    assert!(upload_url.contains("/upload/"), "{upload_url}");
    let token = upload_url
        .rsplit('/')
        .next()
        .expect("a token at the end of the url")
        .to_string();

    // 2. Commit, through the same function the HTTP handler drives. The token is claimed
    //    exactly as a real PUT would claim it, so its single-use binding is exercised.
    let pending = state.uploads.claim(&token).expect("the token must claim");
    assert_eq!(pending.dest_path, logical, "the token is bound to the path");
    let bytes: Vec<u8> = vec![0x00, 0x01, 0xfe, 0xff, 0x42, 0x7f, 0x80];
    let backend = state
        .router
        .backend_for(&pending.dest_path)
        .expect("the couchdb mount must own this path");
    let relative = state
        .router
        .resolve(&pending.dest_path)
        .expect("resolve")
        .backend_relative_path;
    let outcome = deep_obsidian_server::uploads::commit_stream_via_backend(
        backend,
        relative,
        pending.expected_hash.clone(),
        pending.max_bytes,
        deep_obsidian_backend::UploadChunks::new(std::iter::once(Ok(bytes.clone()))),
    )
    .await
    .expect("the commit must land on a writable couchdb mount");
    assert!(outcome.created);
    assert_eq!(outcome.bytes_written, bytes.len());
    state.uploads.consume(&token);

    // 3. `read_artifact` serves it back, base64, from the LOGICAL path. Base64 is opt-in
    //    (`includeBase64`) and bounded by `maxBytes`, so both are asked for explicitly.
    let artifact = tool_call(
        &state,
        "read_artifact",
        json!({"path": logical, "includeBase64": true, "maxBytes": 1024}),
    )
    .await;
    let payload = structured(&artifact);
    // The size comes from `stat` on the couchdb backend, independently of the bytes.
    assert_eq!(payload["size"], json!(bytes.len()), "{payload}");
    assert_eq!(payload["mimeType"], json!("image/png"), "{payload}");
    let encoded = payload["base64"]
        .as_str()
        .unwrap_or_else(|| panic!("read_artifact must return base64: {payload}"));
    // Decoded rather than compared as a string, so this asserts the BYTES round-tripped
    // and not merely that some base64 came back.
    let decoded = base64_decode(encoded);
    assert_eq!(
        decoded, bytes,
        "the uploaded bytes must round-trip through the couchdb mount"
    );

    // A protected destination is still refused at mint on a couchdb mount, before any
    // capability token exists.
    let protected = tool_call(
        &state,
        "request_vault_upload",
        json!({"path": "LiveSync/Templates/sneaky.png"}),
    )
    .await;
    assert!(
        error_message(&protected).contains("protected write path"),
        "{protected}"
    );
}

/// Decode standard base64, for asserting on `read_artifact`'s payload.
fn base64_decode(input: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        if byte == b'=' {
            break;
        }
        let value = ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)
            .unwrap_or_else(|| panic!("invalid base64 character {byte:?}"))
            as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    out
}

/// `vault_info` reports the writable mount's capabilities, so a client can tell which
/// mounts it may write to without trying one.
#[tokio::test]
async fn vault_info_reports_write_capabilities_per_mount() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-capabilities");

    for writable in [false, true] {
        let state = fixture.state_writable(writable).await;
        let info = tool_call(&state, "vault_info", json!({})).await;
        let mounts = structured(&info)["mounts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let live = mounts
            .iter()
            .find(|mount| mount["id"] == json!("live"))
            .unwrap_or_else(|| panic!("the couchdb mount must be listed: {info}"));
        let capabilities = live["capabilities"].as_array().cloned().unwrap_or_default();
        assert!(
            capabilities.contains(&json!("binary-read")),
            "reads are advertised in both modes: {live}"
        );
        assert_eq!(
            capabilities.contains(&json!("binary-write")),
            writable,
            "binary-write must be advertised iff the mount is writable: {live}"
        );
        assert_eq!(
            capabilities.contains(&json!("upload")),
            writable,
            "upload must be advertised iff the mount is writable: {live}"
        );
        // A delete is a write, so it rides the same axis — and the server registers
        // `delete_note` from this very capability, so a read-only mount advertising it
        // would put a tool on the surface that could only ever refuse.
        assert_eq!(
            capabilities.contains(&json!("soft-delete")),
            writable,
            "soft-delete must be advertised iff the mount is writable: {live}"
        );
        // ...while `version-history` is absent in BOTH modes. This mount has a soft delete
        // and no history, which is the combination that stops `delete_note`'s payload
        // assuming a `read_version` exists.
        assert!(
            !capabilities.contains(&json!("version-history")),
            "nothing here can enumerate or fetch a CouchDB revision: {live}"
        );

        // Conflict surfacing rides on the same per-mount detail. The couchdb mount CAN
        // hold sibling revisions, so it reports a count even when that count is zero —
        // "checked, none" is information.
        assert_eq!(
            live["conflictedCount"],
            json!(0),
            "a couchdb mount must report its conflicted count: {live}"
        );
        // ...and reports no PATHS when there are none, rather than an empty array a
        // reader has to interpret.
        assert!(
            live.get("conflictedPaths").is_none(),
            "a healthy mount must not carry an empty conflictedPaths: {live}"
        );

        // The filesystem mount carries NO such field, because the question does not apply
        // to it: a file has exactly one version by construction, so reporting "zero
        // conflicts" would imply a check that was never possible. This is the `Option`
        // distinction in `VaultBackend::conflicted_paths`, observed through MCP.
        let root = mounts
            .iter()
            .find(|mount| mount["id"] == json!("vault"))
            .unwrap_or_else(|| panic!("the root mount must be listed: {info}"));
        assert!(
            root.get("conflictedCount").is_none(),
            "a filesystem mount must not report a conflicted count at all: {root}"
        );
        assert!(root.get("conflictedPaths").is_none(), "{root}");
    }
}

/// `grep_search` scoped to a CouchDB mount is SERVED, with context and logical paths.
///
/// This replaced a refusal. The mount has no files for ripgrep to open, so the backend
/// imitates ripgrep over note text read back through the sidecar — and the caller
/// cannot tell, which is the whole point: same matches, same context, same
/// `exhaustive`-by-omission payload shape.
///
/// # Which path actually serves it, verified rather than assumed
///
/// `AppState::rg_available` is derived from the ROOT mount only, so with a filesystem
/// root it stays true and `grep_search` stays advertised. `VaultRouter::grep` scopes by
/// the caller's `glob` and delegates to that one mount's backend. So a glob that narrows
/// to the couchdb prefix reaches `CouchDbVaultBackend`'s `Recall::Grep` arm, and its
/// matches are what the caller sees — the arm is genuinely reachable through MCP, not
/// dead defence-in-depth.
#[tokio::test]
async fn grep_scoped_to_a_couchdb_mount_is_served_by_the_virtual_grep() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-grep");
    let state = fixture.state().await;
    if !state.rg_available {
        eprintln!("skipping: ripgrep is not available, so grep_search is not advertised");
        return;
    }

    let response = tool_call(
        &state,
        "grep_search",
        json!({"query": "LiveSync", "glob": "LiveSync/**/*.md", "contextLines": 1}),
    )
    .await;
    let payload = structured(&response);
    let matches = payload["matches"].as_array().expect("matches");
    let paths: Vec<&str> = matches
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect();
    // Both stub notes contain "LiveSync", and the paths are LOGICAL: the router
    // re-prefixed the mount-relative paths the backend reported.
    assert!(paths.contains(&"LiveSync/Charter.md"), "{paths:?}");
    assert!(paths.contains(&"LiveSync/Deep/Nested.md"), "{paths:?}");
    assert!(
        paths.iter().all(|path| path.starts_with("LiveSync/")),
        "{paths:?}"
    );
    // Context travelled with the matches, from the same one round trip.
    let charter = matches
        .iter()
        .find(|item| item["path"] == "LiveSync/Charter.md")
        .expect("the charter match");
    assert_eq!(charter["lineNumber"], json!(1));
    assert_eq!(charter["lineText"], json!("# LiveSync Charter"));
    assert_eq!(
        charter["contextAfter"][0]["lineText"],
        json!(""),
        "the blank line under the heading: {charter}"
    );
    // A full scan, so the payload keeps grep's frozen "exhaustive by omission" shape:
    // no `exhaustive` key, no `candidateCount`, nothing about being degraded. This is
    // what distinguishes it from the Algolia mount's candidate-bounded grep.
    for absent in ["exhaustive", "candidateCount", "exhaustiveNote", "degraded"] {
        assert!(
            payload.get(absent).is_none(),
            "an exhaustive grep must not carry {absent}: {payload}"
        );
    }

    // Grep on the FILESYSTEM root still works, so nothing about the couchdb mount
    // changed the rg path. The glob names `Notes/` rather than `*.md` because a
    // root-scoped glob would span the LiveSync mount too, which the router refuses for
    // the unrelated federation reason.
    let root = tool_call(
        &state,
        "grep_search",
        json!({"query": "nested", "glob": "Notes/**/*.md"}),
    )
    .await;
    assert!(
        root.get("result").is_some(),
        "grep on the filesystem root must still work: {root}"
    );
}

/// An UNSCOPED grep now includes the couchdb mount's hits alongside the filesystem
/// root's, and stops reporting the mount as missing.
///
/// # The transition this pins
///
/// Federation concatenates each mount's matches, and a mount whose grep FAILED lands in
/// `missingBackends` with `exhaustive: false` — which is exactly what a couchdb mount
/// used to do, because its `Recall::Grep` arm refused. With the capability present it
/// simply participates, so the degradation keys disappear from the payload. Asserting
/// their ABSENCE is what makes the transition observable rather than incidental.
#[tokio::test]
async fn an_unscoped_federated_grep_includes_the_couchdb_mounts_hits() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-grep-federated");
    let state = fixture.state().await;
    if !state.rg_available {
        eprintln!("skipping: ripgrep is not available, so grep_search is not advertised");
        return;
    }

    // "Nested" is in BOTH vaults: the filesystem root's `Notes/Deep/Nested.md` and the
    // couchdb mount's `Deep/Nested.md`. One unscoped query must return both.
    let response = tool_call(&state, "grep_search", json!({"query": "nested"})).await;
    let payload = structured(&response);
    let paths: Vec<&str> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect();
    assert!(
        paths.iter().any(|path| path.starts_with("LiveSync/")),
        "the couchdb mount must contribute to a federated grep: {paths:?}"
    );
    assert!(
        paths.iter().any(|path| !path.starts_with("LiveSync/")),
        "the filesystem root must still contribute: {paths:?}"
    );
    // Every mount answered, so the answer is neither degraded nor short.
    assert!(payload.get("missingBackends").is_none(), "{payload}");
    assert!(payload.get("degraded").is_none(), "{payload}");
    assert!(payload.get("exhaustive").is_none(), "{payload}");
}

/// The capability is reported through `vault_info`, so a client can see WHY grep works
/// on this mount rather than having to try it.
///
/// Also the proof that it does not ride on `writable`: the mount here is read-only.
#[tokio::test]
async fn a_read_only_couchdb_mount_advertises_grep_search_in_vault_info() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-grep-capability");
    let state = fixture.state().await;

    let response = tool_call(&state, "vault_info", json!({})).await;
    let info = structured(&response);
    let mount = info["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|entry| entry["id"] == "live")
        .unwrap_or_else(|| panic!("the couchdb mount must be listed: {info}"));
    let capabilities: Vec<&str> = mount["capabilities"]
        .as_array()
        .expect("capabilities")
        .iter()
        .map(|item| item.as_str().expect("capability"))
        .collect();
    assert!(capabilities.contains(&"grep-search"), "{capabilities:?}");
    // ...while the write capabilities stay absent, which is the axis `writable` gates.
    assert!(!capabilities.contains(&"binary-write"), "{capabilities:?}");
    assert!(!capabilities.contains(&"upload"), "{capabilities:?}");
    assert!(!capabilities.contains(&"soft-delete"), "{capabilities:?}");
}

// ---------------------------------------------------------------------------
// An ALGOLIA mount, through MCP
// ---------------------------------------------------------------------------

/// A filesystem root mount plus an Algolia mount at `_Shared`, backed by the
/// in-process mock Algolia.
///
/// The mock's `JoinHandle` is held for the fixture's whole life: dropping it aborts the
/// server task and every subsequent request would fail as a transport error, which
/// would look like a backend bug.
struct AlgoliaFixture {
    inner: Fixture,
    base_url: String,
    _mock: tokio::task::JoinHandle<()>,
    /// A clone of the mock's shared state, so a test can take the backend down and
    /// bring it back on a STABLE port. Aborting `_mock` would be the other way to
    /// simulate an outage and could not be undone — the ephemeral port is gone, and
    /// rebinding it races `TIME_WAIT`.
    mock: deep_obsidian_algolia::mock::MockAlgolia,
    secrets: PathBuf,
}

impl AlgoliaFixture {
    async fn new(name: &str) -> Self {
        let inner = Fixture::new(name);
        let base = inner
            .index_dir
            .parent()
            .expect("fixture base")
            .to_path_buf();
        let state = deep_obsidian_algolia::mock::MockAlgolia::default();
        let (base_url, mock) = deep_obsidian_algolia::mock::spawn_mock_with(state.clone()).await;
        Self {
            inner,
            base_url,
            _mock: mock,
            mock: state,
            secrets: base.join("secrets.json"),
        }
    }

    fn config_writable(&self, writable: bool) -> ResolvedServiceConfig {
        let mut config = self.inner.config();
        config.experimental = ExperimentalConfig {
            multi_vault: true,
            algolia_vaults: true,
            ..ExperimentalConfig::default()
        };
        // Replace the `team` filesystem mount with an algolia one at the SAME prefix,
        // so every assertion below is about the backend kind rather than about a
        // different prefix.
        config.mounts[1] = MountConfig {
            unknown: Default::default(),
            recall_weight: None,
            id: "shared".to_string(),
            mount_at: "_Shared".to_string(),
            backend: MountBackendConfig::Algolia {
                app_id: "TESTAPP".to_string(),
                index_name: "team-wiki".to_string(),
                api_key_ref: SecretRef::EncryptedFile {
                    id: "algolia-api-key".to_string(),
                },
                base_url: Some(self.base_url.clone()),
                writable,
                participant_id: Some("paul@test".to_string()),
                cache: None,
                retention: None,
                index_dir: None,
            },
        };
        config
    }

    /// State over the algolia config, with the key in a TEMP secrets file.
    ///
    /// A temp store rather than the `DEEP_OBSIDIAN_ALGOLIA_API_KEY` override: the
    /// environment is process-global, and setting it here would silently shadow the
    /// configured key for every other test in this binary.
    async fn state_writable(&self, writable: bool) -> AppState {
        let resolver = SecretResolver::with_encrypted_file_path(self.secrets.clone());
        resolver
            .put(
                &SecretRef::EncryptedFile {
                    id: "algolia-api-key".to_string(),
                },
                secrecy::SecretString::new("test-key".to_string()),
            )
            .expect("store the fixture api key");
        let config = self.config_writable(writable);
        let backends = MountBackends::build_with_resolver(&config, &resolver);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("an algolia mount must not fail the bootstrap");
        AppState::with_backends(config, runtimes, &backends)
            .with_upload_base("http://127.0.0.1:4100".to_string())
    }
}

/// The gate: an algolia mount needs its own experimental flag. It MAY be the root.
#[test]
fn an_algolia_mount_requires_the_algolia_vaults_flag_and_may_be_the_root() {
    let mounts = vec![
        MountConfig {
            unknown: Default::default(),
            recall_weight: None,
            id: "vault".to_string(),
            mount_at: String::new(),
            backend: MountBackendConfig::Filesystem {
                vault_path: PathBuf::from("/tmp/root-vault"),
                index_dir: None,
            },
        },
        MountConfig {
            unknown: Default::default(),
            recall_weight: None,
            id: "shared".to_string(),
            mount_at: "_Shared".to_string(),
            backend: MountBackendConfig::Algolia {
                app_id: "TESTAPP".to_string(),
                index_name: "team-wiki".to_string(),
                api_key_ref: SecretRef::EncryptedFile {
                    id: "algolia-api-key".to_string(),
                },
                base_url: None,
                writable: false,
                participant_id: None,
                cache: None,
                retention: None,
                index_dir: None,
            },
        },
    ];

    // Ungated: refused, and the message names the flag rather than the multi-vault one.
    let error =
        deep_obsidian_server::normalize_service_config(deep_obsidian_types::ServiceConfigInput {
            mounts: Some(mounts.clone()),
            experimental: Some(ExperimentalConfig {
                multi_vault: true,
                ..ExperimentalConfig::default()
            }),
            ..Default::default()
        })
        .expect_err("an ungated algolia mount must be refused");
    assert!(error.to_string().contains("algoliaVaults"), "{error}");

    // Gated: accepted.
    assert!(deep_obsidian_server::normalize_service_config(
        deep_obsidian_types::ServiceConfigInput {
            mounts: Some(mounts.clone()),
            experimental: Some(ExperimentalConfig {
                multi_vault: true,
                algolia_vaults: true,
                ..ExperimentalConfig::default()
            }),
            ..Default::default()
        }
    )
    .is_ok());

    // At the vault root: ACCEPTED. `vaultPath` resolves to nothing, which is the point —
    // a fully-remote vault has no local directory and no longer has to pretend to.
    // `multiVault` is not needed for a one-mount table.
    let mut rooted = mounts;
    rooted.remove(0);
    rooted[0].mount_at = String::new();
    let resolved =
        deep_obsidian_server::normalize_service_config(deep_obsidian_types::ServiceConfigInput {
            mounts: Some(rooted),
            experimental: Some(ExperimentalConfig {
                algolia_vaults: true,
                ..ExperimentalConfig::default()
            }),
            ..Default::default()
        })
        .expect("an algolia root mount resolves");
    assert_eq!(resolved.vault_path, None);
    assert_eq!(resolved.root_location(), "TESTAPP/team-wiki");
}

/// Reads, writes, listings and outlines all work on an algolia mount, through the
/// public JSON-RPC surface, in the LOGICAL namespace.
#[tokio::test]
async fn an_algolia_mount_serves_reads_writes_listings_and_outlines() {
    let fixture = AlgoliaFixture::new("algolia-crud").await;
    let state = fixture.state_writable(true).await;

    // Write: routed to the algolia mount by prefix.
    let created = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "_Shared/Decisions/Retention.md",
            "content": "# Retention\n\n## Policy\n\nKeep five versions of every note.\n",
        }),
    )
    .await;
    let created = structured(&created);
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["path"], json!("_Shared/Decisions/Retention.md"));

    // Read: the logical path, and the hash a client feeds back as `expectedHash`.
    let read = tool_call(
        &state,
        "read_file",
        json!({"path": "_Shared/Decisions/Retention.md"}),
    )
    .await;
    let read = structured(&read);
    assert!(
        read["text"]
            .as_str()
            .expect("text")
            .contains("Keep five versions"),
        "{read}"
    );
    let hash = read["hash"].as_str().expect("a hash").to_string();
    assert!(hash.starts_with("fnv1a64:"), "{hash}");

    // The root listing merges the root mount with the SYNTHESIZED mount folder.
    let children = tool_call(&state, "list_children", json!({})).await;
    let paths: Vec<&str> = structured(&children)["children"]
        .as_array()
        .expect("children")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert!(paths.contains(&"_Shared"), "{paths:?}");
    assert!(paths.contains(&"Root.md"), "{paths:?}");

    // Listing INSIDE the mount: folders synthesized from the index's facets.
    let inside = tool_call(&state, "list_children", json!({"path": "_Shared"})).await;
    let paths: Vec<&str> = structured(&inside)["children"]
        .as_array()
        .expect("children")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert_eq!(paths, vec!["_Shared/Decisions"]);

    // An outline is composed above the boundary from the hydrated text, so it works
    // unchanged on a mount with no local index.
    let outline = tool_call(
        &state,
        "note_outline",
        json!({"path": "_Shared/Decisions/Retention.md"}),
    )
    .await;
    let headings: Vec<&str> = structured(&outline)["headings"]
        .as_array()
        .expect("headings")
        .iter()
        .filter_map(|heading| heading["title"].as_str())
        .collect();
    assert!(headings.contains(&"Retention"), "{headings:?}");
    assert!(headings.contains(&"Policy"), "{headings:?}");

    // A stale `expectedHash` is rejected ABOVE the boundary, with the frozen hash
    // wording — so the backend's fork path is only ever reached by a true race.
    let stale = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "_Shared/Decisions/Retention.md",
            "content": "# Retention\n\nrewritten\n",
            "expectedHash": "fnv1a64:0000000000000000",
        }),
    )
    .await;
    let message = error_message(&stale);
    assert!(message.contains("hash"), "{message}");
    // ...and the note was NOT rewritten.
    let unchanged = tool_call(
        &state,
        "read_file",
        json!({"path": "_Shared/Decisions/Retention.md"}),
    )
    .await;
    assert_eq!(structured(&unchanged)["hash"], json!(hash));

    // The matching hash is accepted, and updates the note.
    let updated = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "_Shared/Decisions/Retention.md",
            "content": "# Retention\n\nrewritten\n",
            "expectedHash": hash,
        }),
    )
    .await;
    assert_eq!(structured(&updated)["created"], json!(false));
    assert_eq!(
        structured(
            &tool_call(
                &state,
                "read_file",
                json!({"path": "_Shared/Decisions/Retention.md"})
            )
            .await
        )["text"],
        json!("# Retention\n\nrewritten\n")
    );
}

/// A READ-ONLY algolia mount refuses a write with the message that names the setting.
#[tokio::test]
async fn a_read_only_algolia_mount_refuses_a_write_by_naming_the_setting() {
    let fixture = AlgoliaFixture::new("algolia-read-only").await;
    let state = fixture.state_writable(false).await;
    let response = tool_call(
        &state,
        "upsert_note",
        json!({"path": "_Shared/A.md", "content": "# A\n"}),
    )
    .await;
    let message = error_message(&response);
    assert_eq!(message, deep_obsidian_backend::ALGOLIA_READ_ONLY_MESSAGE);
    assert!(message.contains("\"writable\": true"), "{message}");
}

/// Every binary path an MCP client can reach on an algolia mount is refused, and each
/// refusal says the mount is MARKDOWN ONLY rather than reporting a missing file, a
/// permission problem or a generic unsupported operation.
///
/// Four of the five refusal paths are surfaced BY THE BACKEND (`Stat` on a binary path,
/// `ReadBytes`, the upload mint's `ResolvePath`, and the upload commit); the fifth,
/// `search_artifacts`, is surfaced by the INDEX layer instead — there is no local index
/// to search, so it never reaches the backend at all. The distinction matters: the
/// first four are storage facts, the last is an index fact.
#[tokio::test]
async fn every_binary_path_on_an_algolia_mount_is_refused_as_markdown_only() {
    let fixture = AlgoliaFixture::new("algolia-binary").await;
    let state = fixture.state_writable(true).await;

    // (1) Artifact metadata: `read_artifact` stats before it reads, so `Stat` is where
    // this surfaces.
    let response = tool_call(
        &state,
        "read_artifact",
        json!({"path": "_Shared/Assets/diagram.png"}),
    )
    .await;
    assert_eq!(
        error_message(&response),
        deep_obsidian_backend::ALGOLIA_NO_BINARY_MESSAGE
    );

    // (2) Artifact BYTES: the same tool with a payload requested. Refused at the same
    // place, so the caller never gets metadata for something it cannot then read.
    let response = tool_call(
        &state,
        "read_artifact",
        json!({"path": "_Shared/Assets/diagram.png", "includeBase64": true, "maxBytes": 1024}),
    )
    .await;
    assert_eq!(
        error_message(&response),
        deep_obsidian_backend::ALGOLIA_NO_BINARY_MESSAGE
    );

    // (3) The out-of-band UPLOAD mint. Refused BEFORE a token is issued, which is the
    // whole point: a token would fail only after the body had been uploaded.
    let response = tool_call(
        &state,
        "request_vault_upload",
        json!({"path": "_Shared/Assets/diagram.png"}),
    )
    .await;
    assert_eq!(
        error_message(&response),
        deep_obsidian_backend::ALGOLIA_NO_UPLOAD_MESSAGE
    );
    // ...and a MARKDOWN destination is refused too: the upload endpoint is for bytes,
    // and markdown reaches this mount through `upsert_note`.
    let response = tool_call(
        &state,
        "request_vault_upload",
        json!({"path": "_Shared/Notes/pasted.md"}),
    )
    .await;
    assert_eq!(
        error_message(&response),
        deep_obsidian_backend::ALGOLIA_NO_UPLOAD_MESSAGE
    );

    // (4) `search_artifacts` is scoped recall, so it is refused by the INDEX layer for
    // having no local index — not by the backend.
    let response = tool_call(
        &state,
        "search_artifacts",
        json!({"query": "diagram", "scope": "_Shared"}),
    )
    .await;
    let message = error_message(&response);
    assert!(
        message.contains("no local search index"),
        "search_artifacts must be refused by the index layer: {message}"
    );

    // The root mount is unaffected: an upload there still mints.
    let response = tool_call(
        &state,
        "request_vault_upload",
        json!({"path": "Assets/diagram.png"}),
    )
    .await;
    assert!(
        structured(&response)["uploadUrl"].is_string(),
        "the filesystem root must still mint upload tokens: {response}"
    );
}

/// GRAPH-shaped recall on an algolia mount is still refused, and the refusal now also
/// names the recall that DOES work there.
///
/// The split is the point of this slice: a ranked list is something the shared index can
/// produce, while a link graph, a similarity neighbourhood and an artifact embedding table
/// are not things a remote corpus exposes at all. So these three keep refusing — and the
/// refusal must point AT the native recall rather than away from it, or the feature stays
/// undiscovered by exactly the user who hit the wall.
#[tokio::test]
async fn graph_shaped_recall_on_an_algolia_mount_is_refused_honestly() {
    let fixture = AlgoliaFixture::new("algolia-recall").await;
    let state = fixture.state_writable(true).await;
    tool_call(
        &state,
        "upsert_note",
        json!({"path": "_Shared/A.md", "content": "# A\n\nshared body\n"}),
    )
    .await;

    for (tool, arguments) in [
        ("related_notes", json!({"path": "_Shared/A.md"})),
        ("graph_traverse", json!({"path": "_Shared/A.md"})),
        (
            "search_artifacts",
            json!({"query": "shared", "scope": "_Shared"}),
        ),
    ] {
        let response = tool_call(&state, tool, arguments).await;
        let message = error_message(&response);
        assert!(
            message.contains("no local search index"),
            "{tool} must be refused for having no local index: {message}"
        );
        // The refusal names the BACKEND KIND and points at the mounts that do work.
        assert!(message.contains("algolia"), "{tool}: {message}");
        assert!(
            message.contains("read_file") && message.contains("grep_search"),
            "{tool} must name what DOES work on this mount: {message}"
        );
        // ...and must not read as a malfunction.
        assert!(
            !message.contains("has no index."),
            "{tool} must not use the old bare wording: {message}"
        );
        // It DOES point at the recall this mount serves natively, and says what the
        // missing piece actually is.
        assert!(
            message.contains("hybrid_search") && message.contains("load_knowledge"),
            "{tool} must name the recall this mount DOES serve: {message}"
        );
        assert!(
            message.contains("link graph"),
            "{tool} must say what is genuinely absent: {message}"
        );
        // The index-backed scope list still excludes this mount: for THESE tools it
        // really cannot serve, so naming it would be telling the user to retry the exact
        // call that failed.
        assert!(
            message.contains("index: '/'"),
            "{tool} must name the root mount as the index-backed scope: {message}"
        );
    }

    // Scoping an index-backed tool to the filesystem root still works, so the refusal is
    // per-mount rather than a global loss of the tool.
    let root = tool_call(&state, "related_notes", json!({"path": "Root.md"})).await;
    assert!(
        root.get("result").is_some(),
        "recall on the filesystem root must still work: {root}"
    );
}

/// Scoped `hybrid_search` and `load_knowledge` on an algolia mount are SERVED, by that
/// mount's own index, in the logical namespace — and the payload says so.
#[tokio::test]
async fn scoped_recall_on_an_algolia_mount_is_served_by_the_mounts_own_index() {
    let fixture = AlgoliaFixture::new("algolia-native-recall").await;
    let state = fixture.state_writable(true).await;
    tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "_Shared/Decisions/Retention.md",
            "content": "# Retention\n\nThe retention policy keeps five versions of every note.\n",
        }),
    )
    .await;
    // A root-mount note with the SAME vocabulary. Nothing served from the shared mount may
    // include it, and nothing served from the root may include the shared note: native
    // recall answers for its own mount only.
    fs::write(
        fixture.inner.root_vault.join("Retention.md"),
        "# Local Retention\n\nA local note about the retention policy.\n",
    )
    .expect("write the root-mount note");

    let response = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "retention policy", "scope": "_Shared"}),
    )
    .await;
    let payload = structured(&response);
    // Provenance, stated rather than implied.
    assert_eq!(payload["nativeRecall"], json!(true), "{payload}");
    assert_eq!(payload["mountId"], json!("shared"), "{payload}");
    assert_eq!(payload["recallMode"], json!("lexical"), "{payload}");
    assert!(payload["exhausted"].is_boolean(), "{payload}");
    // A local hybrid search reports the local embedding backend; a natively-served one
    // must not claim one it never used.
    assert!(
        payload.get("semanticBackend").is_none() && payload.get("degraded").is_none(),
        "a natively-served payload must not report the LOCAL embedding backend: {payload}"
    );

    let matches = payload["matches"].as_array().expect("matches");
    assert!(!matches.is_empty(), "{payload}");
    let paths: Vec<&str> = matches
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    // LOGICAL paths: the mount prefix is put back on.
    assert!(
        paths.contains(&"_Shared/Decisions/Retention.md"),
        "{paths:?}"
    );
    // No cross-mount content flow: the root mount's note is not in this mount's answer.
    assert!(
        !paths.contains(&"Retention.md"),
        "native recall must serve only its own mount: {paths:?}"
    );
    let hit = &matches[0];
    assert_eq!(
        hit["resourceUri"],
        json!("obsidian://note?path=_Shared%2FDecisions%2FRetention.md"),
        "the resource URI moves with the logical path: {hit}"
    );
    assert!(hit["score"].as_f64().expect("a score") > 0.0, "{hit}");
    assert!(hit["text"]
        .as_str()
        .expect("snippet text")
        .contains("retention policy"));
    // The two local ranker signals are ABSENT rather than fabricated: a remote index
    // reports one ranking, not a decomposition into semantic and BM25 halves.
    assert!(
        hit.get("semanticScore").is_none() && hit.get("bm25Score").is_none(),
        "a native hit must not invent the local ranker's input signals: {hit}"
    );

    // `load_knowledge` serves the chunks and notes, and is explicit that the graph is
    // empty because none was traversed rather than because none was found.
    let response = tool_call(
        &state,
        "load_knowledge",
        json!({"subject": "retention policy", "scope": "_Shared"}),
    )
    .await;
    let payload = structured(&response);
    assert_eq!(payload["nativeRecall"], json!(true), "{payload}");
    assert_eq!(payload["recallMode"], json!("lexical"), "{payload}");
    let notes: Vec<&str> = payload["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|note| note["path"].as_str())
        .collect();
    assert_eq!(notes, vec!["_Shared/Decisions/Retention.md"], "{payload}");
    assert!(!payload["chunks"].as_array().expect("chunks").is_empty());
    assert_eq!(payload["graph"]["nodes"], json!([]), "{payload}");
    let reason = payload["graphUnavailableReason"]
        .as_str()
        .expect("a graph reason");
    assert!(
        reason.contains("no local link graph") && reason.contains("none was traversed"),
        "an empty graph must distinguish itself from a graph with no results: {reason}"
    );

    // The ROOT mount's own recall is unaffected and does not see the shared note.
    let response = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "retention policy", "scope": "/"}),
    )
    .await;
    let payload = structured(&response);
    assert!(
        payload.get("nativeRecall").is_none(),
        "the filesystem root is served by its LOCAL index: {payload}"
    );
    let paths: Vec<&str> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(
        !paths.iter().any(|path| path.starts_with("_Shared/")),
        "the root mount's index must not contain the shared mount's notes: {paths:?}"
    );
}

/// A federated answer reports each mount by the mechanism that actually served it, and says
/// honestly when a mount could not take part at all.
///
/// The three tools split three ways over the same two-mount vault, and the differences are
/// the contract:
///
/// * `hybrid_search` and `load_knowledge` — the algolia mount answers RANKED SEARCH itself,
///   so it takes part as `native-recall` and reports its `recallMode`;
/// * `search_artifacts` — the algolia mount has no local artifact table AND its backend
///   cannot store a binary file, so it holds no artifacts. It is reported as SKIPPED with a
///   reason and the answer is NOT degraded: nothing was omitted. Calling that a missing
///   backend would train a reader to ignore `missingBackends`.
#[tokio::test]
async fn a_federated_answer_names_each_mounts_recall_mechanism() {
    let fixture = AlgoliaFixture::new("algolia-scope-hint").await;
    let state = fixture.state_writable(true).await;

    for tool in ["hybrid_search", "load_knowledge"] {
        let arguments = if tool == "hybrid_search" {
            json!({"query": "shared"})
        } else {
            json!({"subject": "shared"})
        };
        let response = tool_call(&state, tool, arguments).await;
        let payload = structured(&response).clone();
        assert_eq!(payload["federated"], json!(true), "{tool}: {payload}");
        let mounts = payload["mounts"].as_array().expect("mounts").clone();
        let shared = mounts
            .iter()
            .find(|mount| mount["id"] == json!("shared"))
            .unwrap_or_else(|| panic!("{tool} must report the shared mount: {payload}"));
        assert_eq!(shared["source"], json!("native-recall"), "{tool}: {shared}");
        // The field that makes the shared mount's scores interpretable at all.
        assert!(shared["recallMode"].is_string(), "{tool}: {shared}");
        let root = mounts
            .iter()
            .find(|mount| mount["id"] == json!("vault"))
            .expect("the root mount");
        assert_eq!(root["source"], json!("local-index"), "{tool}: {root}");
        // A mount that ranked for itself is not a shortfall.
        assert_eq!(payload["degraded"], json!(false), "{tool}: {payload}");
        assert!(
            payload.get("missingBackends").is_none(),
            "{tool}: {payload}"
        );
    }

    // `search_artifacts` cannot use the shared mount at all. This fixture also configures no
    // artifact embedding backend, so the ROOT mount cannot rank either and every mount that
    // could have answered failed -- which must be an error rather than an empty `matches[]`.
    // Artifacts have no lexical fallback, so "no ranking was produced" and "there are no
    // matching artifacts" are different facts and reporting the second would be a lie. The
    // SKIPPED-not-missing reporting for the algolia mount is asserted in `federation_eval.rs`,
    // where an artifact embedding backend is available.
    let response = tool_call(&state, "search_artifacts", json!({"query": "x"})).await;
    let message = error_message(&response);
    assert!(
        message.contains("artifact embedding") || message.contains("embedding configuration"),
        "the failure must name the artifact embedding backend, got: {message}"
    );
}

/// `vault_info` describes the algolia mount honestly: its kind, its real capability
/// set, that it has NO local index — and it must not appear in `degradedMounts`, nor
/// make the server report itself degraded, for a mount that is working as designed.
#[tokio::test]
async fn vault_info_describes_the_algolia_mount_and_never_calls_it_degraded() {
    let fixture = AlgoliaFixture::new("algolia-vault-info").await;
    let state = fixture.state_writable(true).await;
    tool_call(
        &state,
        "upsert_note",
        json!({"path": "_Shared/A.md", "content": "# A\n\nshared\n"}),
    )
    .await;

    let info = tool_call(&state, "vault_info", json!({})).await;
    let info = structured(&info);
    let mounts = info["mounts"].as_array().expect("a mounts array");
    let shared = mounts
        .iter()
        .find(|mount| mount["id"] == json!("shared"))
        .expect("the algolia mount is listed");

    assert_eq!(shared["backendKind"], json!("algolia"));
    assert_eq!(shared["mountAt"], json!("_Shared"));
    // Capabilities are the backend's own, and they are honest: a bounded grep, its own
    // ranked recall, a version history, and — because this mount is writable — a soft
    // delete and a rename. Nothing binary. The ORDER is the `Capability` enum's
    // declaration order, which is what reaches a client, so this pins it too.
    assert_eq!(
        shared["capabilities"],
        json!([
            "grep-search",
            "native-recall",
            "version-history",
            "rename",
            "soft-delete"
        ])
    );
    // No local index, said in a way a client can branch on, plus a note saying why.
    assert_eq!(shared["localIndex"], json!(false));
    assert_eq!(shared["indexStatus"], json!("none"));
    assert!(
        shared["indexNote"]
            .as_str()
            .expect("an index note")
            .contains("no local search index by design"),
        "{shared}"
    );
    // NOT degraded: a mount with no index cannot have a broken one.
    assert_eq!(shared["ready"], json!(true));
    assert!(
        info.get("degradedMounts").is_none(),
        "a working algolia mount must not be reported as degraded: {info}"
    );
    // The mount reports `Some(vec![])` from `conflicted_paths`, so `conflictedCount`
    // IS present and is zero. That is a real answer, not an inapplicable one: this
    // storage can record a divergence and currently records none — see
    // `AlgoliaVaultBackend::conflicted_paths` for why "divergence" here is not
    // CouchDB's unreconciled sibling revision.
    assert_eq!(shared["conflictedCount"], json!(0));

    // The root mount is still described as an indexed filesystem mount.
    let root = mounts
        .iter()
        .find(|mount| mount["id"] == json!("vault"))
        .expect("the root mount");
    assert_eq!(root["backendKind"], json!("filesystem"));
    assert_eq!(root["localIndex"], json!(true));
    // ...and it answers `None` from `conflicted_paths`, so it gains no conflict field at
    // all — which is the distinction `Some(vec![])` vs `None` exists to preserve.
    assert!(
        root.get("conflictedCount").is_none(),
        "a filesystem mount has no sibling-version notion to report: {root}"
    );

    // ...and readiness is green, which is the property that would have been destroyed
    // by registering a failing index runtime for the algolia mount.
    let mut payload =
        build_readiness_payload(&state.config, &state.runtimes.aggregate_diagnostics());
    insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
    assert_eq!(
        readiness_status_code(&state.runtimes.aggregate_diagnostics()),
        axum::http::StatusCode::OK,
        "{payload}"
    );

    // `build_index` reports the algolia mount as SKIPPED rather than omitting it, so
    // its `mounts` array is a complete list of the vault's mounts.
    let rebuilt = tool_call(&state, "build_index", json!({})).await;
    let rebuilt = structured(&rebuilt);
    let shared = rebuilt["mounts"]
        .as_array()
        .expect("a mounts array")
        .iter()
        .find(|mount| mount["id"] == json!("shared"))
        .expect("the algolia mount is reported");
    assert_eq!(shared["skipped"], json!(true));
    assert_eq!(shared["rebuilt"], json!(false));
    assert!(shared["reason"]
        .as_str()
        .expect("a reason")
        .contains("no local search index"));
}

/// `grep_search` scoped to the algolia mount by a glob DOES reach its bounded grep, and
/// an anchorless regex there is refused with the message that explains the mechanism.
#[tokio::test]
async fn grep_scoped_to_an_algolia_mount_runs_and_refuses_honestly() {
    let fixture = AlgoliaFixture::new("algolia-grep").await;
    let state = fixture.state_writable(true).await;
    if !state.rg_available {
        eprintln!("skipping: ripgrep is not available, so grep_search is not advertised");
        return;
    }
    tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "_Shared/Decisions/Retention.md",
            "content": "# Retention\n\nThe retention policy keeps five versions.\n",
        }),
    )
    .await;

    let response = tool_call(
        &state,
        "grep_search",
        json!({"query": "retention policy", "glob": "_Shared/**/*.md"}),
    )
    .await;
    let matches = structured(&response)["matches"]
        .as_array()
        .expect("matches")
        .clone();
    assert_eq!(matches.len(), 1, "{matches:?}");
    // Reported in the LOGICAL namespace: the router relabels the mount-relative path.
    assert_eq!(
        matches[0]["path"],
        json!("_Shared/Decisions/Retention.md"),
        "{matches:?}"
    );

    // An anchorless regex is refused with the backend's own message, reached through
    // the router — so the arm is genuinely live rather than defence in depth.
    let response = tool_call(
        &state,
        "grep_search",
        json!({"query": "[a-z]+", "regex": true, "glob": "_Shared/**/*.md"}),
    )
    .await;
    let message = error_message(&response);
    assert_eq!(
        message,
        deep_obsidian_backend::algolia::grep::ALGOLIA_GREP_NO_ANCHOR_MESSAGE
    );
    assert!(message.contains("lexical prefilter"), "{message}");
    assert!(
        !message.contains("ripgrep"),
        "the refusal must not blame a missing binary: {message}"
    );
}

/// A grep served by a mount that cannot read every file SAYS SO, in the payload.
///
/// `grep_search` has always meant ripgrep, so a caller treats an empty or short result as
/// proof of absence. On this mount it is not, and 5b could only say so in a log line
/// nobody reads. Both halves are asserted: the shared mount reports itself bounded, and
/// the filesystem root's payload is UNCHANGED — the keys simply do not appear, which is
/// what keeps the frozen grep behaviour frozen.
#[tokio::test]
async fn grep_on_an_algolia_mount_reports_that_it_is_not_exhaustive() {
    let fixture = AlgoliaFixture::new("algolia-grep-honesty").await;
    let state = fixture.state_writable(true).await;
    if !state.rg_available {
        eprintln!("skipping: ripgrep is not available, so grep_search is not advertised");
        return;
    }
    tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "_Shared/Decisions/Retention.md",
            "content": "# Retention\n\nThe retention policy keeps five versions.\n",
        }),
    )
    .await;

    let shared = tool_call(
        &state,
        "grep_search",
        json!({"query": "retention policy", "glob": "_Shared/**/*.md"}),
    )
    .await;
    let payload = structured(&shared);
    assert_eq!(payload["exhaustive"], json!(false), "{payload}");
    // The number that tells a caller whether raising the bound would help.
    assert!(
        payload["candidateCount"]
            .as_u64()
            .expect("a candidate count")
            >= 1,
        "{payload}"
    );
    let note = payload["exhaustiveNote"].as_str().expect("a note");
    assert!(
        note.contains("NOT proof of absence") && note.contains("hybrid_search"),
        "the note must say what a short result does not prove, and what to use instead: {note}"
    );

    // A FILESYSTEM-served grep's payload is byte-for-byte the one it always had: no
    // `exhaustive`, no `candidateCount`, no note. That asymmetry is what keeps the frozen
    // grep behaviour frozen, and it is asserted on a filesystem mount rather than on this
    // fixture's root because a root-mount grep cannot be glob-scoped on a multi-mount
    // vault (the root's subtree contains every other mount, so the router refuses it).
    let plain = Fixture::new("grep-exhaustive-unchanged");
    let plain_state = plain.state().await;
    let response = tool_call(
        &plain_state,
        "grep_search",
        json!({"query": "charter", "glob": "Team/**/*.md"}),
    )
    .await;
    let payload = structured(&response);
    assert!(
        !payload["matches"].as_array().expect("matches").is_empty(),
        "{payload}"
    );
    for absent in ["exhaustive", "candidateCount", "exhaustiveNote"] {
        assert!(
            payload.get(absent).is_none(),
            "an exhaustive grep's payload must be unchanged, but it carries {absent}: {payload}"
        );
    }
}

/// A folder listing whose SUBFOLDERS were cut short by the provider's facet cap says so,
/// and says which half is short.
///
/// Algolia refuses more than 100 facet values outright, so a folder with more direct
/// subfolders than that cannot be enumerated. 5b could only `warn!` it. Staged with 101
/// sibling folders, which is the smallest corpus that crosses the cap.
#[tokio::test]
async fn a_truncated_folder_listing_says_so_in_the_payload() {
    let fixture = AlgoliaFixture::new("algolia-folders-truncated").await;
    let state = fixture.state_writable(true).await;
    // 101 top-level folders inside the mount. Written through the backend directly rather
    // than through 101 `upsert_note` round trips: this test is about the LISTING, and the
    // write path is covered elsewhere.
    let mount = state
        .router
        .mounts()
        .iter()
        .find(|mount| mount.id == "shared")
        .expect("the algolia mount");
    for index in 0..101 {
        mount
            .backend
            .execute(deep_obsidian_backend::BackendRequest::write_text(
                format!("F{index:03}/Note.md"),
                format!("# Note {index}\n\nbody {index}\n"),
            ))
            .await
            .expect("seed a folder");
    }

    let response = tool_call(&state, "list_children", json!({"path": "_Shared"})).await;
    let payload = structured(&response);
    assert_eq!(payload["foldersTruncated"], json!(true), "{payload}");
    let reason = payload["foldersTruncatedReason"]
        .as_str()
        .expect("a reason");
    assert!(
        reason.contains("SUBFOLDERS") && reason.contains("FILES listed here are complete"),
        "the reason must say which half of the listing is short: {reason}"
    );
    assert!(
        reason.contains("100"),
        "the reason must name the cap: {reason}"
    );
    // `foldersOnly` carries it too: that caller is the one a short folder list misleads
    // most.
    let folders_only = tool_call(
        &state,
        "list_children",
        json!({"path": "_Shared", "foldersOnly": true}),
    )
    .await;
    assert_eq!(
        structured(&folders_only)["foldersTruncated"],
        json!(true),
        "{folders_only}"
    );

    // The filesystem root's listing is unchanged: a real directory enumerates every
    // subfolder, so the key never appears.
    let root = tool_call(&state, "list_children", json!({})).await;
    let payload = structured(&root);
    assert!(
        payload.get("foldersTruncated").is_none()
            && payload.get("foldersTruncatedReason").is_none(),
        "a real directory listing must be unchanged: {payload}"
    );
}

/// `resources/list` includes the notes of a mount with NO local index.
///
/// The enumeration federates where recall cannot: concatenating each mount's note list is
/// a COMPLETE answer, so omitting an index-less mount would tell a client those notes do
/// not exist — the one failure mode a manifest must not have.
#[tokio::test]
async fn resources_list_includes_an_index_less_mounts_notes() {
    let fixture = AlgoliaFixture::new("algolia-resources").await;
    let state = fixture.state_writable(true).await;
    tool_call(
        &state,
        "upsert_note",
        json!({"path": "_Shared/Decisions/Retention.md", "content": "# Retention\n\nbody\n"}),
    )
    .await;

    let response = request(&state, "resources/list", json!({})).await;
    let uris: Vec<&str> = response["result"]["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .filter_map(|resource| resource["uri"].as_str())
        .collect();
    let shared_uri = "obsidian://note?path=_Shared%2FDecisions%2FRetention.md";
    assert!(
        uris.contains(&shared_uri),
        "the shared mount's note must be listed: {uris:?}"
    );
    // ...alongside the root mount's, in one globally sorted list.
    assert!(uris.contains(&"obsidian://note?path=Root.md"), "{uris:?}");

    // Advertising a resource and being able to READ it are two facts. Before this slice no
    // index-less mount's note was ever listed, so nobody would have called `resources/read`
    // on one; now a client that walks the listing will, and a listing full of unreadable
    // URIs would be worse than the omission it replaced.
    let read = request(&state, "resources/read", json!({"uri": shared_uri})).await;
    assert_eq!(
        read["result"]["contents"][0]["text"],
        json!("# Retention\n\nbody\n"),
        "every advertised resource must be readable: {read}"
    );
    assert_eq!(read["result"]["contents"][0]["uri"], json!(shared_uri));

    // The compact manifest agrees with the listing.
    let manifest = request(
        &state,
        "resources/read",
        json!({"uri": "obsidian://vault/notes-index"}),
    )
    .await;
    let text = manifest["result"]["contents"][0]["text"]
        .as_str()
        .expect("manifest text");
    assert!(
        text.contains("_Shared/Decisions/Retention.md"),
        "the notes index must agree with resources/list: {text}"
    );
}

// ---------------------------------------------------------------------------
// The capability-gated tools
// ---------------------------------------------------------------------------

fn tool_names(response: &Value) -> Vec<String> {
    response["result"]["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .map(str::to_string)
        .collect()
}

/// The four tools exist only when a mount can serve them, and `delete_note` tracks
/// `writable` separately from the three read-only history tools.
///
/// This is the `grep_search` discipline applied to a capability: a tool that is advertised
/// and can only ever refuse costs an agent a round trip and a wrong conclusion about the
/// vault.
#[tokio::test]
async fn the_capability_tools_appear_only_for_a_mount_that_can_serve_them() {
    const HISTORY_TOOLS: [&str; 3] = ["note_history", "read_version", "resolve_divergence"];

    // Two FILESYSTEM mounts: none of the four, and no `resolveDivergence` argument.
    let plain = Fixture::new("no-capability-tools");
    let names = tool_names(&request(&plain.state().await, "tools/list", json!({})).await);
    for absent in HISTORY_TOOLS.iter().chain(["delete_note"].iter()) {
        assert!(
            !names.contains(&absent.to_string()),
            "{absent} must not be advertised when no mount can serve it: {names:?}"
        );
    }

    let fixture = AlgoliaFixture::new("capability-tools").await;

    // A WRITABLE algolia mount: all four.
    let response = request(&fixture.state_writable(true).await, "tools/list", json!({})).await;
    let names = tool_names(&response);
    for present in HISTORY_TOOLS.iter().chain(["delete_note"].iter()) {
        assert!(
            names.contains(&present.to_string()),
            "{present} must be advertised for a capable mount: {names:?}"
        );
    }
    // ...and `upsert_note` gains the one argument that can clear a divergence.
    let upsert = response["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == json!("upsert_note"))
        .expect("upsert_note");
    let description = upsert["inputSchema"]["properties"]["resolveDivergence"]["description"]
        .as_str()
        .expect("a resolveDivergence description");
    assert!(
        description.contains("never merges"),
        "the argument must say the server does not merge: {description}"
    );
    // `update_note_section` does NOT gain it: a reconciliation is a whole-note decision.
    let section = response["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == json!("update_note_section"))
        .expect("update_note_section");
    assert!(
        section["inputSchema"]["properties"]
            .get("resolveDivergence")
            .is_none(),
        "a section replacement cannot assert a whole-note reconciliation: {section}"
    );

    // A READ-ONLY algolia mount: the history tools, but NOT `delete_note`. Reading history
    // is a read; deleting is a write.
    let names = tool_names(
        &request(
            &fixture.state_writable(false).await,
            "tools/list",
            json!({}),
        )
        .await,
    );
    for present in HISTORY_TOOLS {
        assert!(
            names.contains(&present.to_string()),
            "{present} is a READ and must survive a read-only mount: {names:?}"
        );
    }
    assert!(
        !names.contains(&"delete_note".to_string()),
        "delete_note must not be advertised for a read-only mount: {names:?}"
    );
}

/// `note_history`'s `limit`: the newest versions, and truncation reported only when real.
///
/// The payload was O(versions) with no way to ask for less. Three properties matter and
/// each one is a way the fix could have been wrong:
///
/// * **untruncated answers do not change shape** — no `truncated`, no `totalCount`,
///   nothing. Every client written before this keeps working, and `count` keeps meaning
///   what it meant.
/// * **the versions kept are the NEWEST** — the list is newest-first, so a limit takes a
///   prefix. A limit that had kept the oldest would be useless in exactly the case that
///   motivates it.
/// * **truncation is stated** — `truncated: true` plus `totalCount`, so a caller can tell
///   "that is all of them" from "that is the first page".
#[tokio::test]
async fn note_history_limits_to_the_newest_versions_and_says_when_it_truncated() {
    let fixture = AlgoliaFixture::new("algolia-history-limit").await;
    let state = fixture.state_writable(true).await;
    let path = "_Shared/Decisions/Paginated.md";

    for revision in 1..=3 {
        tool_call(
            &state,
            "upsert_note",
            json!({"path": path, "content": format!("# Paginated\n\nbody {revision}\n")}),
        )
        .await;
    }

    // Unlimited: three versions, and NOT a single new key.
    let full = structured(&tool_call(&state, "note_history", json!({"path": path})).await).clone();
    assert_eq!(full["count"], json!(3), "{full}");
    assert!(
        full.get("truncated").is_none(),
        "an untruncated history must carry no `truncated` key at all: {full}"
    );
    assert!(full.get("totalCount").is_none(), "{full}");
    let newest = full["versions"][0]["versionId"].clone();
    let second_newest = full["versions"][1]["versionId"].clone();

    // Limited: the two NEWEST, in the same order, with the shortfall named.
    let page =
        structured(&tool_call(&state, "note_history", json!({"path": path, "limit": 2})).await)
            .clone();
    assert_eq!(page["count"], json!(2), "{page}");
    assert_eq!(page["truncated"], json!(true), "{page}");
    assert_eq!(page["totalCount"], json!(3), "{page}");
    assert!(
        page["truncationNote"].is_string(),
        "a truncation flag travels with prose explaining it: {page}"
    );
    assert_eq!(page["versions"][0]["versionId"], newest, "{page}");
    assert_eq!(page["versions"][1]["versionId"], second_newest, "{page}");
    assert_eq!(
        page["versions"][0]["current"],
        json!(true),
        "the head survives truncation, which is what `resolve_divergence` depends on: {page}"
    );

    // A limit at or above the total is not truncation.
    let exact =
        structured(&tool_call(&state, "note_history", json!({"path": path, "limit": 3})).await)
            .clone();
    assert_eq!(exact["count"], json!(3), "{exact}");
    assert!(
        exact.get("truncated").is_none(),
        "exactly enough is not truncated: {exact}"
    );
}

/// `note_history`, `read_version` and `delete_note` round-trip: versions accumulate, an
/// old one is still readable, a delete hides the note everywhere, and the content is
/// recoverable from the version the delete named.
#[tokio::test]
async fn version_history_and_soft_delete_round_trip_through_mcp() {
    let fixture = AlgoliaFixture::new("algolia-history").await;
    let state = fixture.state_writable(true).await;
    let path = "_Shared/Decisions/Deletable.md";
    let first = "# Deletable\n\nthe first body\n";
    let second = "# Deletable\n\nthe second body\n";

    tool_call(
        &state,
        "upsert_note",
        json!({"path": path, "content": first}),
    )
    .await;
    let history =
        structured(&tool_call(&state, "note_history", json!({"path": path})).await).clone();
    assert_eq!(history["count"], json!(1), "{history}");
    assert_eq!(history["hasDivergence"], json!(false), "{history}");
    let v1 = history["versions"][0]["versionId"]
        .as_str()
        .expect("a version id")
        .to_string();
    assert_eq!(history["versions"][0]["current"], json!(true));
    // Every key is present on every entry, `null` where the link does not exist — so a
    // client walking `versions[]` needs no branch.
    for key in [
        "versionId",
        "participantId",
        "updatedAtMs",
        "parentVersionId",
        "forkedFrom",
        "supersededBy",
        "current",
    ] {
        assert!(
            history["versions"][0].get(key).is_some(),
            "{key} must be present (null when absent): {history}"
        );
    }

    tool_call(
        &state,
        "upsert_note",
        json!({"path": path, "content": second}),
    )
    .await;
    let history =
        structured(&tool_call(&state, "note_history", json!({"path": path})).await).clone();
    assert_eq!(history["count"], json!(2), "{history}");
    // Newest first, and the current version is first.
    assert_eq!(history["versions"][0]["current"], json!(true), "{history}");
    assert_eq!(history["versions"][1]["versionId"], json!(v1), "{history}");
    assert!(
        history["versions"][1]["supersededBy"].is_string(),
        "an archived version names what replaced it: {history}"
    );

    // The superseded version is still readable, byte-exact, with its own hash.
    let old = structured(
        &tool_call(
            &state,
            "read_version",
            json!({"path": path, "versionId": v1}),
        )
        .await,
    )
    .clone();
    assert_eq!(old["text"], json!(first), "{old}");
    assert!(old["hash"]
        .as_str()
        .expect("a hash")
        .starts_with("fnv1a64:"));

    // Delete: the note leaves every read.
    let deleted =
        structured(&tool_call(&state, "delete_note", json!({"path": path})).await).clone();
    assert_eq!(deleted["deleted"], json!(true), "{deleted}");
    assert_eq!(deleted["alreadyDeleted"], json!(false), "{deleted}");
    let recoverable = deleted["recoverableFrom"]
        .as_str()
        .expect("recoverableFrom")
        .to_string();
    assert!(deleted["howToRecover"]
        .as_str()
        .expect("recovery guidance")
        .contains("read_version"));

    assert!(
        tool_call(&state, "read_file", json!({"path": path}))
            .await
            .get("error")
            .is_some(),
        "a deleted note must not be readable"
    );
    let listed = tool_call(
        &state,
        "list_children",
        json!({"path": "_Shared/Decisions"}),
    )
    .await;
    let names: Vec<&str> = structured(&listed)["children"]
        .as_array()
        .expect("children")
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Deletable.md"),
        "a tombstone must not appear in listings: {names:?}"
    );
    // ...and it leaves the mount's own recall too, because the delete removed its chunks.
    let recall = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "second body", "scope": "_Shared"}),
    )
    .await;
    let paths: Vec<&str> = structured(&recall)["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(
        !paths.contains(&path),
        "a deleted note must not be findable: {paths:?}"
    );

    // Deleting it again is a successful no-op, not an error.
    let again = structured(&tool_call(&state, "delete_note", json!({"path": path})).await).clone();
    assert_eq!(again["alreadyDeleted"], json!(true), "{again}");

    // The content is still there, and writing it back undeletes the note.
    let recovered = structured(
        &tool_call(
            &state,
            "read_version",
            json!({"path": path, "versionId": recoverable}),
        )
        .await,
    )
    .clone();
    assert_eq!(recovered["text"], json!(second), "{recovered}");
    tool_call(
        &state,
        "upsert_note",
        json!({"path": path, "content": second}),
    )
    .await;
    assert_eq!(
        structured(&tool_call(&state, "read_file", json!({"path": path})).await)["text"],
        json!(second)
    );
    // The resurrection is NOT recorded as a divergence: a read reports a tombstone as
    // absent, so the writer's observation was correct.
    assert_eq!(
        structured(&tool_call(&state, "note_history", json!({"path": path})).await)
            ["hasDivergence"],
        json!(false)
    );
}

/// `delete_note` refuses a LOCAL path and leaves the file exactly where it was.
///
/// The assertion PR #40 pinned, and the one that matters most in this slice: MCP has never
/// exposed local file deletion, and adding `delete_note` must not grant it by side effect.
#[tokio::test]
async fn delete_note_refuses_a_local_path_and_leaves_the_file() {
    let fixture = AlgoliaFixture::new("algolia-delete-local").await;
    let state = fixture.state_writable(true).await;

    let message =
        error_message(&tool_call(&state, "delete_note", json!({"path": "Root.md"})).await)
            .to_string();
    assert!(
        message.contains("mount 'vault'") && message.contains("filesystem"),
        "the refusal must name the mount and its backend: {message}"
    );
    assert!(
        message.contains("no deletion of local vault files"),
        "the refusal must say the omission is deliberate: {message}"
    );
    assert!(
        message.contains("'_Shared/'"),
        "the refusal must name the mounts that DO support it: {message}"
    );
    // The file is still there.
    assert!(
        fixture.inner.root_vault.join("Root.md").exists(),
        "a refused delete must not have removed anything"
    );

    // The same shape for the history tools on a filesystem path.
    for tool in ["note_history", "resolve_divergence"] {
        let message =
            error_message(&tool_call(&state, tool, json!({"path": "Root.md"})).await).to_string();
        assert!(
            message.contains("one content per note"),
            "{tool} must explain the storage model rather than report a failure: {message}"
        );
    }
}

/// The divergence loop, end to end: a fork is recorded, `resolve_divergence` hands back
/// all three corners of the merge, and a write asserting the reconciliation clears the
/// mark.
///
/// The fork is staged through the BACKEND rather than through MCP, and it has to be: the
/// tool layer's `expectedHash` guard rejects a stale caller ABOVE the boundary, so the only
/// way to reach the fork path is the TOCTOU window between that check and the write — which
/// is exactly what writing directly with a stale `BaseVersion` reproduces.
#[tokio::test]
async fn a_recorded_divergence_is_resolvable_and_only_an_asserted_merge_clears_it() {
    use deep_obsidian_backend::{BackendRequest, BaseVersion};

    let fixture = AlgoliaFixture::new("algolia-divergence").await;
    let state = fixture.state_writable(true).await;
    let logical = "_Shared/Decisions/Contested.md";
    // Mount-relative: the backend is addressed directly below, without the router.
    let remote = "Decisions/Contested.md";

    let backend = state
        .router
        .mounts()
        .iter()
        .find(|mount| mount.id == "shared")
        .expect("the algolia mount")
        .backend
        .clone();

    // v1, the common ancestor.
    tool_call(
        &state,
        "upsert_note",
        json!({"path": logical, "content": "# Contested\n\nthe ancestor body\n"}),
    )
    .await;
    let v1 = structured(&tool_call(&state, "note_history", json!({"path": logical})).await)
        ["versions"][0]["versionId"]
        .as_str()
        .expect("v1")
        .to_string();

    // v2, a continuation of v1 by another participant. Not a fork: the base IS the head.
    backend
        .execute(BackendRequest::write_text_guarded(
            remote,
            "# Contested\n\nthe overtaking body\n",
            BaseVersion::Version(v1.clone()),
        ))
        .await
        .expect("the second write lands");
    let history =
        structured(&tool_call(&state, "note_history", json!({"path": logical})).await).clone();
    assert_eq!(
        history["hasDivergence"],
        json!(false),
        "a head-based write is not a fork: {history}"
    );
    let v2 = history["versions"][0]["versionId"]
        .as_str()
        .expect("v2")
        .to_string();

    // v3, written from the STALE v1 base: the head has moved to v2, so this forks.
    backend
        .execute(BackendRequest::write_text_guarded(
            remote,
            "# Contested\n\nthe forked body\n",
            BaseVersion::Version(v1.clone()),
        ))
        .await
        .expect("a stale-based write lands as a fork rather than failing");

    let history =
        structured(&tool_call(&state, "note_history", json!({"path": logical})).await).clone();
    assert_eq!(
        history["hasDivergence"],
        json!(true),
        "a stale-based write records a divergence: {history}"
    );
    assert_eq!(
        history["versions"][0]["forkedFrom"],
        json!(v2),
        "the fork names the head it displaced: {history}"
    );
    assert_eq!(
        history["versions"][0]["parentVersionId"],
        json!(v1),
        "...and the version its content came from: {history}"
    );
    // `vault_info` reports it too, in the logical namespace.
    let info = structured(&tool_call(&state, "vault_info", json!({})).await).clone();
    let shared = info["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|mount| mount["id"] == json!("shared"))
        .expect("the shared mount")
        .clone();
    assert_eq!(shared["conflictedCount"], json!(1), "{shared}");
    assert_eq!(shared["conflictedPaths"], json!([logical]), "{shared}");

    // All three corners of the merge, and no merge.
    let divergence =
        structured(&tool_call(&state, "resolve_divergence", json!({"path": logical})).await)
            .clone();
    assert_eq!(divergence["hasDivergence"], json!(true), "{divergence}");
    assert!(divergence["head"]["text"]
        .as_str()
        .expect("head text")
        .contains("the forked body"));
    // The head block carries no hash: `read_file` already reports the current hash, and a
    // second one here would invite a client to feed the wrong value back as expectedHash.
    assert!(divergence["head"].get("hash").is_none(), "{divergence}");
    assert!(divergence["overtaken"]["text"]
        .as_str()
        .expect("overtaken text")
        .contains("the overtaking body"));
    assert_eq!(divergence["overtaken"]["versionId"], json!(v2));
    assert!(divergence["overtaken"]["hash"].is_string());
    assert!(divergence["commonAncestor"]["text"]
        .as_str()
        .expect("ancestor text")
        .contains("the ancestor body"));
    assert_eq!(divergence["commonAncestor"]["versionId"], json!(v1));
    assert!(divergence["howToResolve"]
        .as_str()
        .expect("guidance")
        .contains("resolveDivergence: true"));

    // A plain write does NOT clear the mark: divergence is sticky until something asserts
    // the reconciliation.
    tool_call(
        &state,
        "upsert_note",
        json!({"path": logical, "content": "# Contested\n\njust another edit\n"}),
    )
    .await;
    assert_eq!(
        structured(&tool_call(&state, "note_history", json!({"path": logical})).await)
            ["hasDivergence"],
        json!(true),
        "an ordinary write must not clear a divergence it did not reconcile"
    );

    // The asserted merge clears it.
    tool_call(
        &state,
        "upsert_note",
        json!({
            "path": logical,
            "content": "# Contested\n\nthe forked body\n\nthe overtaking body\n",
            "resolveDivergence": true,
        }),
    )
    .await;
    let history =
        structured(&tool_call(&state, "note_history", json!({"path": logical})).await).clone();
    assert_eq!(
        history["hasDivergence"],
        json!(false),
        "an asserted reconciliation clears the mark: {history}"
    );
    // ...and `resolve_divergence` now says there is nothing to resolve, rather than
    // erroring.
    let resolved =
        structured(&tool_call(&state, "resolve_divergence", json!({"path": logical})).await)
            .clone();
    assert_eq!(resolved["hasDivergence"], json!(false), "{resolved}");
    assert_eq!(
        resolved["note"],
        json!("no divergence recorded on this note"),
        "{resolved}"
    );
    // The mount is no longer reporting a conflicted path.
    let info = structured(&tool_call(&state, "vault_info", json!({})).await).clone();
    let shared = info["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|mount| mount["id"] == json!("shared"))
        .expect("the shared mount")
        .clone();
    assert_eq!(shared["conflictedCount"], json!(0), "{shared}");
    assert!(shared.get("conflictedPaths").is_none(), "{shared}");
}

/// On a READ-ONLY algolia mount `delete_note` is not advertised, and calling it anyway is
/// refused by the SAME capability check — so the registration gate and the call guard
/// cannot disagree.
///
/// Two gates rather than one is deliberate: registration keeps the tool off `tools/list`,
/// and the call guard means a client working from a cached tool list, or one that guesses,
/// gets an explanation instead of a delete.
#[tokio::test]
async fn a_read_only_algolia_mount_refuses_a_delete_it_never_advertised() {
    let fixture = AlgoliaFixture::new("algolia-read-only-delete").await;
    let state = fixture.state_writable(false).await;

    let message =
        error_message(&tool_call(&state, "delete_note", json!({"path": "_Shared/A.md"})).await)
            .to_string();
    assert!(
        message.contains("mount 'shared'") && message.contains("algolia"),
        "the refusal must name the mount and its backend: {message}"
    );
    assert!(
        message.contains("No mount in this vault supports it."),
        "with no capable mount there is nothing to suggest: {message}"
    );

    // The history tools, which are READS, still work on the same mount.
    tool_call(
        &state,
        "note_history",
        json!({"path": "_Shared/Missing.md"}),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Resilience: recovery
// ---------------------------------------------------------------------------
//
// The failure halves of these scenarios are asserted above and in `federation_eval.rs`.
// What is added here is the other half — that a mount which failed comes BACK, without a
// process restart — because a degradation nobody can recover from is an outage with extra
// steps, and none of the earlier slices proved the return path.
//
// # No sleeps
//
// Every mount's freshness is checked inside the operation that needs it (`fresh_snapshot`
// on a query, a live HTTP call on an algolia read), so the polls below RE-ISSUE THE
// OPERATION rather than watching a status field — a field-watching poll would sit there
// until its deadline and then fail. The deadline is the only timing these tests contain.
//
// One recovery in this repository IS driven by a background loop: a couchdb mount whose
// remote was unreachable at handshake time re-hand-shakes on its own (see
// `a_remote_root_down_at_startup_starts_degraded_and_recovers_without_a_restart`). Its
// poll re-issues a read all the same, and the docstring there explains why that is a
// proof rather than a shortcut — a read has no power to re-handshake.

/// The bound every recovery poll below is allowed.
const RECOVERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Poll `attempt` until it returns `Some`, or panic with `what` and the last value seen.
///
/// Returns the value rather than a bool so the caller asserts on what it actually got.
async fn poll_until_some<T, F, Fut>(what: &str, mut attempt: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let deadline = std::time::Instant::now() + RECOVERY_DEADLINE;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        match attempt().await {
            Ok(value) => return value,
            Err(reason) => {
                last = reason;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    panic!("{what} did not recover within {RECOVERY_DEADLINE:?}; last observation: {last}");
}

/// A mount that was unreadable at startup serves — and clears its degraded readiness —
/// once its vault appears, with no process restart.
///
/// The companion to `a_broken_mount_degrades_readiness_by_name_while_the_root_keeps_serving`,
/// which proves the failure. This proves the return, and it is the one recovery in this
/// repository that happens with no help at all: a filesystem mount re-scans its vault on
/// every query that needs a fresh snapshot, so the directory appearing is the whole fix.
#[tokio::test]
async fn a_mount_that_was_unreadable_at_startup_recovers_when_its_vault_appears() {
    let fixture = Fixture::new("mount-recovery");
    let missing = fixture.index_dir.parent().expect("base").join("late-vault");
    let config = fixture.config_with_team_vault(missing.clone());
    let state = fixture.state_for(config).await;

    // Degraded to start with, and NAMED — the precondition, restated so a failure here
    // is distinguishable from a failure of the recovery.
    let diagnostics = state.runtimes.aggregate_diagnostics();
    assert_eq!(diagnostics.status.as_str(), "degraded");
    let mut payload = build_readiness_payload(&state.config, &diagnostics);
    insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
    assert_eq!(payload["degradedMounts"], json!(["team"]));

    // The vault appears, as a remounted volume or a restored backup would make it.
    fs::create_dir_all(&missing).expect("create the late vault");
    fs::write(
        missing.join("Late.md"),
        "# Late\n\nThis note arrived after startup.\n",
    )
    .expect("write the late note");

    // The mount serves. Polled by RE-READING, because nothing refreshes on its own.
    let text = poll_until_some("a read on the recovered mount", || async {
        let response = tool_call(&state, "read_file", json!({"path": "Team/Late.md"})).await;
        response
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| response.to_string())
    })
    .await;
    assert!(text.contains("arrived after startup"), "{text}");

    // Readiness clears: no mount is degraded any more and the payload stops naming one.
    // A server that served the mount while still reporting it broken would make
    // readiness useless as a gate.
    let ready = poll_until_some("readiness to clear", || async {
        // A scoped query is what re-refreshes the mount's index; readiness only reports
        // what the last refresh recorded.
        let _ = tool_call(
            &state,
            "hybrid_search",
            json!({"query": "late note", "scope": "Team"}),
        )
        .await;
        let diagnostics = state.runtimes.aggregate_diagnostics();
        let mut payload = build_readiness_payload(&state.config, &diagnostics);
        insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
        if payload["ready"] == json!(true) {
            Ok(payload)
        } else {
            Err(payload.to_string())
        }
    })
    .await;
    assert_eq!(ready["status"], json!("ready"), "{ready}");
    assert!(
        ready.get("degradedMounts").is_none(),
        "a recovered mount must stop being named: {ready}"
    );
    let mounts = ready["mounts"].as_array().expect("mounts");
    let team = mounts
        .iter()
        .find(|mount| mount["id"] == json!("team"))
        .expect("the team mount");
    assert_eq!(team["indexStatus"], json!("ready"), "{team}");

    // ...and enumeration works again, rather than still refusing by naming the mount.
    let listed = request(&state, "resources/list", json!({})).await;
    assert!(
        listed.get("error").is_none(),
        "enumeration must stop refusing once the mount is back: {listed}"
    );
}

/// An algolia mount that goes down MID-SESSION fails reads honestly — it never serves a
/// cached body as if it were live — and recovers when the backend answers again.
///
/// # Why the cache cannot lie here, and why that is asserted rather than assumed
///
/// The algolia backend is the only one in this repository with a note cache, so it is the
/// only one where "the backend is down" could plausibly be answered from local state. It
/// cannot be: `reads::read_note` looks the head record up FIRST and only then consults the
/// cache, keyed by the version that lookup returned. The head lookup is a live call, so an
/// outage fails before the cache is ever reached — the freshness check and the network call
/// are the same call, by construction.
///
/// The test therefore warms the cache with a successful read before the outage. Without
/// that step a failing read would prove nothing: an empty cache has nothing to serve
/// staleley either way.
#[tokio::test]
async fn an_algolia_mount_that_goes_down_mid_session_fails_honestly_and_recovers() {
    let fixture = AlgoliaFixture::new("algolia-outage").await;
    let state = fixture.state_writable(true).await;

    let path = "_Shared/Outage/Note.md";
    let body = "# Outage\n\nThe body that is in the cache when the backend goes away.\n";
    let created = tool_call(
        &state,
        "upsert_note",
        json!({"path": path, "content": body}),
    )
    .await;
    assert_eq!(structured(&created)["created"], json!(true));

    // Warm the cache: this read hydrates from chunks and fills it.
    let warm = structured(&tool_call(&state, "read_file", json!({"path": path})).await).clone();
    assert!(
        warm["text"]
            .as_str()
            .expect("text")
            .contains("in the cache"),
        "{warm}"
    );

    fixture.mock.begin_outage();

    // The read FAILS. The cached body exists and is byte-correct, and it is still not
    // served: without a head lookup the server cannot know it is current, and answering
    // anyway would hand a caller stale content it could not detect.
    let response = tool_call(&state, "read_file", json!({"path": path})).await;
    let message = error_message(&response).to_string();
    assert!(
        !message.contains("in the cache"),
        "a read during an outage must not leak the cached body: {message}"
    );
    // And it says the BACKEND failed, rather than looking like a missing note. That
    // distinction is the honesty that matters most here: "not found" would be a claim
    // about the vault's contents, and a caller acting on it could recreate a note that
    // already exists.
    assert!(
        message.contains("algolia") && message.contains("503"),
        "the failure must identify the backend and the remote status: {message}"
    );
    assert!(
        !message.to_lowercase().contains("not found"),
        "an unreachable backend must not be reported as a missing note: {message}"
    );
    // A listing fails too, rather than reporting the mount as empty — which a caller
    // could not tell from every shared note having been deleted.
    let listed = tool_call(&state, "list_children", json!({"path": "_Shared/Outage"})).await;
    assert!(
        listed.get("error").is_some(),
        "a listing during an outage must fail rather than report an empty folder: {listed}"
    );

    fixture.mock.end_outage();

    // Recovery, with no restart of anything: the next read succeeds and answers the same
    // content it answered before the outage.
    let recovered = poll_until_some("a read after the algolia outage ended", || async {
        let response = tool_call(&state, "read_file", json!({"path": path})).await;
        response
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| response.to_string())
    })
    .await;
    assert_eq!(
        recovered,
        warm["text"].as_str().expect("text"),
        "the content after recovery is the one that was always there"
    );

    // The root mount was never involved in any of it.
    let root = tool_call(&state, "read_file", json!({"path": "Root.md"})).await;
    assert!(structured(&root)["text"]
        .as_str()
        .expect("text")
        .contains("Root note"));
}

/// A federated answer degrades and NAMES the algolia mount while it is down, then stops
/// doing either once it is back.
///
/// The permanently-dead-mount half of this is `federation_eval.rs`'s
/// `an_unavailable_mount_degrades_the_answer_and_is_named_without_disturbing_the_rest`.
/// What is new is that `degraded` and `missingBackends` are TRANSIENT: they describe the
/// answer that was just produced, not a latched state, so a caller that retries after an
/// outage gets a clean answer rather than a permanent warning it learns to ignore.
#[tokio::test]
async fn a_federated_answer_stops_being_degraded_once_the_algolia_mount_returns() {
    let fixture = AlgoliaFixture::new("algolia-federated-outage").await;
    let state = fixture.state_writable(true).await;

    tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "_Shared/Charter.md",
            "content": "# Charter\n\nQuaalbrook retention charter for the shared corpus.\n",
        }),
    )
    .await;

    let query = json!({"query": "Quaalbrook retention charter"});

    // Healthy: not degraded, nothing missing.
    let healthy = structured(&tool_call(&state, "hybrid_search", query.clone()).await).clone();
    assert_eq!(healthy["degraded"], json!(false), "{healthy}");
    assert!(healthy.get("missingBackends").is_none(), "{healthy}");

    fixture.mock.begin_outage();

    // Down: the ROOT mount still answers, so the call succeeds — and says what it lost.
    let degraded = structured(&tool_call(&state, "hybrid_search", query.clone()).await).clone();
    assert_eq!(degraded["degraded"], json!(true), "{degraded}");
    assert_eq!(
        degraded["missingBackends"],
        json!(["shared"]),
        "the unreachable mount must be named: {degraded}"
    );
    let reason = degraded["degradationReason"]
        .as_str()
        .expect("a degradation reason");
    assert!(
        reason.contains("shared"),
        "the reason must say which mount: {reason}"
    );
    // Nothing from the down mount is presented as a result.
    let paths: Vec<&str> = degraded["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(
        paths.iter().all(|path| !path.starts_with("_Shared/")),
        "{paths:?}"
    );

    fixture.mock.end_outage();

    // Back: the very same query is no longer degraded and the mount is no longer named.
    let recovered = poll_until_some("a federated answer after the outage ended", || async {
        let payload = structured(&tool_call(&state, "hybrid_search", query.clone()).await).clone();
        if payload["degraded"] == json!(false) {
            Ok(payload)
        } else {
            Err(payload.to_string())
        }
    })
    .await;
    assert!(
        recovered.get("missingBackends").is_none(),
        "a recovered answer must not keep naming the mount: {recovered}"
    );
    let shared = recovered["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|mount| mount["id"] == json!("shared"))
        .expect("the shared mount is reported again")
        .clone();
    assert!(
        shared.get("error").is_none(),
        "the recovered mount must not still carry an error: {shared}"
    );
    // And it is contributing again, which is the difference between "not degraded"
    // and "actually working".
    let paths: Vec<&str> = recovered["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(
        paths.iter().any(|path| path.starts_with("_Shared/")),
        "the recovered mount must contribute results again: {paths:?}"
    );
}

// ---------------------------------------------------------------------------
// A REMOTE backend at the vault root
// ---------------------------------------------------------------------------
//
// Every topology above puts a filesystem mount at `mountAt: ""`, because until this slice
// the config refused anything else. These are the configurations that restriction used to
// forbid: a couchdb root, an algolia root, and a table with no filesystem mount in it at
// all.
//
// They live in THIS suite rather than in `mcp_contract.rs` for the same reason the
// non-root remote mounts do: `mcp_contract.rs`'s goldens describe a single-mount
// FILESYSTEM vault and must not move. Nothing here is single-mount in that sense, even
// when the table has one entry — the entry is not a directory.

/// A single-mount config whose only mount is the couchdb stub, AT THE VAULT ROOT.
///
/// `vault_path` is `None`, which is the whole point, and `index_dir` is the fixture's own
/// directory rather than the XDG-anchored default. That override is deliberate and
/// load-bearing for the test suite: `default_remote_root_index_dir` resolves under the
/// user's real Application Support / `XDG_DATA_HOME`, so a test that took the default
/// would write a SQLite index into the developer's data directory and would collide with
/// itself between runs. The default derivation is asserted where it belongs — as a pure
/// function, in the config crate's own tests.
fn couchdb_root_config(fixture: &CouchdbFixture, mount_id: &str) -> ResolvedServiceConfig {
    couchdb_root_config_writable(fixture, mount_id, false)
}

/// The same root table, with the mount opted in to writes. `writable` is per mount and does
/// not become implicit by being at the root.
fn couchdb_root_config_writable(
    fixture: &CouchdbFixture,
    mount_id: &str,
    writable: bool,
) -> ResolvedServiceConfig {
    let mut config = fixture.config_writable(writable);
    config.vault_path = None;
    config.experimental = ExperimentalConfig {
        // NOT set: a one-mount table is the legacy shape spelled out longhand, so the
        // multi-mount gate does not apply to it. Only the couchdb flag is needed.
        multi_vault: false,
        couchdb_vaults: true,
        algolia_vaults: false,
    };
    let mut mount = config.mounts.remove(1);
    mount.id = mount_id.to_string();
    mount.mount_at = String::new();
    config.mounts = vec![mount];
    config
}

/// Build state from an arbitrary config, resolving the fixture's couchdb password.
///
/// `CouchdbFixture::state` hard-codes its own two-mount table; these tests need the same
/// secret plumbing over a table they built themselves.
async fn couchdb_state_from(fixture: &CouchdbFixture, config: ResolvedServiceConfig) -> AppState {
    let resolver = SecretResolver::with_encrypted_file_path(fixture.secrets.clone());
    resolver
        .put(
            &SecretRef::EncryptedFile {
                id: "livesync-password".to_string(),
            },
            secrecy::SecretString::new("s3cr3t-password-value".to_string()),
        )
        .expect("store the fixture password");
    let backends = MountBackends::build_with_resolver(&config, &resolver);
    let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
        .await
        .expect("a couchdb root must not fail the bootstrap");
    AppState::with_backends(config, runtimes, &backends)
}

/// A COUCHDB ROOT serves the whole vault: reads, listings, and the index-backed tools.
///
/// The centrepiece of the slice. A CouchDB-backed mount at `mountAt: ""` means a vault
/// with no local directory anywhere, and every logical path in it — no prefix, because
/// there is no prefix — routes to the remote.
///
/// # Why the index-backed tools matter here specifically
///
/// A couchdb mount HAS a local search index (over content it reads back from the remote),
/// which is what separates it from an algolia root. So `vault_info` and `hybrid_search`
/// must keep working on a fully-remote LiveSync vault, and `tools::root_index` must find a
/// root runtime rather than refuse. Asserting that is asserting the difference between the
/// two remote backends is real and not incidental.
#[tokio::test]
async fn a_couchdb_root_serves_the_whole_vault() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-root");
    let config = couchdb_root_config(&fixture, "live");
    assert_eq!(config.vault_path, None);
    assert_eq!(config.root_location(), "http://couch.invalid/vault");
    let state = couchdb_state_from(&fixture, config).await;

    // A read at a path with NO mount prefix, because the remote is the root.
    let read = tool_call(&state, "read_file", json!({"path": "Charter.md"})).await;
    let text = structured(&read)["text"]
        .as_str()
        .expect("text")
        .to_string();
    assert!(
        text.contains("Served from the CouchDB mount"),
        "the root read must come from the remote: {text}"
    );

    // The ROOT listing is the remote's, with no filesystem entries mixed into it.
    let listed = tool_call(&state, "list_children", json!({"path": ""})).await;
    let names: Vec<&str> = structured(&listed)["children"]
        .as_array()
        .expect("children")
        .iter()
        .filter_map(|child| child["name"].as_str())
        .collect();
    assert!(names.contains(&"Charter.md"), "{names:?}");
    assert!(
        !names.contains(&"Root.md"),
        "the filesystem fixture vault must not appear: {names:?}"
    );

    // `vault_info` works, which is the couchdb-specific half: the root HAS a local index.
    let info = structured(&tool_call(&state, "vault_info", json!({})).await).clone();
    assert_eq!(
        info["vaultPath"],
        json!("http://couch.invalid/vault"),
        "the overview names the remote rather than an empty path: {info}"
    );
    // A lone REMOTE mount still gets the additive per-mount report, unlike a lone
    // filesystem one — see `health::mount_detail_applies`. Without it a fully-remote
    // LiveSync vault would have no surface for its capabilities or its conflicts.
    let mounts = info["mounts"].as_array().expect("mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0]["id"], json!("live"));
    assert_eq!(mounts[0]["backendKind"], json!("couchdb"));
    assert_eq!(mounts[0]["mountAt"], json!(""));
    assert!(
        mounts[0]["capabilities"].is_array(),
        "the one place a caller can read what this vault supports: {info}"
    );

    // And the index really covers the remote's notes, so recall is not merely available
    // but populated.
    let recall =
        structured(&tool_call(&state, "hybrid_search", json!({"query": "charter"})).await).clone();
    let paths: Vec<&str> = recall["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(
        paths.contains(&"Charter.md"),
        "a couchdb root's index must cover the remote: {paths:?}"
    );

    // Readiness is GREEN: nothing about this configuration is degraded.
    let diagnostics = state.runtimes.aggregate_diagnostics();
    assert_eq!(diagnostics.status.as_str(), "ready");
    assert_eq!(
        readiness_status_code(&diagnostics),
        axum::http::StatusCode::OK
    );
    let payload = build_readiness_payload(&state.config, &diagnostics);
    assert_eq!(payload["vaultPath"], json!("http://couch.invalid/vault"));
    assert_eq!(payload["ready"], json!(true));
}

/// `delete_note` works on a couchdb mount that IS the vault root.
///
/// The refusal `delete_note` carries for a local path keys on the mount's CAPABILITY, not on
/// whether it is the root — so a fully-remote LiveSync vault must be deletable at
/// unprefixed paths, with no `LiveSync/` prefix anywhere to make the path look remote. This
/// is the topology where a capability check written as "is this the root?" would silently
/// give the wrong answer, so it gets its own test rather than riding on the two-mount one.
#[tokio::test]
async fn delete_note_works_on_a_couchdb_root() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("couchdb-root-delete");
    let config = couchdb_root_config_writable(&fixture, "live", true);
    assert_eq!(config.vault_path, None);
    let state = couchdb_state_from(&fixture, config).await;

    let names = tool_names(&request(&state, "tools/list", json!({})).await);
    assert!(
        names.contains(&"delete_note".to_string()),
        "a writable couchdb ROOT must advertise delete_note: {names:?}"
    );

    // An UNPREFIXED path: there is no mount prefix, because the remote is the root.
    let deleted =
        structured(&tool_call(&state, "delete_note", json!({"path": "Charter.md"})).await).clone();
    assert_eq!(deleted["deleted"], json!(true), "{deleted}");
    assert_eq!(deleted["alreadyDeleted"], json!(false), "{deleted}");
    assert!(
        deleted.get("recoverableFrom").is_none(),
        "a root couchdb mount has no more version history than a nested one: {deleted}"
    );
    assert!(deleted["howToRecover"]
        .as_str()
        .expect("recovery guidance")
        .contains("upsert_note"));

    // The root listing no longer offers it.
    let listed = tool_call(&state, "list_children", json!({"path": ""})).await;
    let names: Vec<&str> = structured(&listed)["children"]
        .as_array()
        .expect("children")
        .iter()
        .filter_map(|child| child["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Charter.md"),
        "the tombstone must leave the ROOT listing: {names:?}"
    );
}

/// An ALGOLIA ROOT serves reads and writes, and refuses the index-backed tools with the
/// reason a scoped call on an index-less mount already gives.
///
/// The other half of the pair. An Algolia mount has no local index BY DESIGN — the remote
/// index is the corpus — so when it is the root there is no root index anywhere, and the
/// tools derived from one have to say so. What must NOT happen is a panic (the old
/// `MountRuntimes::root()` was an `expect`) or a permanently red `/readyz` (a mount that
/// by design has no index is working exactly as designed).
#[tokio::test]
async fn an_algolia_root_serves_reads_and_refuses_the_index_backed_tools() {
    let fixture = AlgoliaFixture::new("algolia-root").await;

    // WRITABLE, because the corpus is seeded through the tool surface: the mock has no
    // back door, and writing through `upsert_note` is also the only way to prove a
    // remote-rooted vault accepts writes at an unprefixed path.
    let mut config = fixture.config_writable(true);
    config.vault_path = None;
    config.experimental = ExperimentalConfig {
        multi_vault: false,
        couchdb_vaults: false,
        algolia_vaults: true,
    };
    let mut mount = config.mounts.remove(1);
    mount.mount_at = String::new();
    config.mounts = vec![mount];
    assert_eq!(
        config.root_location(),
        format!("TESTAPP/team-wiki via {}", fixture.base_url)
    );

    let resolver = SecretResolver::with_encrypted_file_path(fixture.secrets.clone());
    resolver
        .put(
            &SecretRef::EncryptedFile {
                id: "algolia-api-key".to_string(),
            },
            secrecy::SecretString::new("test-key".to_string()),
        )
        .expect("store the fixture api key");
    let backends = MountBackends::build_with_resolver(&config, &resolver);
    let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
        .await
        .expect("an algolia root must not fail the bootstrap");
    let state = AppState::with_backends(config, runtimes, &backends);

    // A write at an UNPREFIXED path, because the remote corpus is the vault root.
    let created = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "Handbook.md",
            "content": "# Handbook\n\nThe shared handbook body.\n",
        }),
    )
    .await;
    assert_eq!(structured(&created)["created"], json!(true));
    assert_eq!(structured(&created)["path"], json!("Handbook.md"));

    // And it reads back from the remote at the same unprefixed path.
    let read = tool_call(&state, "read_file", json!({"path": "Handbook.md"})).await;
    let text = structured(&read)["text"]
        .as_str()
        .expect("text")
        .to_string();
    assert!(text.contains("shared handbook body"), "{text}");

    // `vault_info` refuses, and the refusal is `mount_index`'s: it names the mount, says
    // WHY there is no index, and says what does work on such a mount. A second wording
    // invented for the root would have said less.
    let refused = tool_call(&state, "vault_info", json!({})).await;
    let message = error_message(&refused);
    assert!(message.contains("shared"), "must name the mount: {message}");
    assert!(
        message.contains("grep_search"),
        "must name what still works: {message}"
    );

    // Readiness is NOT degraded: there is nothing wrong. `ready` is false because there
    // is no ROOT INDEX SNAPSHOT to report statistics from, which is a different question
    // — and the HTTP code keys on `status`, so a monitor gets 200.
    let diagnostics = state.runtimes.aggregate_diagnostics();
    assert_eq!(
        diagnostics.status.as_str(),
        "ready",
        "a root with no index by design is not a degraded root"
    );
    assert_eq!(
        readiness_status_code(&diagnostics),
        axum::http::StatusCode::OK
    );
    let mut payload = build_readiness_payload(&state.config, &diagnostics);
    insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
    assert_eq!(payload["ready"], json!(false));
    assert!(
        payload["vaultPath"]
            .as_str()
            .expect("a vault path")
            .starts_with("TESTAPP/team-wiki"),
        "{payload}"
    );
    // No index statistics rather than another mount's, which would report one mount's
    // corpus as the vault's.
    assert!(payload.get("markdownFileCount").is_none(), "{payload}");
    // The mount reports "no index", not "degraded index", and so does NOT appear in
    // `degradedMounts` — a mount that by design never has one is working as designed.
    let mounts = payload["mounts"].as_array().expect("mounts");
    assert_eq!(mounts[0]["indexStatus"], json!("none"));
    assert_eq!(mounts[0]["localIndex"], json!(false));
    assert!(payload.get("degradedMounts").is_none(), "{payload}");
}

/// A FULLY REMOTE two-mount vault: a couchdb root with an algolia mount grafted under it,
/// and not one filesystem directory in the table.
///
/// The shape the multi-backend documentation now describes. Worth its own test because it
/// is the only configuration in which every question about the vault root is answered by a
/// remote AND the router still has a prefix to resolve — so a regression that quietly
/// reintroduced a filesystem assumption at either level would show up here and nowhere
/// else.
#[tokio::test]
async fn a_fully_remote_two_mount_vault_serves_and_reports_both_mounts() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let couchdb = CouchdbFixture::new("fully-remote-couch");
    let algolia = AlgoliaFixture::new("fully-remote-algolia").await;

    let mut config = couchdb_root_config(&couchdb, "live");
    config.experimental = ExperimentalConfig {
        multi_vault: true,
        couchdb_vaults: true,
        algolia_vaults: true,
    };
    config.mounts.push(MountConfig {
        unknown: Default::default(),
        recall_weight: None,
        id: "shared".to_string(),
        mount_at: "_Shared".to_string(),
        backend: MountBackendConfig::Algolia {
            app_id: "TESTAPP".to_string(),
            index_name: "team-wiki".to_string(),
            api_key_ref: SecretRef::EncryptedFile {
                id: "algolia-api-key".to_string(),
            },
            base_url: Some(algolia.base_url.clone()),
            // Writable so the corpus can be seeded through the tool surface; the mock has
            // no back door.
            writable: true,
            participant_id: Some("paul@test".to_string()),
            cache: None,
            retention: None,
            index_dir: None,
        },
    });

    // Both secrets, into the SAME store: the resolver is per-state, not per-mount.
    let resolver = SecretResolver::with_encrypted_file_path(couchdb.secrets.clone());
    for (id, value) in [
        ("livesync-password", "s3cr3t-password-value"),
        ("algolia-api-key", "test-key"),
    ] {
        resolver
            .put(
                &SecretRef::EncryptedFile { id: id.to_string() },
                secrecy::SecretString::new(value.to_string()),
            )
            .expect("store a fixture secret");
    }
    let backends = MountBackends::build_with_resolver(&config, &resolver);
    let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
        .await
        .expect("a fully-remote table must not fail the bootstrap");
    let state = AppState::with_backends(config, runtimes, &backends);

    // Seed the shared corpus through the mount that owns it.
    let created = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "_Shared/Handbook.md",
            "content": "# Handbook\n\nThe shared handbook body.\n",
        }),
    )
    .await;
    assert_eq!(structured(&created)["created"], json!(true));

    // Each mount serves its own paths, and the root's paths carry no prefix.
    let root_read = tool_call(&state, "read_file", json!({"path": "Charter.md"})).await;
    assert!(structured(&root_read)["text"]
        .as_str()
        .expect("text")
        .contains("Served from the CouchDB mount"));
    let shared_read = tool_call(&state, "read_file", json!({"path": "_Shared/Handbook.md"})).await;
    assert!(structured(&shared_read)["text"]
        .as_str()
        .expect("text")
        .contains("shared handbook body"));

    // `vault_info` names the root's location and both mounts with their backends.
    let info = structured(&tool_call(&state, "vault_info", json!({})).await).clone();
    assert_eq!(info["vaultPath"], json!("http://couch.invalid/vault"));
    let mounts = info["mounts"].as_array().expect("mounts");
    assert_eq!(mounts.len(), 2);
    assert_eq!(mounts[0]["backendKind"], json!("couchdb"));
    assert_eq!(mounts[0]["mountAt"], json!(""));
    assert_eq!(mounts[1]["backendKind"], json!("algolia"));
    assert_eq!(mounts[1]["mountAt"], json!("_Shared"));
    assert!(
        !mounts
            .iter()
            .any(|mount| mount["backendKind"] == json!("filesystem")),
        "there is no filesystem anywhere in this vault: {info}"
    );

    // Readiness is green, and the `mounts[]` detail states each mount's own index state
    // including the algolia mount's "has none".
    let diagnostics = state.runtimes.aggregate_diagnostics();
    assert_eq!(diagnostics.status.as_str(), "ready");
    assert_eq!(
        readiness_status_code(&diagnostics),
        axum::http::StatusCode::OK
    );
    let mut payload = build_readiness_payload(&state.config, &diagnostics);
    insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
    assert_eq!(payload["vaultPath"], json!("http://couch.invalid/vault"));
    let reported = payload["mounts"].as_array().expect("mounts");
    assert_eq!(reported[0]["id"], json!("live"));
    assert_eq!(reported[0]["indexStatus"], json!("ready"));
    assert_eq!(reported[1]["id"], json!("shared"));
    assert_eq!(
        reported[1]["indexStatus"],
        json!("none"),
        "an algolia mount reports no index rather than a degraded one: {payload}"
    );
}

/// The stub sidecar, rewritten so its compatibility verdict depends on a FLAG FILE.
///
/// See `READY_FLAG` in `STUB_SIDECAR` for why the gate is read at child startup: it is
/// the only observation point the sidecar protocol leaves, since a second `initialize` on
/// one connection is refused.
fn gated_stub_sidecar(ready_flag: &std::path::Path) -> String {
    let literal = serde_json::to_string(&ready_flag.to_string_lossy().to_string())
        .expect("a path renders as a JSON string");
    let gated = STUB_SIDECAR.replace(
        "const READY_FLAG = null;",
        &format!("const READY_FLAG = {literal};"),
    );
    assert_ne!(
        gated, STUB_SIDECAR,
        "the READY_FLAG declaration must be rewritten"
    );
    gated
}

/// A REMOTE ROOT that is unreachable at startup starts the server DEGRADED — not fatally
/// — and then heals with no process restart.
///
/// # The three claims, and why they belong together
///
/// 1. **Not fatal.** `MountRuntimes::bootstrap` returns `Ok` even though the root mount
///    could not be indexed. A filesystem root in the same position still aborts; see
///    `runtime::root_failure_is_fatal` for why the asymmetry is about the failure MODE
///    (permanent local misconfiguration vs transient outage) rather than about position.
/// 2. **Honestly degraded.** `/readyz` answers 503, the mount is named, and reads refuse
///    with the backend's own reason instead of answering an empty vault — which a caller
///    could not distinguish from a vault that really is empty.
/// 3. **Recovered by itself.** The poll re-issues a read, and that is a proof rather than
///    a loophole: a read CANNOT recover a not-ready mount. `ready_connection` returns the
///    live connection and then refuses on the verdict it already recorded; nothing on the
///    data path re-runs `initialize`, and the child never died so the restart path never
///    runs either. So the only thing that can turn those refusals into content is the
///    supervisor's own background readiness-recovery loop.
///
/// Together they are the reason a remote root is allowed at all: without (3), a network
/// blip at the wrong moment would leave a fully-remote vault permanently unserveable
/// behind a process that looks alive, which is strictly worse than failing to start.
///
/// # Readiness comes back one step later, and on purpose
///
/// The mount healing is not the same event as `/readyz` turning green: the root's
/// `RuntimeState` is `Degraded` because its startup INDEX BUILD failed, and a runtime
/// re-indexes when something asks it for a fresh snapshot (or on the auto-reindex tick,
/// which this fixture disables). So the last step below asks for one and then asserts
/// readiness. Reporting `ready` the instant the mount was reachable would have been the
/// lie — the index really is still empty at that point.
#[tokio::test]
async fn a_remote_root_down_at_startup_starts_degraded_and_recovers_without_a_restart() {
    if !node_available() {
        eprintln!("skipping: `node` is not available on PATH");
        return;
    }
    let fixture = CouchdbFixture::new("remote-root-degraded");
    // The flag does NOT exist yet, so the first child classifies the remote unreachable.
    let ready_flag = fixture.secrets.with_file_name("remote-is-up");
    fs::write(&fixture.stub, gated_stub_sidecar(&ready_flag)).expect("rewrite the stub");
    assert!(!ready_flag.exists());

    let config = couchdb_root_config(&fixture, "live");
    let state = couchdb_state_from(&fixture, config).await;

    // (1) and (2): the server is up, and it says it is not serving.
    let diagnostics = state.runtimes.aggregate_diagnostics();
    assert_eq!(diagnostics.status.as_str(), "degraded");
    assert_eq!(
        readiness_status_code(&diagnostics),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let mut payload = build_readiness_payload(&state.config, &diagnostics);
    insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
    assert_eq!(payload["degradedMounts"], json!(["live"]));
    assert_eq!(
        payload["vaultPath"],
        json!("http://couch.invalid/vault"),
        "a degraded remote root is still nameable: {payload}"
    );

    // Reads refuse, and the refusal carries the sidecar's verdict rather than a generic
    // failure or an empty result.
    let refused = tool_call(&state, "read_file", json!({"path": "Charter.md"})).await;
    let message = error_message(&refused);
    assert!(
        message.contains("unreachable"),
        "the refusal must carry the verdict: {message}"
    );

    // The remote comes back. Nothing else is touched: no restart, no rebuild, no probe.
    fs::write(&ready_flag, b"up").expect("raise the ready flag");

    // (3): the same read that refused above starts working. See the docstring for why a
    // read cannot be what recovered it.
    let text = poll_until_some("the degraded remote root to heal by itself", || async {
        let response = tool_call(&state, "read_file", json!({"path": "Charter.md"})).await;
        match response.get("result") {
            Some(_) => Ok(structured(&response)["text"]
                .as_str()
                .expect("text")
                .to_string()),
            None => Err(error_message(&response).to_string()),
        }
    })
    .await;
    assert!(text.contains("Served from the CouchDB mount"), "{text}");

    // And readiness follows, once the index has been given the chance to build — which is
    // one step later than the mount healing, deliberately. See the docstring.
    let rebuilt = tool_call(&state, "build_index", json!({})).await;
    assert_eq!(structured(&rebuilt)["rebuilt"], json!(true));
    let diagnostics = state.runtimes.aggregate_diagnostics();
    assert_eq!(diagnostics.status.as_str(), "ready");
    assert_eq!(
        readiness_status_code(&diagnostics),
        axum::http::StatusCode::OK
    );
}
