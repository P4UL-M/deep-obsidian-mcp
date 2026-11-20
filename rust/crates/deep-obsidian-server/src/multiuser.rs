//! Application users and request-scoped access. Backend credentials stay server-side.
use crate::mcp::AppState;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use deep_obsidian_backend::{
    BackendDescriptor, BackendError, BackendRequest, BackendResponse, Capability, ChangeStream,
    Mount, OpaqueCursor, VaultBackend, VaultRouter,
};
use deep_obsidian_control::{Action, ControlStore, Principal};
use deep_obsidian_types::ResolvedServiceConfig;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub struct Access {
    pub principal: Principal,
    pub original_router: Arc<VaultRouter>,
    pub original_runtimes: Arc<crate::runtime::MountRuntimes>,
    pub original_config: Arc<ResolvedServiceConfig>,
}

pub fn state_dir(config: &ResolvedServiceConfig) -> PathBuf {
    for key in ["DEEP_OBSIDIAN_STATE_DIR", "DO_STATE_DIR"] {
        if let Some(value) = std::env::var_os(key).filter(|v| !v.is_empty()) {
            return PathBuf::from(value);
        }
    }
    config
        .config_file_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.index_dir.clone())
}
pub fn database_path(config: &ResolvedServiceConfig) -> PathBuf {
    state_dir(config).join("control.sqlite")
}
pub fn socket_path(config: &ResolvedServiceConfig) -> PathBuf {
    let path = state_dir(config).join("control.sock");
    #[cfg(unix)]
    if path.as_os_str().len() > 90 {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
        let id: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        return PathBuf::from("/tmp")
            .join(format!("deep-obsidian-{id}"))
            .join("control.sock");
    }
    path
}

pub fn configured_enabled(config: &ResolvedServiceConfig) -> Result<bool> {
    let override_path = state_dir(config).join("multiuser-mode.json");
    if override_path.exists() {
        let value: Value = serde_json::from_slice(&std::fs::read(override_path)?)?;
        return value["enabled"]
            .as_bool()
            .context("invalid persisted multiuser mode");
    }
    let Some(path) = &config.config_file_path else {
        return Ok(false);
    };
    if !path.exists() {
        return Ok(false);
    }
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    match value.get("multiuser").and_then(|v| v.get("enabled")) {
        None => Ok(false),
        Some(Value::Bool(enabled)) => Ok(*enabled),
        Some(_) => bail!("multiuser.enabled must be a boolean"),
    }
}

/// Configuration is an opt-in; mismatches fail closed rather than exposing private vaults.
pub fn attach(mut state: AppState) -> Result<AppState> {
    let path = database_path(&state.config);
    let requested = configured_enabled(&state.config)?;
    {
        let store = ControlStore::open(path)?;
        if requested != store.enabled()? {
            bail!("multiuser configuration and control state differ; use deep-obsidian-cli users enable/disable to reconcile");
        }
        if requested {
            validate_mounts(&state.config)?;
        }
        state.control = Some(store);
    }
    Ok(state)
}

/// A shared physical subtree would put private data in an authorized mount's index.
pub fn validate_mounts(config: &ResolvedServiceConfig) -> Result<()> {
    let roots: Vec<_> = config
        .mount_table()
        .iter()
        .filter_map(|m| match &m.backend {
            deep_obsidian_types::MountBackendConfig::Filesystem { vault_path, .. } => {
                Some(std::fs::canonicalize(vault_path))
            }
            _ => None,
        })
        .collect::<std::io::Result<_>>()?;
    let mut protected = vec![state_dir(config), config.index_dir.clone()];
    if let Some(path) = &config.config_file_path {
        protected.push(path.clone());
    }
    for mount in config.mount_table() {
        if let Some(index) = match mount.backend {
            deep_obsidian_types::MountBackendConfig::Filesystem { index_dir, .. }
            | deep_obsidian_types::MountBackendConfig::Couchdb { index_dir, .. } => index_dir,
            _ => None,
        } {
            protected.push(index);
        }
    }
    for path in protected {
        let canonical = canonical_destination(&path)?;
        if roots.iter().any(|root| canonical.starts_with(root)) {
            bail!("multiuser config, control state and indexes must live outside every filesystem vault; configure an external index/state directory");
        }
    }
    for (i, root) in roots.iter().enumerate() {
        if roots[..i]
            .iter()
            .any(|other| root.starts_with(other) || other.starts_with(root))
        {
            bail!("multiuser filesystem mounts must have separate, non-overlapping physical roots");
        }
    }
    Ok(())
}

