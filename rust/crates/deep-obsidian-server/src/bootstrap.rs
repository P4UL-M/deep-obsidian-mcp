use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use deep_obsidian_config::{build_service_endpoints, default_config_path, normalize_service_config};
use deep_obsidian_types::{HttpConfigInput, ResolvedServiceConfig, ServiceConfigInput, ServiceEndpoints, TransportMode};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::mcp::{handle_request, AppState};
use crate::protocol::JsonRpcRequest;
use crate::vault::{ensure_vault_path, vault_info};

#[derive(Debug, Clone)]
pub struct ServiceBootstrapContext {
    pub config: ResolvedServiceConfig,
    pub endpoints: ServiceEndpoints,
}

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    match vault_info(&state.config.vault_path) {
        Ok(info) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "vaultPath": info.vault_path,
                "markdownFileCount": info.markdown_file_count,
                "generatedAt": serde_json::Value::Null,
                "semanticBackend": "prototype",
                "autoReindex": state.config.auto_reindex.enabled,
                "service": info.service,
                "prototype": info.prototype,
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": error.to_string(),
            })),
        )
            .into_response(),
    }
}

async fn mcp_handler(State(state): State<AppState>, Json(request): Json<JsonRpcRequest>) -> impl IntoResponse {
    match handle_request(state, request).await {
        Ok(Some(response)) => (StatusCode::OK, Json(response)).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (StatusCode::OK, Json(error)).into_response(),
    }
}

pub async fn run_http_service(config: ResolvedServiceConfig) -> Result<ServiceBootstrapContext, std::io::Error> {
    if !matches!(config.transport, TransportMode::Http) {
        warn!("HTTP service requested with non-http transport; coercing to HTTP for the prototype");
    }

    ensure_vault_path(&config.vault_path)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;

    let endpoints = build_service_endpoints(&config);
    let state = AppState::new(config.clone());
    let router = Router::new()
        .route(config.http.health_path.as_str(), get(health_handler))
        .route(config.http.mcp_path.as_str(), post(mcp_handler))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", config.http.host, config.http.port)
        .parse()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let listener = TcpListener::bind(addr).await?;
    info!("deep-obsidian-server listening on {}", addr);

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            warn!("server exited with error: {error}");
        }
    });

    Ok(ServiceBootstrapContext { config, endpoints })
}

pub async fn bootstrap_from_input(
    input: ServiceConfigInput,
) -> Result<ServiceBootstrapContext, Box<dyn std::error::Error + Send + Sync>> {
    let config = normalize_service_config(input)?;
    Ok(run_http_service(config).await?)
}

pub fn prototype_config(vault_path: PathBuf) -> ServiceConfigInput {
    ServiceConfigInput {
        vault_path: Some(vault_path),
        index_dir: None,
        transport: Some(TransportMode::Http),
        stdio_mode: None,
        http: Some(HttpConfigInput {
            host: Some("127.0.0.1".to_string()),
            port: Some(4100),
            mcp_path: Some("/mcp".to_string()),
            health_path: Some("/healthz".to_string()),
        }),
        auto_reindex: None,
        embedding: None,
        config_file_path: Some(default_config_path()),
    }
}
