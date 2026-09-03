//! Black-box MCP contract snapshot tests.
//!
//! These tests drive the server at the JSON-RPC layer -- they build a
//! `JsonRpcRequest` from raw JSON, hand it to `mcp::handle_request`, and compare
//! the serialized response against a golden file under `tests/golden/`. Nothing
//! here calls an internal tool handler directly, so the suite freezes exactly
//! what an MCP client observes.
//!
//! The point is to make "zero public behavior change" provable across
//! refactors: any drift in a response's shape, key names, ordering, or error
//! taxonomy fails a snapshot.
//!
//! Hermetic by construction: every test gets a fresh temp vault, embeddings stay
//! on the default sparse backend (no network, no ollama), and `rg_available` is
//! forced to `false` so ripgrep's presence on the host cannot change the tool
//! list.
//!
//! Re-accept intentional changes with `UPDATE_GOLDEN=1 cargo test -p
//! deep-obsidian-server --test mcp_contract`, then review the golden diff.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use deep_obsidian_server::mcp::{handle_request, initialize_response, AppState};
use deep_obsidian_server::mounts::MountBackends;
use deep_obsidian_server::protocol::JsonRpcRequest;
use deep_obsidian_server::runtime::MountRuntimes;
use deep_obsidian_types::{
    AuthConfig, AutoReindexConfig, EmbeddingConfig, HttpConfig, ResolvedServiceConfig, StdioMode,
    TransportMode,
};
use serde_json::{json, Value};

/// A valid 1x1 transparent PNG. `read_artifact` only keys off the extension,
/// but real bytes keep the fixture honest (and the `size`/`hash` stable).
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Fixture vault
// ---------------------------------------------------------------------------

