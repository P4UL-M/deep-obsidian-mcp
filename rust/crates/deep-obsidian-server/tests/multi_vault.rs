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
const STUB_SIDECAR: &str = r##"
import { createInterface } from "node:readline";
const NOTES = {
    "Charter.md": "# LiveSync Charter\n\nServed from the CouchDB mount.\n",
    "Deep/Nested.md": "# Nested\n\nA nested LiveSync note.\n",
};
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
            return reply({
                protocolVersion: 1,
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
                entries: Object.entries(NOTES).map(([path, body]) => ({
                    path,
                    size: Buffer.byteLength(body),
                    mtimeMs: 1700000000000,
                    ctimeMs: 1700000000000,
                    deleted: false,
                    conflicted: false,
                    kind: "markdown",
                })),
                exhausted: true,
            });
        case "read": {
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
                rev: "1-stub",
            });
        }
        case "stat": {
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
                rev: "1-stub",
            });
        }
        case "changesSince":
            return reply({ changes: [], nextCursor: "c1", exhausted: true });
        case "watch":
            return reply({ watching: true, cursor: "c1" });
        case "unwatch":
            return reply({ watching: false });
        case "health":
            return reply({ status: "ok", compatibility: { status: "ok" }, watching: false, uptimeMs: 1 });
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

    /// The two-mount config, with the couchdb mount pointed at the stub.
    fn config(&self) -> ResolvedServiceConfig {
        let mut config = self.inner.config();
        config.experimental = ExperimentalConfig {
            multi_vault: true,
            couchdb_vaults: true,
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
            },
        };
        config
    }

    /// State over the couchdb config, with the password stored in a TEMP secrets file.
    ///
    /// A temp store rather than `XDG_CONFIG_HOME`: that variable is process-global and
    /// mutating it races every other test that reads the default secrets path.
    async fn state(&self) -> AppState {
        let resolver = SecretResolver::with_encrypted_file_path(self.secrets.clone());
        resolver
            .put(
                &SecretRef::EncryptedFile {
                    id: "livesync-password".to_string(),
                },
                secrecy::SecretString::new("s3cr3t-password-value".to_string()),
            )
            .expect("store the fixture password");

        let config = self.config();
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
