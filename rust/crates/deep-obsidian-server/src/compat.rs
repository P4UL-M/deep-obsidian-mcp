use std::env;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use deep_obsidian_types::{EmbeddingProvider, ResolvedServiceConfig, ServiceEndpoints, TransportMode};
use thiserror::Error;
use tokio::process::Child;
use tokio::time::sleep;

const BACKEND_STARTUP_TIMEOUT_MS: u64 = 15_000;
const BACKEND_STARTUP_POLL_MS: u64 = 200;

#[derive(Debug, Clone)]
pub struct NodeServeInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

impl NodeServeInvocation {
    pub fn into_std_command(self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        command.current_dir(&self.cwd);
        for (key, value) in self.env {
            command.env(key, value);
        }
        command
    }

    pub fn into_tokio_command(self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.args);
        command.current_dir(&self.cwd);
        for (key, value) in self.env {
            command.env(key, value);
        }
        command
    }
}

pub struct NodeCompatibilityBackend {
    child: Child,
    endpoints: ServiceEndpoints,
}

impl NodeCompatibilityBackend {
    pub fn endpoints(&self) -> &ServiceEndpoints {
        &self.endpoints
    }

    pub fn child_id(&self) -> Option<u32> {
        self.child.id()
    }
}

#[derive(Debug, Error)]
pub enum CompatError {
    #[error("failed to resolve the repository root from {0}")]
    InvalidRepoRoot(PathBuf),
    #[error("missing TypeScript compatibility entrypoint: {0}")]
    MissingEntrypoint(PathBuf),
    #[error("failed to reserve a local port for the TypeScript compatibility backend: {0}")]
    PortReservation(#[source] std::io::Error),
    #[error("failed to spawn the TypeScript compatibility backend: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("TypeScript compatibility backend exited before becoming healthy (status: {0})")]
    EarlyExit(std::process::ExitStatus),
    #[error("timed out waiting for the TypeScript compatibility backend at {0}")]
    StartupTimeout(String),
    #[error("failed to build the compatibility HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
}

pub fn node_serve_invocation(
    config: &ResolvedServiceConfig,
    transport: TransportMode,
    host_override: Option<&str>,
    port_override: Option<u16>,
) -> Result<NodeServeInvocation, CompatError> {
    let repo_root = repo_root()?;
    let entrypoint = repo_root.join("dist").join("index.js");
    if !entrypoint.is_file() {
        return Err(CompatError::MissingEntrypoint(entrypoint));
    }

    let host = host_override.unwrap_or(config.http.host.as_str());
    let port = port_override.unwrap_or(config.http.port);

    let mut args = vec![
        entrypoint.to_string_lossy().to_string(),
        "serve".to_string(),
        "--transport".to_string(),
        transport_mode_arg(transport).to_string(),
        "--vault".to_string(),
        config.vault_path.to_string_lossy().to_string(),
        "--index-dir".to_string(),
        config.index_dir.to_string_lossy().to_string(),
        "--stdio-mode".to_string(),
        stdio_mode_arg(config.stdio_mode).to_string(),
        format!("--auto-reindex={}", config.auto_reindex.enabled),
        "--reindex-debounce-ms".to_string(),
        config.auto_reindex.debounce_ms.to_string(),
        "--reindex-interval-ms".to_string(),
        config.auto_reindex.interval_ms.to_string(),
    ];

    if matches!(transport, TransportMode::Http) {
        args.extend([
            "--host".to_string(),
            host.to_string(),
            "--port".to_string(),
            port.to_string(),
            "--mcp-path".to_string(),
            config.http.mcp_path.clone(),
            "--health-path".to_string(),
            config.http.health_path.clone(),
        ]);
    }

    if let Some(provider) = config.embedding.provider.as_ref() {
        args.extend([
            "--embedding-provider".to_string(),
            embedding_provider_arg(provider).to_string(),
        ]);
    }
    if let Some(model) = config.embedding.model.as_ref() {
        args.extend(["--embedding-model".to_string(), model.clone()]);
    }
    if let Some(base_url) = config.embedding.base_url.as_ref() {
        args.extend(["--embedding-base-url".to_string(), base_url.clone()]);
    }
    if let Some(api_key_env) = config.embedding.api_key_env.as_ref() {
        args.extend(["--embedding-api-key-env".to_string(), api_key_env.clone()]);
    }

    let mut env_vars = Vec::new();
    if let Some(api_key) = config.embedding.api_key.as_ref() {
        env_vars.push(("DEEP_OBSIDIAN_EMBEDDING_API_KEY".to_string(), api_key.clone()));
    }

    Ok(NodeServeInvocation {
        program: env::var("NODE_BINARY").unwrap_or_else(|_| "node".to_string()),
        args,
        cwd: repo_root,
        env: env_vars,
    })
}

pub async fn spawn_http_backend(config: &ResolvedServiceConfig) -> Result<NodeCompatibilityBackend, CompatError> {
    let port = reserve_local_port()?;
    let invocation = node_serve_invocation(config, TransportMode::Http, Some("127.0.0.1"), Some(port))?;
    let endpoints = ServiceEndpoints {
        mcp: format!("http://127.0.0.1:{port}{}", config.http.mcp_path),
        health: format!("http://127.0.0.1:{port}{}", config.http.health_path),
    };

    let mut command = invocation.into_tokio_command();
    command.kill_on_drop(true);
    command.stdout(Stdio::null());
    command.stderr(Stdio::inherit());

    let mut child = command.spawn().map_err(CompatError::Spawn)?;
    wait_for_backend_ready(&mut child, &endpoints.health).await?;

    Ok(NodeCompatibilityBackend { child, endpoints })
}

async fn wait_for_backend_ready(child: &mut Child, health_url: &str) -> Result<(), CompatError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(BACKEND_STARTUP_POLL_MS))
        .build()
        .map_err(CompatError::HttpClient)?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(BACKEND_STARTUP_TIMEOUT_MS);

    loop {
        if let Some(status) = child.try_wait().map_err(CompatError::Spawn)? {
            return Err(CompatError::EarlyExit(status));
        }

        match client.get(health_url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            _ => {}
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(CompatError::StartupTimeout(health_url.to_string()));
        }

        sleep(Duration::from_millis(BACKEND_STARTUP_POLL_MS)).await;
    }
}

fn reserve_local_port() -> Result<u16, CompatError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(CompatError::PortReservation)?;
    let port = listener
        .local_addr()
        .map_err(CompatError::PortReservation)?
        .port();
    drop(listener);
    Ok(port)
}

fn repo_root() -> Result<PathBuf, CompatError> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .nth(3)
        .map(PathBuf::from)
        .ok_or(CompatError::InvalidRepoRoot(manifest_dir))
}

fn transport_mode_arg(transport: TransportMode) -> &'static str {
    match transport {
        TransportMode::Stdio => "stdio",
        TransportMode::Http => "http",
    }
}

fn stdio_mode_arg(mode: deep_obsidian_types::StdioMode) -> &'static str {
    match mode {
        deep_obsidian_types::StdioMode::Auto => "auto",
        deep_obsidian_types::StdioMode::Newline => "newline",
        deep_obsidian_types::StdioMode::Framed => "framed",
    }
}

fn embedding_provider_arg(provider: &EmbeddingProvider) -> &'static str {
    match provider {
        EmbeddingProvider::OpenAiCompatible => "openai-compatible",
    }
}
