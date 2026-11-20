//! Forced contention through the actual HTTP MCP handler and real backends.
use super::*;
use deep_obsidian_backend::{
    AlgoliaCredentials, AlgoliaOptions, AlgoliaVaultBackend, BackendRequest, CouchDbVaultBackend,
    FilesystemVaultBackend, SidecarConfig, SidecarCredentials, SidecarMode, SidecarSupervisor,
    VaultBackend, VaultRouter,
};
use deep_obsidian_types::AutoReindexConfigInput;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;

#[path = "../tests/support/mutation_gate.rs"]
mod mutation_gate;
use mutation_gate::MutationGate;

static NEXT: AtomicU64 = AtomicU64::new(0);
const ORIGINAL: &str = "# Doc\n\nbody\n";

struct Fixture {
    root: PathBuf,
    state: AppState,
    gate: Arc<MutationGate>,
    couch: Option<Child>,
    supervisor: Option<Arc<SidecarSupervisor>>,
    algolia: Option<tokio::task::JoinHandle<()>>,
}

impl Fixture {
    async fn new(kind: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "dob-wire-locks-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("vault")).unwrap();
        let config = normalize_service_config(ServiceConfigInput {
            vault_path: Some(root.join("vault")),
            index_dir: Some(root.join("index")),
            auto_reindex: Some(AutoReindexConfigInput {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        let backends = MountBackends::build(&config);
        let runtimes = MountRuntimes::new(&config, &backends);
        let mut state = AppState::with_backends(config, runtimes, &backends);
        let (mut couch, mut supervisor, mut algolia) = (None, None, None);
        let backend: Arc<dyn VaultBackend> = match kind {
            "filesystem" => Arc::new(FilesystemVaultBackend::new(root.join("vault"))),
            "algolia" => {
                let (url, server) = deep_obsidian_algolia::mock::spawn_mock().await;
                algolia = Some(server);
                Arc::new(
                    AlgoliaVaultBackend::connect(
                        AlgoliaCredentials {
                            app_id: "LOCKTEST".into(),
                            index_name: "notes".into(),
                            api_key: SecretString::new("test-key".into()),
                            base_url: Some(url),
                        },
                        AlgoliaOptions {
                            writable: true,
                            ..Default::default()
                        },
                        &root.join("algolia-cache"),
                    )
                    .unwrap(),
                )
            }
            "couchdb" => {
                let dir =
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../sidecar/livesync-sidecar");
                assert!(
                    dir.join("dist/sidecar.mjs").is_file(),
                    "build the sidecar before running the remote contention tests"
                );
                let mut child = Command::new("node")
                    .current_dir(&dir)
                    .args([
                        "test/mock-couch-server.mjs",
                        "--vault",
                        "small",
                        "--writable",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap();
                let mut line = String::new();
                BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let handshake: Value = serde_json::from_str(&line).unwrap();
                let config = SidecarConfig::resolve(
                    Some(&dir.join("dist/sidecar.mjs")),
                    SidecarCredentials {
                        url: handshake["url"].as_str().unwrap().into(),
                        database: handshake["database"].as_str().unwrap().into(),
                        username: "vaultuser".into(),
                        password: SecretString::new("s3cr3t-password-value".into()),
                        e2ee_passphrase: None,
                        e2ee_obfuscate_passphrase: None,
                    },
                    SidecarMode::ReadWrite,
                    None,
                    Some(Duration::from_secs(10)),
                )
                .unwrap();
                let sidecar = SidecarSupervisor::new(config);
                let backend = Arc::new(CouchDbVaultBackend::from_supervisor(sidecar.clone()));
                supervisor = Some(sidecar);
                couch = Some(child);
                backend
            }
            _ => unreachable!(),
        };
        backend
            .execute(BackendRequest::write_text("Doc.md", ORIGINAL))
            .await
            .unwrap();
        let gate = MutationGate::new(backend);
        // Exercise the same mutation dispatcher with explicitly constructed real
        // storage, without requiring test credentials in the operator's keyring.
        state.router = Arc::new(VaultRouter::single("vault", gate.clone()));
        state.backend = gate.clone();
        Self {
            root,
            state,
            gate,
            couch,
            supervisor,
            algolia,
        }
    }

    async fn close(&self) {
        if let Some(supervisor) = &self.supervisor {
            supervisor.shutdown().await;
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.couch {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(server) = &self.algolia {
            server.abort();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone)]
struct WireState {
    state: AppState,
    second: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

async fn dispatch(
    State(wire): State<WireState>,
    Json(request): Json<JsonRpcRequest>,
) -> axum::response::Response {
    // On the current-thread test runtime the handler runs until its first await
    // before the notified test resumes. Thus the read counters below prove where
    // the second HTTP request actually parks, without sleeps or scheduler guesses.
    if request.id == Some(json!(2)) {
        if let Some(tx) = wire.second.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
    mcp_handler(State(wire.state), None, Json(request))
        .await
        .into_response()
}

struct HttpServer {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl HttpServer {
    async fn new(state: AppState) -> (Self, oneshot::Receiver<()>) {
        let (second, arrived) = oneshot::channel();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/mcp", post(dispatch))
            .with_state(WireState {
                state,
                second: Arc::new(Mutex::new(Some(second))),
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (Self { url, task }, arrived)
    }
}
impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn http_tool(url: String, id: u64, name: &str, arguments: Value) -> Value {
    reqwest::Client::new().post(url).json(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
}

async fn tool(state: &AppState, name: &str, arguments: Value) -> Value {
    let request = serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})).unwrap();
    match handle_request(state.clone(), request).await {
        Ok(Some(value)) => value,
        Err(error) => serde_json::to_value(error).unwrap(),
        other => panic!("unexpected response: {other:?}"),
    }
}
fn success(value: &Value) {
    assert!(value.get("result").is_some(), "{value}");
}

#[tokio::test]
async fn http_same_note_reads_wait_for_filesystem_couchdb_and_algolia_writes() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for kind in ["filesystem", "couchdb", "algolia"] {
            for guarded in [false, true] {
                let fixture = Fixture::new(kind).await;
                let (entered, release) = fixture.gate.arm("Doc.md");
                let (server, second) = HttpServer::new(fixture.state.clone()).await;
                let hash = deep_obsidian_core::content_hash(ORIGINAL.as_bytes());
                let mut a_args = json!({"path":"Doc.md","edits":[{"old":"# Doc","new":"# First"}]});
                let mut b_args = json!({"path":"Doc.md","edits":[{"old":"body","new":"second"}]});
                if guarded {
                    a_args["expectedHash"] = json!(hash);
                    b_args["expectedHash"] = json!(hash);
                }
                let a = tokio::spawn(http_tool(server.url.clone(), 1, "edit_note", a_args));
                entered.await.unwrap();
                assert_eq!(fixture.gate.reads("Doc.md"), 1);
                let b = tokio::spawn(http_tool(server.url.clone(), 2, "edit_note", b_args));
                second.await.unwrap();
                assert_eq!(
                    fixture.gate.reads("Doc.md"),
                    1,
                    "{kind}: B must wait before its read"
                );
                release.send(()).unwrap();
                success(&a.await.unwrap());
                let b = b.await.unwrap();
                if guarded {
                    let expected = deep_obsidian_core::content_hash(b"# First\n\nbody\n");
                    assert!(
                        b["error"]["message"].as_str().unwrap().contains(&expected),
                        "{b}"
                    );
                } else {
                    success(&b);
                }
                let text = fixture
                    .gate
                    .inner
                    .execute(BackendRequest::read_text("Doc.md"))
                    .await
                    .unwrap()
                    .into_text()
                    .unwrap();
                assert!(text.contains("# First"));
                assert_eq!(text.contains("second"), !guarded);
                assert_eq!(fixture.state.mutation_locks.live_keys(), 0);
                drop(server);
                fixture.close().await;
            }
        }
    })
    .await
    .expect("HTTP contention scenarios must terminate");
}

#[tokio::test]
async fn concurrent_http_creates_and_session_writes_rederive_from_the_winner() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for kind in ["filesystem", "couchdb", "algolia"] {
            let fixture = Fixture::new(kind).await;
            let (entered, release) = fixture.gate.arm("New.md");
            let (server, second) = HttpServer::new(fixture.state.clone()).await;
            let a = tokio::spawn(http_tool(server.url.clone(), 1, "upsert_note", json!({"path":"New.md","content":"# New\n\n## Manual Notes\n\nkeep this\n"})));
            entered.await.unwrap();
            let b = tokio::spawn(http_tool(server.url.clone(), 2, "upsert_session_note", json!({"path":"New.md","content":"# Session\n\nupdated\n","preserveManualNotes":true})));
            second.await.unwrap();
            assert_eq!(fixture.gate.reads("New.md"), 1, "{kind}: the second creator must not read absence");
            release.send(()).unwrap();
            let a = a.await.unwrap(); let b = b.await.unwrap(); success(&a); success(&b);
            assert_eq!(a["result"]["structuredContent"]["created"], true);
            assert_eq!(b["result"]["structuredContent"]["created"], false);
            let text = fixture.gate.inner.execute(BackendRequest::read_text("New.md")).await.unwrap().into_text().unwrap();
            assert!(text.contains("updated") && text.contains("keep this"), "{kind}: {text}");
            drop(server); fixture.close().await;
        }
    }).await.expect("concurrent create scenarios must terminate");
}

#[tokio::test]
async fn remote_delete_waits_for_an_edit_and_removes_its_completed_result() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    for kind in ["couchdb", "algolia"] {
        let fixture = Fixture::new(kind).await;
        let (entered, release) = fixture.gate.arm("Doc.md");
        let state = fixture.state.clone();
        let a = tokio::spawn(async move {
            tool(
                &state,
                "edit_note",
                json!({"path":"Doc.md","edits":[{"old":"body","new":"updated"}]}),
            )
            .await
        });
        entered.await.unwrap();
        let mut delete = Box::pin(tool(
            &fixture.state,
            "delete_note",
            json!({"path":"Doc.md"}),
        ));
        assert!(matches!(
            delete
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        assert_eq!(fixture.gate.mutations("Doc.md"), ["write"]);
        release.send(()).unwrap();
        success(&a.await.unwrap());
        success(&delete.await);
        let notes = fixture
            .gate
            .inner
            .execute(BackendRequest::walk_markdown())
            .await
            .unwrap()
            .into_markdown_files()
            .unwrap();
        assert!(!notes.iter().any(|note| note == "Doc.md"));
        fixture.close().await;
    }
}

#[tokio::test]
async fn cancelling_remote_handlers_keeps_the_write_locked_until_completion() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    for kind in ["couchdb", "algolia"] {
        let fixture = Fixture::new(kind).await;
        let (entered, release) = fixture.gate.arm("Doc.md");
        let state = fixture.state.clone();
        let request = serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"edit_note","arguments":{"path":"Doc.md","edits":[{"old":"# Doc","new":"# First"}]}
        }})).unwrap();
        let a = tokio::spawn(async move {
            mcp_handler(State(state), None, Json(request))
                .await
                .into_response()
        });
        entered.await.unwrap();
        a.abort();
        let _ = a.await;
        let mut b = Box::pin(tool(
            &fixture.state,
            "edit_note",
            json!({"path":"Doc.md","edits":[{"old":"body","new":"second"}]}),
        ));
        assert!(matches!(
            b.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        assert_eq!(
            fixture.gate.reads("Doc.md"),
            1,
            "{kind}: cancelling the handler must not unlock its detached write"
        );
        release.send(()).unwrap();
        success(&b.await);
        let text = fixture
            .gate
            .inner
            .execute(BackendRequest::read_text("Doc.md"))
            .await
            .unwrap()
            .into_text()
            .unwrap();
        assert!(
            text.contains("# First") && text.contains("second"),
            "{kind}: {text}"
        );
        fixture.close().await;
    }
}

#[tokio::test]
async fn an_edit_waits_for_a_rename_backlink_repair_and_preserves_both_changes() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let fixture = Fixture::new("filesystem").await;
    fixture
        .gate
        .inner
        .execute(BackendRequest::write_text("Target.md", "# Target\n"))
        .await
        .unwrap();
    fixture
        .gate
        .inner
        .execute(BackendRequest::write_text(
            "Doc.md",
            "# Doc\n\n[[Target]]\n",
        ))
        .await
        .unwrap();
    let (entered, release) = fixture.gate.arm("Doc.md");
    let state = fixture.state.clone();
    let rename = tokio::spawn(async move {
        tool(
            &state,
            "rename_note",
            json!({"from":"Target.md","to":"Moved.md"}),
        )
        .await
    });
    entered.await.unwrap();
    let reads = fixture.gate.reads("Doc.md");
    let mut edit = Box::pin(tool(
        &fixture.state,
        "edit_note",
        json!({"path":"Doc.md","edits":[{"old":"# Doc","new":"# Edited"}]}),
    ));
    assert!(matches!(
        edit.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    assert_eq!(fixture.gate.reads("Doc.md"), reads);
    release.send(()).unwrap();
    success(&rename.await.unwrap());
    success(&edit.await);
    let text = fixture
        .gate
        .inner
        .execute(BackendRequest::read_text("Doc.md"))
        .await
        .unwrap()
        .into_text()
        .unwrap();
    assert!(
        text.contains("# Edited") && text.contains("[[Moved]]"),
        "{text}"
    );
}

#[tokio::test]
async fn a_queued_edit_across_a_live_reload_uses_the_inflight_writers_registry() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let mut fixture = Fixture::new("filesystem").await;
    let path = fixture.root.join("config.json");
    Arc::make_mut(&mut fixture.state.config).config_file_path = Some(path.clone());
    let mut saved = serde_json::to_value(deep_obsidian_config::to_persisted_config(
        &fixture.state.config,
    ))
    .unwrap();
    let added = fixture.root.join("added");
    std::fs::create_dir_all(&added).unwrap();
    saved["experimental"] = json!({"multiVault":true});
    saved["vaultPath"] = Value::Null;
    saved["mounts"] = json!([
        {"id":"vault","mountAt":"","backend":{"kind":"filesystem","vaultPath":fixture.root.join("vault")}},
        {"id":"added","mountAt":"Added","backend":{"kind":"filesystem","vaultPath":added}}
    ]);
    std::fs::write(&path, saved.to_string()).unwrap();
    let (entered, release) = fixture.gate.arm("Doc.md");
    let state = fixture.state.clone();
    let a = tokio::spawn(async move {
        tool(
            &state,
            "edit_note",
            json!({"path":"Doc.md","edits":[{"old":"# Doc","new":"# First"}]}),
        )
        .await
    });
    entered.await.unwrap();
    crate::reload::apply(&fixture.state).await.unwrap();
    let current = fixture.state.current();
    assert!(current.router.resolve("Added/New.md").is_ok());
    let mut b = Box::pin(tool(
        &fixture.state,
        "edit_note",
        json!({"path":"Doc.md","edits":[{"old":"body","new":"second"}]}),
    ));
    assert!(matches!(
        b.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    assert_eq!(fixture.gate.reads("Doc.md"), 1);
    release.send(()).unwrap();
    success(&a.await.unwrap());
    success(&b.await);
    let text = std::fs::read_to_string(fixture.root.join("vault/Doc.md")).unwrap();
    assert!(
        text.contains("# First") && text.contains("second"),
        "{text}"
    );
    current.stop_mount_background();
}