fn canonical_destination(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut tail = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut base) => {
                for part in tail.iter().rev() {
                    base.push(part);
                }
                return Ok(base);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tail.push(
                    ancestor
                        .file_name()
                        .context("invalid state path")?
                        .to_os_string(),
                );
                ancestor = ancestor.parent().context("invalid state path")?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

impl AppState {
    pub fn prepare_arguments(&self, name: &str, args: &Value) -> Result<Value> {
        let mut args = args.clone();
        let Some(access) = &self.access else {
            return Ok(args);
        };
        if !matches!(
            name,
            "upsert_note"
                | "upsert_session_note"
                | "edit_note"
                | "rename_note"
                | "delete_note"
                | "request_vault_upload"
        ) {
            return Ok(args);
        }
        let store = self.control.as_ref().context("access denied")?;
        let default = store
            .get_user(&access.principal.user_id)?
            .and_then(|u| u.default_mount);
        let explicit = args.get("mountId").and_then(Value::as_str);
        let selected = match explicit {
            Some("default") => default.as_deref(),
            other => other,
        };
        if let Some(id) = selected {
            let mount = self
                .router
                .mounts()
                .iter()
                .find(|m| m.id == id)
                .context("access denied")?;
            for key in ["path", "folder", "from", "to"] {
                if let Some(path) = args.get(key).and_then(Value::as_str).map(str::to_owned) {
                    args[key] = json!(mount.to_logical(&path));
                }
            }
        } else if explicit.is_some() {
            bail!("no default vault configured");
        }
        if name == "upsert_session_note"
            && args.get("path").is_none()
            && args.get("folder").is_none()
        {
            let id = default
                .as_deref()
                .or_else(|| {
                    if self.router.mounts().len() == 1 {
                        Some(self.router.mounts()[0].id.as_str())
                    } else {
                        None
                    }
                })
                .context("session destination is ambiguous; select a folder or default vault")?;
            let mount = self
                .router
                .mounts()
                .iter()
                .find(|m| m.id == id)
                .context("access denied")?;
            args["folder"] = json!(mount.to_logical("_Agent/Sessions"));
        }
        Ok(args)
    }

    pub fn membership_tool(&self, args: &Value) -> Result<Value> {
        let access = self.access.as_ref().context("authentication required")?;
        let store = self.control.as_ref().context("access denied")?;
        let mount = args
            .get("mountId")
            .and_then(Value::as_str)
            .context("mountId required")?;
        if !self.router.mounts().iter().any(|m| m.id == mount) {
            bail!("access denied");
        }
        let user = args
            .get("user")
            .and_then(Value::as_str)
            .context("user required")?;
        let role = match args
            .get("role")
            .and_then(Value::as_str)
            .context("role required")?
        {
            "reader" => Some(deep_obsidian_control::Role::Reader),
            "editor" => Some(deep_obsidian_control::Role::Editor),
            "owner" => Some(deep_obsidian_control::Role::Owner),
            "none" => None,
            _ => bail!("role must be reader, editor, owner or none"),
        };
        store.change_membership(Some(&access.principal), user, mount, role)?;
        store.record_event(
            &access.principal.user_id,
            Some(mount),
            "set_vault_membership",
        )?;
        Ok(json!({"mountId":mount,"user":user,"role":role,"revoked":role.is_none()}))
    }

    pub fn multiuser_enabled(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|c| c.enabled().unwrap_or(true))
    }

    /// Scope BEFORE any candidate retrieval, resource enumeration or metadata rendering.
    pub fn for_principal(&self, principal: Principal) -> Result<Self> {
        let current = self.current();
        let store = current
            .control
            .as_ref()
            .context("multiuser is unavailable")?;
        if !store.principal_valid(&principal)? || !store.enabled()? {
            bail!("access denied");
        }
        let grants = store.grants(&principal.user_id)?;
        let allowed: Vec<_> = grants.iter().map(|g| g.mount_id.clone()).collect();
        let original = current
            .access
            .as_ref()
            .map(|a| a.original_router.clone())
            .unwrap_or_else(|| current.router.clone());
        let mounts = original
            .mounts()
            .iter()
            .filter(|m| allowed.contains(&m.id))
            .map(|m| {
                Mount::new(
                    &m.id,
                    &m.mount_at,
                    Arc::new(GrantedBackend {
                        inner: m.backend.clone(),
                        store: store.clone(),
                        principal: principal.clone(),
                        mount: m.id.clone(),
                    }),
                )
            })
            .collect();
        let full_runtimes = current
            .access
            .as_ref()
            .map(|a| a.original_runtimes.clone())
            .unwrap_or_else(|| current.runtimes.clone());
        let mut scoped = current.clone();
        scoped.router = Arc::new(VaultRouter::new(mounts)?);
        scoped.runtimes = full_runtimes.restricted(&allowed, principal.default_mount.as_deref());
        // The root-only convenience handle must never refer to an unauthorized backend.
        if let Some(mount) = scoped
            .router
            .root()
            .or_else(|| scoped.router.mounts().first())
        {
            scoped.backend = mount.backend.clone();
        }
        scoped.rg_available = scoped
            .router
            .mounts()
            .iter()
            .any(|m| m.backend.descriptor().supports(Capability::GrepSearch));
        let original_config = current
            .access
            .as_ref()
            .map(|a| a.original_config.clone())
            .unwrap_or_else(|| current.config.clone());
        let mut config = (*original_config).clone();
        config.mounts = original_config
            .mount_table()
            .into_iter()
            .filter(|m| allowed.contains(&m.id))
            .collect();
        config.vault_path = None;
        scoped.config = Arc::new(config);
        scoped.access = Some(Access {
            principal,
            original_router: original,
            original_runtimes: full_runtimes,
            original_config,
        });
        Ok(scoped)
    }

    pub fn check_path(&self, path: &str, action: Action) -> Result<()> {
        if path.contains('\\')
            || path.trim_matches('/').split('/').any(|p| {
                p == "." || p == ".." || p.is_empty() && !path.trim_matches('/').is_empty()
            })
        {
            bail!("invalid vault-relative path");
        }
        let Some(access) = &self.access else {
            if self.multiuser_enabled() {
                bail!("authentication required");
            }
            return Ok(());
        };
        let mount = access
            .original_router
            .resolve(path)
            .map_err(|_| anyhow::anyhow!("access denied"))?;
        if !self.control.as_ref().context("access denied")?.permits(
            &access.principal,
            &mount.mount.id,
            action,
        )? {
            bail!("access denied");
        }
        Ok(())
    }

    pub fn check_tool(&self, name: &str, args: &Value) -> Result<()> {
        if !self.multiuser_enabled() {
            return Ok(());
        }
        let access = self.access.as_ref().context("authentication required")?;
        let store = self.control.as_ref().context("access denied")?;
        if !store.principal_valid(&access.principal)? {
            bail!("access denied");
        }
        let write = matches!(
            name,
            "upsert_note"
                | "upsert_session_note"
                | "edit_note"
                | "rename_note"
                | "delete_note"
                | "request_vault_upload"
                | "request_artifact_upload"
        );
        let action = if write { Action::Write } else { Action::Read };
        for key in ["path", "scope", "folder", "directory", "from", "to"] {
            if name == "list_children" && args.get(key).and_then(Value::as_str) == Some("") {
                continue;
            }
            if let Some(path) = args.get(key).and_then(Value::as_str) {
                if name == "list_children" && path.is_empty() {
                    continue;
                }
                self.check_path(path, action)?;
            }
        }
        if write && name != "rename_note" && !args.get("path").is_some_and(Value::is_string) {
            // Session-note folder/topic writes must explicitly select an authorized mount.
            let path = args.get("folder").and_then(Value::as_str).unwrap_or("");
            self.check_path(path, Action::Write)?;
        }
        if name == "build_index" {
            for mount in self.router.mounts() {
                if !store.permits(&access.principal, &mount.id, Action::Manage)? {
                    bail!("owner permission required to rebuild indexes");
                }
            }
            if self.router.mounts().is_empty() {
                bail!("access denied");
            }
        }
        Ok(())
    }

    pub fn visible_path(&self, path: &str) -> bool {
        self.check_path(path, Action::Read).is_ok()
    }

    pub fn user_vault_info(&self) -> Value {
        let mounts: Vec<_> = self.mount_index_summaries().into_iter().filter(|m|self.access.as_ref().is_none_or(|a|self.control.as_ref().is_some_and(|c|c.permits(&a.principal,&m.id,Action::Read).unwrap_or(false)))).map(|m| {
            let d = m.diagnostics;
            json!({"id":m.id,"mountAt":m.mount_at,"backendKind":m.backend_kind,"indexStatus":d.as_ref().map(|d|d.status.as_str()),"noteCount": d.as_ref().and_then(|d|d.snapshot.as_ref()).map(|s|s.index.note_count)})
        }).collect();
        json!({"multiuser":true,"mounts":mounts,"mountCount":self.router.mounts().len()})
    }
}

