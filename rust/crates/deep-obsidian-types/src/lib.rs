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
// The couchdb variant is much larger than the filesystem one (a connection shape and
// two secret references against two paths). Boxing it to equalize them is refused:
// this enum is deserialized once per mount at startup and lives in a `Vec` of at most
// a handful of entries, so its size is not a cost anywhere, while an indirection would
// complicate every `match` arm and the serde round trip for nothing.
#[allow(clippy::large_enum_variant)]
pub enum MountBackendConfig {
    /// A vault rooted at a local directory. `vaultPath` accepts `~` just like the
    /// top-level `vaultPath` does.
    ///
    /// The per-variant `rename_all` is required: on a tagged enum the container
    /// attribute renames VARIANTS, not their fields.
    #[serde(rename_all = "camelCase")]
    Filesystem {
        vault_path: PathBuf,
        /// Where this mount's search index lives.
        ///
        /// For the ROOT mount it is the fallback when the top-level `indexDir` is
        /// unset. For a non-root mount it overrides the derived default,
        /// `<root indexDir>/mounts/<id>` — see
        /// `deep_obsidian_config::default_mount_index_dir` for why that default is
        /// keyed by mount id and cannot collide with any other mount's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index_dir: Option<PathBuf>,
    },
    /// A read-only Self-hosted LiveSync vault in CouchDB, reached through the
    /// supervised Node sidecar (`sidecar/livesync-sidecar`).
    ///
    /// # Why there is no plaintext password field
    ///
    /// Mirrors [`EmbeddingConfig::api_key_ref`] and [`AuthConfig::token_ref`]: a
    /// secret is only ever a [`SecretRef`] pointing at the OS keyring or the
    /// encrypted secrets file, never a literal. That is what makes
    /// `redact_config` an identity function — the persisted config has nothing
    /// secret in it to redact. `username` is deliberately plaintext: a CouchDB
    /// user name is an identifier, not a credential, exactly like `baseUrl` on
    /// the embedding config.
    ///
    /// `url` is validated to carry no userinfo (`https://user:pw@host`), because
    /// it is rendered verbatim by `doctor` and `print-config`.
    #[serde(rename_all = "camelCase")]
    Couchdb {
        /// CouchDB server origin WITHOUT the database path, e.g.
        /// `https://couch.example`. Must not contain userinfo.
        url: String,
        /// The LiveSync database name.
        database: String,
        /// CouchDB user name. Not a secret; see the variant docs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        /// Reference to the stored CouchDB password. Resolved at backend
        /// construction and handed to the sidecar only through `initialize`.
        password_ref: SecretRef,
        /// End-to-end-encryption material, when the vault is encrypted or has
        /// path obfuscation enabled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        e2ee: Option<CouchdbE2eeConfig>,
        /// Explicit path to the built sidecar bundle. When unset the backend
        /// falls back to `DEEP_OBSIDIAN_LIVESYNC_SIDECAR` and then to a
        /// bundled-relative default; see
        /// `deep_obsidian_backend::sidecar::locate_sidecar_bundle`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sidecar_path: Option<PathBuf>,
        /// Where this mount's search index lives. Defaults to
        /// `<root indexDir>/mounts/<id>`, exactly as for a filesystem mount.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index_dir: Option<PathBuf>,
        /// Chunking / hashing knobs forwarded verbatim to the sidecar's
        /// `initialize.options`. These must match how the vault was written.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<CouchdbOptions>,
        /// Whether this mount accepts writes. **Defaults to `false`.**
        ///
        /// # Why this is per-mount and not another experimental flag
        ///
        /// `experimental.couchdbVaults` answers "may this build talk to CouchDB at
        /// all", which is a question about the backend's maturity and is the same
        /// answer for every mount. Writability answers "may the agent edit THIS
        /// vault", which is a question about one vault's role — a user may well want
        /// a writable scratch vault and a read-only archive in the same table, and a
        /// single global flag could not express that.
        ///
        /// The two therefore compose rather than duplicate: `couchdbVaults` gates
        /// the mount existing, `writable` gates it being written. A writable mount
        /// needs both, and `false` by default means every existing config keeps
        /// exactly the read-only behaviour it has today.
        ///
        /// Setting it is what makes the mount's sidecar initialize `read-write`;
        /// nothing else in the process can unlock a write.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        writable: bool,
    },
    /// A shared, Markdown-only corpus stored as records in an Algolia index.
    ///
    /// # What this backend is, and is not
    ///
    /// The index IS the vault: there is no local mirror of the corpus and no
    /// intention of building one, which is what makes a mount joinable by several
    /// participants at once. Notes are stored as one small `note` record (metadata
    /// plus the head-version pointer) and one `chunk` record per chunk of the
    /// current version; a read reassembles the body from the chunks.
    ///
    /// Consequently it holds **Markdown only**. Binary attachments have no record
    /// shape here, so reads, writes and out-of-band uploads of one are refused with
    /// a message that says exactly that — see
    /// `deep_obsidian_backend::ALGOLIA_NO_BINARY_MESSAGE`.
    ///
    /// # Why there is no plaintext key field
    ///
    /// Identical to the couchdb variant's reasoning: a secret is only ever a
    /// [`SecretRef`], never a literal, which is what keeps `redact_config` an
    /// identity function. `appId` and `indexName` are identifiers, not credentials,
    /// exactly like `username` on the couchdb variant.
    ///
    /// `baseUrl` is validated to carry no userinfo, because it is rendered verbatim
    /// by `doctor` and `print-config`.
    #[serde(rename_all = "camelCase")]
    Algolia {
        /// Algolia application id, e.g. `ABC1234XYZ`. Not a secret.
        app_id: String,
        /// The main index holding the corpus. Its `_history` sibling is derived
        /// from this name and provisioned lazily on the first supersession.
        index_name: String,
        /// Reference to the stored Algolia API key. Resolved at backend
        /// construction; see the variant docs.
        api_key_ref: SecretRef,
        /// Override for the REST endpoint (`https://{appId}.algolia.net` by
        /// default). Exists so a test, a demo or a proxy can be pointed at;
        /// must not contain userinfo.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        /// Whether this mount accepts writes. **Defaults to `false`**, for the
        /// same reason it does on the couchdb variant: `experimental.algoliaVaults`
        /// gates the mount existing, `writable` gates it being written, and a user
        /// may well want one writable shared wiki and one read-only mirror in the
        /// same table.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        writable: bool,
        /// Who this participant is in the shared corpus's audit trail. Lands on
        /// every record this mount writes, so several people writing one index can
        /// tell whose version they are looking at.
        ///
        /// Defaults to `<user>@unknown`; see
        /// `deep_obsidian_backend::algolia::default_participant_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        participant_id: Option<String>,
        /// Bounded local cache of hydrated note bodies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<AlgoliaCacheConfig>,
        /// How much version history to keep per note.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retention: Option<AlgoliaRetentionConfig>,
        /// Where this mount's *cache* lives. There is no local search index for an
        /// Algolia mount (see the variant docs), so this directory holds the
        /// hydrated-note cache only. Defaults to `<root indexDir>/mounts/<id>`,
        /// exactly as for every other mount kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index_dir: Option<PathBuf>,
    },
}