/// A temp vault plus the sibling index dir. The index dir deliberately lives
/// *outside* the vault so it can never show up in `list_children` or the
/// markdown walk, whatever the include flags are.
struct Fixture {
    vault_path: PathBuf,
    index_dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "deep-obsidian-contract-{name}-{}-{id}",
            std::process::id()
        ));
        // A leftover directory from a previous run would make counts drift.
        let _ = fs::remove_dir_all(&root);
        let vault_path = root.join("vault");
        let index_dir = root.join("index");
        fs::create_dir_all(&vault_path).expect("create vault dir");
        fs::create_dir_all(&index_dir).expect("create index dir");
        let fixture = Self {
            vault_path,
            index_dir,
        };
        fixture.seed();
        fixture
    }

    /// Every test seeds the identical vault so cross-test payloads
    /// (`markdownFileCount`, `noteCount`, `chunkCount`, resource lists) stay
    /// comparable and each golden is independent of test execution order.
    fn seed(&self) {
        let vault = &self.vault_path;
        fs::create_dir_all(vault.join("Folder/Nested")).expect("create nested dir");
        fs::create_dir_all(vault.join("Artifacts")).expect("create artifacts dir");
        fs::write(
            vault.join("Root.md"),
            "---\ntitle: Root\ntags:\n  - contract\n---\n\n# Root\n\n\
             Root preamble text linking to [[Folder/Child]] and [[Missing Note]].\n\n\
             ## Overview\n\nOverview body about the vault backend contract.\n\n\
             ## Details\n\nDetails body mentioning ripgrep and embeddings.\n",
        )
        .expect("write Root.md");
        fs::write(
            vault.join("Folder/Child.md"),
            "# Child\n\nChild body linking back to [[Root]].\n\n\
             ### Child Section\n\nNested child content.\n",
        )
        .expect("write Child.md");
        fs::write(
            vault.join("Folder/Nested/Deep.md"),
            "# Deep\n\nDeep note with no links.\n",
        )
        .expect("write Deep.md");
        fs::write(vault.join("Artifacts/diagram.png"), TINY_PNG).expect("write diagram.png");
    }

    fn config(&self) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            federated_rerank: true,
            vault_path: Some(self.vault_path.clone()),
            index_dir: self.index_dir.clone(),
            // Legacy shape: no declared mounts, so the router synthesizes the one
            // implicit root mount. Every golden below is therefore asserting the
            // single-mount path.
            mounts: Vec::new(),
            experimental: Default::default(),
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
        let config = self.config();
        let backends = MountBackends::build(&config);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("bootstrap runtime");
        let mut state = AppState::with_backends(config, runtimes, &backends);
        // Pin the one environment-dependent input to the tool list.
        state.rg_available = false;
        state
    }

    /// Absolute path spellings that may leak into a payload or an error message.
    /// `canonicalize` matters on macOS, where `/var` is a symlink to
    /// `/private/var` and vault IO errors report the resolved form.
    fn scrub_prefixes(&self) -> Vec<String> {
        let mut prefixes = vec![self.vault_path.to_string_lossy().to_string()];
        if let Ok(resolved) = self.vault_path.canonicalize() {
            prefixes.push(resolved.to_string_lossy().to_string());
        }
        prefixes.push(self.index_dir.to_string_lossy().to_string());
        if let Ok(resolved) = self.index_dir.canonicalize() {
            prefixes.push(resolved.to_string_lossy().to_string());
        }
        // Longest first so a prefix never shadows a longer sibling.
        prefixes.sort_by_key(|prefix| std::cmp::Reverse(prefix.len()));
        prefixes.dedup();
        prefixes
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC driver
// ---------------------------------------------------------------------------

/// Round-trip a request through `handle_request` exactly as a transport would:
/// raw JSON in, serialized JSON-RPC frame out. Errors are returned as the
/// serialized `JsonRpcErrorResponse` so the error taxonomy is snapshotted with
/// the same machinery as the success payloads.
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
        // No test below expects a notification; surface it rather than hide it.
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

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/// Rewrite the fields that cannot be stable across machines and runs, and
/// nothing else. Every rule here is a documented non-determinism, not a
/// convenience: the whole value of the snapshot is that untouched fields must
/// match byte for byte.
fn normalize(value: &Value, scrub: &[String]) -> Value {
    normalize_keyed(None, value, scrub)
}

fn normalize_keyed(key: Option<&str>, value: &Value, scrub: &[String]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(name, child)| {
                    (
                        name.clone(),
                        normalize_keyed(Some(name.as_str()), child, scrub),
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| normalize_keyed(None, item, scrub))
                .collect(),
        ),
        // `expiresAt` is `SystemTime::now() + TTL` rendered as a unix epoch.
        Value::Number(_) if key == Some("expiresAt") => json!("<EPOCH>"),
        // `vault_info.rebuilt` reports runtime cache state, not vault content:
        // `RuntimeState` serves the bootstrap snapshot (`rebuilt: true`) only
        // while it is younger than `DEFAULT_FRESH_SNAPSHOT_MAX_AGE` (2s), after
        // which a fresh refresh over an unchanged vault reports `false`. That is
        // a wall-clock race on a loaded runner, so the value is asserted
        // structurally instead (see `vault_info_is_frozen`).
        Value::Bool(_) if key == Some("rebuilt") => json!("<BOOL>"),
        Value::String(text) => normalize_string(key, text, scrub),
        other => other.clone(),
    }
}

fn normalize_string(key: Option<&str>, text: &str, scrub: &[String]) -> Value {
    // Index build timestamp (RFC3339, wall clock).
    if key == Some("indexGeneratedAt") {
        return json!("<TIMESTAMP>");
    }

    let mut scrubbed = text.to_string();
    for prefix in scrub {
        scrubbed = scrubbed.replace(prefix.as_str(), "<VAULT>");
    }

    // Upload capability tokens are randomly generated per mint. They appear
    // both bare in the URL and embedded in the copy-pasteable curl example.
    if matches!(key, Some("uploadUrl") | Some("curlExample")) {
        scrubbed = mask_upload_token(&scrubbed);
    }

    // `ToolCallResult.content[0].text` is the pretty-printed form of
    // `structuredContent`. Re-parsing it and normalizing recursively keeps the
    // two halves consistent, and proves the text block really is that JSON.
    // Non-JSON text (note bodies, outline excerpts) falls through untouched.
    if key == Some("text") {
        if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&scrubbed) {
            let normalized = normalize(&Value::Object(parsed), scrub);
            return json!(serde_json::to_string_pretty(&normalized)
                .expect("normalized text block to serialize"));
        }
    }

    json!(scrubbed)
}

