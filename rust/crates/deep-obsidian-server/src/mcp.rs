use std::sync::Arc;

use deep_obsidian_backend::{Capability, VaultBackend, VaultRouter};
use deep_obsidian_types::ResolvedServiceConfig;
use serde_json::{json, Value};

use crate::auth::AuthState;
use crate::health::MountIndexSummary;
use crate::mounts::MountBackends;
use crate::protocol::{
    InitializeResult, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResponse,
    PromptGetResult, PromptListResult, ResourceListResult, ResourceReadResult,
    ResourceTemplateListResult, ServerInfo, ToolCallResult, ToolListResult,
};
use crate::runtime::{MountRuntimes, RuntimeState};
use crate::uploads::UploadStore;
use crate::{prompts, resources, tools};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ResolvedServiceConfig>,
    /// One index runtime per mount, each with its own index directory, watcher and
    /// refresh lifecycle. A single-mount config holds exactly one, built from the
    /// config verbatim, so it behaves identically to the pre-slice single runtime.
    ///
    /// Use [`AppState::runtime`] for the ROOT mount, or
    /// [`MountRuntimes::for_mount`] to serve a path that routes elsewhere. Never
    /// serve a caller-supplied path from the root runtime on a multi-mount config:
    /// its index does not contain the other mounts' notes.
    pub runtimes: Arc<MountRuntimes>,
    /// Resolved HTTP authentication state. Disabled by default; populated by the
    /// HTTP bootstrap via [`AppState::with_auth`]. Unused under stdio.
    pub auth: Arc<AuthState>,
    /// The mount table, and the source of truth for every vault path resolution.
    /// All vault IO for tool and resource handling goes through here.
    ///
    /// `config.vault_path` deliberately stays available alongside it: it is the
    /// ROOT mount's path, which is what `vaultPath` has always meant. Each mount's
    /// own vault path now reaches the index crate through that mount's
    /// [`RuntimeState`] instead (see [`MountRuntimes`]).
    pub router: Arc<VaultRouter>,
    /// The ROOT mount's backend, unrouted.
    ///
    /// A convenience handle for the two callers that are about the root vault
    /// itself rather than about a path in it. Never use it to serve a
    /// caller-supplied path — that must go through [`AppState::router`], or a path
    /// on a non-root mount would silently be read from the root vault.
    pub backend: Arc<dyn VaultBackend>,
    /// Whether line search can be served. Derived from [`Capability::GrepSearch`]
    /// on the ROOT mount at construction; drives both conditional `grep_search`
    /// registration and the defensive call guard.
    ///
    /// Keyed on the root mount deliberately this slice: `tools/list` is computed
    /// once per process and cannot say "available for some paths", and a
    /// multi-mount grep must be scoped to a single mount anyway. A future slice
    /// that reports per-mount capabilities should surface them through
    /// `vault_info.mounts[].capabilities`, which already carries them.
    pub rg_available: bool,
    /// Shared store of pending out-of-band uploads. Both the `request_vault_upload`
    /// tool handler (mint) and the `PUT /upload/{token}` endpoint (consume) share it.
    pub uploads: UploadStore,
    /// Base URL the upload endpoint is reachable at, e.g. `http://127.0.0.1:7777`.
    /// `Some` only under the HTTP transport; `None` under stdio (no HTTP listener),
    /// in which case `request_vault_upload` returns a clear transport error.
    pub upload_base: Option<String>,
}

impl AppState {
    /// Build state with no upload base (used by the stdio transport).
    ///
    /// Builds the mount backends itself, which is right for a caller that has no
    /// other use for them. A caller that ALSO needs the backends (to build the index
    /// runtimes over the same sidecar supervisors) must use
    /// [`AppState::with_backends`] instead — building them twice would give a couchdb
    /// mount two child processes.
    ///
    /// Must be called from inside a tokio runtime.
    pub fn new(config: ResolvedServiceConfig, runtimes: Arc<MountRuntimes>) -> Self {
        let backends = MountBackends::build(&config);
        Self::with_backends(config, runtimes, &backends)
    }

    /// Build state over already-built mount backends.
    pub fn with_backends(
        config: ResolvedServiceConfig,
        runtimes: Arc<MountRuntimes>,
        backends: &MountBackends,
    ) -> Self {
        let router = backends.router();
        let root = router
            .root()
            .expect("resolved config to declare a root mount");
        let backend = root.backend.clone();
        let rg_available = backend.descriptor().supports(Capability::GrepSearch);
        Self {
            config: Arc::new(config),
            runtimes,
            auth: Arc::new(AuthState::disabled()),
            router: Arc::new(router),
            backend,
            rg_available,
            uploads: UploadStore::new(),
            upload_base: None,
        }
    }

    /// The ROOT mount's index runtime.
    ///
    /// The right handle for anything that is about the vault root itself — health,
    /// readiness, the vault overview. NOT the right handle for a caller-supplied
    /// path on a multi-mount config; route that through
    /// [`MountRuntimes::for_mount`].
    pub fn runtime(&self) -> &Arc<RuntimeState> {
        self.runtimes.root()
    }

