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

use deep_obsidian_server::health::{
    build_readiness_payload, insert_mount_index_detail, readiness_status_code,
};
use deep_obsidian_server::mcp::{handle_request, AppState};
use deep_obsidian_server::protocol::JsonRpcRequest;
use deep_obsidian_server::runtime::MountRuntimes;
use deep_obsidian_types::{
    AuthConfig, AutoReindexConfig, EmbeddingConfig, ExperimentalConfig, HttpConfig,
    MountBackendConfig, MountConfig, ResolvedServiceConfig, StdioMode, TransportMode,
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
            // Still the ROOT mount's path: it is what `vaultPath` has always meant,
            // and the root mount's own runtime is built from this config verbatim.
            vault_path: self.root_vault.clone(),
            mounts: vec![
                MountConfig {
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: self.root_vault.clone(),
                        index_dir: None,
                    },
                },
                MountConfig {
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
            experimental: ExperimentalConfig { multi_vault: true },
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
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config)
            .await
            .expect("bootstrap runtime");
        AppState::new(config, runtimes)
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

#[tokio::test]
async fn an_unscoped_grep_is_refused_rather_than_answered_from_one_mount() {
    let fixture = Fixture::new("grep");
    let state = fixture.state().await;
    if !state.rg_available {
        return;
    }

    let response = tool_call(&state, "grep_search", json!({"query": "charter"})).await;
    let message = error_message(&response);
    // Names the limitation AND the workaround. Answering from the root mount would
    // report zero matches for text that exists in the vault.
    assert!(
        message.contains("multiple vault mounts"),
        "message must name the limitation: {message}"
    );
    assert!(
        message.contains("glob"),
        "message must name the workaround: {message}"
    );

    // Scoping to a single mount works, and reports logical paths.
    let scoped = tool_call(
        &state,
        "grep_search",
        json!({"query": "charter", "glob": "Team/**/*.md"}),
    )
    .await;
    let matches = structured(&scoped)["matches"].as_array().expect("matches");
    assert!(!matches.is_empty());
    assert_eq!(matches[0]["path"], json!("Team/Charter.md"));
}

#[tokio::test]
async fn unscoped_index_recall_is_refused_and_names_the_scopes_that_would_work() {
    let fixture = Fixture::new("recall-unscoped");
    let state = fixture.state().await;

    for (tool, arguments) in [
        ("hybrid_search", json!({"query": "charter"})),
        ("load_knowledge", json!({"subject": "charter"})),
        ("search_artifacts", json!({"query": "charter"})),
    ] {
        let response = tool_call(&state, tool, arguments).await;
        let message = error_message(&response);
        assert!(
            message.starts_with(tool) && message.contains("'scope'"),
            "{tool} must name the missing argument, got: {message}"
        );
        // Every mount has an index now, so the refusal is about NOT MERGING them --
        // and it lists the scopes that would work, both of them.
        assert!(
            message.contains("'/'") && message.contains("'Team'"),
            "{tool} must name the usable scopes, got: {message}"
        );
    }
}

/// The tools with no argument that could ever name one mount. Their answer is a
/// whole-vault ranking (a limit-truncated path match; a folder recommendation), so
/// merging mounts is the only way to answer them and that is not this slice.
#[tokio::test]
async fn tools_without_a_scope_argument_still_refuse_a_multi_mount_vault() {
    let fixture = Fixture::new("recall-unscopable");
    let state = fixture.state().await;

    for (tool, arguments) in [
        ("find_files", json!({"query": "Charter"})),
        ("recommend_folder", json!({"topic": "charter"})),
    ] {
        let response = tool_call(&state, tool, arguments).await;
        let message = error_message(&response);
        assert!(
            message.starts_with(tool) && message.contains("multi-mount"),
            "{tool} must refuse explicitly, got: {message}"
        );
        assert!(
            message.contains("merging"),
            "{tool} must name the reason, got: {message}"
        );
    }
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
        // Required, because the tool genuinely cannot answer without it here.
        assert!(
            tool["inputSchema"]["required"]
                .as_array()
                .expect("required")
                .contains(&json!("scope")),
            "{name} must require 'scope' on a multi-mount vault"
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
