//! Local operator channel. It never provisions external backends or exposes an HTTP admin API.
use crate::{mcp::AppState, multiuser};
use anyhow::{bail, Context, Result};
use deep_obsidian_control::{ControlStore, InitialUser, Role};
use deep_obsidian_types::ResolvedServiceConfig;
use secrecy::ExposeSecret;
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

pub struct ControlGuard {
    _lock: File,
    #[cfg(unix)]
    task: Option<tokio::task::JoinHandle<()>>,
    #[cfg(unix)]
    socket: std::path::PathBuf,
}

impl Drop for ControlGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(task) = &self.task {
                task.abort();
            }
            let _ = std::fs::remove_file(&self.socket);
        }
    }
}

fn lock(config: &ResolvedServiceConfig) -> Result<File> {
    let dir = multiuser::state_dir(config);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("control.lock");
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock().map_err(|_| anyhow::anyhow!("backend is running or another management operation is active; its control socket must be reachable"))?;
    Ok(file)
}

pub fn start(state: &AppState) -> Result<ControlGuard> {
    let file = lock(&state.config)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let socket = multiuser::socket_path(&state.config);
        let parent = socket.parent().context("invalid control socket")?;
        if parent != multiuser::state_dir(&state.config) {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            match std::fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            let meta = std::fs::symlink_metadata(parent)?;
            if !meta.is_dir()
                || meta.permissions().mode() & 0o777 != 0o700
                || meta.uid() != std::fs::metadata(multiuser::state_dir(&state.config))?.uid()
            {
                bail!("unsafe control socket directory");
            }
        }
        match std::fs::symlink_metadata(&socket) {
            Ok(meta) => {
                use std::os::unix::fs::FileTypeExt;
                if !meta.file_type().is_socket() {
                    bail!("control socket path is occupied by a non-socket");
                }
                std::fs::remove_file(&socket)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = tokio::net::UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let state = state.clone();
        let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = state.clone();
                let gate = gate.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
                    let (reader, mut writer) = stream.into_split();
                    let mut line = String::new();
                    let request = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        BufReader::new(reader.take(65537)).read_line(&mut line),
                    )
                    .await;
                    let response = if matches!(request,Ok(Ok(n)) if n > 0 && n <= 65536) {
                        let _serial = gate.lock().await;
                        match serde_json::from_str::<Value>(&line) {
                            Ok(request) if request["operation"] == "mounts.reload" => {
                                match tokio::time::timeout(std::time::Duration::from_secs(25), crate::reload::apply(&state)).await {
                                    Ok(Ok(result)) => json!({"ok":true,"result":result}),
                                    Ok(Err(error)) => json!({"ok":false,"error":error.to_string()}),
                                    Err(_) => json!({"ok":false,"error":"mount reload timed out; previous live mounts were retained"}),
                                }
                            }
                            Ok(request) => {
                                let active = state.current();
                                let config = active.config.clone();
                                let store = active.control.clone().expect("control store missing");
                                let runtimes = active.runtimes.clone();
                                let transition = matches!(
                                    request["operation"].as_str(),
                                    Some("multiuser.enable" | "multiuser.disable")
                                );
                                match tokio::task::spawn_blocking(move || {
                                    execute(&config, &store, request)
                                })
                                .await
                                {
                                    Ok(Ok(result)) => {
                                        if transition {
                                            for entry in runtimes.entries() {
                                                entry.runtime.invalidate_snapshot();
                                                entry
                                                    .runtime
                                                    .request_rebuild("multiuser mode changed");
                                            }
                                        }
                                        json!({"ok":true,"result":result})
                                    }
                                    Ok(Err(e)) => json!({"ok":false,"error":e.to_string()}),
                                    Err(_) => {
                                        json!({"ok":false,"error":"management operation failed"})
                                    }
                                }
                            }
                            Err(_) => json!({"ok":false,"error":"invalid control request"}),
                        }
                    } else {
                        json!({"ok":false,"error":"invalid control request"})
                    };
                    let mut bytes = response.to_string().into_bytes();
                    bytes.push(b'\n');
                    let _ = writer.write_all(&bytes).await;
                });
            }
        });
        Ok(ControlGuard {
            _lock: file,
            task: Some(task),
            socket,
        })
    }
    #[cfg(not(unix))]
    {
        Ok(ControlGuard { _lock: file })
    }
}

