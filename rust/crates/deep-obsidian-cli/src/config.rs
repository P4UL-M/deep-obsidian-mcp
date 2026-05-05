use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use deep_obsidian_config::{
    default_config_path, default_packaged_index_dir, expand_home_path, normalize_http_path,
    normalize_service_config, read_config_file,
};
use deep_obsidian_types::{
    AutoReindexConfigInput, EmbeddingConfigInput, EmbeddingProvider, HttpConfigInput,
    PersistedServiceConfig, ResolvedServiceConfig, ServiceConfigInput,
    StdioMode as SharedStdioMode, TransportMode as SharedTransportMode,
};
use serde::{Deserialize, Serialize};

use crate::cli::{ServiceOptions, StdioMode, TransportMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ResolvedSource {
    Cli,
    Config,
    Env,
    #[default]
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSources {
    pub vault_path: ResolvedSource,
    pub index_dir: ResolvedSource,
    pub transport: ResolvedSource,
    pub stdio_mode: ResolvedSource,
    pub http_host: ResolvedSource,
    pub http_port: ResolvedSource,
    pub http_mcp_path: ResolvedSource,
    pub http_health_path: ResolvedSource,
    pub auto_reindex_enabled: ResolvedSource,
    pub auto_reindex_debounce_ms: ResolvedSource,
    pub auto_reindex_interval_ms: ResolvedSource,
    pub embedding_provider: ResolvedSource,
    pub embedding_model: ResolvedSource,
    pub embedding_base_url: ResolvedSource,
    pub embedding_api_key: ResolvedSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedRuntimeConfig {
    pub config_path: PathBuf,
    pub config_file: Option<PersistedServiceConfig>,
    pub service: ResolvedServiceConfig,
    pub sources: ResolvedSources,
}

pub fn resolve_runtime_config(options: &ServiceOptions) -> Result<ResolvedRuntimeConfig> {
    let config_path = options
        .config
        .clone()
        .map(expand_home_path)
        .unwrap_or_else(default_config_path);
    let config_file = read_config_file(&config_path)
        .with_context(|| format!("failed to load config file {}", config_path.display()))?;

    let (vault_path, vault_path_source) = first_path(
        options.vault_path.clone(),
        config_file
            .as_ref()
            .and_then(|config| config.vault_path.clone()),
        env_path(&["DEEP_OBSIDIAN_VAULT_PATH", "OBSIDIAN_VAULT_PATH"]),
    );
    let packaged_mode = options.packaged || env_bool(&["DEEP_OBSIDIAN_PACKAGED"]).unwrap_or(false);
    let (index_dir, index_dir_source) = first_path(
        options.index_dir.clone(),
        config_file
            .as_ref()
            .and_then(|config| config.index_dir.clone()),
        env_path(&["DEEP_OBSIDIAN_INDEX_DIR", "INDEX_DIR"]),
    );
    let (transport, transport_source) = first_transport(
        options.transport.map(map_transport),
        config_file.as_ref().and_then(|config| config.transport),
        env_transport(&["MCP_TRANSPORT_MODE", "DEEP_OBSIDIAN_TRANSPORT_MODE"]),
        SharedTransportMode::Stdio,
    );
    let (stdio_mode, stdio_mode_source) = first_stdio_mode(
        options.stdio_mode.map(map_stdio_mode),
        config_file.as_ref().and_then(|config| config.stdio_mode),
        env_stdio_mode(&["MCP_STDIO_MODE", "DEEP_OBSIDIAN_STDIO_MODE"]),
        SharedStdioMode::Auto,
    );

    let (http_host, http_host_source) = first_string(
        options.host.clone(),
        config_file
            .as_ref()
            .and_then(|config| config.http.as_ref().and_then(|http| http.host.clone())),
        env_string(&[
            "MCP_HTTP_HOST",
            "DEEP_OBSIDIAN_HOST",
            "DEEP_OBSIDIAN_HTTP_HOST",
        ]),
        Some("127.0.0.1".to_string()),
    );
    let (http_port, http_port_source) = first_u16(
        options.port,
        config_file
            .as_ref()
            .and_then(|config| config.http.as_ref().and_then(|http| http.port)),
        env_u16(&[
            "MCP_HTTP_PORT",
            "DEEP_OBSIDIAN_PORT",
            "DEEP_OBSIDIAN_HTTP_PORT",
        ]),
        4100,
    );
    let (http_mcp_path_raw, http_mcp_path_source) = first_string(
        options.mcp_path.clone(),
        config_file
            .as_ref()
            .and_then(|config| config.http.as_ref().and_then(|http| http.mcp_path.clone())),
        env_string(&["MCP_HTTP_PATH", "DEEP_OBSIDIAN_MCP_PATH"]),
        Some("/mcp".to_string()),
    );
    let (http_health_path_raw, http_health_path_source) = first_string(
        options.health_path.clone(),
        config_file.as_ref().and_then(|config| {
            config
                .http
                .as_ref()
                .and_then(|http| http.health_path.clone())
        }),
        env_string(&["MCP_HTTP_HEALTH_PATH", "DEEP_OBSIDIAN_HEALTH_PATH"]),
        Some("/healthz".to_string()),
    );

    let cli_auto_reindex = if options.no_auto_reindex {
        Some(false)
    } else if options.auto_reindex {
        Some(true)
    } else {
        None
    };
    let (auto_reindex_enabled, auto_reindex_enabled_source) = first_bool(
        cli_auto_reindex,
        config_file
            .as_ref()
            .and_then(|config| config.auto_reindex.as_ref().and_then(|value| value.enabled)),
        env_bool(&["AUTO_REINDEX", "DEEP_OBSIDIAN_AUTO_REINDEX"]),
        true,
    );
    let (auto_reindex_debounce_ms, auto_reindex_debounce_ms_source) = first_u64(
        options.reindex_debounce_ms,
        config_file.as_ref().and_then(|config| {
            config
                .auto_reindex
                .as_ref()
                .and_then(|value| value.debounce_ms)
        }),
        env_u64(&["REINDEX_DEBOUNCE_MS", "DEEP_OBSIDIAN_REINDEX_DEBOUNCE_MS"]),
        1500,
    );
    let (auto_reindex_interval_ms, auto_reindex_interval_ms_source) = first_u64(
        options.reindex_interval_ms,
        config_file.as_ref().and_then(|config| {
            config
                .auto_reindex
                .as_ref()
                .and_then(|value| value.interval_ms)
        }),
        env_u64(&["REINDEX_INTERVAL_MS", "DEEP_OBSIDIAN_REINDEX_INTERVAL_MS"]),
        30000,
    );

    let (embedding_model, embedding_model_source) = first_string(
        options.embedding_model.clone(),
        config_file.as_ref().and_then(|config| {
            config
                .embedding
                .as_ref()
                .and_then(|value| value.model.clone())
        }),
        env_string(&[
            "DEEP_OBSIDIAN_EMBEDDING_MODEL",
            "EMBEDDING_MODEL",
            "OPENAI_EMBEDDING_MODEL",
        ]),
        None,
    );
    let (embedding_provider, embedding_provider_source) = first_embedding_provider(
        options.embedding_provider.clone(),
        config_file.as_ref().and_then(|config| {
            config
                .embedding
                .as_ref()
                .and_then(|value| value.provider.clone())
        }),
        env_embedding_provider(&["DEEP_OBSIDIAN_EMBEDDING_PROVIDER", "EMBEDDING_PROVIDER"]),
        embedding_model.is_some(),
    );
    let (embedding_base_url, embedding_base_url_source) = first_string(
        options.embedding_base_url.clone(),
        config_file.as_ref().and_then(|config| {
            config
                .embedding
                .as_ref()
                .and_then(|value| value.base_url.clone())
        }),
        env_string(&[
            "DEEP_OBSIDIAN_EMBEDDING_BASE_URL",
            "EMBEDDING_BASE_URL",
            "OPENAI_BASE_URL",
        ]),
        None,
    );
    let (embedding_api_key_env, embedding_api_key_source) = first_string(
        options.embedding_api_key_env.clone(),
        config_file.as_ref().and_then(|config| {
            config
                .embedding
                .as_ref()
                .and_then(|value| value.api_key_env.clone())
        }),
        env_string(&[
            "DEEP_OBSIDIAN_EMBEDDING_API_KEY_ENV",
            "EMBEDDING_API_KEY_ENV",
        ]),
        if env::var("OPENAI_API_KEY").is_ok() {
            Some("OPENAI_API_KEY".to_string())
        } else {
            None
        },
    );
    let embedding_api_key = options
        .embedding_api_key
        .clone()
        .or_else(|| {
            config_file.as_ref().and_then(|config| {
                config
                    .embedding
                    .as_ref()
                    .and_then(|value| value.api_key.clone())
            })
        })
        .or_else(|| {
            env_string(&[
                "DEEP_OBSIDIAN_EMBEDDING_API_KEY",
                "EMBEDDING_API_KEY",
                "OPENAI_API_KEY",
            ])
        });

    let input = ServiceConfigInput {
        vault_path,
        index_dir,
        transport: Some(transport),
        stdio_mode: Some(stdio_mode),
        http: Some(HttpConfigInput {
            host: http_host,
            port: Some(http_port),
            mcp_path: Some(normalize_http_path(http_mcp_path_raw.as_deref(), "/mcp")),
            health_path: Some(normalize_http_path(
                http_health_path_raw.as_deref(),
                "/healthz",
            )),
        }),
        auto_reindex: Some(AutoReindexConfigInput {
            enabled: Some(auto_reindex_enabled),
            debounce_ms: Some(auto_reindex_debounce_ms),
            interval_ms: Some(auto_reindex_interval_ms),
        }),
        embedding: Some(EmbeddingConfigInput {
            provider: embedding_provider,
            model: embedding_model,
            base_url: embedding_base_url,
            api_key: embedding_api_key,
            api_key_env: embedding_api_key_env,
        }),
        artifact_embedding: config_file
            .as_ref()
            .and_then(|config| config.artifact_embedding.clone()),
        config_file_path: Some(config_path.clone()),
    };
    let mut service = normalize_service_config(input)?;
    if packaged_mode && matches!(index_dir_source, ResolvedSource::Default) {
        service.index_dir = default_packaged_index_dir(&service.vault_path);
    }

    Ok(ResolvedRuntimeConfig {
        config_path,
        config_file,
        service,
        sources: ResolvedSources {
            vault_path: vault_path_source,
            index_dir: index_dir_source,
            transport: transport_source,
            stdio_mode: stdio_mode_source,
            http_host: http_host_source,
            http_port: http_port_source,
            http_mcp_path: http_mcp_path_source,
            http_health_path: http_health_path_source,
            auto_reindex_enabled: auto_reindex_enabled_source,
            auto_reindex_debounce_ms: auto_reindex_debounce_ms_source,
            auto_reindex_interval_ms: auto_reindex_interval_ms_source,
            embedding_provider: embedding_provider_source,
            embedding_model: embedding_model_source,
            embedding_base_url: embedding_base_url_source,
            embedding_api_key: embedding_api_key_source,
        },
    })
}

fn map_transport(value: TransportMode) -> SharedTransportMode {
    match value {
        TransportMode::Stdio => SharedTransportMode::Stdio,
        TransportMode::Http => SharedTransportMode::Http,
    }
}

fn map_stdio_mode(value: StdioMode) -> SharedStdioMode {
    match value {
        StdioMode::Auto => SharedStdioMode::Auto,
        StdioMode::Newline => SharedStdioMode::Newline,
        StdioMode::Framed => SharedStdioMode::Framed,
    }
}

fn first_path(
    cli: Option<PathBuf>,
    config: Option<PathBuf>,
    env: Option<PathBuf>,
) -> (Option<PathBuf>, ResolvedSource) {
    if let Some(value) = cli {
        return (Some(value), ResolvedSource::Cli);
    }
    if let Some(value) = config {
        return (Some(value), ResolvedSource::Config);
    }
    if let Some(value) = env {
        return (Some(value), ResolvedSource::Env);
    }
    (None, ResolvedSource::Default)
}

fn first_string(
    cli: Option<String>,
    config: Option<String>,
    env: Option<String>,
    default: Option<String>,
) -> (Option<String>, ResolvedSource) {
    if let Some(value) = trim_optional(cli) {
        return (Some(value), ResolvedSource::Cli);
    }
    if let Some(value) = trim_optional(config) {
        return (Some(value), ResolvedSource::Config);
    }
    if let Some(value) = trim_optional(env) {
        return (Some(value), ResolvedSource::Env);
    }
    (default, ResolvedSource::Default)
}

fn first_bool(
    cli: Option<bool>,
    config: Option<bool>,
    env: Option<bool>,
    default: bool,
) -> (bool, ResolvedSource) {
    if let Some(value) = cli {
        return (value, ResolvedSource::Cli);
    }
    if let Some(value) = config {
        return (value, ResolvedSource::Config);
    }
    if let Some(value) = env {
        return (value, ResolvedSource::Env);
    }
    (default, ResolvedSource::Default)
}

fn first_u16(
    cli: Option<u16>,
    config: Option<u16>,
    env: Option<u16>,
    default: u16,
) -> (u16, ResolvedSource) {
    if let Some(value) = cli {
        return (value, ResolvedSource::Cli);
    }
    if let Some(value) = config {
        return (value, ResolvedSource::Config);
    }
    if let Some(value) = env {
        return (value, ResolvedSource::Env);
    }
    (default, ResolvedSource::Default)
}

fn first_u64(
    cli: Option<u64>,
    config: Option<u64>,
    env: Option<u64>,
    default: u64,
) -> (u64, ResolvedSource) {
    if let Some(value) = cli {
        return (value, ResolvedSource::Cli);
    }
    if let Some(value) = config {
        return (value, ResolvedSource::Config);
    }
    if let Some(value) = env {
        return (value, ResolvedSource::Env);
    }
    (default, ResolvedSource::Default)
}

fn first_transport(
    cli: Option<SharedTransportMode>,
    config: Option<SharedTransportMode>,
    env: Option<SharedTransportMode>,
    default: SharedTransportMode,
) -> (SharedTransportMode, ResolvedSource) {
    if let Some(value) = cli {
        return (value, ResolvedSource::Cli);
    }
    if let Some(value) = config {
        return (value, ResolvedSource::Config);
    }
    if let Some(value) = env {
        return (value, ResolvedSource::Env);
    }
    (default, ResolvedSource::Default)
}

fn first_stdio_mode(
    cli: Option<SharedStdioMode>,
    config: Option<SharedStdioMode>,
    env: Option<SharedStdioMode>,
    default: SharedStdioMode,
) -> (SharedStdioMode, ResolvedSource) {
    if let Some(value) = cli {
        return (value, ResolvedSource::Cli);
    }
    if let Some(value) = config {
        return (value, ResolvedSource::Config);
    }
    if let Some(value) = env {
        return (value, ResolvedSource::Env);
    }
    (default, ResolvedSource::Default)
}

fn first_embedding_provider(
    cli: Option<String>,
    config: Option<EmbeddingProvider>,
    env: Option<EmbeddingProvider>,
    infer_from_model: bool,
) -> (Option<EmbeddingProvider>, ResolvedSource) {
    if let Some(value) = parse_embedding_provider(cli.as_deref()) {
        return (Some(value), ResolvedSource::Cli);
    }
    if let Some(value) = config {
        return (Some(value), ResolvedSource::Config);
    }
    if let Some(value) = env {
        return (Some(value), ResolvedSource::Env);
    }
    if infer_from_model {
        return (
            Some(EmbeddingProvider::OpenAiCompatible),
            ResolvedSource::Default,
        );
    }
    (None, ResolvedSource::Default)
}

fn parse_transport(value: &str) -> Option<SharedTransportMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "stdio" => Some(SharedTransportMode::Stdio),
        "http" => Some(SharedTransportMode::Http),
        _ => None,
    }
}

fn parse_stdio_mode(value: &str) -> Option<SharedStdioMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(SharedStdioMode::Auto),
        "newline" => Some(SharedStdioMode::Newline),
        "framed" => Some(SharedStdioMode::Framed),
        _ => None,
    }
}

