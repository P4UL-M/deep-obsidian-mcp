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
use crate::health::{build_health_payload, build_readiness_payload, readiness_status_code};
use crate::mcp::{handle_request, AppState};
use crate::protocol::JsonRpcRequest;
use crate::runtime::{AutoReindexHandle, RuntimeState};
use crate::vault::ensure_vault_path;

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
    let allowed_origins = Arc::new(config.auth.allowed_origins.clone());

    if let Ok(token) = std::env::var(AUTH_TOKEN_ENV) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            info!("HTTP authentication enabled via {AUTH_TOKEN_ENV}");
            return Ok(AuthState {
                enabled: true,
                token: Some(SecretString::new(token)),
                allowed_origins,
            });
        }
    }

    if !config.auth.enabled {
        return Ok(AuthState {
            enabled: false,
            token: None,
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
    pub auto_reindex: Option<AutoReindexHandle>,
    pub initial_index: tokio::task::JoinHandle<()>,
    pub server_handle: tokio::task::JoinHandle<io::Result<()>>,
}

impl Drop for ServiceBootstrapContext {
    fn drop(&mut self) {
        self.initial_index.abort();
        self.server_handle.abort();
    }
}

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let diagnostics = state.runtime.diagnostics();
    (
        StatusCode::OK,
        Json(build_health_payload(&state.config, &diagnostics)),
    )
        .into_response()
}

async fn ready_handler(State(state): State<AppState>) -> impl IntoResponse {
    let diagnostics = state.runtime.diagnostics();
    (
        readiness_status_code(&diagnostics),
        Json(build_readiness_payload(&state.config, &diagnostics)),
    )
        .into_response()
}

async fn mcp_handler(
    State(state): State<AppState>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
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
    body: Body,
) -> impl IntoResponse {
    use crate::uploads::{commit_stream, ClaimError, CommitError};

    // Atomically claim the token (or reject). Generic errors only: never leak
    // whether a vault path exists.
    let pending = match state.uploads.claim(&token) {
        Ok(pending) => pending,
        Err(ClaimError::Expired) => {
            return (StatusCode::GONE, "upload token expired").into_response();
        }
        Err(ClaimError::Unknown) | Err(ClaimError::InFlight) => {
            return (StatusCode::FORBIDDEN, "invalid upload token").into_response();
        }
    };

    // Bridge the async body stream into a synchronous, bounded commit on a
    // blocking thread. The lock is NOT held during streaming.
    let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(4);
    let vault_path = state.config.vault_path.clone();
    let dest_path = pending.dest_path.clone();
    let expected_hash = pending.expected_hash.clone();
    let max_bytes = pending.max_bytes;

    let commit = tokio::task::spawn_blocking(move || {
        commit_stream(
            &vault_path,
            &dest_path,
            expected_hash.as_deref(),
            max_bytes,
            chunk_rx.into_iter(),
        )
    });

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

    let outcome = match commit.await {
        Ok(result) => result,
        Err(join_error) => {
            state.uploads.release(&token);
            warn!("upload commit task failed: {join_error}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "upload failed").into_response();
        }
    };

    match outcome {
        Ok(outcome) => {
            // Only a successful commit consumes the token.
            state.uploads.consume(&token);
            (
                StatusCode::OK,
                Json(json!({
                    "action": if outcome.created { "created" } else { "updated" },
                    "path": pending.dest_path,
                    "bytesWritten": outcome.bytes_written,
                    "hash": outcome.hash,
                })),
            )
                .into_response()
        }
        Err(error) => {
            // Failed upload keeps the token alive until TTL so a transient
            // curl failure can retry.
            state.uploads.release(&token);
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
    if !matches!(config.transport, TransportMode::Http) {
        warn!(
            "HTTP service requested with non-http transport; coercing to HTTP for native runtime"
        );
    }

    ensure_vault_path(&config.vault_path)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    let auth_state = resolve_auth_state(&config)?;
    enforce_auth_exposure(&config, auth_state.enabled)?;

    let endpoints = build_service_endpoints(&config);
    let runtime = RuntimeState::new(config.clone());
    let state = AppState::new(config.clone(), runtime.clone())
        .with_upload_base(upload_base_url(&config))
        .with_auth(auth_state);

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
    let initial_index = runtime.start_initial_refresh();
    let auto_reindex = if runtime.config().auto_reindex.enabled {
        Some(crate::runtime::start_auto_reindex_tasks(runtime.clone()))
    } else {
        None
    };

    Ok(ServiceBootstrapContext {
        config,
        endpoints,
        auto_reindex,
        initial_index,
        server_handle,
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

    fn config_with(host: &str, auth_enabled: bool) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            vault_path: PathBuf::from("/tmp/vault"),
            index_dir: PathBuf::from("/tmp/index"),
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
        assert_eq!(
            state.token.expect("token present").expose_secret(),
            &token
        );
    }
}