/// Replace everything after the last `/upload/` segment up to the closing quote
/// (or end of string) with a placeholder.
fn mask_upload_token(text: &str) -> String {
    const MARKER: &str = "/upload/";
    let Some(start) = text.rfind(MARKER) else {
        return text.to_string();
    };
    let token_start = start + MARKER.len();
    let token_end = text[token_start..]
        .find('"')
        .map(|offset| token_start + offset)
        .unwrap_or(text.len());
    format!("{}<TOKEN>{}", &text[..token_start], &text[token_end..])
}

// ---------------------------------------------------------------------------
// Golden files
// ---------------------------------------------------------------------------

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn assert_golden(name: &str, actual: &Value) {
    let path = golden_dir().join(format!("{name}.json"));
    let serialized = format!(
        "{}\n",
        serde_json::to_string_pretty(actual).expect("actual value to serialize")
    );

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(golden_dir()).expect("create golden dir");
        fs::write(&path, serialized.as_bytes()).expect("write golden file");
        return;
    }

    let expected_text = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "missing golden snapshot {}: {error}\nre-run with UPDATE_GOLDEN=1 to create it",
            path.display()
        )
    });
    let expected: Value = serde_json::from_str(&expected_text).unwrap_or_else(|error| {
        panic!(
            "golden snapshot {} is not valid JSON: {error}",
            path.display()
        )
    });

    assert!(
        &expected == actual,
        "MCP contract drift in golden `{name}`\n\
         --- expected ({}) ---\n{expected_text}\n--- actual ---\n{serialized}\
         --- end ---\nIf this change is intentional, re-run with UPDATE_GOLDEN=1 and review the diff.",
        path.display()
    );
}