/// CLI sends changes to a live server, or locks an offline config for initial setup.
pub async fn try_call_running(config: &ResolvedServiceConfig, request: Value) -> Result<Option<Value>> {
    #[cfg(unix)]
    {
        match tokio::net::UnixStream::connect(multiuser::socket_path(config)).await {
            Ok(mut stream) => {
                use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
                let mut bytes = serde_json::to_vec(&request)?;
                bytes.push(b'\n');
                stream.write_all(&bytes).await?;
                let mut line = String::new();
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    BufReader::new(stream.take(1048577)).read_line(&mut line),
                )
                .await??;
                let reply: Value = serde_json::from_str(&line)?;
                if reply["ok"] != true {
                    bail!(
                        "{}",
                        reply["error"]
                            .as_str()
                            .unwrap_or("management operation failed")
                    );
                }
                return Ok(Some(reply["result"].clone()));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

pub async fn call_local(config: &ResolvedServiceConfig, request: Value) -> Result<Value> {
    if let Some(result) = try_call_running(config, request.clone()).await? { return Ok(result); }
    let _lock = lock(config)?;
    let store = ControlStore::open(multiuser::database_path(config))?;
    execute(config, &store, request)
}

fn string<'a>(request: &'a Value, key: &str) -> Result<&'a str> {
    request
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing {key}"))
}
fn generated(g: deep_obsidian_control::GeneratedUser) -> Value {
    json!({"user":g.user,"password":g.password.expose_secret()})
}

fn save_mode(config: &ResolvedServiceConfig, enabled: bool) -> Result<Vec<u8>> {
    let path = config
        .config_file_path
        .as_ref()
        .context("multiuser requires a persistent --config file")?;
    let old = std::fs::read(path)?;
    let mut value: Value = serde_json::from_slice(&old)?;
    value["multiuser"] = json!({"enabled":enabled});
    atomic_write(
        &multiuser::state_dir(config).join("multiuser-mode.json"),
        &serde_json::to_vec(&json!({"enabled":enabled}))?,
    )?;
    // A bind-mounted operator config may be read-only. The private state override is authoritative.
    let _ = atomic_write(path, &serde_json::to_vec_pretty(&value)?);
    Ok(old)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("{}.tmp", crate::auth::generate_token()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(Into::into)
}

pub fn execute(
    config: &ResolvedServiceConfig,
    store: &ControlStore,
    request: Value,
) -> Result<Value> {
    let operation = string(&request, "operation")?;
    let mounts: Vec<_> = config.mount_table().iter().map(|m| m.id.clone()).collect();
    if operation.starts_with("users.") && !store.enabled()? {
        bail!("multiuser is disabled; run users enable --wizard first");
    }
    let old_enabled = store.enabled()?;
    let result = match operation {
        "mounts.status" => json!({"mounts":mounts}),
        "multiuser.status" => json!({"enabled":store.enabled()?,"users":store.list_users()?}),
        "multiuser.enable" => {
            multiuser::validate_mounts(config)?;
            if matches!(config.transport, deep_obsidian_types::TransportMode::Http)
                && config.auth.oauth.is_none()
            {
                bail!("HTTP multiuser requires auth.oauth; configure Legacy bearer + OAuth in the setup wizard before enabling users");
            }
            let users: Vec<InitialUser> =
                serde_json::from_value(request.get("users").cloned().context("missing users")?)?;
            let old = save_mode(config, true)?;
            match store.enable(&users, &mounts) {
                Ok(users) => {
                    json!({"enabled":true,"users":users.into_iter().map(generated).collect::<Vec<_>>()})
                }
                Err(e) => {
                    let _ = atomic_write(config.config_file_path.as_ref().unwrap(), &old);
                    atomic_write(
                        &multiuser::state_dir(config).join("multiuser-mode.json"),
                        &serde_json::to_vec(&json!({"enabled":old_enabled}))?,
                    )?;
                    return Err(e);
                }
            }
        }
        "multiuser.disable" => {
            let old = save_mode(config, false)?;
            if let Err(e) = store.disable() {
                let _ = atomic_write(config.config_file_path.as_ref().unwrap(), &old);
                atomic_write(
                    &multiuser::state_dir(config).join("multiuser-mode.json"),
                    &serde_json::to_vec(&json!({"enabled":old_enabled}))?,
                )?;
                return Err(e);
            }
            json!({"enabled":false,"usersDeleted":true,"classicAccess":"all configured mounts"})
        }
        "users.list" => json!({"users":store.list_users()?}),
        "users.add" => generated(store.add_user(string(&request, "name")?)?),
        "users.show" => {
            let name = string(&request, "name")?;
            json!({"user":store.get_user(name)?.context("user not found")?,"grants":store.grants(name)?})
        }
        "users.remove" => {
            store.remove_user(string(&request, "name")?)?;
            json!({"removed":true})
        }
        "users.rotate-password" => generated(store.rotate_password(string(&request, "name")?)?),
        "users.grant" => {
            let role: Role =
                serde_json::from_value(request.get("role").cloned().context("missing role")?)?;
            store.grant(
                string(&request, "name")?,
                string(&request, "mount")?,
                role,
                &mounts,
            )?;
            json!({"granted":true})
        }
        "users.revoke" => {
            store.revoke(string(&request, "name")?, string(&request, "mount")?)?;
            json!({"revoked":true})
        }
        "users.default" => {
            store.set_default_mount(
                string(&request, "name")?,
                request.get("defaultMount").and_then(Value::as_str),
            )?;
            json!({"updated":true})
        }
        _ => bail!("unknown management operation"),
    };
    store.record_event(
        "operator",
        request.get("mount").and_then(Value::as_str),
        operation,
    )?;
    Ok(result)
}
