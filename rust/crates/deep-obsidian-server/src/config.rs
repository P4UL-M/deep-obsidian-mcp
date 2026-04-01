use std::env;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    Stdio,
    Http,
}

impl Default for TransportMode {
    fn default() -> Self {
        Self::Http
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub mcp_path: String,
    pub health_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoReindexConfig {
    pub enabled: bool,
    pub debounce_ms: u64,
    pub interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EmbeddingConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedServiceConfig {
    pub vault_path: PathBuf,
    pub index_dir: PathBuf,
    pub transport: TransportMode,
    pub http: HttpConfig,
    pub auto_reindex: AutoReindexConfig,
    pub embedding: EmbeddingConfig,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpConfigInput {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub mcp_path: Option<String>,
    pub health_path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoReindexConfigInput {
    pub enabled: Option<bool>,
    pub debounce_ms: Option<u64>,
    pub interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfigInput {
    pub vault_path: Option<PathBuf>,
    pub index_dir: Option<PathBuf>,
    pub transport: Option<TransportMode>,
    pub http: Option<HttpConfigInput>,
    pub auto_reindex: Option<AutoReindexConfigInput>,
    pub embedding: Option<EmbeddingConfig>,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing vault path")]
    MissingVaultPath,
    #[error("invalid port: {0}")]
    InvalidPort(u16),
    #[error("invalid vault-relative path: {0}")]
    InvalidVaultRelativePath(String),
}

pub const DEFAULT_HTTP_HOST: &str = "127.0.0.1";
pub const DEFAULT_HTTP_PORT: u16 = 4100;
pub const DEFAULT_HTTP_MCP_PATH: &str = "/mcp";
pub const DEFAULT_HTTP_HEALTH_PATH: &str = "/healthz";
pub const DEFAULT_AUTO_REINDEX_DEBOUNCE_MS: u64 = 1500;
pub const DEFAULT_AUTO_REINDEX_INTERVAL_MS: u64 = 30000;

pub fn normalize_http_path(value: Option<&str>, fallback_value: &str) -> String {
    let candidate = value.unwrap_or(fallback_value).trim();
    if candidate.is_empty() || candidate == "/" {
        return "/".to_string();
    }
    format!("/{}", candidate.trim_start_matches('/').trim_end_matches('/'))
}

pub fn default_index_dir(vault_path: &Path) -> PathBuf {
    vault_path.join(".deep-obsidian-mcp")
}

pub fn default_config_path() -> PathBuf {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("deep-obsidian-mcp").join("config.json")
}

fn normalize_port(value: Option<u16>) -> Result<u16, ConfigError> {
    let port = value.unwrap_or(DEFAULT_HTTP_PORT);
    if port == 0 {
        return Err(ConfigError::InvalidPort(port));
    }
    Ok(port)
}

fn normalize_path_segment(value: Option<String>) -> String {
    let trimmed = value.unwrap_or_default().trim().to_string();
    if trimmed.is_empty() {
        DEFAULT_HTTP_HOST.to_string()
    } else {
        trimmed
    }
}

pub fn normalize_service_config(input: ServiceConfigInput) -> Result<ResolvedServiceConfig, ConfigError> {
    let vault_path = input.vault_path.ok_or(ConfigError::MissingVaultPath)?;
    let index_dir = input.index_dir.unwrap_or_else(|| default_index_dir(&vault_path));
    let http_input = input.http.unwrap_or_default();
    let auto_input = input.auto_reindex.unwrap_or_default();
    let embedding = input.embedding.unwrap_or_default();

    let transport = input.transport.unwrap_or_default();
    let http = HttpConfig {
        host: normalize_path_segment(http_input.host),
        port: normalize_port(http_input.port)?,
        mcp_path: normalize_http_path(http_input.mcp_path.as_deref(), DEFAULT_HTTP_MCP_PATH),
        health_path: normalize_http_path(http_input.health_path.as_deref(), DEFAULT_HTTP_HEALTH_PATH),
    };

    Ok(ResolvedServiceConfig {
        vault_path,
        index_dir,
        transport,
        http,
        auto_reindex: AutoReindexConfig {
            enabled: auto_input.enabled.unwrap_or(true),
            debounce_ms: auto_input.debounce_ms.unwrap_or(DEFAULT_AUTO_REINDEX_DEBOUNCE_MS),
            interval_ms: auto_input.interval_ms.unwrap_or(DEFAULT_AUTO_REINDEX_INTERVAL_MS),
        },
        embedding,
        config_file_path: input.config_file_path,
    })
}

pub fn is_valid_vault_relative_path(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }

    let path = Path::new(value);
    if path.is_absolute() {
        return false;
    }

    !path.components().any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
}
