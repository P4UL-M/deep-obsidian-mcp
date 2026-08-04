use deep_obsidian_types::{
    AuthConfig, AuthConfigInput, AutoReindexConfig, AutoReindexConfigInput, EmbeddingConfig,
    EmbeddingConfigInput, EmbeddingProvider, HttpConfig, HttpConfigInput, MountBackendConfig,
    MountConfig, PersistedServiceConfig, ResolvedServiceConfig, ServiceConfigInput, StdioMode,
    TransportMode,
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
pub const DEFAULT_SECRETS_FILE_NAME: &str = "secrets.json";
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
    /// Both the legacy top-level `vaultPath` and an explicit `mounts` table were
    /// given. Rejected rather than resolved by precedence: both spell "where the
    /// root of the vault is", and silently preferring one would mean a user who
    /// added `mounts` to an existing config could keep serving the old vault
    /// without any signal. An empty `mounts` array is NOT ambiguous and is
    /// treated as absent.
    #[error("config sets both 'vaultPath' and 'mounts'; use one or the other (move the vault path into a mount with mountAt \"\", or drop the mounts array)")]
    VaultPathAndMountsBothSet,
    /// A mount table with no mount at the vault root. Rejected this slice:
    /// `vaultPath` is the root mount's path and feeds the runtime watcher, the
    /// search index, and `doctor`, so a rootless table would leave it undefined.
    /// Lifting this requires those consumers to become per-mount first.
    #[error("mount table has no root mount; exactly one mount must have mountAt \"\" (or \"/\")")]
    MissingRootMount,
    /// A mount id outside the accepted slug shape.
    #[error("invalid mount id {id:?}: ids must match [a-z0-9][a-z0-9-]* (lowercase letters, digits and hyphens, not starting with a hyphen)")]
    InvalidMountId { id: String },
    #[error("duplicate mount id {id:?}: mount ids must be unique")]
    DuplicateMountId { id: String },
    #[error("invalid mountAt {mount_at:?} on mount {id:?}: {reason}")]
    InvalidMountAt {
        id: String,
        mount_at: String,
        reason: &'static str,
    },
    /// Two mounts claiming the identical normalized prefix. Nesting is fine
    /// (longest prefix wins), an exact tie is not resolvable.
    #[error("mounts {first:?} and {second:?} both mount at {mount_at:?}; each mountAt must be claimed by exactly one mount")]
    DuplicateMountAt {
        mount_at: String,
        first: String,
        second: String,
    },
    /// More than a single root mount without the opt-in flag.
    #[error("multi-vault mounts are experimental: set {{\"experimental\": {{\"multiVault\": true}}}} in the config to resolve a table of {count} mounts")]
    MultiVaultNotEnabled { count: usize },
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

pub fn default_secrets_path() -> PathBuf {
    default_config_dir()
        .join(DEFAULT_CONFIG_APP_DIR)
        .join(DEFAULT_SECRETS_FILE_NAME)
}

pub fn default_index_dir(vault_path: &Path) -> PathBuf {
    vault_path.join(".deep-obsidian-mcp")
}

pub fn default_packaged_index_dir(vault_path: &Path) -> PathBuf {
    packaged_data_dir()
        .join("indexes")
        .join(stable_vault_hash(vault_path))
}

/// Per-user application data directory used in packaged mode (where indexes live
/// outside the vault). Platform-native: macOS Application Support, otherwise the
/// XDG data home (Linux/apt installs).
#[cfg(target_os = "macos")]
fn packaged_data_dir() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Application Support")
        .join(DEFAULT_CONFIG_APP_DIR)
}

#[cfg(not(target_os = "macos"))]
fn packaged_data_dir() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        // The XDG spec requires absolute paths; ignore relative overrides.
        if xdg.is_absolute() {
            return xdg.join(DEFAULT_CONFIG_APP_DIR);
        }
    }
    home_dir()
        .join(".local")
        .join("share")
        .join(DEFAULT_CONFIG_APP_DIR)
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

