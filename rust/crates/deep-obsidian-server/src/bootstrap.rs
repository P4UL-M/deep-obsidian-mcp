use std::io;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_config::{build_service_endpoints, is_loopback_host, normalize_service_config};
use deep_obsidian_types::{
    ResolvedServiceConfig, ServiceConfigInput, ServiceEndpoints, TransportMode,
};
use futures_util::StreamExt as _;
use secrecy::SecretString;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::auth::AuthState;
use crate::health::{
    build_health_payload, build_readiness_payload, insert_mount_index_detail, readiness_status_code,
};
use crate::mcp::{handle_request, AppState};
use crate::mounts::MountBackends;
use crate::protocol::JsonRpcRequest;
use crate::runtime::MountRuntimes;

#[cfg(test)]
#[path = "concurrency_tests.rs"]
mod concurrency_tests;

/// Environment variable carrying a literal bearer token. When set and non-empty
/// it enables authentication and overrides any configured `token_ref` — useful
/// for containers, tunnels, and headless hosts where the OS keyring is absent.
const AUTH_TOKEN_ENV: &str = "DEEP_OBSIDIAN_AUTH_TOKEN";

/// Environment variable that, when truthy, allows binding a non-loopback host
/// without authentication (the fail-closed escape hatch).
const ALLOW_INSECURE_ENV: &str = "DEEP_OBSIDIAN_ALLOW_INSECURE";

fn env_is_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Resolve the authentication state for the HTTP transport from the environment
/// override and persisted config. Errors when auth is enabled but no token can
/// be resolved (fail closed rather than silently allow).
fn resolve_auth_state(config: &ResolvedServiceConfig) -> Result<AuthState, io::Error> {
    let mut auth = resolve_legacy_auth_state(config)?;
    if let Some(oauth) = &config.auth.oauth {
        if !auth.enabled || auth.token.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "OAuth requires the existing auth token (tokenRef or DEEP_OBSIDIAN_AUTH_TOKEN)",
            ));
        }
        auth.oauth = Some(Arc::new(crate::oauth::OAuthState::new(
            oauth,
            &config.http.mcp_path,
            Some(config.index_dir.join("oauth-clients.json")),
        )?));
    }
    Ok(auth)
}

fn resolve_legacy_auth_state(config: &ResolvedServiceConfig) -> Result<AuthState, io::Error> {
    let allowed_origins = Arc::new(config.auth.allowed_origins.clone());

    if let Ok(token) = std::env::var(AUTH_TOKEN_ENV) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            info!("HTTP authentication enabled via {AUTH_TOKEN_ENV}");
            return Ok(AuthState {
                enabled: true,
                token: Some(SecretString::new(token)),
                oauth: None,
                allowed_origins,
            });
        }
    }

    if !config.auth.enabled {
        return Ok(AuthState {
            enabled: false,
            token: None,
            oauth: None,
            allowed_origins,
        });
    }

    let token = SecretResolver::new()
        .resolve_auth_token(&config.auth)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "HTTP authentication is enabled but the token could not be resolved: {error}"
                ),
            )
        })?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP authentication is enabled but no token reference is configured; \
                 run `deep-obsidian-mcp setup-service --wizard` or set DEEP_OBSIDIAN_AUTH_TOKEN",
            )
        })?;

    info!("HTTP authentication enabled (bearer token)");
    Ok(AuthState {
        enabled: true,
        token: Some(token),
        oauth: None,
        allowed_origins,
    })
}

/// Fail closed: refuse to expose a non-loopback bind without authentication
/// unless the operator explicitly opts out via [`ALLOW_INSECURE_ENV`].
fn enforce_auth_exposure(
    config: &ResolvedServiceConfig,
    auth_enabled: bool,
) -> Result<(), io::Error> {
    if auth_enabled || is_loopback_host(&config.http.host) {
        return Ok(());
    }
    if env_is_truthy(ALLOW_INSECURE_ENV) {
        warn!(
            "binding {} without authentication ({ALLOW_INSECURE_ENV} is set); the vault is exposed to the network",
            config.http.host
        );
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "refusing to bind non-loopback host {} without authentication; \
             enable auth with `deep-obsidian-mcp setup-service --wizard`, set DEEP_OBSIDIAN_AUTH_TOKEN, \
             or override with DEEP_OBSIDIAN_ALLOW_INSECURE=1",
            config.http.host
        ),
    ))
}

