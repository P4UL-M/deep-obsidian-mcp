use std::io;
use std::net::SocketAddr;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
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
    let state = AppState::new(config.clone(), runtime.clone());
    let mut router = Router::new()
        .route(config.http.health_path.as_str(), get(health_handler))
        .route(config.http.mcp_path.as_str(), post(mcp_handler));
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