/// Canonicalize a `mountAt` into its internal form: no leading or trailing
/// slash, forward slashes only, `""` for the vault root.
///
/// A leading `/` is accepted and stripped rather than rejected as "absolute".
/// That is forced by the spec of the field itself: `"/"` must mean the vault
/// root, so the leading slash is unambiguously vault-root-relative here, and
/// reading `"/Team"` as anything other than `"Team"` would be inconsistent with
/// it. Genuinely path-shaped input is still refused: backslashes (Windows
/// separators, which would make `a\b` a single opaque segment), `.`/`..`
/// segments, empty interior segments, and `~` (home expansion has no meaning in
/// a logical namespace).
fn normalize_mount_at(id: &str, raw: &str) -> Result<String, ConfigError> {
    let reject = |reason: &'static str| ConfigError::InvalidMountAt {
        id: id.to_string(),
        mount_at: raw.to_string(),
        reason,
    };

    let trimmed = raw.trim();
    if trimmed.contains('\\') {
        return Err(reject(
            "backslashes are not path separators here; use forward slashes",
        ));
    }
    if trimmed.starts_with('~') {
        return Err(reject(
            "mountAt is a logical vault-relative prefix, not a filesystem path, so '~' cannot be expanded",
        ));
    }
    let inner = trimmed.trim_matches('/');
    if inner.is_empty() {
        return Ok(String::new());
    }
    for segment in inner.split('/') {
        match segment {
            "" => return Err(reject("contains an empty path segment")),
            "." | ".." => {
                return Err(reject("contains a '.' or '..' segment"));
            }
            _ => {}
        }
    }
    Ok(inner.to_string())
}

/// True for ids matching `[a-z0-9][a-z0-9-]*`.
///
/// Deliberately narrower than "any non-empty string". A mount id is a durable,
/// user-visible name that this slice already puts into error messages and
/// `vault_info`, and that the per-mount index slice will want to use as a
/// directory component. Restricting it to a lowercase slug now avoids two
/// specific traps later: ids that collide on a case-insensitive filesystem
/// (`Work` vs `work`), and ids that need quoting or escaping wherever they are
/// interpolated. Widening the rule later is compatible; narrowing it is not.
fn is_valid_mount_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    })
}

/// Expand `~` in whatever paths a mount's backend carries.
fn expand_mount_backend_paths(backend: MountBackendConfig) -> MountBackendConfig {
    match backend {
        MountBackendConfig::Filesystem {
            vault_path,
            index_dir,
        } => MountBackendConfig::Filesystem {
            vault_path: expand_home_path(vault_path),
            index_dir: index_dir.map(expand_home_path),
        },
    }
}

/// Validate and canonicalize a declared mount table.
///
/// Returns the normalized mounts and the index of the root mount. Nesting is
/// allowed — `""` and `"Team"` and `"Team/Alpha"` can coexist, and the router
/// resolves by longest prefix — but an exact `mountAt` tie is rejected because
/// nothing could break it.
fn normalize_mounts(mounts: Vec<MountConfig>) -> Result<(Vec<MountConfig>, usize), ConfigError> {
    let mut normalized: Vec<MountConfig> = Vec::with_capacity(mounts.len());
    for mount in mounts {
        let id = mount.id.trim().to_string();
        if !is_valid_mount_id(&id) {
            return Err(ConfigError::InvalidMountId { id });
        }
        if let Some(existing) = normalized.iter().find(|other| other.id == id) {
            return Err(ConfigError::DuplicateMountId {
                id: existing.id.clone(),
            });
        }
        let mount_at = normalize_mount_at(&id, &mount.mount_at)?;
        if let Some(existing) = normalized.iter().find(|other| other.mount_at == mount_at) {
            return Err(ConfigError::DuplicateMountAt {
                mount_at,
                first: existing.id.clone(),
                second: id,
            });
        }
        normalized.push(MountConfig {
            id,
            mount_at,
            backend: expand_mount_backend_paths(mount.backend),
        });
    }

    let root = normalized
        .iter()
        .position(|mount| mount.mount_at.is_empty())
        .ok_or(ConfigError::MissingRootMount)?;
    Ok((normalized, root))
}

