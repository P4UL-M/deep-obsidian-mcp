//! Deterministic concurrency regressions for the per-note mutation locks.
//!
//! Every wait is coordinated through channels or barriers, never through
//! sleeps: a test that depends on timing proves nothing about a serializer.
//! The scenarios pin the acceptance criteria from the implementation plan
//! (`_Agent/Sessions/Deep Obsidian/2026-11-20 - pr80-review.md`,
//! "Implementation plan: in-process per-note mutation locks"): two requests
//! handled by this server cannot both derive mutations from the same pre-write
//! note state; a stale `expectedHash` is then rejected; an unguarded patch
//! applies to the latest content; different notes stay concurrent.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use deep_obsidian_server::mcp::{handle_request, AppState};
use deep_obsidian_server::mounts::MountBackends;
use deep_obsidian_server::protocol::JsonRpcRequest;
use deep_obsidian_server::runtime::MountRuntimes;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Fixture: a temp vault whose seed is this file's own, independent of the
// contract fixtures so scenarios here never drift with golden updates.
// ---------------------------------------------------------------------------

struct Fixture {
    vault_path: PathBuf,
    index_dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "deep-obsidian-locks-{name}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let vault_path = root.join("vault");
        let index_dir = root.join("index");
        std::fs::create_dir_all(&vault_path).expect("create vault dir");
        std::fs::create_dir_all(&index_dir).expect("create index dir");
        let fixture = Self {
            vault_path,
            index_dir,
        };
        fixture.seed();
        fixture
    }

    fn seed(&self) {
        for (path, content) in [
            ("Doc.md", "# Doc\n\nbody\n"),
            ("A.md", "# A\n\nbody-a\n"),
            ("B.md", "# B\n\nbody-b\n"),
        ] {
            std::fs::write(self.vault_path.join(path), content).expect("seed note");
        }
    }

    fn config(&self) -> deep_obsidian_types::ResolvedServiceConfig {
        deep_obsidian_types::ResolvedServiceConfig {
            freshness: Default::default(),
            federated_rerank: false,
            vault_path: Some(self.vault_path.clone()),
            index_dir: self.index_dir.clone(),
            mounts: Vec::new(),
            experimental: Default::default(),
            transport: deep_obsidian_types::TransportMode::Http,
            stdio_mode: deep_obsidian_types::StdioMode::Auto,
            http: deep_obsidian_types::HttpConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                mcp_path: "/mcp".to_string(),
                health_path: "/healthz".to_string(),
            },
            auto_reindex: deep_obsidian_types::AutoReindexConfig {
                enabled: false,
                debounce_ms: 0,
                interval_ms: 0,
            },
            embedding: deep_obsidian_types::EmbeddingConfig::default(),
            artifact_embedding: deep_obsidian_types::EmbeddingConfig::default(),
            auth: deep_obsidian_types::AuthConfig::default(),
            config_file_path: None,
        }
    }

    async fn state(&self) -> AppState {
        let config = self.config();
        let backends = MountBackends::build(&config);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("bootstrap runtime");
        AppState::with_backends(config, runtimes, &backends)
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.vault_path.join(path)).expect("read note")
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC driver, identical to the contract suite's
// ---------------------------------------------------------------------------

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

fn hash_of(content: &str) -> String {
    deep_obsidian_core::content_hash(content.as_bytes())
}

fn live_locks(state: &AppState) -> usize {
    state.mutation_locks.live_keys()
}

