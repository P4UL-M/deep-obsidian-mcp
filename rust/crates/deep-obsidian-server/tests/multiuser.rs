//! Requests from two independent users plus an ungranted user, through the public
//! JSON-RPC dispatcher. Private content is deliberately present in every index.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use deep_obsidian_config::normalize_service_config;
use deep_obsidian_control::{ControlStore, Grant, InitialUser, Principal, Role};
use deep_obsidian_server::mcp::{handle_request, AppState};
use deep_obsidian_server::mounts::MountBackends;
use deep_obsidian_server::protocol::JsonRpcRequest;
use deep_obsidian_server::runtime::MountRuntimes;
use deep_obsidian_types::{
    AutoReindexConfigInput, ExperimentalConfig, MountBackendConfig, MountConfig, ServiceConfigInput,
};
use secrecy::ExposeSecret;
use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    state: AppState,
    paul: Principal,
    laura: Principal,
    outsider: Principal,
}

impl Fixture {
    async fn new(shared_role: Role) -> Self {
        let base = std::env::temp_dir().join(format!(
            "deep-obsidian-multiuser-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for dir in ["paul", "laura", "shared"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        fs::write(
            base.join("paul/PaulSecret.md"),
            "# Paul\nprivatepaulword.\n",
        )
        .unwrap();
        fs::write(
            base.join("laura/LauraSecret.md"),
            "# Laura\nprivatelauraword.\n",
        )
        .unwrap();
        fs::write(
            base.join("shared/Welcome.md"),
            "# Shared\nsharedword.\nSee [[Other]].\n",
        )
        .unwrap();
        fs::write(
            base.join("shared/Other.md"),
            "# Other\nsharedword collaboration.\n",
        )
        .unwrap();
        fs::write(base.join("laura/image.png"), b"private image bytes").unwrap();
        // A physical root file under a logical mount owned by someone else must
        // not become reachable when their mount is removed from the user view.
        fs::create_dir_all(base.join("paul/Laura")).unwrap();
        fs::write(
            base.join("paul/Laura/Shadow.md"),
            "# Shadow\nshadowsecretword.\n",
        )
        .unwrap();
        let mounts: Vec<_> = [
            ("paul", "", "paul"),
            ("laura", "Laura", "laura"),
            ("shared", "Shared", "shared"),
        ]
        .into_iter()
        .map(|(id, prefix, directory)| MountConfig {
            id: id.into(),
            mount_at: prefix.into(),
            recall_weight: None,
            unknown: Default::default(),
            backend: MountBackendConfig::Filesystem {
                vault_path: base.join(directory),
                index_dir: None,
            },
        })
        .collect();
        let config = normalize_service_config(ServiceConfigInput {
            mounts: Some(mounts),
            index_dir: Some(base.join("index")),
            experimental: Some(ExperimentalConfig {
                multi_vault: true,
                ..Default::default()
            }),
            auto_reindex: Some(AutoReindexConfigInput {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        let mut config = config;
        let config_path = base.join("config.json");
        std::fs::write(&config_path, "{\"multiuser\":{\"enabled\":true}}").unwrap();
        config.config_file_path = Some(config_path);
        let backends = MountBackends::build(&config);
        let (runtimes, _) = MountRuntimes::bootstrap(&config, &backends).await.unwrap();
        let mut state = AppState::with_backends(config, runtimes, &backends)
            .with_upload_base("http://127.0.0.1:4100".into());
        let store = ControlStore::open(base.join("control.sqlite")).unwrap();
        let generated = store
            .enable(
                &[
                    InitialUser {
                        name: "paul".into(),
                        default_mount: Some("paul".into()),
                        grants: vec![
                            Grant {
                                mount_id: "paul".into(),
                                role: Role::Owner,
                            },
                            Grant {
                                mount_id: "shared".into(),
                                role: Role::Owner,
                            },
                        ],
                    },
                    InitialUser {
                        name: "laura".into(),
                        default_mount: Some("laura".into()),
                        grants: vec![
                            Grant {
                                mount_id: "laura".into(),
                                role: Role::Owner,
                            },
                            Grant {
                                mount_id: "shared".into(),
                                role: shared_role,
                            },
                        ],
                    },
                ],
                &["paul".into(), "laura".into(), "shared".into()],
            )
            .unwrap();
        let paul = store
            .authenticate("paul", generated[0].password.expose_secret())
            .unwrap()
            .unwrap();
        let laura = store
            .authenticate("laura", generated[1].password.expose_secret())
            .unwrap()
            .unwrap();
        let outsider_user = store.add_user("outsider").unwrap();
        let outsider = store
            .authenticate("outsider", outsider_user.password.expose_secret())
            .unwrap()
            .unwrap();
        state.control = Some(store);
        Self {
            base,
            state,
            paul,
            laura,
            outsider,
        }
    }
    fn as_paul(&self) -> AppState {
        self.state.for_principal(self.paul.clone()).unwrap()
    }
    fn as_laura(&self) -> AppState {
        self.state.for_principal(self.laura.clone()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

async fn request(state: &AppState, method: &str, params: Value) -> Value {
    let req: JsonRpcRequest =
        serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .unwrap();
    match handle_request(state.clone(), req).await {
        Ok(Some(response)) => response,
        Ok(None) => Value::Null,
        Err(error) => serde_json::to_value(error).unwrap(),
    }
}
async fn tool(state: &AppState, name: &str, arguments: Value) -> Value {
    request(
        state,
        "tools/call",
        json!({"name":name,"arguments":arguments}),
    )
    .await
}
fn structured(response: &Value) -> &Value {
    response
        .get("result")
        .and_then(|r| r.get("structuredContent"))
        .unwrap_or_else(|| panic!("expected tool success: {response}"))
}
fn denied(response: &Value) {
    assert!(
        response.get("error").is_some()
            || response.pointer("/result/isError") == Some(&json!(true)),
        "expected denied request: {response}"
    );
}

#[tokio::test]
async fn private_reads_and_resource_reads_are_scoped_to_the_caller() {
    let f = Fixture::new(Role::Reader).await;
    let paul = f.as_paul();
    let laura = f.as_laura();
    assert!(
        structured(&tool(&paul, "read_file", json!({"path":"PaulSecret.md"})).await)["text"]
            .as_str()
            .unwrap()
            .contains("privatepaulword")
    );
    assert!(
        structured(&tool(&laura, "read_file", json!({"path":"Laura/LauraSecret.md"})).await)
            ["text"]
            .as_str()
            .unwrap()
            .contains("privatelauraword")
    );
    denied(&tool(&paul, "read_file", json!({"path":"Laura/LauraSecret.md"})).await);
    denied(&tool(&laura, "read_file", json!({"path":"PaulSecret.md"})).await);
    denied(
        &request(
            &paul,
            "resources/read",
            json!({"uri":"obsidian://note?path=Laura%2FLauraSecret.md"}),
        )
        .await,
    );
    denied(&tool(&paul, "read_file", json!({"path":"Laura/Shadow.md"})).await);
}

#[tokio::test]
async fn shared_reader_can_read_but_cannot_mutate_or_upload() {
    let f = Fixture::new(Role::Reader).await;
    let laura = f.as_laura();
    assert!(
        structured(&tool(&laura, "read_file", json!({"path":"Shared/Welcome.md"})).await)["text"]
            .as_str()
            .unwrap()
            .contains("sharedword")
    );
    for (name, args) in [
        (
            "upsert_note",
            json!({"path":"Shared/Denied.md","content":"must not be written"}),
        ),
        (
            "edit_note",
            json!({"path":"Shared/Welcome.md","edits":[{"old":"sharedword","new":"changed"}]}),
        ),
        ("delete_note", json!({"path":"Shared/Welcome.md"})),
        ("request_vault_upload", json!({"path":"Shared/image.png"})),
        (
            "upsert_session_note",
            json!({"folder":"Shared","topic":"denied","content":"must not be written"}),
        ),
    ] {
        denied(&tool(&laura, name, args).await);
    }
    assert!(!f.base.join("shared/Denied.md").exists());
    assert!(fs::read_to_string(f.base.join("shared/Welcome.md"))
        .unwrap()
        .contains("sharedword"));
}

#[tokio::test]
async fn shared_editor_writes_use_only_the_selected_mount() {
    let f = Fixture::new(Role::Editor).await;
    let laura = f.as_laura();
    let result = tool(
        &laura,
        "upsert_note",
        json!({"path":"Shared/Created.md","content":"# Shared creation"}),
    )
    .await;
    structured(&result);
    assert_eq!(
        fs::read_to_string(f.base.join("shared/Created.md")).unwrap(),
        "# Shared creation\n"
    );
    assert!(!f.base.join("paul/Shared/Created.md").exists());
    denied(
        &tool(
            &laura,
            "upsert_note",
            json!({"path":"PaulSecret.md","content":"denied"}),
        )
        .await,
    );
    let default_write = tool(
        &laura,
        "upsert_note",
        json!({"mountId":"default","path":"Default.md","content":"Laura default"}),
    )
    .await;
    assert_eq!(structured(&default_write)["path"], "Laura/Default.md");
    assert!(fs::read_to_string(f.base.join("laura/Default.md"))
        .unwrap()
        .contains("Laura default"));
    assert!(!f.base.join("paul/Default.md").exists());
}

#[tokio::test]
async fn searches_listings_and_metadata_do_not_expose_private_mounts() {
    let f = Fixture::new(Role::Reader).await;
    let paul = f.as_paul();
    for (name, args) in [
        ("find_files", json!({"query":"Secret","limit":100})),
        (
            "hybrid_search",
            json!({"query":"privatelauraword shadowsecretword","limit":50}),
        ),
        ("list_children", json!({"path":""})),
        ("vault_info", json!({})),
    ] {
        let response = tool(&paul, name, args).await;
        let mut sanitized = response.clone();
        if let Some(data) = sanitized.pointer_mut("/result/structuredContent") {
            data.as_object_mut().map(|o| o.remove("query"));
        }
        if let Some(contents) = sanitized
            .pointer_mut("/result/content")
            .and_then(Value::as_array_mut)
        {
            contents.clear();
        }
        let text = sanitized.to_string();
        assert!(
            !text.contains("LauraSecret"),
            "{name} leaked private note: {response}"
        );
        assert!(
            !text.contains("Shadow.md"),
            "{name} leaked shadowed path: {response}"
        );
        assert!(
            !text.contains("privatelauraword"),
            "{name} leaked private content: {response}"
        );
        assert!(
            !text.contains("shadowsecretword"),
            "{name} leaked shadowed content: {response}"
        );
        if name == "vault_info" {
            let data = structured(&response);
            assert!(data["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["id"] != "laura"));
            assert!(!text.contains(&f.base.join("laura").to_string_lossy().to_string()));
        }
    }
    let resources = request(&paul, "resources/list", json!({})).await;
    assert!(!resources.to_string().contains("LauraSecret"));
}

#[tokio::test]
async fn ungranted_user_cannot_fall_back_to_the_root_vault() {
    let f = Fixture::new(Role::Reader).await;
    let outsider = f.state.for_principal(f.outsider.clone()).unwrap();
    for (name, args) in [
        ("read_file", json!({"path":"PaulSecret.md"})),
        ("read_file", json!({"path":"Laura/LauraSecret.md"})),
        ("upsert_note", json!({"path":"Oops.md","content":"denied"})),
    ] {
        denied(&tool(&outsider, name, args).await);
    }
    for (name, args) in [
        ("find_files", json!({"query":"Secret"})),
        ("hybrid_search", json!({"query":"privatepaulword"})),
        ("vault_info", json!({})),
        ("list_children", json!({"path":""})),
    ] {
        let response = tool(&outsider, name, args).await;
        assert!(
            !response.to_string().contains("PaulSecret"),
            "{name} leaked root: {response}"
        );
        assert!(
            !response
                .pointer("/result/structuredContent/matches")
                .unwrap_or(&Value::Null)
                .to_string()
                .contains("privatepaulword"),
            "{name} leaked root content: {response}"
        );
    }
}

#[tokio::test]
async fn graph_artifact_and_upload_entry_points_deny_hidden_mounts() {
    let f = Fixture::new(Role::Editor).await;
    let paul = f.as_paul();
    for (name, args) in [
        (
            "graph_traverse",
            json!({"path":"Laura/LauraSecret.md","depth":1}),
        ),
        ("related_notes", json!({"path":"Laura/LauraSecret.md"})),
        ("note_outline", json!({"path":"Laura/LauraSecret.md"})),
        (
            "read_artifact",
            json!({"path":"Laura/image.png","includeBase64":true}),
        ),
        ("request_vault_upload", json!({"path":"Laura/upload.png"})),
    ] {
        denied(&tool(&paul, name, args).await);
    }
}

#[tokio::test]
async fn rotation_and_revocation_apply_to_existing_scoped_states() {
    let f = Fixture::new(Role::Reader).await;
    let laura = f.as_laura();
    let store = f.state.control.as_ref().unwrap();
    structured(&tool(&laura, "read_file", json!({"path":"Shared/Welcome.md"})).await);
    store.revoke("laura", "shared").unwrap();
    denied(&tool(&laura, "read_file", json!({"path":"Shared/Welcome.md"})).await);
    store.rotate_password("laura").unwrap();
    denied(&tool(&laura, "read_file", json!({"path":"Laura/LauraSecret.md"})).await);
    assert!(f.state.for_principal(f.laura.clone()).is_err());
}

#[tokio::test]
async fn a_user_with_only_one_nonroot_mount_keeps_search_and_graph_prefixes() {
    let f = Fixture::new(Role::Reader).await;
    let store = f.state.control.as_ref().unwrap();
    store
        .grant(
            "outsider",
            "shared",
            Role::Reader,
            &["paul".into(), "laura".into(), "shared".into()],
        )
        .unwrap();
    let user = f.state.for_principal(f.outsider.clone()).unwrap();
    let search = tool(
        &user,
        "hybrid_search",
        json!({"query":"sharedword","limit":20}),
    )
    .await;
    let hits = structured(&search)["matches"].as_array().unwrap();
    assert!(
        !hits.is_empty(),
        "the only permitted non-root mount must be searchable: {search}"
    );
    assert!(
        hits.iter()
            .all(|hit| hit["path"].as_str().unwrap().starts_with("Shared/")),
        "search paths must retain the mount prefix: {search}"
    );
    let graph = tool(
        &user,
        "graph_traverse",
        json!({"path":"Shared/Welcome.md","depth":1}),
    )
    .await;
    let data = structured(&graph);
    let nodes = data["nodes"].as_array().unwrap();
    assert_eq!(
        nodes.len(),
        2,
        "the mount-relative graph lookup must resolve the requested note: {graph}"
    );
    assert!(nodes
        .iter()
        .all(|node| node["path"].as_str().unwrap().starts_with("Shared/")));
    let related = tool(&user, "related_notes", json!({"path":"Shared/Welcome.md"})).await;
    structured(&related);
    let resources = request(&user, "resources/list", json!({})).await;
    assert!(resources.to_string().contains("Shared%2FWelcome.md"));
    assert!(!resources.to_string().contains("PaulSecret"));
}

#[tokio::test]
async fn revoked_mount_disappears_from_metadata_on_an_existing_scoped_state() {
    let f = Fixture::new(Role::Reader).await;
    let laura = f.as_laura();
    let initial = tool(&laura, "vault_info", json!({})).await;
    assert!(structured(&initial)["mounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "shared"));
    f.state
        .control
        .as_ref()
        .unwrap()
        .revoke("laura", "shared")
        .unwrap();
    let result = tool(&laura, "vault_info", json!({})).await;
    let data = structured(&result);
    assert!(
        data["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != "shared"),
        "revocation must update metadata without recreating the scoped state: {result}"
    );
    assert_eq!(data["mountCount"], 1);
    let resources = request(&laura, "resources/list", json!({})).await;
    assert!(!resources.to_string().contains("Shared%2F"));
}

#[tokio::test]
async fn graph_edges_and_counts_exclude_notes_shadowed_by_a_denied_mount() {
    let f = Fixture::new(Role::Reader).await;
    fs::write(
        f.base.join("paul/PaulSecret.md"),
        "# Paul\nprivatepaulword.\n[[Laura/Shadow]]\n",
    )
    .unwrap();
    f.state
        .runtimes
        .for_mount("paul")
        .unwrap()
        .rebuild("shadowed-graph-test")
        .await
        .unwrap();
    let paul = f.as_paul();
    let response = tool(
        &paul,
        "graph_traverse",
        json!({"path":"PaulSecret.md","depth":2}),
    )
    .await;
    let data = structured(&response);
    assert_eq!(data["nodeCount"], data["nodes"].as_array().unwrap().len());
    assert_eq!(data["edgeCount"], data["edges"].as_array().unwrap().len());
    assert_eq!(data["nodeCount"], 1);
    assert_eq!(data["edgeCount"], 0);
    assert!(
        !data.to_string().contains("Shadow.md"),
        "shadowed paths must not remain in graph edges/counts: {response}"
    );
    assert!(!data.to_string().contains("shadowsecretword"));
}

#[tokio::test]
async fn dot_aliases_cannot_bypass_a_denied_shadowed_mount() {
    let f = Fixture::new(Role::Reader).await;
    let paul = f.as_paul();
    for path in [
        "./Laura/Shadow.md",
        "unrelated/../Laura/Shadow.md",
        "Shared/../Laura/Shadow.md",
    ] {
        denied(&tool(&paul, "read_file", json!({"path":path})).await);
        denied(
            &request(
                &paul,
                "resources/read",
                json!({"uri":format!("obsidian://note?path={}",urlencoding::encode(path))}),
            )
            .await,
        );
    }
}

#[tokio::test]
async fn multiuser_rejects_control_config_or_indexes_inside_a_vault() {
    let f = Fixture::new(Role::Reader).await;
    let mut config = (*f.state.config).clone();
    config.index_dir = f.base.join("paul/private-state");
    assert!(deep_obsidian_server::multiuser::validate_mounts(&config).is_err());
    let mut config = (*f.state.config).clone();
    config.config_file_path = Some(f.base.join("laura/config.json"));
    assert!(deep_obsidian_server::multiuser::validate_mounts(&config).is_err());
}

#[tokio::test]
async fn owners_manage_only_their_own_vault_and_transfer_before_leaving() {
    let f = Fixture::new(Role::Reader).await;
    let paul = f.as_paul();
    let laura = f.as_laura();
    denied(
        &tool(
            &laura,
            "set_vault_membership",
            json!({"mountId":"shared","user":"outsider","role":"editor"}),
        )
        .await,
    );
    denied(
        &tool(
            &paul,
            "set_vault_membership",
            json!({"mountId":"laura","user":"outsider","role":"owner"}),
        )
        .await,
    );
    denied(
        &tool(
            &paul,
            "set_vault_membership",
            json!({"mountId":"shared","user":"paul","role":"none"}),
        )
        .await,
    );
    let result = tool(
        &paul,
        "set_vault_membership",
        json!({"mountId":"shared","user":"outsider","role":"owner"}),
    )
    .await;
    assert_eq!(structured(&result)["role"], "owner");
    let result = tool(
        &paul,
        "set_vault_membership",
        json!({"mountId":"shared","user":"paul","role":"none"}),
    )
    .await;
    assert_eq!(structured(&result)["revoked"], true);
    denied(
        &tool(
            &paul,
            "set_vault_membership",
            json!({"mountId":"shared","user":"laura","role":"editor"}),
        )
        .await,
    );
    let outsider = f.state.for_principal(f.outsider.clone()).unwrap();
    assert!(
        structured(&tool(&outsider, "read_file", json!({"path":"Shared/Welcome.md"})).await)
            ["text"]
            .as_str()
            .unwrap()
            .contains("sharedword")
    );
}

#[tokio::test]
async fn folders_only_listing_does_not_disclose_a_denied_mount() {
    let f = Fixture::new(Role::Reader).await;
    let response = tool(
        &f.as_paul(),
        "list_children",
        json!({"path":"","foldersOnly":true}),
    )
    .await;
    let data = structured(&response);
    assert!(
        !data["folders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "Laura"),
        "{response}"
    );
    assert!(
        data["folders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "Shared"),
        "{response}"
    );
    assert_eq!(
        data["count"].as_u64().unwrap() as usize,
        data["folders"].as_array().unwrap().len()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn live_reload_preserves_sessions_and_updates_grants_without_restarting() {
    use deep_obsidian_server::control::{call_local, start};
    let f = Fixture::new(Role::Reader).await;
    let session = f.as_laura();
    let auth = f.state.auth.clone();
    let root = f.state.router.root().unwrap().backend.clone();
    let _control = start(&f.state).unwrap();
    let path = f.state.config.config_file_path.as_ref().unwrap();
    let private = f.base.join("new-private");
    fs::create_dir(&private).unwrap();
    fs::write(private.join("New.md"), "new-private-secret").unwrap();
    let mut saved =
        serde_json::to_value(deep_obsidian_config::to_persisted_config(&f.state.config)).unwrap();
    saved["multiuser"] = json!({"enabled":true});
    saved["mounts"].as_array_mut().unwrap().push(json!({
        "id":"new-private","mountAt":"NewPrivate","backend":{"kind":"filesystem","vaultPath":private}
    }));
    fs::write(path, saved.to_string()).unwrap();
    let result = call_local(&f.state.config, json!({"operation":"mounts.reload"}))
        .await
        .unwrap();
    assert_eq!(result["sessionsPreserved"], true);
    assert!(std::sync::Arc::ptr_eq(&auth, &f.state.current().auth));
    assert!(std::sync::Arc::ptr_eq(
        &root,
        &f.state.current().router.root().unwrap().backend
    ));
    denied(&tool(&session, "read_file", json!({"path":"NewPrivate/New.md"})).await);
    call_local(
        &f.state.config,
        json!({"operation":"users.grant","name":"laura","mount":"new-private","role":"owner"}),
    )
    .await
    .unwrap();
    assert!(
        structured(&tool(&session, "read_file", json!({"path":"NewPrivate/New.md"})).await)["text"]
            .as_str()
            .unwrap()
            .contains("new-private-secret")
    );
    // Removal affects the existing session and prunes grants before this ID can be reused.
    saved["mounts"].as_array_mut().unwrap().pop();
    fs::write(path, saved.to_string()).unwrap();
    call_local(&f.state.config, json!({"operation":"mounts.reload"}))
        .await
        .unwrap();
    denied(&tool(&session, "read_file", json!({"path":"NewPrivate/New.md"})).await);
    assert!(!f
        .state
        .control
        .as_ref()
        .unwrap()
        .grants(&f.laura.user_id)
        .unwrap()
        .iter()
        .any(|g| g.mount_id == "new-private"));
    // A rejected candidate keeps the live routing and session usable.
    let duplicate = saved["mounts"][0].clone();
    saved["mounts"].as_array_mut().unwrap().push(duplicate);
    fs::write(path, saved.to_string()).unwrap();
    assert!(
        call_local(&f.state.config, json!({"operation":"mounts.reload"}))
            .await
            .is_err()
    );
    assert!(structured(
        &tool(
            &session,
            "read_file",
            json!({"path":"Laura/LauraSecret.md"})
        )
        .await
    )["text"]
        .as_str()
        .unwrap()
        .contains("privatelauraword"));
}