pub fn normalize_service_config(
    input: ServiceConfigInput,
) -> Result<ResolvedServiceConfig, ConfigError> {
    let experimental = input.experimental.unwrap_or_default();
    // An empty `mounts` array carries no information, so it is treated as absent
    // rather than as an (unsatisfiable) rootless table.
    let declared_mounts = input.mounts.filter(|mounts| !mounts.is_empty());

    let (mounts, vault_path, mount_index_dir) = match declared_mounts {
        Some(mounts) => {
            if input.vault_path.is_some() {
                return Err(ConfigError::VaultPathAndMountsBothSet);
            }
            let (mounts, root) = normalize_mounts(mounts)?;
            // A single explicit root mount is exactly the legacy shape spelled out
            // longhand, so it needs no flag. Anything else does.
            if mounts.len() > 1 && !experimental.multi_vault {
                return Err(ConfigError::MultiVaultNotEnabled {
                    count: mounts.len(),
                });
            }
            let (vault_path, mount_index_dir) = match &mounts[root].backend {
                MountBackendConfig::Filesystem {
                    vault_path,
                    index_dir,
                } => (vault_path.clone(), index_dir.clone()),
            };
            (mounts, vault_path, mount_index_dir)
        }
        // Legacy: one implicit root mount. `mounts` stays empty so saving the
        // config back cannot invent a mount table the user never wrote.
        None => (
            Vec::new(),
            expand_home_path(input.vault_path.ok_or(ConfigError::MissingVaultPath)?),
            None,
        ),
    };

    let index_dir = input
        .index_dir
        .map(expand_home_path)
        .or(mount_index_dir)
        .unwrap_or_else(|| default_index_dir(&vault_path));
    let transport = input.transport.unwrap_or(TransportMode::Http);
    let stdio_mode = input.stdio_mode.unwrap_or(StdioMode::Auto);
    let http = normalize_http_input(input.http);
    let auto_reindex = normalize_auto_reindex_input(input.auto_reindex);
    let embedding = normalize_embedding_input(input.embedding);
    let artifact_embedding = normalize_embedding_input(input.artifact_embedding);
    let auth = normalize_auth_input(input.auth);

    Ok(ResolvedServiceConfig {
        vault_path,
        mounts,
        experimental,
        index_dir,
        transport,
        stdio_mode,
        http,
        auto_reindex,
        embedding,
        artifact_embedding,
        auth,
        config_file_path: input.config_file_path.map(expand_home_path),
    })
}

/// True for hosts that only accept connections from the local machine. Used to
/// decide whether running without authentication is safe.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host == "[::1]"
        || host.starts_with("127.")
}

pub fn normalize_persisted_config(
    input: PersistedServiceConfig,
) -> Result<PersistedServiceConfig, ConfigError> {
    let resolved = normalize_service_config(ServiceConfigInput {
        vault_path: input.vault_path,
        mounts: input.mounts,
        experimental: input.experimental,
        index_dir: input.index_dir,
        transport: input.transport,
        stdio_mode: input.stdio_mode,
        http: input.http,
        auto_reindex: input.auto_reindex,
        embedding: input.embedding,
        artifact_embedding: input.artifact_embedding,
        auth: input.auth,
        config_file_path: None,
    })?;

    Ok(to_persisted_config(&resolved))
}