/// Digest of `tools/list` that stays reviewable while still catching shape
/// drift: the exact tool ordering and names, whether the optional
/// `annotations`/`execution` blocks are emitted, and each input schema's
/// top-level keys plus its property names and required list.
fn tool_list_digest(response: &Value) -> Value {
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list result to carry a tools array");
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                let schema = &tool["inputSchema"];
                let schema_keys = schema
                    .as_object()
                    .map(|object| object.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                let mut properties = schema["properties"]
                    .as_object()
                    .map(|object| object.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                properties.sort();
                json!({
                    "name": tool["name"],
                    "hasDescription": tool["description"].is_string(),
                    "hasAnnotations": tool.get("annotations").is_some(),
                    "hasExecution": tool.get("execution").is_some(),
                    "inputSchemaKeys": schema_keys,
                    "properties": properties,
                    "required": schema.get("required").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
    )
}

fn tool_names(response: &Value) -> Vec<String> {
    response["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .expect("tool name to be a string")
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Handshake and discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initialize_response_shape_is_frozen() {
    let fixture = Fixture::new("initialize");
    let state = fixture.state().await;
    let response = request(&state, "initialize", json!({})).await;
    assert_golden(
        "initialize",
        &normalize(&response, &fixture.scrub_prefixes()),
    );

    // The pre-built handshake used by the HTTP transport must stay identical to
    // what the `initialize` method returns.
    let prebuilt =
        serde_json::to_value(initialize_response()).expect("initialize_response to serialize");
    assert_eq!(prebuilt, response);
}

#[tokio::test]
async fn initialized_notification_produces_no_response() {
    let fixture = Fixture::new("notification");
    let state = fixture.state().await;
    let payload = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    });
    let parsed: JsonRpcRequest = serde_json::from_value(payload).expect("notification to parse");
    let response = handle_request(state, parsed)
        .await
        .expect("notification must not error");
    assert!(
        response.is_none(),
        "notifications/initialized must stay silent, got {response:?}"
    );
}

/// The tool list is frozen for a single-mount filesystem vault, and everything that can
/// vary is enumerated here.
///
/// # The three inputs, and why the list is not simply constant
///
/// `tools/list` is computed per process from exactly three things:
///
/// 1. **`rg_available`** — adds `grep_search`. Asserted below.
///
///    Named for the environment fact it started as, but it is now a MOUNT-TABLE fact:
///    `AppState` computes it as "at least one mount's descriptor carries
///    `Capability::GrepSearch`", because a couchdb or algolia mount serves line search
///    with no `rg` anywhere on the host. On this fixture the two coincide — a single
///    filesystem mount declares the capability only when `rg` resolved — and
///    `Fixture::state` pins the flag to `false` regardless, which is what makes the
///    golden independent of whether the machine running the suite happens to have
///    ripgrep installed. The `true` case is exercised below by setting the flag.
/// 2. **multi-mount** (configuration) — adds the `scope` ARGUMENT to the routed recall
///    tools, never a tool. Asserted in `multi_vault.rs`.
/// 3. **mount capabilities** (configuration) — a mount advertising `version-history` adds
///    `note_history`, `read_version`, `resolve_divergence` and the `resolveDivergence`
///    argument on `upsert_note`; one advertising `soft-delete` adds `delete_note`.
///    Asserted below for their ABSENCE, and in `multi_vault.rs` for their presence.
///
/// This test owns the absence half of (3): the fixture is a single filesystem mount, which
/// can advertise neither capability, so the four tools must not exist and the golden must
/// not contain them. That is what makes the gating load-bearing rather than decorative —
/// the digest includes each tool's declared property names, so an ungated
/// `resolveDivergence` would change the frozen bytes.
#[tokio::test]
async fn tools_list_is_frozen() {
    let fixture = Fixture::new("tools-list");
    let state = fixture.state().await;
    let response = request(&state, "tools/list", json!({})).await;

    assert_golden("tools_list", &tool_list_digest(&response));

    // Same request twice in-process must produce an identical list.
    let repeat = request(&state, "tools/list", json!({})).await;
    assert_eq!(response, repeat, "tools/list must be deterministic");

    // Ripgrep availability is the only ENVIRONMENT-dependent input, and it may add
    // exactly `grep_search`.
    let mut with_rg = state.clone();
    with_rg.rg_available = true;
    let rg_names = tool_names(&request(&with_rg, "tools/list", json!({})).await);
    let base_names = tool_names(&response);
    assert!(
        !base_names.contains(&"grep_search".to_string()),
        "grep_search must be absent when rg is unavailable"
    );
    let added = rg_names
        .iter()
        .filter(|name| !base_names.contains(name))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(added, vec!["grep_search".to_string()]);
    assert_eq!(rg_names.len(), base_names.len() + 1);

    // A single filesystem mount advertises neither `version-history` nor `soft-delete`, so
    // the four capability tools must be absent from BOTH lists — with rg and without, so
    // the two conditional mechanisms are shown to be independent.
    for names in [&base_names, &rg_names] {
        for absent in [
            "delete_note",
            "note_history",
            "read_version",
            "resolve_divergence",
        ] {
            assert!(
                !names.contains(&absent.to_string()),
                "{absent} must not be advertised when no mount can serve it: {names:?}"
            );
        }
    }

    // ...and `upsert_note` declares no `resolveDivergence`, which is what keeps the golden
    // digest's property list unchanged. Asserted on the payload rather than the digest so
    // the failure names the cause instead of dumping a diff.
    let upsert = response["result"]["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .find(|tool| tool["name"] == json!("upsert_note"))
        .expect("upsert_note is always registered");
    assert!(
        upsert["inputSchema"]["properties"]
            .get("resolveDivergence")
            .is_none(),
        "no mount here can record a divergence, so the argument must not be declared: {upsert}"
    );
}

#[tokio::test]
async fn resources_and_prompts_listings_are_frozen() {
    let fixture = Fixture::new("discovery");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    let resources = request(&state, "resources/list", json!({})).await;
    assert_golden("resources_list", &normalize(&resources, &scrub));

    let templates = request(&state, "resources/templates/list", json!({})).await;
    assert_golden("resources_templates_list", &normalize(&templates, &scrub));

    let prompts = request(&state, "prompts/list", json!({})).await;
    assert_golden("prompts_list", &normalize(&prompts, &scrub));

    // All three are read-only, so repeating them must be a no-op.
    assert_eq!(
        resources,
        request(&state, "resources/list", json!({})).await
    );
    assert_eq!(
        templates,
        request(&state, "resources/templates/list", json!({})).await
    );
    assert_eq!(prompts, request(&state, "prompts/list", json!({})).await);
}

// ---------------------------------------------------------------------------
// Read-only tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vault_info_is_frozen() {
    let fixture = Fixture::new("vault-info");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    let response = tool_call(&state, "vault_info", json!({})).await;
    assert_golden("tool_vault_info", &normalize(&response, &scrub));

    // `rebuilt` is masked in the snapshot because it tracks the runtime snapshot
    // cache rather than the vault; the contract is only that it stays a boolean
    // and stays present.
    assert!(
        response["result"]["structuredContent"]["rebuilt"].is_boolean(),
        "vault_info must always report a boolean `rebuilt`"
    );

    let second = normalize(&tool_call(&state, "vault_info", json!({})).await, &scrub);
    let third = normalize(&tool_call(&state, "vault_info", json!({})).await, &scrub);
    assert_eq!(second, third, "vault_info must be stable across calls");
}

#[tokio::test]
async fn list_children_is_frozen() {
    let fixture = Fixture::new("list-children");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    let root = tool_call(&state, "list_children", json!({})).await;
    assert_golden("tool_list_children_root", &normalize(&root, &scrub));

    let folder = tool_call(&state, "list_children", json!({"path": "Folder"})).await;
    assert_golden("tool_list_children_folder", &normalize(&folder, &scrub));

    let folders_only = tool_call(&state, "list_children", json!({"foldersOnly": true})).await;
    assert_golden(
        "tool_list_children_folders_only",
        &normalize(&folders_only, &scrub),
    );

    // Directory iteration order is OS-dependent, so the sort must be doing the
    // work: repeat every variant and require an identical payload.
    assert_eq!(root, tool_call(&state, "list_children", json!({})).await);
    assert_eq!(
        folder,
        tool_call(&state, "list_children", json!({"path": "Folder"})).await
    );
    assert_eq!(
        folders_only,
        tool_call(&state, "list_children", json!({"foldersOnly": true})).await
    );
}

#[tokio::test]
async fn read_file_is_frozen() {
    let fixture = Fixture::new("read-file");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    let full = tool_call(&state, "read_file", json!({"path": "Root.md"})).await;
    assert_golden("tool_read_file_full", &normalize(&full, &scrub));

    let sliced = tool_call(
        &state,
        "read_file",
        json!({"path": "Root.md", "startLine": 7, "endLine": 10}),
    )
    .await;
    assert_golden("tool_read_file_range", &normalize(&sliced, &scrub));

    // `hash` is a pure content hash, so it must be reproducible from the
    // fixture bytes alone and usable as a conditional-read token.
    let hash = full["result"]["structuredContent"]["hash"]
        .as_str()
        .expect("read_file to report a hash")
        .to_string();
    let unchanged = tool_call(
        &state,
        "read_file",
        json!({"path": "Root.md", "knownHash": hash}),
    )
    .await;
    assert_golden("tool_read_file_unchanged", &normalize(&unchanged, &scrub));

    assert_eq!(
        full,
        tool_call(&state, "read_file", json!({"path": "Root.md"})).await
    );
}

#[tokio::test]
async fn note_outline_is_frozen() {
    let fixture = Fixture::new("note-outline");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    // Default: headings and blocks only, section text omitted.
    let response = tool_call(&state, "note_outline", json!({"path": "Root.md"})).await;
    assert_golden("tool_note_outline", &normalize(&response, &scrub));

    // With `includeText` the section bodies are inlined.
    let with_text = tool_call(
        &state,
        "note_outline",
        json!({"path": "Root.md", "includeText": true}),
    )
    .await;
    assert_golden(
        "tool_note_outline_with_text",
        &normalize(&with_text, &scrub),
    );

    assert_eq!(
        response,
        tool_call(&state, "note_outline", json!({"path": "Root.md"})).await
    );
}

#[tokio::test]
async fn find_files_is_frozen() {
    let fixture = Fixture::new("find-files");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    let substring = tool_call(&state, "find_files", json!({"query": "o"})).await;
    assert_golden("tool_find_files_substring", &normalize(&substring, &scrub));

    let regex = tool_call(
        &state,
        "find_files",
        json!({"query": "^Folder/.*\\.md$", "mode": "regex"}),
    )
    .await;
    assert_golden("tool_find_files_regex", &normalize(&regex, &scrub));

    // Results come off a filesystem walk, so ordering must be sorted, not
    // incidental.
    assert_eq!(
        substring,
        tool_call(&state, "find_files", json!({"query": "o"})).await
    );
    assert_eq!(
        regex,
        tool_call(
            &state,
            "find_files",
            json!({"query": "^Folder/.*\\.md$", "mode": "regex"}),
        )
        .await
    );
}

#[tokio::test]
async fn read_artifact_is_frozen() {
    let fixture = Fixture::new("read-artifact");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    // Metadata only: `maxBytes` defaults to 0, so no bytes are read and neither
    // `hash` nor `base64` is emitted.
    let metadata = tool_call(
        &state,
        "read_artifact",
        json!({"path": "Artifacts/diagram.png"}),
    )
    .await;
    assert_golden("tool_read_artifact_metadata", &normalize(&metadata, &scrub));

    // `includeBase64` requires an explicit `maxBytes` at or above the file size,
    // because the default clamp of 0 always trips the size guard.
    let embedded = tool_call(
        &state,
        "read_artifact",
        json!({"path": "Artifacts/diagram.png", "includeBase64": true, "maxBytes": 4096}),
    )
    .await;
    assert_golden("tool_read_artifact_base64", &normalize(&embedded, &scrub));

    let over_limit = tool_call(
        &state,
        "read_artifact",
        json!({"path": "Artifacts/diagram.png", "includeBase64": true, "maxBytes": 8}),
    )
    .await;
    assert_golden(
        "tool_read_artifact_over_max_bytes",
        &normalize(&over_limit, &scrub),
    );

    assert_eq!(
        metadata,
        tool_call(
            &state,
            "read_artifact",
            json!({"path": "Artifacts/diagram.png"})
        )
        .await
    );
}

// ---------------------------------------------------------------------------
// Write tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upsert_note_is_frozen() {
    let fixture = Fixture::new("upsert-note");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();
    let content = "# New Note\n\nFresh body linking to [[Root]].\n";
    let target = fixture.vault_path.join("Folder/New Note.md");

    // dryRun previews the write, including the hash the write *would* produce,
    // without touching the vault.
    let preview = tool_call(
        &state,
        "upsert_note",
        json!({"path": "Folder/New Note.md", "content": content, "dryRun": true}),
    )
    .await;
    assert_golden("tool_upsert_note_dry_run", &normalize(&preview, &scrub));
    assert!(
        !target.exists(),
        "dryRun must not create {}",
        target.display()
    );

    let created = tool_call(
        &state,
        "upsert_note",
        json!({"path": "Folder/New Note.md", "content": content}),
    )
    .await;
    assert_golden("tool_upsert_note_created", &normalize(&created, &scrub));
    assert!(target.exists(), "the real write must create the note");

    // The preview and the real write must agree on the resulting content hash;
    // only `dryRun` differs.
    assert_eq!(
        preview["result"]["structuredContent"]["newHash"],
        created["result"]["structuredContent"]["newHash"]
    );

    // `newHash` from a write feeds straight back in as `expectedHash`.
    let previous_hash = created["result"]["structuredContent"]["newHash"]
        .as_str()
        .expect("upsert_note to report newHash")
        .to_string();
    let updated = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "Folder/New Note.md",
            "content": "# New Note\n\nSecond revision.\n",
            "expectedHash": previous_hash,
        }),
    )
    .await;
    assert_golden("tool_upsert_note_updated", &normalize(&updated, &scrub));

    // A stale `expectedHash` is a conflict, not a silent overwrite.
    let conflict = tool_call(
        &state,
        "upsert_note",
        json!({
            "path": "Folder/New Note.md",
            "content": "# New Note\n\nThird revision.\n",
            "expectedHash": previous_hash,
        }),
    )
    .await;
    assert_golden(
        "error_upsert_note_hash_conflict",
        &normalize(&conflict, &scrub),
    );
}

