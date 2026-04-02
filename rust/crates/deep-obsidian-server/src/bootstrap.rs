use std::io;
use std::net::SocketAddr;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use deep_obsidian_config::{build_service_endpoints, normalize_service_config};
use deep_obsidian_types::{ResolvedServiceConfig, ServiceConfigInput, ServiceEndpoints, TransportMode};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::health::build_health_payload;
use crate::mcp::{handle_request, AppState};
use crate::protocol::JsonRpcRequest;
use crate::runtime::{AutoReindexHandle, RuntimeState};
use crate::vault::ensure_vault_path;

pub struct ServiceBootstrapContext {
    pub config: ResolvedServiceConfig,
    pub endpoints: ServiceEndpoints,
    pub auto_reindex: Option<AutoReindexHandle>,
}

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    match state.runtime.refresh("health").await {
        Ok(snapshot) => (StatusCode::OK, Json(build_health_payload(&state.config, &snapshot))).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status":"error","error":error})),
        )
            .into_response(),
    }
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
            Json(serde_json::to_value(error).unwrap_or_else(|_| json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32603, "message": "internal server error" }
            }))),
        )
            .into_response(),
    }
}

pub async fn run_http_service(config: ResolvedServiceConfig) -> Result<ServiceBootstrapContext, io::Error> {
    if !matches!(config.transport, TransportMode::Http) {
        warn!("HTTP service requested with non-http transport; coercing to HTTP for native runtime");
    }

    ensure_vault_path(&config.vault_path)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    let endpoints = build_service_endpoints(&config);
    let (runtime, auto_reindex) = RuntimeState::bootstrap(config.clone())
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
    let state = AppState::new(config.clone(), runtime);
    let router = Router::new()
        .route(config.http.health_path.as_str(), get(health_handler))
        .route(config.http.mcp_path.as_str(), post(mcp_handler))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", config.http.host, config.http.port)
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let listener = TcpListener::bind(addr).await?;
    info!("deep-obsidian-server native runtime listening on {}", addr);

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            warn!("server exited with error: {error}");
        }
    });

    Ok(ServiceBootstrapContext {
        config,
        endpoints,
        auto_reindex,
    })
}

pub async fn bootstrap_from_input(
    input: ServiceConfigInput,
) -> Result<ServiceBootstrapContext, Box<dyn std::error::Error + Send + Sync>> {
    let config = normalize_service_config(input)?;
    Ok(run_http_service(config).await?)
}