pub fn to_persisted_config(config: &ResolvedServiceConfig) -> PersistedServiceConfig {
    // A legacy config round-trips as legacy: `mounts` is empty exactly when the
    // user never wrote one, and `vaultPath` is written back as it always was. A
    // config that DID declare mounts round-trips the other way -- `mounts` is
    // emitted and `vaultPath` is omitted, because emitting both would produce a
    // file this same function's input validation rejects as ambiguous.
    let declared_mounts = !config.mounts.is_empty();
    PersistedServiceConfig {
        vault_path: if declared_mounts {
            None
        } else {
            Some(config.vault_path.clone())
        },
        mounts: if declared_mounts {
            Some(config.mounts.clone())
        } else {
            None
        },
        experimental: if config.experimental.is_default() {
            None
        } else {
            Some(config.experimental.clone())
        },
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
            api_key_ref: config.embedding.api_key_ref.clone(),
            max_chars: config.embedding.max_chars,
            max_input_tokens: config.embedding.max_input_tokens,
            context_tokens: config.embedding.context_tokens,
            query_instruction: config.embedding.query_instruction.clone(),
        }),
        artifact_embedding: if config.artifact_embedding.provider.is_some()
            || config.artifact_embedding.model.is_some()
            || config.artifact_embedding.base_url.is_some()
            || config.artifact_embedding.api_key_ref.is_some()
        {
            Some(EmbeddingConfigInput {
                provider: config.artifact_embedding.provider.clone(),
                model: config.artifact_embedding.model.clone(),
                base_url: config.artifact_embedding.base_url.clone(),
                api_key_ref: config.artifact_embedding.api_key_ref.clone(),
                max_chars: config.artifact_embedding.max_chars,
                max_input_tokens: config.artifact_embedding.max_input_tokens,
                context_tokens: config.artifact_embedding.context_tokens,
                query_instruction: config.artifact_embedding.query_instruction.clone(),
            })
        } else {
            None
        },
        auth: if config.auth.enabled
            || config.auth.token_ref.is_some()
            || !config.auth.allowed_origins.is_empty()
        {
            Some(AuthConfigInput {
                enabled: Some(config.auth.enabled),
                token_ref: config.auth.token_ref.clone(),
                allowed_origins: if config.auth.allowed_origins.is_empty() {
                    None
                } else {
                    Some(config.auth.allowed_origins.clone())
                },
            })
        } else {
            None
        },
    }
}