pub struct ServiceBootstrapContext {
    pub config: ResolvedServiceConfig,
    pub endpoints: ServiceEndpoints,
    /// One handle per mount, when auto-reindex is enabled. Dropping them stops
    /// every watcher.
    live_state: AppState,
    pub server_handle: tokio::task::JoinHandle<io::Result<()>>,
    /// Sidecar supervisors to stop when the service stops. Empty unless a couchdb
    /// mount is configured.
    sidecars: Vec<Arc<deep_obsidian_backend::SidecarSupervisor>>,
    _control: crate::control::ControlGuard,
}

impl ServiceBootstrapContext {
    /// Stop the sidecar children gracefully: `shutdown`, close stdin, then SIGKILL
    /// after a grace period.
    ///
    /// Separate from `Drop` because a graceful stop is async and `Drop` is not. The
    /// `Drop` below is the backstop for a process that exits without calling this;
    /// it relies on `kill_on_drop(true)`, set when the child is spawned, so no
    /// sidecar can outlive the server either way.
    pub async fn shutdown_sidecars(&self) {
        for mount in self.live_state.current().router.mounts() {
            if let Some(backend) = mount.backend.as_couchdb() {
                backend.supervisor().shutdown().await;
            }
        }
        for supervisor in &self.sidecars {
            supervisor.shutdown().await;
        }
    }
}

impl Drop for ServiceBootstrapContext {
    fn drop(&mut self) {
        self.live_state.stop_mount_background();
        self.server_handle.abort();
        // The supervisors' own `Drop` (and the child's `kill_on_drop`) does the
        // killing; dropping the last handle here is what triggers it.
        self.sidecars.clear();
    }
}

async fn health_handler(State(state): State<AppState>) -> axum::response::Response {
    let state = state.current();
    if state.multiuser_enabled() {
        return (StatusCode::OK, Json(json!({"status":"ok"}))).into_response();
    }
    // Aggregated across mounts: identical to the root runtime's diagnostics for a
    // single-mount config, worst-of across mounts otherwise.
    let diagnostics = state.runtimes.aggregate_diagnostics();
    let mut payload = build_health_payload(&state.config, &diagnostics);
    insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
    (StatusCode::OK, Json(payload)).into_response()
}

async fn ready_handler(State(state): State<AppState>) -> axum::response::Response {
    let state = state.current();
    if state.multiuser_enabled() {
        let d = state.runtimes.aggregate_diagnostics();
        return (
            readiness_status_code(&d),
            Json(json!({"ready":d.snapshot.is_some()})),
        )
            .into_response();
    }
    let diagnostics = state.runtimes.aggregate_diagnostics();
    let mut payload = build_readiness_payload(&state.config, &diagnostics);
    insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
    (readiness_status_code(&diagnostics), Json(payload)).into_response()
}

async fn mcp_handler(
    State(state): State<AppState>,
    principal: Option<axum::Extension<deep_obsidian_control::Principal>>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let state = if state.multiuser_enabled() {
        match principal.and_then(|p| state.for_principal(p.0).ok()) {
            Some(state) => state,
            None => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error":"authentication required"})),
                )
                    .into_response()
            }
        }
    } else {
        state
    };
    match handle_request(state, request).await {
        Ok(Some(response)) => (StatusCode::OK, Json(response)).into_response(),
        Ok(None) => (StatusCode::NO_CONTENT, Json(Value::Null)).into_response(),
        Err(error) => (
            StatusCode::OK,
            Json(serde_json::to_value(error).unwrap_or_else(|_| {
                json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32603, "message": "internal server error" }
                })
            })),
        )
            .into_response(),
    }
}