struct GrantedBackend {
    inner: Arc<dyn VaultBackend>,
    store: ControlStore,
    principal: Principal,
    mount: String,
}
#[async_trait]
impl VaultBackend for GrantedBackend {
    fn storage_id(&self) -> String {
        // The wrapper serves the SAME storage as its inner backend: a scoped
        // principal and the principal-free state must agree on which note a
        // mutation lock names.
        self.inner.storage_id()
    }

    fn mutation_key(&self, path: &str) -> std::result::Result<String, BackendError> {
        self.inner.mutation_key(path)
    }

    fn descriptor(&self) -> BackendDescriptor {
        let mut d = self.inner.descriptor();
        if !self
            .store
            .permits(&self.principal, &self.mount, Action::Write)
            .unwrap_or(false)
        {
            for c in [
                Capability::BinaryWrite,
                Capability::Upload,
                Capability::SoftDelete,
                Capability::Rename,
            ] {
                d.capabilities.remove(&c);
            }
        }
        d
    }
    async fn execute(
        &self,
        request: BackendRequest,
    ) -> std::result::Result<BackendResponse, BackendError> {
        let action = if matches!(&request, BackendRequest::Mutation(_)) {
            Action::Write
        } else {
            Action::Read
        };
        if !self
            .store
            .permits(&self.principal, &self.mount, action)
            .unwrap_or(false)
        {
            return Err(BackendError::Unsupported("access denied".into()));
        }
        let request = match request {
            BackendRequest::Mutation(
                deep_obsidian_backend::MutationRequest::StageUploadStream {
                    path,
                    max_bytes,
                    chunks,
                },
            ) => {
                let store = self.store.clone();
                let principal = self.principal.clone();
                let mount = self.mount.clone();
                let mut chunks = chunks.into_inner();
                let guarded = std::iter::from_fn(move || {
                    if !store
                        .permits(&principal, &mount, Action::Write)
                        .unwrap_or(false)
                    {
                        return Some(Err("access denied".into()));
                    }
                    chunks.next()
                });
                BackendRequest::Mutation(
                    deep_obsidian_backend::MutationRequest::StageUploadStream {
                        path,
                        max_bytes,
                        chunks: deep_obsidian_backend::UploadChunks::new(guarded),
                    },
                )
            }
            request => request,
        };
        self.inner.execute(request).await
    }
    fn changes(&self, after: Option<OpaqueCursor>) -> ChangeStream {
        self.inner.changes(after)
    }
}