// Drain helper: bounded, poll-based, no sleeps. Returns the last observed
// value within the poll budget.
async fn drain(mut rx: tokio::sync::oneshot::Receiver<bool>) -> bool {
    for _ in 0..200 {
        tokio::task::yield_now().await;
        if let Ok(value) = rx.try_recv() {
            return value;
        }
    }
    rx.try_recv().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Two same-hash edits to one note: exactly one succeeds, and the loser's
/// conflict names the WINNER's new hash — proof its authoritative read waited
/// for the winner's write instead of racing it.
#[tokio::test]
async fn simultaneous_same_hash_edits_produce_one_success_and_one_conflict() {
    let fixture = Fixture::new("same-hash");
    let state = Arc::new(fixture.state().await);

    let a = tokio::spawn({
        let state = state.clone();
        async move {
            tool_call(
                &state,
                "edit_note",
                json!({
                    "path": "Doc.md",
                    "expectedHash": hash_of("# Doc\n\nbody\n"),
                    "edits": [{"old": "body", "new": "patch-a"}]
                }),
            )
            .await
        }
    });
    let b = tokio::spawn({
        let state = state.clone();
        async move {
            tool_call(
                &state,
                "edit_note",
                json!({
                    "path": "Doc.md",
                    "expectedHash": hash_of("# Doc\n\nbody\n"),
                    "edits": [{"old": "body", "new": "patch-b"}]
                }),
            )
            .await
        }
    });
    let (a, b) = tokio::join!(a, b);
    let (a, b) = (a.unwrap(), b.unwrap());
    let a_ok = a["result"].is_object();
    let b_ok = b["result"].is_object();
    assert!(
        a_ok ^ b_ok,
        "exactly one of two same-hash edits may succeed: a={a}, b={b}"
    );
    let loser = if a_ok { &b } else { &a };
    let message = loser["error"]["message"].as_str().expect("an error");
    let winner_patch = if a_ok { "patch-a" } else { "patch-b" };
    let winner_content = format!("# Doc\n\n{winner_patch}\n");
    assert!(
        message.contains(&hash_of(&winner_content)),
        "the conflict must name the post-winner hash: {message}"
    );
    assert_eq!(fixture.read("Doc.md"), winner_content);
    assert_eq!(live_locks(&state), 0);
}

/// Two unguarded (no expectedHash) edits to one note, touching DIFFERENT
/// anchors, both succeed and BOTH survive: the second authoritative read
/// waited for the first's write and derived from its output. Without the
/// lock, both could read the original content and the second write would
/// silently erase the first.
#[tokio::test]
async fn unguarded_patches_apply_to_the_latest_content() {
    let fixture = Fixture::new("unguarded");
    let state = Arc::new(fixture.state().await);

    let a = tokio::spawn({
        let state = state.clone();
        async move {
            tool_call(
                &state,
                "edit_note",
                json!({
                    "path": "Doc.md",
                    "edits": [{"old": "# Doc", "new": "# Doc v2"}]
                }),
            )
            .await
        }
    });
    let b = tokio::spawn({
        let state = state.clone();
        async move {
            tool_call(
                &state,
                "edit_note",
                json!({
                    "path": "Doc.md",
                    "edits": [{"old": "body", "new": "first"}]
                }),
            )
            .await
        }
    });
    let (a, b) = tokio::join!(a, b);
    assert!(
        a.unwrap()["result"].is_object(),
        "a: both anchors are valid in any order"
    );
    assert!(
        b.unwrap()["result"].is_object(),
        "b: the second read waited and re-derived from the first's result"
    );

    let final_content = fixture.read("Doc.md");
    assert_eq!(
        final_content, "# Doc v2\n\nfirst\n",
        "both edits must survive, in whichever order the lock ran them: {final_content:?}"
    );
    assert_eq!(live_locks(&state), 0);
}

/// Mutations to DIFFERENT notes do not serialize: while A's lock is held, B's
/// mutation completes. (A held guard stands in for a parked mid-flight
/// mutation — same critical section, deterministic timing.)
#[tokio::test]
async fn unrelated_notes_stay_concurrent() {
    let fixture = Fixture::new("independent");
    let state = Arc::new(fixture.state().await);

    let guard = state
        .mutation_locks
        .acquire(deep_obsidian_server::locks::note_lock_key(&state.router, "A.md").unwrap())
        .await;

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<bool>();
    let task_state = state.clone();
    tokio::spawn(async move {
        let response = tool_call(
            &task_state,
            "edit_note",
            json!({
                "path": "B.md",
                "edits": [{"old": "body-b", "new": "patched"}]
            }),
        )
        .await;
        let _ = done_tx.send(response["result"].is_object());
    });

    let b_completed = drain(done_rx).await;
    assert!(
        b_completed,
        "a mutation on B must not wait for A's held lock"
    );
    drop(guard);
    assert_eq!(live_locks(&state), 0, "all entries reclaimed after release");
}

/// Two opposite-direction renames (A→B and B→A) each need both keys. The
/// deterministic key order makes one win both and complete; the other then
/// refuses on existence. The absolute proof of no deadlock: both calls
/// TERMINATE within the timeout.
#[tokio::test]
async fn opposite_direction_renames_do_not_deadlock() {
    let fixture = Fixture::new("opposite-rename");
    let state = Arc::new(fixture.state().await);

    let first = tokio::spawn({
        let state = state.clone();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tool_call(&state, "rename_note", json!({"from": "A.md", "to": "B.md"})),
            )
            .await
        }
    });
    let second = tokio::spawn({
        let state = state.clone();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tool_call(&state, "rename_note", json!({"from": "B.md", "to": "A.md"})),
            )
            .await
        }
    });
    let (first, second) = tokio::join!(first, second);
    let (first, second) = (first.unwrap().unwrap(), second.unwrap().unwrap());
    let first_ok = first["result"].is_object();
    let second_ok = second["result"].is_object();
    assert!(
        !(first_ok && second_ok),
        "both opposite renames cannot succeed: {first}, {second}"
    );
    assert_eq!(live_locks(&state), 0, "no lock leaks after both renames");
}