pub(crate) async fn upload_handler(
    State(state): State<AppState>,
    AxumPath(token): AxumPath<String>,
    principal: Option<axum::Extension<deep_obsidian_control::Principal>>,
    body: Body,
) -> impl IntoResponse {
    use crate::uploads::{
        commit_staged_via_backend, stage_stream_via_backend, ClaimError, CommitError,
    };
    let state = state.current();
    let principal = principal.map(|p| p.0);
    if !state.uploads.owner_matches(&token, principal.as_ref()) {
        return (StatusCode::FORBIDDEN, "invalid upload token").into_response();
    }
    let state = if state.multiuser_enabled() {
        match principal.and_then(|p| state.for_principal(p).ok()) {
            Some(state) => state,
            None => return (StatusCode::UNAUTHORIZED, "authentication required").into_response(),
        }
    } else {
        state
    };

    // Atomically claim the token (or reject). Generic errors only: never leak
    // whether a vault path exists.
    let (pending, claim) = match state.uploads.claim_owned(&token) {
        Ok(claimed) => claimed,
        Err(ClaimError::Expired) => {
            return (StatusCode::GONE, "upload token expired").into_response();
        }
        Err(ClaimError::Unknown) | Err(ClaimError::InFlight) => {
            return (StatusCode::FORBIDDEN, "invalid upload token").into_response();
        }
    };

    if state
        .check_path(&pending.dest_path, deep_obsidian_control::Action::Write)
        .is_err()
    {
        return (StatusCode::FORBIDDEN, "access denied").into_response();
    }
    // Bridge the async body stream into the backend's bounded staging, which
    // pulls the chunks synchronously on its own blocking thread. Staging holds
    // NO note lock: it runs concurrently with the pump below and can take as
    // long as the network transfer does. The lock is acquired afterwards,
    // around the commit alone — the read/hash-check/swap critical section.
    let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(4);
    // Route to the mount owning the destination. The commit needs an owned handle
    // it can hold across the stream, so it takes the backend rather than issuing a
    // routed request. The path is rewritten to be mount-relative here, exactly as
    // `VaultRouter::execute` would.
    let (backend, dest_path) = match state.router.resolve(&pending.dest_path) {
        Ok(resolved) => (
            resolved.mount.backend.clone(),
            resolved.backend_relative_path.clone(),
        ),
        // A minted token always named a routable path (the mint validates it
        // through the router), so this is unreachable. Report it as an invalid
        // token rather than leaking anything about the mount table.
        Err(_) => {
            return (StatusCode::FORBIDDEN, "invalid upload token").into_response();
        }
    };
    let expected_hash = pending.expected_hash.clone();
    let max_bytes = pending.max_bytes;

    let stage = tokio::spawn(stage_stream_via_backend(
        backend.clone(),
        dest_path.clone(),
        max_bytes,
        deep_obsidian_backend::UploadChunks::new(chunk_rx.into_iter()),
    ));

    let mut stream = body.into_data_stream();
    let mut stream_error: Option<String> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => {
                if chunk_tx.send(Ok(bytes.to_vec())).is_err() {
                    // Receiver dropped (commit already aborted, e.g. oversize).
                    break;
                }
            }
            Err(error) => {
                stream_error = Some(error.to_string());
                let _ = chunk_tx.send(Err(error.to_string()));
                break;
            }
        }
    }
    drop(chunk_tx);

    let staged_handle = match stage.await {
        Ok(Ok(handle)) => handle,
        Ok(Err(error)) => {
            if let Some(stream_error) = stream_error {
                warn!("upload stream error: {stream_error}");
            }
            return match error {
                CommitError::TooLarge => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "upload exceeds maximum allowed size",
                )
                    .into_response(),
                CommitError::EscapesVault => {
                    (StatusCode::FORBIDDEN, "invalid upload token").into_response()
                }
                CommitError::Io(message) => {
                    warn!("upload staging error: {message}");
                    (StatusCode::INTERNAL_SERVER_ERROR, "upload failed").into_response()
                }
                CommitError::HashConflict { .. } => {
                    (StatusCode::CONFLICT, error.to_string()).into_response()
                }
            };
        }
        Err(join_error) => {
            warn!("upload staging task failed: {join_error}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "upload failed").into_response();
        }
    };

    // The commit is the only part that touches the destination: hold the
    // destination's per-note mutation lock across it and nothing else, so a
    // concurrent edit_note/upsert/rename on the same note serializes against
    // this swap instead of interleaving with it.
    let lock_guard =
        match crate::locks::lock_note(&state.mutation_locks, &state.router, &pending.dest_path)
            .await
        {
            Ok(guard) => guard,
            Err(message) => {
                warn!("upload destination unroutable at commit: {message}");
                return (StatusCode::FORBIDDEN, "invalid upload token").into_response();
            }
        };
    // Keep both the lock and token claim in the task that actually commits.
    // Dropping the request's JoinHandle detaches this work instead of unlocking
    // while a blocking filesystem operation or remote call is still running.
    let commit_response = match tokio::spawn(async move {
        let result =
            commit_staged_via_backend(backend, dest_path, staged_handle, expected_hash).await;
        if result.is_ok() {
            claim.consume();
        }
        drop(lock_guard);
        result
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(CommitError::Io(error.to_string())),
    };

    match commit_response {
        Ok(outcome) => (
            StatusCode::OK,
            Json(json!({
                "action": if outcome.created { "created" } else { "updated" },
                "path": pending.dest_path,
                "bytesWritten": outcome.bytes_written,
                "hash": outcome.hash,
            })),
        )
            .into_response(),
        Err(error) => {
            // Failed upload keeps the token alive until TTL so a transient
            // curl failure can retry.
            if let Some(stream_error) = stream_error {
                warn!("upload stream error: {stream_error}");
            }
            match error {
                CommitError::TooLarge => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "upload exceeds maximum allowed size",
                )
                    .into_response(),
                CommitError::HashConflict { .. } => {
                    (StatusCode::CONFLICT, error.to_string()).into_response()
                }
                CommitError::EscapesVault => {
                    (StatusCode::FORBIDDEN, "invalid upload token").into_response()
                }
                CommitError::Io(message) => {
                    warn!("upload io error: {message}");
                    (StatusCode::INTERNAL_SERVER_ERROR, "upload failed").into_response()
                }
            }
        }
    }
}