/// Sanitize structured path carriers from authorized indexes, including shadowed prefixes.
pub fn filter_paths(value: &mut Value, state: &AppState) {
    match value {
        Value::Array(items) => {
            items.retain(|v| {
                ["path", "source", "target"].iter().all(|key| {
                    v.get(*key)
                        .and_then(Value::as_str)
                        .is_none_or(|p| state.visible_path(p))
                })
            });
            for item in items {
                filter_paths(item, state);
            }
        }
        Value::Object(object) => {
            for key in ["folders", "sharedLinks"] {
                if let Some(Value::Array(paths)) = object.get_mut(key) {
                    paths.retain(|p| p.as_str().is_none_or(|p| state.visible_path(p)));
                }
            }
            for item in object.values_mut() {
                filter_paths(item, state);
            }
            for (count, items) in [
                ("count", "matches"),
                ("count", "folders"),
                ("count", "children"),
                ("nodeCount", "nodes"),
                ("edgeCount", "edges"),
            ] {
                if object.contains_key(count) {
                    if let Some(items) = object.get(items).and_then(Value::as_array) {
                        object.insert(count.into(), json!(items.len()));
                    }
                }
            }
            if let Some(count) = object
                .get("matches")
                .and_then(Value::as_array)
                .map(Vec::len)
            {
                if object.contains_key("count") {
                    object.insert("count".into(), json!(count));
                }
            }
        }
        _ => {}
    }
}

