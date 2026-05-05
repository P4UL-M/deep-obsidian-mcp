use deep_obsidian_types::{
    AutoReindexConfig, AutoReindexConfigInput, EmbeddingConfig, EmbeddingConfigInput,
    EmbeddingProvider, HttpConfig, HttpConfigInput, PersistedServiceConfig, ResolvedServiceConfig,
    ServiceConfigInput, StdioMode, TransportMode,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const DEFAULT_CONFIG_DIR_NAME: &str = ".config";
pub const DEFAULT_CONFIG_APP_DIR: &str = "deep-obsidian-mcp";
pub const DEFAULT_CONFIG_FILE_NAME: &str = "config.json";
pub const DEFAULT_HTTP_HOST: &str = "127.0.0.1";
pub const DEFAULT_HTTP_PORT: u16 = 4100;
pub const DEFAULT_HTTP_MCP_PATH: &str = "/mcp";
pub const DEFAULT_HTTP_HEALTH_PATH: &str = "/healthz";
pub const DEFAULT_AUTO_REINDEX_DEBOUNCE_MS: u64 = 1500;
pub const DEFAULT_AUTO_REINDEX_INTERVAL_MS: u64 = 30000;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing vault path")]
    MissingVaultPath,
    #[error("invalid transport mode for HTTP service: {0:?}")]
    InvalidTransport(TransportMode),
    #[error("failed to read config file {path}: {source}")]
    ReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write config file {path}: {source}")]
    WriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    ParseFailed {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("failed to serialize config file {path}: {source}")]
    SerializeFailed {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_config_dir() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    home_dir().join(DEFAULT_CONFIG_DIR_NAME)
}

pub fn default_config_path() -> PathBuf {
    default_config_dir()
        .join(DEFAULT_CONFIG_APP_DIR)
        .join(DEFAULT_CONFIG_FILE_NAME)
}

pub fn default_index_dir(vault_path: &Path) -> PathBuf {
    vault_path.join(".deep-obsidian-mcp")
}

pub fn default_packaged_index_dir(vault_path: &Path) -> PathBuf {
    home_dir()
        .join("Library")
        .join("Application Support")
        .join(DEFAULT_CONFIG_APP_DIR)
        .join("indexes")
        .join(stable_vault_hash(vault_path))
}

fn stable_vault_hash(vault_path: &Path) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in expand_home_path(vault_path).to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn expand_home_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };

    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }

    path.to_path_buf()
}

pub fn normalize_http_path(value: Option<&str>, fallback: &str) -> String {
    let candidate = value.unwrap_or(fallback).trim();
    if candidate.is_empty() || candidate == "/" {
        return "/".to_string();
    }
    format!(
        "/{}",
        candidate.trim_start_matches('/').trim_end_matches('/')
    )
}

pub fn normalize_service_config(
    input: ServiceConfigInput,
) -> Result<ResolvedServiceConfig, ConfigError> {
    let vault_path = expand_home_path(input.vault_path.ok_or(ConfigError::MissingVaultPath)?);
    let index_dir = input
        .index_dir
        .map(expand_home_path)
        .unwrap_or_else(|| default_index_dir(&vault_path));
    let transport = input.transport.unwrap_or(TransportMode::Http);
    let stdio_mode = input.stdio_mode.unwrap_or(StdioMode::Auto);
    let http = normalize_http_input(input.http);
    let auto_reindex = normalize_auto_reindex_input(input.auto_reindex);
    let embedding = normalize_embedding_input(input.embedding);
    let artifact_embedding = normalize_embedding_input(input.artifact_embedding);

    Ok(ResolvedServiceConfig {
        vault_path,
        index_dir,
        transport,
        stdio_mode,
        http,
        auto_reindex,
        embedding,
        artifact_embedding,
        config_file_path: input.config_file_path.map(expand_home_path),
    })
}

pub fn normalize_persisted_config(
    input: PersistedServiceConfig,
) -> Result<PersistedServiceConfig, ConfigError> {
    let resolved = normalize_service_config(ServiceConfigInput {
        vault_path: input.vault_path,
        index_dir: input.index_dir,
        transport: input.transport,
        stdio_mode: input.stdio_mode,
        http: input.http,
        auto_reindex: input.auto_reindex,
        embedding: input.embedding,
        artifact_embedding: input.artifact_embedding,
        config_file_path: None,
    })?;

    Ok(to_persisted_config(&resolved))
}

pub fn to_persisted_config(config: &ResolvedServiceConfig) -> PersistedServiceConfig {
    PersistedServiceConfig {
        vault_path: Some(config.vault_path.clone()),
        index_dir: Some(config.index_dir.clone()),
        transport: Some(config.transport),
        stdio_mode: Some(config.stdio_mode),
        http: Some(HttpConfigInput {
            host: Some(config.http.host.clone()),
            port: Some(config.http.port),
            mcp_path: Some(config.http.mcp_path.clone()),
            health_path: Some(config.http.health_path.clone()),
        }),
        auto_reindex: Some(AutoReindexConfigInput {
            enabled: Some(config.auto_reindex.enabled),
            debounce_ms: Some(config.auto_reindex.debounce_ms),
            interval_ms: Some(config.auto_reindex.interval_ms),
        }),
        embedding: Some(EmbeddingConfigInput {
            provider: config.embedding.provider.clone(),
            model: config.embedding.model.clone(),
            base_url: config.embedding.base_url.clone(),
            api_key: config.embedding.api_key.clone(),
            api_key_env: config.embedding.api_key_env.clone(),
        }),
        artifact_embedding: if config.artifact_embedding.provider.is_some()
            || config.artifact_embedding.model.is_some()
            || config.artifact_embedding.base_url.is_some()
            || config.artifact_embedding.api_key.is_some()
            || config.artifact_embedding.api_key_env.is_some()
        {
            Some(EmbeddingConfigInput {
                provider: config.artifact_embedding.provider.clone(),
                model: config.artifact_embedding.model.clone(),
                base_url: config.artifact_embedding.base_url.clone(),
                api_key: config.artifact_embedding.api_key.clone(),
                api_key_env: config.artifact_embedding.api_key_env.clone(),
            })
        } else {
            None
        },
    }
}