    /// One summary per mount, joining the router's view of a mount (id, prefix,
    /// backend kind) with its runtime's index state.
    ///
    /// The single input to every additive multi-mount payload field. Mount order is
    /// the ROUTER's, i.e. config order, so `vault_info.mounts`, the health payloads
    /// and the vault overview all list mounts in the same order.
    pub fn mount_index_summaries(&self) -> Vec<MountIndexSummary> {
        self.router
            .mounts()
            .iter()
            .map(|mount| MountIndexSummary {
                id: mount.id.clone(),
                mount_at: mount.mount_at.clone(),
                backend_kind: mount.backend.descriptor().kind.as_str(),
                // `None` when the mount has no runtime of its own — which is a mount
                // with no LOCAL index, not a broken one. See
                // `MountIndexSummary::diagnostics` for why the two must not be
                // conflated, and `runtime::mount_has_local_index` for which mounts
                // these are.
                diagnostics: self
                    .runtimes
                    .for_mount(&mount.id)
                    .map(|runtime| runtime.diagnostics()),
            })
            .collect()
    }

    /// Attach an upload base URL, enabling `request_vault_upload`.
    pub fn with_upload_base(mut self, upload_base: String) -> Self {
        self.upload_base = Some(upload_base);
        self
    }

    /// Attach resolved authentication state (used by the HTTP transport).
    pub fn with_auth(mut self, auth: AuthState) -> Self {
        self.auth = Arc::new(auth);
        self
    }
}

fn json_response<T>(id: Value, result: T) -> JsonRpcResponse<T> {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result,
    }
}

fn json_error_response(id: Value, code: i64, message: impl Into<String>) -> JsonRpcErrorResponse {
    JsonRpcErrorResponse {
        jsonrpc: "2.0",
        id,
        error: JsonRpcError {
            code,
            message: message.into(),
        },
    }
}

fn initialize_result() -> InitializeResult {
    InitializeResult {
        protocol_version: "2025-03-26",
        capabilities: json!({
            "tools": {},
            "resources": {},
            "prompts": {}
        }),
        server_info: ServerInfo {
            name: "deep-obsidian-mcp",
            version: "0.1.0",
        },
    }
}

pub fn initialize_response() -> JsonRpcResponse<InitializeResult> {
    json_response(json!(1), initialize_result())
}

pub async fn handle_request(
    state: AppState,
    request: JsonRpcRequest,
) -> Result<Option<Value>, JsonRpcErrorResponse> {
    let id = request.id.unwrap_or(Value::Null);

    match request.method.as_str() {
        "notifications/initialized" => Ok(None),
        "initialize" => Ok(Some(
            serde_json::to_value(json_response(id, initialize_result()))
                .expect("initialize response to serialize"),
        )),
        "tools/list" => Ok(Some(
            serde_json::to_value(json_response(
                id,
                ToolListResult {
                    tools: tools::list_tools(
                        state.rg_available,
                        state.router.is_multi_mount(),
                        tools::CapabilitySet::of(&state.router),
                    ),
                },
            ))
            .expect("tool list response to serialize"),
        )),
        "tools/call" => {
            let tool_name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| json_error_response(id.clone(), -32602, "missing tool name"))?;
            let arguments = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result: ToolCallResult = tools::call_tool(&state, tool_name, &arguments)
                .await
                .map_err(|error| json_error_response(id.clone(), -32000, error))?;
            Ok(Some(
                serde_json::to_value(json_response(id, result))
                    .expect("tool response to serialize"),
            ))
        }
        "resources/list" => {
            let result: ResourceListResult = resources::list_resources(&state)
                .await
                .map_err(|error| json_error_response(id.clone(), -32000, error))?;
            Ok(Some(
                serde_json::to_value(json_response(id, result))
                    .expect("resource list response to serialize"),
            ))
        }
        "resources/templates/list" => {
            let result: ResourceTemplateListResult = resources::list_resource_templates();
            Ok(Some(
                serde_json::to_value(json_response(id, result))
                    .expect("resource template list response to serialize"),
            ))
        }
        "resources/read" => {
            let uri = request
                .params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| json_error_response(id.clone(), -32602, "missing resource uri"))?;
            let result: ResourceReadResult = resources::read_resource(&state, uri)
                .await
                .map_err(|error| json_error_response(id.clone(), -32000, error))?;
            Ok(Some(
                serde_json::to_value(json_response(id, result))
                    .expect("resource read response to serialize"),
            ))
        }
        "prompts/list" => {
            let result: PromptListResult = prompts::list_prompts();
            Ok(Some(
                serde_json::to_value(json_response(id, result))
                    .expect("prompt list response to serialize"),
            ))
        }
        "prompts/get" => {
            let prompt_name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| json_error_response(id.clone(), -32602, "missing prompt name"))?;
            let arguments = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result: PromptGetResult = prompts::get_prompt(prompt_name, &arguments)
                .map_err(|error| json_error_response(id.clone(), -32602, error))?;
            Ok(Some(
                serde_json::to_value(json_response(id, result))
                    .expect("prompt get response to serialize"),
            ))
        }
        _ => Err(json_error_response(
            id,
            -32601,
            format!("unsupported method: {}", request.method),
        )),
    }
}