/// A grafted child belongs to its own mount, never to its parent's search index.
pub(crate) struct BoundedSource {
    pub inner: Arc<dyn deep_obsidian_index::source::NoteSource>,
    pub excluded: Vec<String>,
}
impl BoundedSource {
    fn contains(&self, path: &str) -> bool {
        !self
            .excluded
            .iter()
            .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
    }
}
impl deep_obsidian_index::source::NoteSource for BoundedSource {
    fn ensure_ready(&self) -> deep_obsidian_index::index::Result<()> {
        self.inner.ensure_ready()
    }
    fn note_snapshots(
        &self,
    ) -> deep_obsidian_index::index::Result<Vec<deep_obsidian_index::index::FileSnapshot>> {
        let mut s = self.inner.note_snapshots()?;
        s.retain(|s| self.contains(&s.path));
        Ok(s)
    }
    fn artifact_snapshots(
        &self,
    ) -> deep_obsidian_index::index::Result<Vec<deep_obsidian_index::index::ArtifactSnapshot>> {
        let mut s = self.inner.artifact_snapshots()?;
        s.retain(|s| self.contains(&s.path));
        Ok(s)
    }
    fn ensure_path(&self, path: &str) -> deep_obsidian_index::index::Result<()> {
        if !self.contains(path) {
            return Err(deep_obsidian_index::index::IndexError::source(
                "path belongs to another mount",
            ));
        }
        self.inner.ensure_path(path)
    }
    fn read_note(&self, path: &str) -> deep_obsidian_index::index::Result<String> {
        self.ensure_path(path)?;
        self.inner.read_note(path)
    }
    fn read_artifact(
        &self,
        path: &str,
        max: u64,
    ) -> deep_obsidian_index::index::Result<Option<Vec<u8>>> {
        self.ensure_path(path)?;
        self.inner.read_artifact(path, max)
    }
    fn local_vault_path(&self) -> Option<&Path> {
        self.inner.local_vault_path()
    }
}

/// This tool exists only for authenticated multiuser sessions. Account lifecycle
/// and password rotation remain restricted to the local operator CLI.
pub fn membership_definition() -> crate::protocol::ToolDefinition {
    crate::protocol::ToolDefinition {
        name: "set_vault_membership".into(),
        description: "As a vault owner, assign an existing application user a role on that vault, or remove access with role none. To transfer ownership, grant the new owner first. Account creation and password rotation require the operator CLI.".into(),
        annotations: Some(json!({"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false})),
        execution: None,
        input_schema: json!({"type":"object","properties":{"mountId":{"type":"string"},"user":{"type":"string"},"role":{"type":"string","enum":["reader","editor","owner","none"]}},"required":["mountId","user","role"],"additionalProperties":false}),
    }
}
