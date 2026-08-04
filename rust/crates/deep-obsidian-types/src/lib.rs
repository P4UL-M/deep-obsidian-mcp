use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportMode {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StdioMode {
    Auto,
    Newline,
    Framed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub mcp_path: String,
    pub health_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HttpConfigInput {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub mcp_path: Option<String>,
    pub health_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoReindexConfig {
    pub enabled: bool,
    pub debounce_ms: u64,
    pub interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoReindexConfigInput {
    pub enabled: Option<bool>,
    pub debounce_ms: Option<u64>,
    pub interval_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EmbeddingProvider {
    #[default]
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SecretRef {
    OsKeyring { service: String, account: String },
    EncryptedFile { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<EmbeddingProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<SecretRef>,
    /// Hard per-input character ceiling (default applied if unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
    /// Per-input token budget (default applied if unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<usize>,
    /// Backend context window in tokens; must match the embedding server's
    /// allocated `num_ctx` (default applied if unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<usize>,
    /// Optional query-side task instruction for instruction-tuned embedding models
    /// (e.g. qwen3-embedding). Explicit user override; when unset an auto-default is
    /// applied at runtime for recognized instruct models. Query-side only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_instruction: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddingConfigInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<EmbeddingProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<SecretRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_instruction: Option<String>,
}

/// HTTP transport authentication. Optional and disabled by default so existing
/// loopback deployments keep working untouched. The bearer token itself is never
/// stored here; only a [`SecretRef`] pointing at the OS keyring or the encrypted
/// secrets file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    /// When true, `/mcp` and `/upload` require a matching `Authorization: Bearer`
    /// token.
    pub enabled: bool,
    /// Reference to the stored bearer token. Resolved at startup through the
    /// shared secret store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_ref: Option<SecretRef>,
    /// Browser `Origin` values permitted to reach the protected routes. Empty
    /// means any request that sends an `Origin` header is rejected (DNS-rebinding
    /// protection); non-browser clients that omit `Origin` are always allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfigInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_ref: Option<SecretRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_origins: Option<Vec<String>>,
}

/// Where one mount's content actually lives.
///
/// Tagged by `kind` so a new provider is a purely additive change: adding
/// `{"kind": "couchdb", ...}` neither renames nor reshapes the existing variant,
/// and an old binary reading a new config fails with "unknown variant" rather
/// than silently mis-parsing. Every variant is expected to carry its own
/// connection shape; nothing is hoisted to the mount level except `id` and
/// `mountAt`, which are provider-independent by definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MountBackendConfig {
    /// A vault rooted at a local directory. `vaultPath` accepts `~` just like the
    /// top-level `vaultPath` does.
    ///
    /// The per-variant `rename_all` is required: on a tagged enum the container
    /// attribute renames VARIANTS, not their fields.
    #[serde(rename_all = "camelCase")]
    Filesystem {
        vault_path: PathBuf,
        /// Reserved for the per-mount index slice. For the ROOT mount it is
        /// honoured as the fallback when the top-level `indexDir` is unset; for a
        /// non-root mount it is recorded and validated but not yet consumed,
        /// because the search index still covers only the root mount.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index_dir: Option<PathBuf>,
    },
}

impl MountBackendConfig {
    /// The provider name, matching the serde tag.
    pub fn kind_name(&self) -> &'static str {
        match self {
            MountBackendConfig::Filesystem { .. } => "filesystem",
        }
    }
}

/// One mount in the vault's logical namespace.
///
/// The same type serves the input, persisted, and resolved forms: the only
/// normalization a mount needs is `mountAt` canonicalization and `~` expansion,
/// and both are idempotent, so a second pass over an already-resolved mount is a
/// no-op. That keeps `print-config` a true round trip instead of a lossy
/// re-render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountConfig {
    /// Stable, user-chosen identifier. Surfaces in error messages and in
    /// `vault_info`, so it is restricted to a conservative slug (see
    /// `deep-obsidian-config`).
    pub id: String,
    /// Logical vault-relative folder prefix this mount appears at. `""` is the
    /// vault root. Stored without leading or trailing slashes, forward slashes
    /// only; `"/"` and `"/Team/"` both normalize to `""` and `"Team"`.
    #[serde(default)]
    pub mount_at: String,
    pub backend: MountBackendConfig,
}

/// Opt-in flags for behaviour that is not yet stable.
///
/// Deliberately WITHOUT `deny_unknown_fields`: a config written by a newer build
/// that names a flag this build has never heard of must still load, because the
/// flags themselves are expected to churn and disappear. Unknown flags are
/// dropped, which is the correct reading of "a feature this build does not have".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentalConfig {
    /// Required to resolve a config with anything other than a single root mount.
    #[serde(default)]
    pub multi_vault: bool,
}

impl ExperimentalConfig {
    /// True when no flag is set, i.e. the section carries no information and can
    /// be omitted from a persisted config.
    pub fn is_default(&self) -> bool {
        self == &ExperimentalConfig::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceConfigInput {
    pub vault_path: Option<PathBuf>,
    /// Explicit mount table. Mutually exclusive with `vault_path`; see
    /// `deep-obsidian-config::normalize_service_config`.
    pub mounts: Option<Vec<MountConfig>>,
    pub experimental: Option<ExperimentalConfig>,
    pub index_dir: Option<PathBuf>,
    pub transport: Option<TransportMode>,
    pub stdio_mode: Option<StdioMode>,
    pub http: Option<HttpConfigInput>,
    pub auto_reindex: Option<AutoReindexConfigInput>,
    pub embedding: Option<EmbeddingConfigInput>,
    pub artifact_embedding: Option<EmbeddingConfigInput>,
    pub auth: Option<AuthConfigInput>,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PersistedServiceConfig {
    pub vault_path: Option<PathBuf>,
    /// Omitted entirely for a legacy `vaultPath`-only config, so saving one back
    /// leaves it legacy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<MountConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<ExperimentalConfig>,
    pub index_dir: Option<PathBuf>,
    pub transport: Option<TransportMode>,
    pub stdio_mode: Option<StdioMode>,
    pub http: Option<HttpConfigInput>,
    pub auto_reindex: Option<AutoReindexConfigInput>,
    pub embedding: Option<EmbeddingConfigInput>,
    pub artifact_embedding: Option<EmbeddingConfigInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfigInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedServiceConfig {
    /// The ROOT mount's vault path. Unchanged meaning: the runtime watcher, the
    /// search index, and `doctor` all still consume exactly this. A resolved
    /// config always has a root mount, so this always has a meaning.
    pub vault_path: PathBuf,
    /// The mount table AS DECLARED, normalized.
    ///
    /// Empty means the config declared no `mounts` at all — i.e. it is a legacy
    /// `vaultPath` config whose single root mount is implicit. Reading this field
    /// directly is almost always wrong; use [`ResolvedServiceConfig::mount_table`],
    /// which materializes the implicit root mount so callers see one shape.
    ///
    /// The distinction is kept only so a legacy config saved back stays legacy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountConfig>,
    #[serde(default, skip_serializing_if = "ExperimentalConfig::is_default")]
    pub experimental: ExperimentalConfig,
    pub index_dir: PathBuf,
    pub transport: TransportMode,
    pub stdio_mode: StdioMode,
    pub http: HttpConfig,
    pub auto_reindex: AutoReindexConfig,
    pub embedding: EmbeddingConfig,
    pub artifact_embedding: EmbeddingConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEndpoints {
    pub mcp: String,
    pub health: String,
}

/// Identifier given to the implicit root mount of a legacy `vaultPath` config.
/// Chosen to be a valid mount id, so a user migrating to an explicit `mounts`
/// table can keep it verbatim and nothing (ids in errors, in `vault_info`) moves.
pub const IMPLICIT_ROOT_MOUNT_ID: &str = "vault";

impl ResolvedServiceConfig {
    /// The mount table every consumer should route against.
    ///
    /// Always non-empty and always containing exactly one mount whose `mount_at`
    /// is `""`, because [`normalize_service_config`](../deep_obsidian_config/fn.normalize_service_config.html)
    /// enforces both. A legacy config yields one synthesized root mount, so a
    /// caller never has to special-case the legacy shape.
    pub fn mount_table(&self) -> Vec<MountConfig> {
        if self.mounts.is_empty() {
            return vec![MountConfig {
                id: IMPLICIT_ROOT_MOUNT_ID.to_string(),
                mount_at: String::new(),
                backend: MountBackendConfig::Filesystem {
                    vault_path: self.vault_path.clone(),
                    index_dir: None,
                },
            }];
        }
        self.mounts.clone()
    }

    /// True when more than one mount is in play. The gate for every
    /// "not federated yet" guard: single-mount configs must never take one.
    pub fn is_multi_mount(&self) -> bool {
        self.mounts.len() > 1
    }

    pub fn service_endpoints(&self) -> ServiceEndpoints {
        ServiceEndpoints {
            mcp: format!(
                "http://{}:{}{}",
                self.http.host, self.http.port, self.http.mcp_path
            ),
            health: format!(
                "http://{}:{}{}",
                self.http.host, self.http.port, self.http.health_path
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes_api_key_ref() {
        let config: PersistedServiceConfig = serde_json::from_str(
            r#"{
                "embedding": {
                    "provider": "openai-compatible",
                    "model": "nomic-embed-text",
                    "baseUrl": "http://localhost:11434/v1",
                    "apiKeyRef": {
                        "kind": "encryptedFile",
                        "id": "openai-embedding"
                    }
                }
            }"#,
        )
        .expect("parse config");

        let reference = config
            .embedding
            .as_ref()
            .and_then(|embedding| embedding.api_key_ref.as_ref())
            .expect("api key ref");
        assert!(matches!(
            reference,
            SecretRef::EncryptedFile { id } if id == "openai-embedding"
        ));

        let serialized = serde_json::to_string(&config).expect("serialize config");
        assert!(serialized.contains("apiKeyRef"));
        assert!(serialized.contains("encryptedFile"));
        assert!(!serialized.contains("apiKey\""));
        assert!(!serialized.contains("apiKeyEnv"));
    }

    #[test]
    fn rejects_old_plaintext_secret_fields() {
        let error = serde_json::from_str::<PersistedServiceConfig>(
            r#"{
                "embedding": {
                    "provider": "openai-compatible",
                    "model": "text-embedding-3-small",
                    "apiKey": "secret"
                }
            }"#,
        )
        .expect_err("old apiKey should be rejected");

        assert!(error.to_string().contains("apiKey"));
    }
}