#[tokio::test]
async fn update_note_section_is_frozen() {
    let fixture = Fixture::new("update-section");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    let heading = tool_call(
        &state,
        "update_note_section",
        json!({
            "path": "Root.md",
            "heading": "Overview",
            "content": "Replaced overview body.",
        }),
    )
    .await;
    assert_golden(
        "tool_update_note_section_heading",
        &normalize(&heading, &scrub),
    );

    let preamble = tool_call(
        &state,
        "update_note_section",
        json!({
            "path": "Folder/Child.md",
            "target": "preamble",
            "content": "Replaced preamble body.",
            "dryRun": true,
        }),
    )
    .await;
    assert_golden(
        "tool_update_note_section_preamble_dry_run",
        &normalize(&preamble, &scrub),
    );

    let missing_heading = tool_call(
        &state,
        "update_note_section",
        json!({"path": "Root.md", "content": "No heading given."}),
    )
    .await;
    assert_golden(
        "error_update_note_section_missing_heading",
        &normalize(&missing_heading, &scrub),
    );
}

#[tokio::test]
async fn edit_note_is_frozen() {
    let fixture = Fixture::new("edit-note");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    let literal = tool_call(
        &state,
        "edit_note",
        json!({
            "path": "Root.md",
            "edits": [{"old": "ripgrep and embeddings", "new": "ripgrep"}],
        }),
    )
    .await;
    assert_golden("tool_edit_note_literal", &normalize(&literal, &scrub));

    // `body` occurs in both `## Overview` and `## Details`, so this is the refusal, not a
    // first-match edit. The message carries the candidate lines.
    let ambiguous = tool_call(
        &state,
        "edit_note",
        json!({
            "path": "Root.md",
            "edits": [{"old": "body", "new": "BODY"}],
        }),
    )
    .await;
    assert_golden("error_edit_note_ambiguous", &normalize(&ambiguous, &scrub));

    let full = tool_call(
        &state,
        "edit_note",
        json!({
            "path": "Root.md",
            "edits": [{"old": "Overview body", "new": "New overview body"}],
            "verbosity": "full",
            "dryRun": true,
        }),
    )
    .await;
    assert_golden(
        "tool_edit_note_full_verbosity_dry_run",
        &normalize(&full, &scrub),
    );
}