/// Build the externally reachable base URL for the upload endpoint. When the
/// configured host is a wildcard bind (`0.0.0.0` / `::`), present a loopback
/// address instead so the minted URL is actually dialable.
fn upload_base_url(config: &ResolvedServiceConfig) -> String {
    let host = match config.http.host.as_str() {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    format!("http://{}:{}", host, config.http.port)
}

pub async fn run_http_service(
    config: ResolvedServiceConfig,
) -> Result<ServiceBootstrapContext, io::Error> {
    let config = crate::runtime::resolve_embedding_contexts(config)
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if !matches!(config.transport, TransportMode::Http) {
        warn!(
            "HTTP service requested with non-http transport; coercing to HTTP for native runtime"
        );
    }

    // Build state first so the startup gate can run through the backend. Both
    // `MountRuntimes::new` and `AppState::new` are pure construction, so this does
    // not change what happens before the gate — only where the gate asks.
    // The backends are built ONCE and shared: the router serves reads through them
    // and each mount's index runtime reads through the same object, which for a
    // couchdb mount means the same supervised sidecar process.
    let backends = MountBackends::build(&config);
    let runtimes = MountRuntimes::new(&config, &backends);
    let state = crate::multiuser::attach(
        AppState::with_backends(config.clone(), runtimes.clone(), &backends)
            .with_upload_base(upload_base_url(&config)),
    )
    .map_err(io::Error::other)?;

    // Startup validation gate, applied PER MOUNT rather than through the router's
    // all-mounts overview.
    //
    // # Why the loop is here and not delegated
    //
    // `VaultRouter`'s `Health::Overview` fails on the FIRST mount that errors, whichever
    // mount that is. That was the right gate while every mount was a local directory: a
    // missing directory is a permanent configuration error and fail-fast puts it in front
    // of the operator. It is the wrong gate now that a mount can be remote, because a
    // remote backend that could not even be CONSTRUCTED — a missing secret, a missing
    // sidecar bundle — becomes an `UnavailableBackend` that errors on every request
    // including this one. Under the old gate that made such a mount fatal, which
    // contradicted the very message it logs ("the vault root keeps serving and readiness
    // reports the server as degraded") and which for a REMOTE ROOT would mean an
    // unreachable remote can brick a service.
    //
    // So the failure a mount is allowed to cause is decided by
    // [`crate::runtime::root_failure_is_fatal`] — the same predicate the index bootstrap
    // uses, so the two gates cannot drift. A local root still fails startup with the
    // backend's own wording, so the failure a user sees for a single-mount config is
    // unchanged. Anything else degrades: the mount is left in the router refusing its own
    // paths with its own reason, `/readyz` reports 503 naming it, and every healthy mount
    // serves normally.
    //
    // Note that a couchdb mount whose remote is merely DOWN never reached this branch even
    // before: its `Health::Overview` answers `Ok(reachable: false)` rather than erroring,
    // precisely so an outage is a readiness fact instead of a startup failure.
    for entry in backends.entries() {
        let Err(error) = entry
            .backend
            .execute(deep_obsidian_backend::BackendRequest::health_overview())
            .await
        else {
            continue;
        };
        if crate::runtime::root_failure_is_fatal(&entry.mount) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, error));
        }
        warn!(
            "mount '{}' is not serveable at startup: {error}; the server starts DEGRADED and \
readiness reports it as such",
            entry.mount.id
        );
    }

    let mut auth_state = resolve_auth_state(&config)?;
    if let Some(oauth) = auth_state.oauth.take() {
        let oauth = Arc::try_unwrap(oauth)
            .map_err(|_| io::Error::other("OAuth initialization shared too early"))?;
        auth_state.oauth = Some(Arc::new(
            oauth.with_control(
                state
                    .control
                    .clone()
                    .ok_or_else(|| io::Error::other("control store missing"))?,
            ),
        ));
    }
    enforce_auth_exposure(&config, auth_state.enabled)?;

    let endpoints = build_service_endpoints(&config);
    let state = state.with_auth(auth_state);
    state.start_mount_background();
    let control = crate::control::start(&state).map_err(io::Error::other)?;
    let live_state = state.clone();

    // Protected routes share an auth/origin middleware layer; health and
    // readiness are intentionally left open for liveness probes.
    let protected = Router::new()
        .route(config.http.mcp_path.as_str(), post(mcp_handler))
        .route(
            "/upload/{token}",
            // Disable axum's default 2MB body limit; our own per-token
            // `max_bytes` cap (enforced while streaming) is the sole authority.
            put(upload_handler).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_auth,
        ));
    let mut router = Router::new()
        .route(config.http.health_path.as_str(), get(health_handler))
        .merge(protected);
    if config.http.health_path != "/readyz" {
        router = router.route("/readyz", get(ready_handler));
    }
    if state.auth.oauth.is_some() {
        router = router.merge(crate::oauth::routes(&config.http.mcp_path));
    }
    let router = router.with_state(state);

    let addr: SocketAddr = format!("{}:{}", config.http.host, config.http.port)
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let listener = TcpListener::bind(addr).await?;
    info!("deep-obsidian-server native runtime listening on {}", addr);

    let server_handle = tokio::spawn(async move {
        let result = axum::serve(listener, router).await;
        if let Err(error) = &result {
            warn!("server exited with error: {error}");
        }
        result
    });
    // Every mount is refreshed and watched. A non-root mount that fails is logged
    // and left degraded rather than taking the service down with it.

    Ok(ServiceBootstrapContext {
        config,
        endpoints,
        live_state,
        server_handle,
        sidecars: backends.supervisors(),
        _control: control,
    })
}