fn parse_embedding_provider(value: Option<&str>) -> Option<EmbeddingProvider> {
    match value?.trim() {
        "openai-compatible" => Some(EmbeddingProvider::OpenAiCompatible),
        _ => None,
    }
}

fn env_path(keys: &[&str]) -> Option<PathBuf> {
    env_string(keys).map(PathBuf::from)
}

fn env_string(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        env::var(key)
            .ok()
            .and_then(|value| trim_optional(Some(value)))
    })
}

fn env_bool(keys: &[&str]) -> Option<bool> {
    env_string(keys).and_then(|value| match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    })
}

fn env_u16(keys: &[&str]) -> Option<u16> {
    env_string(keys).and_then(|value| value.parse::<u16>().ok())
}

fn env_u64(keys: &[&str]) -> Option<u64> {
    env_string(keys).and_then(|value| value.parse::<u64>().ok())
}

fn env_transport(keys: &[&str]) -> Option<SharedTransportMode> {
    env_string(keys).and_then(|value| parse_transport(&value))
}

fn env_stdio_mode(keys: &[&str]) -> Option<SharedStdioMode> {
    env_string(keys).and_then(|value| parse_stdio_mode(&value))
}

fn env_embedding_provider(keys: &[&str]) -> Option<EmbeddingProvider> {
    env_string(keys).and_then(|value| parse_embedding_provider(Some(&value)))
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{resolve_runtime_config, ResolvedSource};
    use crate::cli::{ServiceOptions, StdioMode, TransportMode};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        values: Vec<(&'static str, Option<String>)>,
    }

    impl EnvRestore {
        fn capture(keys: &[&'static str]) -> Self {
            Self {
                values: keys
                    .iter()
                    .map(|key| (*key, std::env::var(key).ok()))
                    .collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in &self.values {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "deep-obsidian-cli-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn service_options(config: PathBuf, vault: PathBuf) -> ServiceOptions {
        ServiceOptions {
            config: Some(config),
            dry_run: false,
            no_dry_run: false,
            json: false,
            no_json: false,
            vault_path: Some(vault),
            index_dir: None,
            packaged: false,
            transport: None,
            stdio_mode: None,
            host: None,
            port: None,
            mcp_path: None,
            health_path: None,
            auto_reindex: false,
            no_auto_reindex: false,
            reindex_debounce_ms: None,
            reindex_interval_ms: None,
            embedding_provider: None,
            embedding_model: None,
            embedding_base_url: None,
            embedding_api_key: None,
            embedding_api_key_env: None,
        }
    }

    #[test]
    fn resolve_runtime_config_prefers_config_file_over_environment() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let _restore = EnvRestore::capture(&["DEEP_OBSIDIAN_INDEX_DIR", "INDEX_DIR"]);
        let root = unique_temp_dir("config-precedence");
        let vault = root.join("vault");
        let config_index = root.join("config-index");
        let env_index = root.join("env-index");
        fs::create_dir_all(&vault).expect("create vault");
        let config_path = root.join("config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "indexDir": config_index,
                "transport": "http"
            })
            .to_string(),
        )
        .expect("write config");
        std::env::set_var("DEEP_OBSIDIAN_INDEX_DIR", &env_index);

        let resolved =
            resolve_runtime_config(&service_options(config_path, vault)).expect("resolve config");

        assert_eq!(resolved.service.index_dir, config_index);
        assert_eq!(resolved.sources.index_dir, ResolvedSource::Config);
    }

    #[test]
    fn resolve_runtime_config_prefers_cli_over_config_file() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let _restore = EnvRestore::capture(&["DEEP_OBSIDIAN_INDEX_DIR", "INDEX_DIR"]);
        let root = unique_temp_dir("cli-precedence");
        let vault = root.join("vault");
        let cli_index = root.join("cli-index");
        let config_index = root.join("config-index");
        let env_index = root.join("env-index");
        fs::create_dir_all(&vault).expect("create vault");
        let config_path = root.join("config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "indexDir": config_index,
                "transport": "http"
            })
            .to_string(),
        )
        .expect("write config");
        std::env::set_var("DEEP_OBSIDIAN_INDEX_DIR", &env_index);
        let mut options = service_options(config_path, vault);
        options.index_dir = Some(cli_index.clone());
        options.transport = Some(TransportMode::Http);
        options.stdio_mode = Some(StdioMode::Auto);

        let resolved = resolve_runtime_config(&options).expect("resolve config");

        assert_eq!(resolved.service.index_dir, cli_index);
        assert_eq!(resolved.sources.index_dir, ResolvedSource::Cli);
    }

    #[test]
    fn resolve_runtime_config_uses_packaged_index_dir_only_for_default_index() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let _restore = EnvRestore::capture(&[
            "DEEP_OBSIDIAN_INDEX_DIR",
            "INDEX_DIR",
            "DEEP_OBSIDIAN_PACKAGED",
        ]);
        let root = unique_temp_dir("packaged-index");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        let config_path = root.join("missing-config.json");
        let mut options = service_options(config_path, vault.clone());
        options.packaged = true;

        let resolved = resolve_runtime_config(&options).expect("resolve config");

        assert_eq!(resolved.sources.index_dir, ResolvedSource::Default);
        assert!(resolved
            .service
            .index_dir
            .to_string_lossy()
            .contains("Application Support"));
        assert_ne!(resolved.service.index_dir, vault.join(".deep-obsidian-mcp"));
    }

    #[test]
    fn resolve_runtime_config_preserves_explicit_index_dir_in_packaged_mode() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let _restore = EnvRestore::capture(&[
            "DEEP_OBSIDIAN_INDEX_DIR",
            "INDEX_DIR",
            "DEEP_OBSIDIAN_PACKAGED",
        ]);
        let root = unique_temp_dir("packaged-explicit-index");
        let vault = root.join("vault");
        let cli_index = root.join("cli-index");
        fs::create_dir_all(&vault).expect("create vault");
        let config_path = root.join("missing-config.json");
        let mut options = service_options(config_path, vault);
        options.packaged = true;
        options.index_dir = Some(cli_index.clone());

        let resolved = resolve_runtime_config(&options).expect("resolve config");

        assert_eq!(resolved.service.index_dir, cli_index);
        assert_eq!(resolved.sources.index_dir, ResolvedSource::Cli);
    }
}