/// The behaviour contrast `edit_note` exists for, pinned on both sides.
///
/// `Folder/Child.md` has `### Child Section` nested under `# Child`. Addressing `Child`
/// with `update_note_section` replaces the deep range and takes the subsection with it —
/// that is its long-standing behaviour and this test keeps it from changing by accident.
/// Addressing the same heading with `edit_note` stops at the next heading of any level, so
/// the subsection survives.
#[tokio::test]
async fn a_nested_subsection_survives_edit_note_but_not_update_note_section() {
    let section_fixture = Fixture::new("nested-section-old");
    let section_state = section_fixture.state().await;
    tool_call(
        &section_state,
        "update_note_section",
        json!({
            "path": "Folder/Child.md",
            "heading": "Child",
            "level": 1,
            "content": "Rewritten child body.",
        }),
    )
    .await;
    let after_section = std::fs::read_to_string(section_fixture.vault_path.join("Folder/Child.md"))
        .expect("read child note");
    assert!(
        after_section.contains("Rewritten child body."),
        "{after_section}"
    );
    assert!(
        !after_section.contains("### Child Section"),
        "update_note_section replaces the whole subtree, and that is the behaviour being \
         preserved for the shipped skills that call it: {after_section}"
    );

    let edit_fixture = Fixture::new("nested-section-new");
    let edit_state = edit_fixture.state().await;
    tool_call(
        &edit_state,
        "edit_note",
        json!({
            "path": "Folder/Child.md",
            "edits": [{"heading": "Child", "content": "Rewritten child body."}],
        }),
    )
    .await;
    let after_edit = std::fs::read_to_string(edit_fixture.vault_path.join("Folder/Child.md"))
        .expect("read child note");
    assert!(after_edit.contains("Rewritten child body."), "{after_edit}");
    assert!(
        after_edit.contains("### Child Section"),
        "edit_note stops at the next heading of any level, so the nested subsection \
         survives: {after_edit}"
    );
    assert!(after_edit.contains("Nested child content."), "{after_edit}");
}