/// Rename releases its two locks before the repair pass: the moved note's own
/// self-link repair takes the destination's lock, which would deadlock if the
/// rename still held it. Completing at all is the proof; the empty registry is
/// the leak check.
#[tokio::test]
async fn rename_releases_its_locks_before_repairing_links() {
    let fixture = Fixture::new("rename-repair");
    std::fs::write(fixture.vault_path.join("Doc.md"), "# Doc\n\n[[Doc]]\n").unwrap();
    let state = Arc::new(fixture.state().await);

    let response = tool_call(
        &state,
        "rename_note",
        json!({"from": "Doc.md", "to": "Moved.md"}),
    )
    .await;
    assert!(
        response["result"].is_object(),
        "rename must succeed: {response}"
    );
    assert_eq!(live_locks(&state), 0, "no lock leaks after the rename");
    assert!(fixture.read("Moved.md").contains("[[Moved]]"));
}

#[tokio::test]
async fn rename_and_edit_aliases_wait_for_the_same_held_note_lock() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    for rename in [false, true] {
        let fixture = Fixture::new(if rename { "rename-alias" } else { "edit-alias" });
        let state = fixture.state().await;
        let held_path = if rename { "Moved.md" } else { "Doc.md" };
        let guard = state
            .mutation_locks
            .acquire(deep_obsidian_server::locks::note_lock_key(&state.router, held_path).unwrap())
            .await;
        let (name, args) = if rename {
            (
                "rename_note",
                json!({"from":"Doc.md","to":"./Moved.md","rewriteLinks":false}),
            )
        } else {
            (
                "edit_note",
                json!({"path":"Folder/../Doc.md","edits":[{"old":"body","new":"patched"}]}),
            )
        };
        let mut operation = Box::pin(tool_call(&state, name, args));
        assert!(matches!(
            operation
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        assert_eq!(
            live_locks(&state),
            if rename { 2 } else { 1 },
            "the alias must wait on the existing key"
        );
        assert_eq!(fixture.read("Doc.md"), "# Doc\n\nbody\n");
        drop(guard);
        let response = operation.await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(live_locks(&state), 0);
    }
}

/// The lock key folds path spellings the backend treats as the same note:
/// `./Doc.md` and `Doc.md` serialize against one mutex.
#[tokio::test]
async fn path_spellings_of_one_note_share_a_lock() {
    let fixture = Fixture::new("spelling");
    let state = Arc::new(fixture.state().await);

    let direct = deep_obsidian_server::locks::note_lock_key(&state.router, "Doc.md").unwrap();
    let dotted = deep_obsidian_server::locks::note_lock_key(&state.router, "./Doc.md").unwrap();
    assert_eq!(direct, dotted, "one storage note, one key");
}