pub async fn bootstrap_from_input(
    input: ServiceConfigInput,
) -> Result<ServiceBootstrapContext, Box<dyn std::error::Error + Send + Sync>> {
    let config = normalize_service_config(input)?;
    Ok(run_http_service(config).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_obsidian_types::{
        AuthConfig, AutoReindexConfig, EmbeddingConfig, HttpConfig, StdioMode,
    };
    use std::path::PathBuf;
    use std::sync::Mutex;

    // Serializes the env-var mutations below; these globals are process-wide.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct GatedUploadBackend {
        inner: std::sync::Arc<deep_obsidian_backend::FilesystemVaultBackend>,
        staged: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        committing: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl deep_obsidian_backend::VaultBackend for GatedUploadBackend {
        fn descriptor(&self) -> deep_obsidian_backend::BackendDescriptor {
            self.inner.descriptor()
        }
        fn storage_id(&self) -> String {
            self.inner.storage_id()
        }
        fn mutation_key(&self, path: &str) -> Result<String, deep_obsidian_backend::BackendError> {
            self.inner.mutation_key(path)
        }
        fn changes(
            &self,
            after: Option<deep_obsidian_backend::OpaqueCursor>,
        ) -> deep_obsidian_backend::ChangeStream {
            self.inner.changes(after)
        }
        async fn execute(
            &self,
            request: deep_obsidian_backend::BackendRequest,
        ) -> Result<deep_obsidian_backend::BackendResponse, deep_obsidian_backend::BackendError>
        {
            use deep_obsidian_backend::{BackendError, BackendRequest, MutationRequest};
            match &request {
                BackendRequest::Mutation(MutationRequest::StageUploadStream { .. }) => {
                    let result = self.inner.execute(request).await;
                    if let Some(tx) = self.staged.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    result
                }
                BackendRequest::Mutation(MutationRequest::CommitStagedUpload { .. }) => {
                    let started = self.committing.lock().unwrap().take().unwrap();
                    let release = self.release.lock().unwrap().take().unwrap();
                    let inner = self.inner.clone();
                    let runtime = tokio::runtime::Handle::current();
                    tokio::task::spawn_blocking(move || {
                        let _ = started.send(());
                        release
                            .recv()
                            .map_err(|error| BackendError::Message(error.to_string()))?;
                        runtime.block_on(inner.execute(request))
                    })
                    .await
                    .map_err(|error| BackendError::Message(error.to_string()))?
                }
                _ => self.inner.execute(request).await,
            }
        }
    }

    struct UploadFixture {
        state: AppState,
        token: String,
        root: PathBuf,
        staged: tokio::sync::oneshot::Receiver<()>,
        committing: tokio::sync::oneshot::Receiver<()>,
        release: std::sync::mpsc::Sender<()>,
    }

    fn upload_fixture() -> UploadFixture {
        use deep_obsidian_backend::{FilesystemVaultBackend, VaultRouter};
        use std::sync::Arc;
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "dob-upload-cancel-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("vault")).unwrap();
        std::fs::write(root.join("vault/Doc.md"), "# Doc\n\noriginal\n").unwrap();
        let mut config = config_with("127.0.0.1", false);
        config.vault_path = Some(root.join("vault"));
        config.index_dir = root.join("index");
        config.auto_reindex.enabled = false;
        let backends = MountBackends::build(&config);
        let runtimes = crate::runtime::MountRuntimes::new(&config, &backends);
        let mut state = AppState::with_backends(config, runtimes, &backends);
        let (stage_tx, staged) = tokio::sync::oneshot::channel();
        let (commit_tx, committing) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let backend = Arc::new(GatedUploadBackend {
            inner: Arc::new(FilesystemVaultBackend::new(root.join("vault"))),
            staged: Mutex::new(Some(stage_tx)),
            committing: Mutex::new(Some(commit_tx)),
            release: Mutex::new(Some(release_rx)),
        });
        state.router = Arc::new(VaultRouter::single("vault", backend.clone()));
        state.backend = backend;
        let token = state
            .uploads
            .mint(crate::uploads::PendingUpload {
                dest_path: "Doc.md".into(),
                expected_hash: None,
                max_bytes: 1024,
                expires_at: std::time::SystemTime::now() + crate::uploads::TOKEN_TTL,
                in_flight: false,
            })
            .unwrap();
        UploadFixture {
            state,
            token,
            root,
            staged,
            committing,
            release,
        }
    }

    #[tokio::test]
    async fn cancelled_upload_waiting_for_a_note_discards_staging_and_releases_its_claim() {
        let fixture = upload_fixture();
        let state = fixture.state.clone();
        let guard = crate::locks::lock_note(&state.mutation_locks, &state.router, "Doc.md")
            .await
            .unwrap();
        let token = fixture.token.clone();
        let task_state = state.clone();
        let upload = tokio::spawn(async move {
            upload_handler(
                State(task_state),
                AxumPath(token),
                None,
                Body::from("# Doc\n\nuploaded\n"),
            )
            .await
            .into_response()
        });
        fixture.staged.await.unwrap();
        upload.abort();
        let _ = upload.await;
        assert!(
            state.uploads.claim(&fixture.token).is_ok(),
            "the token must be retryable"
        );
        state.uploads.release(&fixture.token);
        let staging_count = std::fs::read_dir(fixture.root.join("vault"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".upload-"))
            .count();
        assert_eq!(staging_count, 0);
        drop(guard);
        assert_eq!(state.mutation_locks.live_keys(), 0);
        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    #[tokio::test]
    async fn cancelled_upload_keeps_the_note_locked_until_its_blocking_commit_finishes() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let fixture = upload_fixture();
        let state = fixture.state.clone();
        let token = fixture.token.clone();
        let task_state = state.clone();
        let upload = tokio::spawn(async move {
            upload_handler(
                State(task_state),
                AxumPath(token),
                None,
                Body::from("# Doc\n\nuploaded\n"),
            )
            .await
            .into_response()
        });
        fixture.committing.await.unwrap();
        upload.abort();
        let _ = upload.await;
        assert!(matches!(
            state.uploads.claim(&fixture.token),
            Err(crate::uploads::ClaimError::InFlight)
        ));
        let request = serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"edit_note","arguments":{"path":"./Doc.md","edits":[{"old":"uploaded","new":"patched"}]}}
        })).unwrap();
        let mut edit = Box::pin(crate::mcp::handle_request(state.clone(), request));
        assert!(matches!(
            edit.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        fixture.release.send(()).unwrap();
        let response = edit.await.unwrap().unwrap();
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("vault/Doc.md")).unwrap(),
            "# Doc\n\npatched\n"
        );
        assert!(matches!(
            state.uploads.claim(&fixture.token),
            Err(crate::uploads::ClaimError::Unknown)
        ));
        assert_eq!(state.mutation_locks.live_keys(), 0);
        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    fn config_with(host: &str, auth_enabled: bool) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            freshness: Default::default(),
            federated_rerank: true,
            vault_path: Some(PathBuf::from("/tmp/vault")),
            index_dir: PathBuf::from("/tmp/index"),
            mounts: Vec::new(),
            experimental: Default::default(),
            transport: TransportMode::Http,
            stdio_mode: StdioMode::Auto,
            http: HttpConfig {
                host: host.to_string(),
                port: 4100,
                mcp_path: "/mcp".to_string(),
                health_path: "/healthz".to_string(),
            },
            auto_reindex: AutoReindexConfig::default(),
            embedding: EmbeddingConfig::default(),
            artifact_embedding: EmbeddingConfig::default(),
            auth: AuthConfig {
                enabled: auth_enabled,
                ..AuthConfig::default()
            },
            config_file_path: None,
        }
    }

    #[test]
    fn exposure_allows_loopback_without_auth() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_INSECURE_ENV);
        assert!(enforce_auth_exposure(&config_with("127.0.0.1", false), false).is_ok());
        assert!(enforce_auth_exposure(&config_with("localhost", false), false).is_ok());
    }

    #[test]
    fn exposure_allows_non_loopback_with_auth() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_INSECURE_ENV);
        assert!(enforce_auth_exposure(&config_with("0.0.0.0", true), true).is_ok());
    }

    #[test]
    fn exposure_rejects_non_loopback_without_auth() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_INSECURE_ENV);
        assert!(enforce_auth_exposure(&config_with("0.0.0.0", false), false).is_err());
    }

    #[test]
    fn exposure_escape_hatch_allows_insecure_bind() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var(ALLOW_INSECURE_ENV, "1");
        let result = enforce_auth_exposure(&config_with("0.0.0.0", false), false);
        std::env::remove_var(ALLOW_INSECURE_ENV);
        assert!(result.is_ok());
    }

    #[test]
    fn env_token_enables_auth_and_overrides_config() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var(AUTH_TOKEN_ENV, "env-token");
        let state = resolve_auth_state(&config_with("0.0.0.0", false));
        std::env::remove_var(AUTH_TOKEN_ENV);
        let state = state.expect("auth state");
        assert!(state.enabled);
        assert!(state.token.is_some());
    }

    #[test]
    fn disabled_auth_resolves_to_disabled_state() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var(AUTH_TOKEN_ENV);
        let state = resolve_auth_state(&config_with("127.0.0.1", false)).expect("auth state");
        assert!(!state.enabled);
        assert!(state.token.is_none());
    }

    // End-to-end proof of the serve-side path: a token stored in the shared
    // secret store (encrypted-file backend, the headless-representative path) is
    // resolved back through `resolve_auth_state` exactly as the HTTP bootstrap
    // does at startup. `SecretResolver::new()` reads the default secrets path,
    // which honours `XDG_CONFIG_HOME`, so point it at a temp dir.
    #[test]
    fn enabled_auth_resolves_token_from_encrypted_file_store() {
        use deep_obsidian_config::secrets::SecretResolver;
        use deep_obsidian_types::SecretRef;
        use secrecy::ExposeSecret;

        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var(AUTH_TOKEN_ENV);

        let dir = std::env::temp_dir().join(format!(
            "deep-obsidian-auth-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let token = crate::auth::generate_token();
        let reference = SecretRef::EncryptedFile {
            id: "http-auth-token".to_string(),
        };
        SecretResolver::new()
            .put(&reference, secrecy::SecretString::new(token.clone()))
            .expect("store token");

        let mut config = config_with("0.0.0.0", true);
        config.auth.token_ref = Some(reference);

        let state = resolve_auth_state(&config);

        // Restore env before asserting so a failure can't leak XDG_CONFIG_HOME.
        match previous_xdg {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);

        let state = state.expect("auth state resolves from store");
        assert!(state.enabled);
        assert_eq!(state.token.expect("token present").expose_secret(), &token);
    }
    #[test]
    fn oauth_fails_closed_without_a_resolved_owner_secret() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var(AUTH_TOKEN_ENV);
        let mut config = config_with("127.0.0.1", false);
        config.auth.oauth = Some(deep_obsidian_types::OAuthConfig {
            issuer_url: "https://server.example".into(),
            access_token_ttl_seconds: 3600,
            refresh_token_ttl_seconds: deep_obsidian_types::default_oauth_refresh_ttl(),
        });
        let error = resolve_auth_state(&config)
            .err()
            .expect("OAuth must never be anonymous");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