pub fn build_service_endpoints(
    config: &ResolvedServiceConfig,
) -> deep_obsidian_types::ServiceEndpoints {
    config.service_endpoints()
}

pub fn ensure_http_service_config(
    config: ResolvedServiceConfig,
) -> Result<ResolvedServiceConfig, ConfigError> {
    if config.transport != TransportMode::Http {
        return Err(ConfigError::InvalidTransport(config.transport));
    }
    Ok(config)
}

pub fn read_config_file(
    path: impl AsRef<Path>,
) -> Result<Option<PersistedServiceConfig>, ConfigError> {
    let path = expand_home_path(path);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigError::ReadFailed { path, source }),
    };

    parse_config_text(&path, &text).map(Some)
}

pub fn write_config_file(
    path: impl AsRef<Path>,
    config: &PersistedServiceConfig,
) -> Result<(), ConfigError> {
    let path = expand_home_path(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::WriteFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let text = serialize_config(&path, config)?;
    fs::write(&path, format!("{text}\n"))
        .map_err(|source| ConfigError::WriteFailed { path, source })
}

fn normalize_http_input(input: Option<HttpConfigInput>) -> HttpConfig {
    let input = input.unwrap_or_default();
    HttpConfig {
        host: input
            .host
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_HTTP_HOST)
            .to_string(),
        port: input
            .port
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_HTTP_PORT),
        mcp_path: normalize_http_path(input.mcp_path.as_deref(), DEFAULT_HTTP_MCP_PATH),
        health_path: normalize_http_path(input.health_path.as_deref(), DEFAULT_HTTP_HEALTH_PATH),
    }
}

fn normalize_auto_reindex_input(input: Option<AutoReindexConfigInput>) -> AutoReindexConfig {
    let input = input.unwrap_or_default();
    AutoReindexConfig {
        enabled: input.enabled.unwrap_or(true),
        debounce_ms: input
            .debounce_ms
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_AUTO_REINDEX_DEBOUNCE_MS),
        interval_ms: input
            .interval_ms
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_AUTO_REINDEX_INTERVAL_MS),
    }
}

fn normalize_embedding_input(input: Option<EmbeddingConfigInput>) -> EmbeddingConfig {
    let input = input.unwrap_or_default();
    let provider = match input.provider {
        Some(EmbeddingProvider::OpenAiCompatible) => Some(EmbeddingProvider::OpenAiCompatible),
        None if input.model.is_some()
            || input.base_url.is_some()
            || input.api_key.is_some()
            || input.api_key_env.is_some() =>
        {
            Some(EmbeddingProvider::OpenAiCompatible)
        }
        None => None,
    };

    EmbeddingConfig {
        provider,
        model: trim_optional(input.model),
        base_url: trim_optional(input.base_url),
        api_key: input.api_key,
        api_key_env: trim_optional(input.api_key_env),
    }
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_config_text(path: &Path, text: &str) -> Result<PersistedServiceConfig, ConfigError> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => parse_toml(path, text),
        _ => parse_json(path, text),
    }
}

fn serialize_config(path: &Path, config: &PersistedServiceConfig) -> Result<String, ConfigError> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => serialize_toml(path, config),
        _ => serialize_json(path, config),
    }
}

fn parse_json<T: DeserializeOwned>(path: &Path, text: &str) -> Result<T, ConfigError> {
    serde_json::from_str(text).map_err(|source| ConfigError::ParseFailed {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

fn parse_toml<T: DeserializeOwned>(path: &Path, text: &str) -> Result<T, ConfigError> {
    toml::from_str(text).map_err(|source| ConfigError::ParseFailed {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

fn serialize_json<T: Serialize>(path: &Path, value: &T) -> Result<String, ConfigError> {
    serde_json::to_string_pretty(value).map_err(|source| ConfigError::SerializeFailed {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

fn serialize_toml<T: Serialize>(path: &Path, value: &T) -> Result<String, ConfigError> {
    toml::to_string_pretty(value).map_err(|source| ConfigError::SerializeFailed {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

#[cfg(test)]
mod tests {
    use super::{default_packaged_index_dir, expand_home_path, DEFAULT_CONFIG_APP_DIR};

    #[test]
    fn expand_home_path_expands_tilde_prefix() {
        let home = std::env::var("HOME").expect("HOME");
        let home_path = std::path::PathBuf::from(home);
        assert_eq!(expand_home_path("~/vault"), home_path.join("vault"));
        assert_eq!(expand_home_path("~"), home_path);
    }

    #[test]
    fn default_packaged_index_dir_uses_application_support() {
        let home = std::env::var("HOME").expect("HOME");
        let path = default_packaged_index_dir(std::path::Path::new("~/Vault"));
        assert!(path.starts_with(
            std::path::Path::new(&home)
                .join("Library")
                .join("Application Support")
                .join(DEFAULT_CONFIG_APP_DIR)
                .join("indexes")
        ));
        assert_eq!(path.file_name().unwrap().to_string_lossy().len(), 16);
    }
}