impl MountBackendConfig {
    /// The provider name, matching the serde tag.
    pub fn kind_name(&self) -> &'static str {
        match self {
            MountBackendConfig::Filesystem { .. } => "filesystem",
            MountBackendConfig::Couchdb { .. } => "couchdb",
            MountBackendConfig::Algolia { .. } => "algolia",
        }
    }
}

/// Bounded LRU cache of hydrated note bodies for an Algolia mount.
///
/// The cache is never a write buffer: a write pushes upstream first and only then
/// updates the cache, so a crash can lose a cache entry but never a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlgoliaCacheConfig {
    /// Byte budget for cached bodies. Defaults to
    /// `deep_obsidian_backend::algolia::DEFAULT_CACHE_MAX_BYTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// Mount-relative path prefixes exempt from eviction. Pinned entries still
    /// count against `maxBytes`, so pinning more than the budget is a
    /// configuration mistake rather than an unbounded cache.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_prefixes: Vec<String>,
}

/// How much version history one note keeps on an Algolia mount.
///
/// The rule is a floor UNIONED with a ceiling, never an intersection: keep the
/// `minVersions` most recent versions PLUS anything younger than `maxAgeDays`. A
/// note nobody has touched in a year therefore still has its last few versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlgoliaRetentionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_versions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_days: Option<u64>,
}

/// E2EE material for a LiveSync vault. Both passphrases are [`SecretRef`]s for
/// the same reason the CouchDB password is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouchdbE2eeConfig {
    /// Reference to the vault's end-to-end-encryption passphrase.
    pub passphrase_ref: SecretRef,
    /// Reference to the path-obfuscation passphrase. Set only when the vault
    /// has path obfuscation enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obfuscate_passphrase_ref: Option<SecretRef>,
}

/// Tuning knobs mirroring the sidecar's `InitializeOptions` one-for-one.
///
/// Every field is optional and omitted when unset, so an unset section forwards
/// nothing and the sidecar applies upstream's own defaults. Deliberately
/// `deny_unknown_fields`: a typo'd chunking knob would silently change how
/// content is reassembled, which is exactly the class of bug that must fail loudly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouchdbOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_chunk_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_chunk_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_alg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_eden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_compression: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle_filename_case_sensitive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_splitter_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee_algorithm: Option<String>,
    /// Per-HTTP-request timeout applied to the sidecar's CouchDB transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
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
    /// Required to resolve a config declaring a `couchdb` mount.
    ///
    /// Separate from [`Self::multi_vault`] even though a couchdb mount always
    /// implies a multi-mount table this slice (couchdb cannot be the root mount):
    /// the two flags gate different risks. `multiVault` is about routing across
    /// several vaults; `couchdbVaults` is about a read-only backend that
    /// supervises a Node child process and reads a format owned by a
    /// community plugin. Retiring one must not silently retire the other.
    #[serde(default)]
    pub couchdb_vaults: bool,
    /// Required to resolve a config declaring an `algolia` mount.
    ///
    /// Its own flag for the same reason `couchdbVaults` is: the risk it gates is
    /// specific to this backend. An Algolia mount sends note bodies to a hosted
    /// third-party service and, when `writable`, is a corpus SEVERAL people write
    /// concurrently — neither of which is what `multiVault` is about.
    #[serde(default)]
    pub algolia_vaults: bool,
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
