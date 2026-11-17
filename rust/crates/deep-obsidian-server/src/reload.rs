//! Replace mount routing without replacing the listener, OAuth state or identities.
use std::sync::{Mutex, RwLock};

use anyhow::{bail, Context, Result};
use deep_obsidian_config::{normalize_service_config, read_config_file, to_persisted_config};
use deep_obsidian_types::ServiceConfigInput;
use serde_json::{json, Value};

use crate::{mcp::AppState, mounts::MountBackends, runtime::AutoReindexHandle};

#[derive(Default)]
pub struct LiveMounts {
    current: RwLock<Option<AppState>>,
    background: Mutex<Option<Background>>,
}

struct Background {
    _watchers: Vec<AutoReindexHandle>,
    initial: tokio::task::JoinHandle<()>,
}
impl Drop for Background {
    fn drop(&mut self) {
        self.initial.abort();
    }
}

impl AppState {
    /// Take one consistent routing generation for this request. A scoped caller
    /// retains its identity, but must be scoped again against the current router.
    pub fn current(&self) -> Self {
        let replacement = self
            .live
            .current
            .read()
            .expect("mount state poisoned")
            .clone();
        let Some(mut state) = replacement else {
            return self.clone();
        };
        state.live = self.live.clone();
        state.access = self.access.as_ref().map(|access| crate::multiuser::Access {
            principal: access.principal.clone(),
            original_router: state.router.clone(),
            original_runtimes: state.runtimes.clone(),
            original_config: state.config.clone(),
        });
        state
    }

    pub(crate) fn start_mount_background(&self) {
        *self.live.background.lock().expect("mount tasks poisoned") = Some(Background {
            _watchers: self.runtimes.start_auto_reindex(),
            initial: self.runtimes.start_initial_refresh(),
        });
    }

    pub(crate) fn stop_mount_background(&self) {
        self.live
            .background
            .lock()
            .expect("mount tasks poisoned")
            .take();
    }
}

pub async fn apply(state: &AppState) -> Result<Value> {
    let current = state.current();
    let path = current
        .config
        .config_file_path
        .as_ref()
        .context("mount reload requires a config file")?;
    let saved = read_config_file(path)?.context("configuration file is missing")?;
    // Reload only mount settings. Startup flags, resolved embedding credentials,
    // listener settings and authentication remain the running service's values.
    let mut effective = to_persisted_config(&current.config);
    effective.mounts = saved.mounts;
    effective.vault_path = saved.vault_path;
    effective.experimental = saved.experimental;
    let mut input: ServiceConfigInput = serde_json::from_value(serde_json::to_value(effective)?)?;
    input.config_file_path = Some(path.clone());
    let candidate = normalize_service_config(input)?;
    if candidate
        .mount_table()
        .iter()
        .find(|m| m.mount_at.is_empty())
        != current
            .config
            .mount_table()
            .iter()
            .find(|m| m.mount_at.is_empty())
    {
        bail!("changing the root mount requires a service restart; live reload supports adding and removing non-root mounts");
    }
    if current.multiuser_enabled() {
        crate::multiuser::validate_mounts(&candidate)?;
    }
    let backends = MountBackends::reload(&candidate, &current);
    // Validate before publication. A rejected generation leaves the live router,
    // background tasks, authorizations and existing sessions untouched.
    for entry in backends.entries() {
        if current.config.mount_table().contains(&entry.mount) {
            continue;
        }
        let response = entry
            .backend
            .execute(deep_obsidian_backend::BackendRequest::health_overview())
            .await?;
        if let deep_obsidian_backend::BackendResponse::Health(
            deep_obsidian_backend::HealthResponse::Overview { reachable },
        ) = response
        {
            if !reachable {
                bail!(
                    "mount '{}' is unavailable; the previous live mounts were retained",
                    entry.mount.id
                );
            }
        }
    }
    let runtimes = crate::runtime::MountRuntimes::new(&candidate, &backends);
    let mut next = AppState::with_backends(candidate, runtimes, &backends);
    next.auth = current.auth.clone();
    next.control = current.control.clone();
    next.upload_base = current.upload_base.clone();
    // Pending upload paths belong to the old routing generation. Do not carry
    // them across a possible remap of a mount ID/prefix.
    let mounts: Vec<_> = next
        .config
        .mount_table()
        .iter()
        .map(|m| m.id.clone())
        .collect();
    if let Some(store) = &next.control {
        store.prune_mounts(&mounts)?;
    }
    state.stop_mount_background();
    // The stored generation owns a separate empty LiveMounts, avoiding an Arc cycle.
    *state.live.current.write().expect("mount state poisoned") = Some(next.clone());
    let mut active = next;
    active.live = state.live.clone();
    active.start_mount_background();
    Ok(json!({"reloaded":true,"mounts":mounts,"sessionsPreserved":true}))
}
