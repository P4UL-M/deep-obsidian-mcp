use std::io;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::StreamExt as _;
use deep_obsidian_config::{build_service_endpoints, normalize_service_config};
use deep_obsidian_types::{
    ResolvedServiceConfig, ServiceConfigInput, ServiceEndpoints, TransportMode,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::health::{build_health_payload, build_readiness_payload, readiness_status_code};
use crate::mcp::{handle_request, AppState};
use crate::protocol::JsonRpcRequest;
use crate::runtime::{AutoReindexHandle, RuntimeState};
use crate::vault::ensure_vault_path;

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

    let endpoints = build_service_endpoints(&config);
    let runtime = RuntimeState::new(config.clone());
    let state =
        AppState::new(config.clone(), runtime.clone()).with_upload_base(upload_base_url(&config));
    let mut router = Router::new()
        .route(config.http.health_path.as_str(), get(health_handler))
        .route(config.http.mcp_path.as_str(), post(mcp_handler))
        .route(
            "/upload/{token}",
            // Disable axum's default 2MB body limit; our own per-token
            // `max_bytes` cap (enforced while streaming) is the sole authority.
            put(upload_handler).layer(axum::extract::DefaultBodyLimit::disable()),
        );
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
