use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{
    header::{HeaderName, CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING, UPGRADE},
    HeaderMap, Request, Response, StatusCode,
};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use deep_obsidian_config::{build_service_endpoints, default_config_path, normalize_service_config};
use deep_obsidian_types::{HttpConfigInput, ResolvedServiceConfig, ServiceConfigInput, ServiceEndpoints, TransportMode};
use reqwest::Client;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::compat::{spawn_http_backend, NodeCompatibilityBackend};
use crate::vault::ensure_vault_path;

#[derive(Clone)]
struct ProxyState {
    client: Client,
    backend_mcp: String,
    backend_health: String,
}

pub struct ServiceBootstrapContext {
    pub config: ResolvedServiceConfig,
    pub endpoints: ServiceEndpoints,
    pub backend_pid: Option<u32>,
    _compat_backend: NodeCompatibilityBackend,
}

async fn health_handler(State(state): State<ProxyState>, request: Request<Body>) -> impl IntoResponse {
    proxy_request(&state.client, &state.backend_health, request).await
}

async fn mcp_handler(State(state): State<ProxyState>, request: Request<Body>) -> impl IntoResponse {
    proxy_request(&state.client, &state.backend_mcp, request).await
}

pub async fn run_http_service(config: ResolvedServiceConfig) -> Result<ServiceBootstrapContext, io::Error> {
    if !matches!(config.transport, TransportMode::Http) {
        warn!("HTTP service requested with non-http transport; coercing to HTTP for compatibility mode");
    }

    ensure_vault_path(&config.vault_path)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    let compat_backend = spawn_http_backend(&config)
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
    let backend_pid = compat_backend.child_id();
    let backend_endpoints = compat_backend.endpoints().clone();
    let endpoints = build_service_endpoints(&config);

    let state = ProxyState {
        client: Client::builder()
            .build()
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?,
        backend_mcp: backend_endpoints.mcp.clone(),
        backend_health: backend_endpoints.health.clone(),
    };

    let router = Router::new()
        .route(config.http.health_path.as_str(), any(health_handler))
        .route(config.http.mcp_path.as_str(), any(mcp_handler))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", config.http.host, config.http.port)
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let listener = TcpListener::bind(addr).await?;
    info!(
        "deep-obsidian-server compatibility proxy listening on {} (backend pid={:?})",
        addr, backend_pid
    );

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            warn!("server exited with error: {error}");
        }
    });

    Ok(ServiceBootstrapContext {
        config,
        endpoints,
        backend_pid,
        _compat_backend: compat_backend,
    })
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

async fn proxy_request(client: &Client, backend_url: &str, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, format!("failed to read request body: {error}")),
    };
    let target_url = with_query(backend_url, parts.uri.query());

    let mut builder = client.request(parts.method.clone(), target_url);
    for (name, value) in filtered_headers(&parts.headers) {
        builder = builder.header(name, value);
    }

    let response = match builder.body(body).send().await {
        Ok(response) => response,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("compatibility backend request failed: {error}"),
            )
        }
    };

    let status = response.status();
    let headers = response.headers().clone();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("failed to read compatibility backend response: {error}"),
            )
        }
    };

    let mut builder = Response::builder().status(status);
    for (name, value) in filtered_headers(&headers) {
        builder = builder.header(name, value);
    }

    match builder.body(Body::from(bytes)) {
        Ok(response) => response,
        Err(error) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("failed to build proxied response: {error}"),
        ),
    }
}

static KEEP_ALIVE_HEADER: HeaderName = HeaderName::from_static("keep-alive");

fn filtered_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| *name != HOST)
        .filter(|(name, _)| *name != CONNECTION)
        .filter(|(name, _)| *name != KEEP_ALIVE_HEADER)
        .filter(|(name, _)| *name != TRANSFER_ENCODING)
        .filter(|(name, _)| *name != CONTENT_LENGTH)
        .filter(|(name, _)| *name != UPGRADE)
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

fn with_query(base: &str, query: Option<&str>) -> String {
    match query {
        Some(query) if !query.is_empty() => format!("{base}?{query}"),
        _ => base.to_string(),
    }
}

fn error_response(status: StatusCode, message: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "status": "error",
                "message": message,
            })
            .to_string(),
        ))
        .expect("error response to build")
}