#[tokio::test]
async fn request_vault_upload_is_frozen() {
    let fixture = Fixture::new("upload");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    // Under stdio there is no HTTP listener, so no upload base is attached and
    // the tool must refuse instead of minting an unusable capability.
    let no_transport = tool_call(
        &state,
        "request_vault_upload",
        json!({"path": "Artifacts/upload.png"}),
    )
    .await;
    assert_golden(
        "error_request_vault_upload_no_http_transport",
        &normalize(&no_transport, &scrub),
    );

    let http_state = state
        .clone()
        .with_upload_base("http://127.0.0.1:7777".to_string());
    let issued = tool_call(
        &http_state,
        "request_vault_upload",
        json!({"path": "Artifacts/upload.png", "mimeType": "image/png"}),
    )
    .await;
    assert_golden(
        "tool_request_vault_upload_issued",
        &normalize(&issued, &scrub),
    );

    // The token is a real capability: it must be unguessable, i.e. a second
    // mint yields a different URL. (The snapshot masks it.)
    let reissued = tool_call(
        &http_state,
        "request_vault_upload",
        json!({"path": "Artifacts/upload.png", "mimeType": "image/png"}),
    )
    .await;
    assert_ne!(
        issued["result"]["structuredContent"]["uploadUrl"],
        reissued["result"]["structuredContent"]["uploadUrl"],
        "each mint must issue a fresh token"
    );

    let traversal = tool_call(
        &http_state,
        "request_vault_upload",
        json!({"path": "../escape.png"}),
    )
    .await;
    assert_golden(
        "error_request_vault_upload_traversal",
        &normalize(&traversal, &scrub),
    );
}

