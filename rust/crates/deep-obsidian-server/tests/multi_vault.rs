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

// ---------------------------------------------------------------------------
// A couchdb mount, end to end through the MCP surface
// ---------------------------------------------------------------------------
//
// A couchdb mount is multi-mount BY DEFINITION (it cannot be the root mount), so it
// belongs in this suite rather than in `mcp_contract.rs`, whose goldens describe a
// single-mount vault and must not move.
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
/// the MCP tools: `initialize.mode` (refusing `write` with `read-only`/-32009 unless
/// `read-write` was asked for), and a real revision-guarded compare-and-swap on
/// `write` (-32008 with `data.conflict.currentRev`). Revisions are a counter rather
/// than CouchDB hashes -- what this suite asserts is that the REVISION THREADING is
/// wired end to end, not how CouchDB derives a rev, which `couchdb_sidecar.rs` covers
/// against the real thing.
const STUB_SIDECAR: &str = r##"
import { createInterface } from "node:readline";
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
                compatibility: { status: "ok" },
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
                    deleted: false,
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
                    deleted: false,
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
                deleted: false,
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
                    deleted: false,
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
                deleted: false,
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
                resurrected: false,
            });
        }
        case "changesSince":
            return reply({ changes: [], nextCursor: "c1", exhausted: true });
        case "watch":
            return reply({ watching: true, cursor: "c1" });
        case "unwatch":
            return reply({ watching: false });
        case "health":
            return reply({ status: "ok", mode, compatibility: { status: "ok" }, watching: false, uptimeMs: 1 });
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

/// `grep_search` scoped to a CouchDB mount is refused, and the refusal says the vault
/// has no local files rather than blaming a missing ripgrep.
///
/// # Which guard actually fires, verified rather than assumed
///
/// `AppState::rg_available` is derived from the ROOT mount only, so with a filesystem
/// root it stays true and `grep_search` stays advertised. `VaultRouter::grep` does NOT
/// check `Capability::GrepSearch`; it scopes by the caller's `glob` and delegates to
/// that one mount's backend. So a glob that narrows to the couchdb prefix DOES reach
/// `CouchDbVaultBackend`'s `Recall::Grep` arm, and its message is what the caller sees
/// — the arm is genuinely reachable through MCP, not dead defence-in-depth.
///
/// An UNSCOPED grep is refused earlier, by the router's federation guard, which is
/// already covered by `an_unscoped_grep_is_refused_rather_than_answered_from_one_mount`.
#[tokio::test]
async fn grep_scoped_to_a_couchdb_mount_is_refused_honestly() {
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
        json!({"query": "Charter", "glob": "LiveSync/**/*.md"}),
    )
    .await;
    let message = error_message(&response);
    // The couchdb backend's own refusal, reached through the router.
    assert!(message.contains("EXPERIMENTAL"), "{message}");
    assert!(message.contains("READ-ONLY"), "{message}");
    assert!(
        message.contains("do not exist for a CouchDB vault"),
        "the refusal must name the real reason: {message}"
    );
    // ...and it must NOT blame a missing binary, which is the honest-error point.
    assert!(
        !message.contains("ripgrep is not installed"),
        "the refusal must not blame a missing binary: {message}"
    );
    // It points at what DOES work on this mount.
    assert!(message.contains("hybrid_search"), "{message}");

    // Sanity: grep on the FILESYSTEM root still works, so the refusal is per-mount and
    // not a global loss of the tool. The glob names `Notes/` rather than `*.md`
    // because a root-scoped glob would span the LiveSync mount too, which the
    // router refuses for the unrelated federation reason.
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
        let (base_url, mock) = deep_obsidian_algolia::mock::spawn_mock().await;
        Self {
            inner,
            base_url,
            _mock: mock,
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

/// The gate: an algolia mount needs its own experimental flag, and cannot be the root.
#[test]
fn an_algolia_mount_requires_the_algolia_vaults_flag_and_a_non_root_prefix() {
    let mounts = vec![
        MountConfig {
            id: "vault".to_string(),
            mount_at: String::new(),
            backend: MountBackendConfig::Filesystem {
                vault_path: PathBuf::from("/tmp/root-vault"),
                index_dir: None,
            },
        },
        MountConfig {
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

    // At the vault root: refused, because `vaultPath` would have nothing to point at.
    let mut rooted = mounts;
    rooted.remove(0);
    rooted[0].mount_at = String::new();
    let error =
        deep_obsidian_server::normalize_service_config(deep_obsidian_types::ServiceConfigInput {
            mounts: Some(rooted),
            experimental: Some(ExperimentalConfig {
                multi_vault: true,
                algolia_vaults: true,
                ..ExperimentalConfig::default()
            }),
            ..Default::default()
        })
        .expect_err("an algolia root mount must be refused");
    assert!(
        error.to_string().contains("no local directory"),
        "the refusal must say why: {error}"
    );
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

/// Scoped index recall on an algolia mount is refused, and the refusal explains that
/// the mount has no LOCAL index rather than implying something is broken.
#[tokio::test]
async fn scoped_index_recall_on_an_algolia_mount_is_refused_honestly() {
    let fixture = AlgoliaFixture::new("algolia-recall").await;
    let state = fixture.state_writable(true).await;
    tool_call(
        &state,
        "upsert_note",
        json!({"path": "_Shared/A.md", "content": "# A\n\nshared body\n"}),
    )
    .await;

    for (tool, arguments) in [
        (
            "hybrid_search",
            json!({"query": "shared", "scope": "_Shared"}),
        ),
        ("related_notes", json!({"path": "_Shared/A.md"})),
        ("graph_traverse", json!({"path": "_Shared/A.md"})),
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
        // The enumerated scopes must EXCLUDE the mount that was just refused. Naming it
        // would tell the user to retry the exact call that failed.
        assert!(
            message.contains("'/'"),
            "{tool} must name the root mount as a usable scope: {message}"
        );
        assert!(
            !message.contains("'_Shared'"),
            "{tool} must not suggest the mount it just refused: {message}"
        );
    }

    // Scoping the SAME tool to the filesystem root still works, so the refusal is
    // per-mount rather than a global loss of the tool.
    let root = tool_call(
        &state,
        "hybrid_search",
        json!({"query": "root", "scope": "/"}),
    )
    .await;
    assert!(
        root.get("result").is_some(),
        "recall on the filesystem root must still work: {root}"
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
    // Capabilities are the backend's own, and they are honest: a bounded grep, and
    // nothing binary.
    assert_eq!(shared["capabilities"], json!(["grep-search"]));
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
