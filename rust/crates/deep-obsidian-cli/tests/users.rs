//! The public CLI drives application identities against an offline, exclusively
//! locked backend. No CouchDB or external account provisioning is involved.
use std::path::{Path, PathBuf};
use std::process::Command;

use deep_obsidian_control::{Action, ControlStore};
use serde_json::{json, Value};

struct Fixture {
    root: PathBuf,
    config: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "deep-obsidian-users-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for folder in ["paul", "laura", "shared"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        let config = root.join("config.json");
        let value = json!({"indexDir":root.join("index"),"transport":"stdio","experimental":{"multiVault":true},"mounts":[
          {"id":"paul","mountAt":"","backend":{"kind":"filesystem","vaultPath":root.join("paul")}},
          {"id":"laura","mountAt":"Laura","backend":{"kind":"filesystem","vaultPath":root.join("laura")}},
          {"id":"shared","mountAt":"Shared","backend":{"kind":"filesystem","vaultPath":root.join("shared")}}
        ]});
        std::fs::write(&config, value.to_string()).unwrap();
        Self { root, config }
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
            .arg("--config")
            .arg(&self.config)
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }
    fn result(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("CLI returns JSON")
    }
    fn plan(&self) -> PathBuf {
        let path = self.root.join("initial-users.json");
        std::fs::write(&path,json!([
          {"name":"paul","default_mount":"paul","grants":[{"mount_id":"paul","role":"owner"},{"mount_id":"shared","role":"owner"}]},
          {"name":"laura","default_mount":"laura","grants":[{"mount_id":"laura","role":"owner"},{"mount_id":"shared","role":"reader"}]}
        ]).to_string()).unwrap();
        path
    }
    fn store(&self) -> ControlStore {
        ControlStore::open(self.root.join("control.sqlite")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn path_string(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn users_enable_grant_rotate_and_disable_are_persistent_cli_operations() {
    let f = Fixture::new();
    let plan = f.plan();
    let created = f.result(&["users", "enable", "--plan", path_string(&plan), "--yes"]);
    assert!(created.to_string().contains("password"));
    let store = f.store();
    assert!(store.enabled().unwrap());
    assert_eq!(store.list_users().unwrap().len(), 2);
    let added = f.result(&["users", "add", "third"]);
    let password = added["password"]
        .as_str()
        .expect("creation returns the generated password");
    assert_eq!(password.len(), 64);
    let third = store.authenticate("third", password).unwrap().unwrap();
    assert!(!store.permits(&third, "shared", Action::Read).unwrap());
    f.result(&[
        "users", "grant", "third", "--mount", "shared", "--role", "reader",
    ]);
    assert!(store.permits(&third, "shared", Action::Read).unwrap());
    assert!(!store.permits(&third, "shared", Action::Write).unwrap());
    f.result(&[
        "users", "grant", "third", "--mount", "shared", "--role", "editor",
    ]);
    assert!(store.permits(&third, "shared", Action::Write).unwrap());
    f.result(&["users", "revoke", "third", "--mount", "shared"]);
    assert!(!store.permits(&third, "shared", Action::Read).unwrap());
    let token = store.create_session(&third, 60).unwrap();
    let rotated = f.result(&["users", "rotate-password", "third"]);
    let replacement = rotated["password"].as_str().unwrap();
    assert_ne!(password, replacement);
    assert!(store.authenticate("third", password).unwrap().is_none());
    assert!(store.authenticate("third", replacement).unwrap().is_some());
    use secrecy::ExposeSecret;
    assert!(store.session(token.expose_secret()).unwrap().is_none());
    assert!(!f.run(&["users", "remove", "paul"]).status.success());
    let sentinel = f.root.join("paul/note.md");
    std::fs::write(&sentinel, "preserve vault data").unwrap();
    f.result(&["users", "disable", "--yes"]);
    assert!(!store.enabled().unwrap());
    assert!(store.list_users().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(sentinel).unwrap(),
        "preserve vault data"
    );
    let config: Value = serde_json::from_slice(&std::fs::read(&f.config).unwrap()).unwrap();
    assert_eq!(config["multiuser"]["enabled"], false);
    assert_eq!(config["mounts"].as_array().unwrap().len(), 3);
}

#[test]
fn invalid_plan_and_dry_run_do_not_create_users_or_enable_multiuser() {
    let f = Fixture::new();
    let invalid = f.root.join("invalid.json");
    std::fs::write(&invalid,json!([{"name":"reader","grants":[{"mount_id":"shared","role":"reader"}],"default_mount":null}]).to_string()).unwrap();
    assert!(!f
        .run(&["users", "enable", "--plan", path_string(&invalid), "--yes"])
        .status
        .success());
    assert!(!f.root.join("control.sqlite").exists());
    let plan = f.plan();
    let output = f.result(&[
        "--dry-run",
        "users",
        "enable",
        "--plan",
        path_string(&plan),
        "--yes",
    ]);
    assert_eq!(output["dryRun"], true);
    assert!(!f.root.join("control.sqlite").exists());
    let config: Value = serde_json::from_slice(&std::fs::read(&f.config).unwrap()).unwrap();
    assert!(config.get("multiuser").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn adding_a_mount_reloads_the_live_backend_before_the_next_grant() {
    use deep_obsidian_cli::{resolve_runtime_config, ServiceOptions};
    use deep_obsidian_server::{
        control, mcp::AppState, mounts::MountBackends, runtime::MountRuntimes,
    };
    let f = Fixture::new();
    f.result(&["users", "enable", "--plan", path_string(&f.plan()), "--yes"]);
    let config = resolve_runtime_config(&ServiceOptions {
        config: Some(f.config.clone()),
        ..Default::default()
    })
    .unwrap()
    .service;
    let backends = MountBackends::build(&config);
    let runtimes = MountRuntimes::new(&config, &backends);
    let state = deep_obsidian_server::multiuser::attach(AppState::with_backends(
        config, runtimes, &backends,
    ))
    .unwrap();
    let _control = control::start(&state).unwrap();
    let extra = f.root.join("extra");
    std::fs::create_dir(&extra).unwrap();
    let path = f.config.clone();
    let added = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
            .arg("--config")
            .arg(path)
            .args([
                "--json",
                "mounts",
                "add",
                "filesystem",
                "--id",
                "extra",
                "--mount-at",
                "Extra",
                "--vault-path",
            ])
            .arg(extra)
            .arg("--yes")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(String::from_utf8_lossy(&added.stdout).contains("reloaded"));
    assert!(state
        .current()
        .router
        .mounts()
        .iter()
        .any(|m| m.id == "extra"));
    let path = f.config.clone();
    let granted = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
            .arg("--config")
            .arg(path)
            .args([
                "users", "grant", "laura", "--mount", "extra", "--role", "owner",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        granted.status.success(),
        "{}",
        String::from_utf8_lossy(&granted.stderr)
    );
}