// ---------------------------------------------------------------------------
// Error taxonomy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_taxonomy_is_frozen() {
    let fixture = Fixture::new("errors");
    let state = fixture.state().await;
    let scrub = fixture.scrub_prefixes();

    // Unknown *tool* falls out of `call_tool`'s catch-all, so it is a -32000
    // application error -- not the -32601 reserved for an unknown method.
    let unknown_tool = tool_call(&state, "no_such_tool", json!({})).await;
    assert_golden("error_unknown_tool", &normalize(&unknown_tool, &scrub));

    let unknown_method = request(&state, "no/such/method", json!({})).await;
    assert_golden("error_unknown_method", &normalize(&unknown_method, &scrub));

    let missing_tool_name = request(&state, "tools/call", json!({"arguments": {}})).await;
    assert_golden(
        "error_missing_tool_name",
        &normalize(&missing_tool_name, &scrub),
    );

    let missing_argument = tool_call(&state, "read_file", json!({})).await;
    assert_golden(
        "error_missing_required_argument",
        &normalize(&missing_argument, &scrub),
    );

    let traversal = tool_call(&state, "read_file", json!({"path": "../escape.md"})).await;
    assert_golden("error_path_traversal", &normalize(&traversal, &scrub));

    let missing_file = tool_call(&state, "read_file", json!({"path": "Nope.md"})).await;
    assert_golden("error_missing_file", &normalize(&missing_file, &scrub));

    let bad_extension = tool_call(
        &state,
        "upsert_note",
        json!({"path": "Folder/Note.txt", "content": "body"}),
    )
    .await;
    assert_golden(
        "error_upsert_note_requires_markdown",
        &normalize(&bad_extension, &scrub),
    );

    let unsupported_artifact = tool_call(&state, "read_artifact", json!({"path": "Root.md"})).await;
    assert_golden(
        "error_unsupported_artifact_type",
        &normalize(&unsupported_artifact, &scrub),
    );
}