fn normalize_auth_input(input: Option<AuthConfigInput>) -> AuthConfig {
    let input = input.unwrap_or_default();
    AuthConfig {
        enabled: input.enabled.unwrap_or(false),
        token_ref: input.token_ref,
        allowed_origins: input
            .allowed_origins
            .unwrap_or_default()
            .into_iter()
            .map(|origin| origin.trim().to_string())
            .filter(|origin| !origin.is_empty())
            .collect(),
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

    let text = render_config_text(&path, config)?;
    fs::write(&path, text).map_err(|source| ConfigError::WriteFailed { path, source })
}

/// The exact text `write_config_file` would write (format chosen by the path's
/// extension, trailing newline included). Lets callers diff against an existing
/// file before overwriting it.
pub fn render_config_text(
    path: impl AsRef<Path>,
    config: &PersistedServiceConfig,
) -> Result<String, ConfigError> {
    let path = expand_home_path(path);
    let text = serialize_config(&path, config)?;
    Ok(format!("{text}\n"))
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
            || input.api_key_ref.is_some() =>
        {
            Some(EmbeddingProvider::OpenAiCompatible)
        }
        None => None,
    };

    EmbeddingConfig {
        provider,
        model: trim_optional(input.model),
        base_url: trim_optional(input.base_url),
        api_key_ref: input.api_key_ref,
        max_chars: input.max_chars,
        max_input_tokens: input.max_input_tokens,
        context_tokens: input.context_tokens,
        query_instruction: trim_optional(input.query_instruction),
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

pub mod secrets;

#[cfg(test)]
mod tests {
    use super::{
        default_packaged_index_dir, expand_home_path, is_loopback_host, normalize_persisted_config,
        normalize_service_config, to_persisted_config, ConfigError, DEFAULT_CONFIG_APP_DIR,
    };
    use deep_obsidian_types::{
        AuthConfigInput, ExperimentalConfig, MountBackendConfig, MountConfig,
        PersistedServiceConfig, SecretRef, ServiceConfigInput,
    };
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Mount table helpers
    // -----------------------------------------------------------------------

    fn filesystem_mount(id: &str, mount_at: &str, vault_path: &str) -> MountConfig {
        MountConfig {
            id: id.to_string(),
            mount_at: mount_at.to_string(),
            backend: MountBackendConfig::Filesystem {
                vault_path: PathBuf::from(vault_path),
                index_dir: None,
            },
        }
    }

    fn mounts_input(mounts: Vec<MountConfig>, multi_vault: bool) -> ServiceConfigInput {
        ServiceConfigInput {
            mounts: Some(mounts),
            experimental: Some(ExperimentalConfig { multi_vault }),
            ..ServiceConfigInput::default()
        }
    }

    // -----------------------------------------------------------------------
    // Legacy equivalence
    // -----------------------------------------------------------------------

    #[test]
    fn legacy_vault_path_resolves_to_an_implicit_root_mount() {
        let resolved = normalize_service_config(ServiceConfigInput {
            vault_path: Some(PathBuf::from("/tmp/vault")),
            ..ServiceConfigInput::default()
        })
        .expect("normalize");

        // Nothing is invented: the declared table stays empty so the config can be
        // saved back as legacy.
        assert!(resolved.mounts.is_empty());
        assert!(!resolved.is_multi_mount());
        assert!(to_persisted_config(&resolved).mounts.is_none());
        assert_eq!(
            to_persisted_config(&resolved).vault_path,
            Some(PathBuf::from("/tmp/vault"))
        );

        // But the routing view sees exactly one root mount.
        let table = resolved.mount_table();
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].id, "vault");
        assert_eq!(table[0].mount_at, "");
        assert_eq!(
            table[0].backend,
            MountBackendConfig::Filesystem {
                vault_path: PathBuf::from("/tmp/vault"),
                index_dir: None,
            }
        );
    }

    #[test]
    fn a_single_explicit_root_mount_matches_legacy_and_needs_no_flag() {
        let legacy = normalize_service_config(ServiceConfigInput {
            vault_path: Some(PathBuf::from("/tmp/vault")),
            ..ServiceConfigInput::default()
        })
        .expect("normalize legacy");
        // No experimental flag: one root mount is the legacy shape written longhand.
        let explicit = normalize_service_config(mounts_input(
            vec![filesystem_mount("vault", "", "/tmp/vault")],
            false,
        ))
        .expect("normalize explicit");

        assert_eq!(explicit.vault_path, legacy.vault_path);
        assert_eq!(explicit.index_dir, legacy.index_dir);
        assert!(!explicit.is_multi_mount());
        // The routing view is identical, which is what makes behaviour identical.
        assert_eq!(explicit.mount_table(), legacy.mount_table());
    }

    #[test]
    fn an_empty_mounts_array_is_treated_as_absent_not_as_a_rootless_table() {
        let resolved = normalize_service_config(ServiceConfigInput {
            vault_path: Some(PathBuf::from("/tmp/vault")),
            mounts: Some(Vec::new()),
            ..ServiceConfigInput::default()
        })
        .expect("normalize");
        assert_eq!(resolved.vault_path, PathBuf::from("/tmp/vault"));
        assert!(resolved.mounts.is_empty());
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    #[test]
    fn vault_path_together_with_mounts_is_rejected_as_ambiguous() {
        let error = normalize_service_config(ServiceConfigInput {
            vault_path: Some(PathBuf::from("/tmp/vault")),
            mounts: Some(vec![filesystem_mount("vault", "", "/tmp/other")]),
            ..ServiceConfigInput::default()
        })
        .expect_err("both set");
        assert!(matches!(error, ConfigError::VaultPathAndMountsBothSet));
    }

    #[test]
    fn multiple_mounts_require_the_experimental_flag() {
        let mounts = vec![
            filesystem_mount("vault", "", "/tmp/vault"),
            filesystem_mount("team", "Team", "/tmp/team"),
        ];
        let error = normalize_service_config(mounts_input(mounts.clone(), false))
            .expect_err("flag missing");
        assert!(matches!(
            error,
            ConfigError::MultiVaultNotEnabled { count: 2 }
        ));
        assert!(error.to_string().contains("multiVault"));

        let resolved = normalize_service_config(mounts_input(mounts, true)).expect("flag set");
        assert!(resolved.is_multi_mount());
        assert_eq!(resolved.vault_path, PathBuf::from("/tmp/vault"));
    }

    #[test]
    fn a_mount_table_without_a_root_mount_is_rejected() {
        let error = normalize_service_config(mounts_input(
            vec![
                filesystem_mount("team", "Team", "/tmp/team"),
                filesystem_mount("archive", "Archive", "/tmp/archive"),
            ],
            true,
        ))
        .expect_err("rootless");
        assert!(matches!(error, ConfigError::MissingRootMount));
    }

    #[test]
    fn duplicate_mount_ids_are_rejected() {
        let error = normalize_service_config(mounts_input(
            vec![
                filesystem_mount("vault", "", "/tmp/vault"),
                filesystem_mount("vault", "Team", "/tmp/team"),
            ],
            true,
        ))
        .expect_err("duplicate id");
        assert!(matches!(
            error,
            ConfigError::DuplicateMountId { ref id } if id == "vault"
        ));
    }

    #[test]
    fn duplicate_mount_at_is_rejected_even_when_spelled_differently() {
        // "/Team/" and "Team" normalize to the same prefix, so this is a real tie.
        let error = normalize_service_config(mounts_input(
            vec![
                filesystem_mount("vault", "", "/tmp/vault"),
                filesystem_mount("team", "Team", "/tmp/team"),
                filesystem_mount("team-two", "/Team/", "/tmp/team2"),
            ],
            true,
        ))
        .expect_err("duplicate mountAt");
        assert!(matches!(
            error,
            ConfigError::DuplicateMountAt { ref mount_at, .. } if mount_at == "Team"
        ));
    }

    #[test]
    fn nested_mounts_are_allowed() {
        // Longest-prefix routing makes "Team" and "Team/Alpha" unambiguous, so a
        // nested mount is legal.
        let resolved = normalize_service_config(mounts_input(
            vec![
                filesystem_mount("vault", "", "/tmp/vault"),
                filesystem_mount("team", "Team", "/tmp/team"),
                filesystem_mount("alpha", "Team/Alpha", "/tmp/alpha"),
            ],
            true,
        ))
        .expect("nested mounts");
        assert_eq!(resolved.mounts.len(), 3);
    }

    #[test]
    fn mount_at_is_normalized_to_a_slashless_prefix() {
        let resolved = normalize_service_config(mounts_input(
            vec![
                // "/" is the documented root spelling.
                filesystem_mount("vault", "/", "/tmp/vault"),
                filesystem_mount("team", "/Team/Alpha/", "/tmp/alpha"),
            ],
            true,
        ))
        .expect("normalize");
        assert_eq!(resolved.mounts[0].mount_at, "");
        assert_eq!(resolved.mounts[1].mount_at, "Team/Alpha");
    }

    #[test]
    fn malformed_mount_at_values_are_rejected() {
        for bad in [
            "../escape",
            "Team/../Other",
            "Team\\Alpha",
            "~/Team",
            "a//b",
            "Team/./Alpha",
        ] {
            let error = normalize_service_config(mounts_input(
                vec![
                    filesystem_mount("vault", "", "/tmp/vault"),
                    filesystem_mount("bad", bad, "/tmp/bad"),
                ],
                true,
            ))
            .expect_err(bad);
            assert!(
                matches!(error, ConfigError::InvalidMountAt { .. }),
                "{bad:?} produced {error}"
            );
        }
    }

    #[test]
    fn invalid_mount_ids_are_rejected() {
        for bad in ["", "Team", "-team", "team_two", "team.two", "team/two"] {
            let error = normalize_service_config(mounts_input(
                vec![
                    filesystem_mount("vault", "", "/tmp/vault"),
                    filesystem_mount(bad, "Team", "/tmp/team"),
                ],
                true,
            ))
            .expect_err("invalid id");
            assert!(
                matches!(error, ConfigError::InvalidMountId { .. }),
                "{bad:?} produced {error}"
            );
        }
        // The accepted shape.
        for good in ["team", "team-two", "t", "2nd-vault"] {
            normalize_service_config(mounts_input(
                vec![
                    filesystem_mount("vault", "", "/tmp/vault"),
                    filesystem_mount(good, "Team", "/tmp/team"),
                ],
                true,
            ))
            .unwrap_or_else(|error| panic!("{good:?} rejected: {error}"));
        }
    }

    // -----------------------------------------------------------------------
    // Derived fields and round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn vault_path_and_index_dir_come_from_the_root_mount() {
        let resolved = normalize_service_config(mounts_input(
            vec![
                MountConfig {
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: PathBuf::from("/tmp/vault"),
                        index_dir: Some(PathBuf::from("/tmp/root-index")),
                    },
                },
                // A non-root mount's indexDir is accepted but not consumed yet.
                MountConfig {
                    id: "team".to_string(),
                    mount_at: "Team".to_string(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: PathBuf::from("/tmp/team"),
                        index_dir: Some(PathBuf::from("/tmp/team-index")),
                    },
                },
            ],
            true,
        ))
        .expect("normalize");
        assert_eq!(resolved.vault_path, PathBuf::from("/tmp/vault"));
        assert_eq!(resolved.index_dir, PathBuf::from("/tmp/root-index"));

        // An explicit top-level indexDir still wins over the root mount's.
        let resolved = normalize_service_config(ServiceConfigInput {
            index_dir: Some(PathBuf::from("/tmp/top-level")),
            ..mounts_input(
                vec![MountConfig {
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: PathBuf::from("/tmp/vault"),
                        index_dir: Some(PathBuf::from("/tmp/root-index")),
                    },
                }],
                false,
            )
        })
        .expect("normalize");
        assert_eq!(resolved.index_dir, PathBuf::from("/tmp/top-level"));
    }

    #[test]
    fn a_mounts_config_round_trips_and_a_legacy_config_stays_legacy() {
        let text = r#"{
            "experimental": { "multiVault": true, "someFutureFlag": "ignored" },
            "mounts": [
                { "id": "vault", "mountAt": "", "backend": { "kind": "filesystem", "vaultPath": "/tmp/vault" } },
                { "id": "team", "mountAt": "/Team/", "backend": { "kind": "filesystem", "vaultPath": "/tmp/team" } }
            ]
        }"#;
        let parsed: PersistedServiceConfig = serde_json::from_str(text).expect("parse");
        let persisted = normalize_persisted_config(parsed).expect("normalize");

        // Emitting `vaultPath` alongside `mounts` would produce a file this same
        // function rejects, so it must be omitted.
        assert!(persisted.vault_path.is_none());
        let mounts = persisted.mounts.as_ref().expect("mounts persisted");
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[1].mount_at, "Team");
        assert_eq!(
            persisted.experimental,
            Some(ExperimentalConfig { multi_vault: true })
        );
        let serialized = serde_json::to_string(&persisted).expect("serialize");
        assert!(serialized.contains("\"mountAt\""));
        assert!(serialized.contains("\"kind\":\"filesystem\""));
        // Re-parsing the emitted form resolves to the same table (a true round trip).
        let reparsed: PersistedServiceConfig = serde_json::from_str(&serialized).expect("reparse");
        assert_eq!(
            normalize_persisted_config(reparsed).expect("renormalize"),
            persisted
        );

        // A legacy config saved back gains neither field.
        let legacy: PersistedServiceConfig =
            serde_json::from_str(r#"{"vaultPath": "/tmp/vault"}"#).expect("parse legacy");
        let legacy = normalize_persisted_config(legacy).expect("normalize legacy");
        assert!(legacy.mounts.is_none());
        assert!(legacy.experimental.is_none());
        let serialized = serde_json::to_string(&legacy).expect("serialize legacy");
        assert!(!serialized.contains("mounts"));
        assert!(!serialized.contains("experimental"));
    }

    #[test]
    fn unknown_experimental_flags_are_tolerated() {
        // A config written by a newer build must still load; the unknown flag is
        // simply dropped.
        let parsed: PersistedServiceConfig = serde_json::from_str(
            r#"{"vaultPath": "/tmp/vault", "experimental": {"notAFlagWeKnow": true}}"#,
        )
        .expect("parse");
        let experimental = parsed.experimental.as_ref().expect("experimental");
        assert!(!experimental.multi_vault);
    }

    #[test]
    fn mount_vault_paths_expand_the_home_prefix() {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let resolved = normalize_service_config(mounts_input(
            vec![filesystem_mount("vault", "", "~/Vault")],
            false,
        ))
        .expect("normalize");
        assert_eq!(resolved.vault_path, home.join("Vault"));
        assert_eq!(
            resolved.mounts[0].backend,
            MountBackendConfig::Filesystem {
                vault_path: home.join("Vault"),
                index_dir: None,
            }
        );
    }

    #[test]
    fn is_loopback_host_recognizes_local_addresses() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.1.2.3"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn auth_config_round_trips_through_persisted_config() {
        let input = ServiceConfigInput {
            vault_path: Some(std::path::PathBuf::from("/tmp/vault")),
            auth: Some(AuthConfigInput {
                enabled: Some(true),
                token_ref: Some(SecretRef::OsKeyring {
                    service: "deep-obsidian-mcp".to_string(),
                    account: "http-auth-token".to_string(),
                }),
                allowed_origins: Some(vec!["https://app.example".to_string()]),
            }),
            ..ServiceConfigInput::default()
        };
        let resolved = normalize_service_config(input).expect("normalize");
        assert!(resolved.auth.enabled);
        assert_eq!(resolved.auth.allowed_origins, vec!["https://app.example"]);

        let persisted = to_persisted_config(&resolved);
        let auth = persisted.auth.as_ref().expect("auth persisted");
        assert_eq!(auth.enabled, Some(true));
        assert!(auth.token_ref.is_some());

        // The serialized form references the secret, never a plaintext token.
        let serialized = serde_json::to_string(&persisted).expect("serialize");
        assert!(serialized.contains("tokenRef"));
        assert!(serialized.contains("osKeyring"));
    }

    #[test]
    fn auth_omitted_when_disabled_and_empty() {
        let input = ServiceConfigInput {
            vault_path: Some(std::path::PathBuf::from("/tmp/vault")),
            ..ServiceConfigInput::default()
        };
        let resolved = normalize_service_config(input).expect("normalize");
        assert!(to_persisted_config(&resolved).auth.is_none());
    }

    #[test]
    fn expand_home_path_expands_tilde_prefix() {
        let home = std::env::var("HOME").expect("HOME");
        let home_path = std::path::PathBuf::from(home);
        assert_eq!(expand_home_path("~/vault"), home_path.join("vault"));
        assert_eq!(expand_home_path("~"), home_path);
    }

    #[cfg(target_os = "macos")]
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

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn default_packaged_index_dir_uses_xdg_data_home() {
        let path = default_packaged_index_dir(std::path::Path::new("~/Vault"));
        // Lives under <app-dir>/indexes/<16-hex-hash>, regardless of whether
        // XDG_DATA_HOME is set in the environment.
        let rendered = path.to_string_lossy();
        assert!(rendered.contains(DEFAULT_CONFIG_APP_DIR));
        assert!(path.parent().unwrap().ends_with("indexes"));
        assert_eq!(path.file_name().unwrap().to_string_lossy().len(), 16);
    }
}
