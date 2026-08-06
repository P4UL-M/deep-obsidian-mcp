use deep_obsidian_types::{
    AuthConfig, AuthConfigInput, AutoReindexConfig, AutoReindexConfigInput, EmbeddingConfig,
    EmbeddingConfigInput, EmbeddingProvider, HttpConfig, HttpConfigInput, MountBackendConfig,
    MountConfig, PersistedServiceConfig, ResolvedServiceConfig, ServiceConfigInput, StdioMode,
    TransportMode, UnknownFields,
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
    /// A mount table with no mount at the vault root.
    ///
    /// Still rejected now that the root mount may be ANY backend kind, and for a
    /// reason that has nothing to do with `vaultPath`: the router resolves a logical
    /// path by longest matching prefix, and `""` is the only prefix that matches
    /// everything. Without a mount there, every path outside every declared prefix
    /// resolves to nothing — `list_children("")` has no answer, and a typo in a prefix
    /// silently becomes "no such path" instead of landing in the root vault. A rootless
    /// table is therefore not a vault with a hole in it, it is a vault with no floor.
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
    /// A `recallWeight` that cannot produce a meaningful federated ordering.
    ///
    /// Zero and negative are rejected rather than clamped: a zero weight would drop
    /// the mount out of every federated ranking while `vault_info` still reported it
    /// healthy and scoped search still answered from it, and a negative weight would
    /// order that mount's hits worst-first. Both are silent wrong answers, which is
    /// exactly what a config error exists to prevent.
    #[error("invalid recallWeight {weight} on mount {id:?}: it must be a finite number greater than 0 (omit it for the default 1.0). A weight of 0 would silently remove the mount from every federated ranking while leaving it listed as healthy.")]
    InvalidRecallWeight { id: String, weight: f64 },
    /// More than a single root mount without the opt-in flag.
    #[error("multi-vault mounts are experimental: set {{\"experimental\": {{\"multiVault\": true}}}} in the config to resolve a table of {count} mounts")]
    MultiVaultNotEnabled { count: usize },
    /// A `couchdb` mount without the opt-in flag.
    ///
    /// Checked BEFORE [`Self::MultiVaultNotEnabled`]: a couchdb mount used to be
    /// non-root-only, so it always made the table multi-mount and both gates would
    /// fire. It can now be the root — in which case a single-mount fully-remote table
    /// needs only THIS flag — but the ordering is kept: when both gates do apply, the
    /// couchdb message is the more specific and more actionable of the two, and it
    /// names the flag the user has actually not set yet for the feature they were
    /// trying to use.
    /// The wording says EXPERIMENTAL but no longer says READ-ONLY: a couchdb mount is
    /// read-only unless it sets `writable`, so the flag this error is about gates the
    /// mount existing, not the mount being written. Saying otherwise would tell a user
    /// that enabling the flag cannot give them writes, which is not true.
    #[error("couchdb (Self-hosted LiveSync) vaults are EXPERIMENTAL: set {{\"experimental\": {{\"couchdbVaults\": true}}}} in the config to resolve mount {id:?} (the mount is read-only unless it also sets \"writable\": true)")]
    CouchdbVaultsNotEnabled { id: String },
    /// A CouchDB URL carrying `user:password@` userinfo.
    ///
    /// Rejected at validation rather than stripped at render time: the URL is
    /// printed verbatim by `doctor` and `print-config`, and a password that
    /// reached the config file at all has already been written to disk in
    /// plaintext. Failing loudly is the only answer that tells the user to move
    /// it into `passwordRef`.
    #[error("mount {id:?} has a couchdb url containing embedded credentials; remove the 'user:password@' userinfo from the url and store the password as a secret reference in 'passwordRef' instead (the url is printed verbatim by 'doctor' and 'print-config')")]
    CouchdbUrlHasUserinfo { id: String },
    /// A `couchdb` mount with an empty `url` or `database`.
    #[error("mount {id:?} has an invalid couchdb backend: {reason}")]
    InvalidCouchdbBackend { id: String, reason: &'static str },
    /// An `algolia` mount without the opt-in flag.
    ///
    /// Checked before [`Self::MultiVaultNotEnabled`] for the same reason the
    /// couchdb gate is; see [`Self::CouchdbVaultsNotEnabled`] for the ordering
    /// argument. This one names the flag the user has actually not set for the
    /// feature they were reaching for.
    #[error("algolia (shared Markdown corpus) vaults are EXPERIMENTAL: set {{\"experimental\": {{\"algoliaVaults\": true}}}} in the config to resolve mount {id:?} (the mount is read-only unless it also sets \"writable\": true)")]
    AlgoliaVaultsNotEnabled { id: String },
    /// An Algolia `baseUrl` carrying `user:password@` userinfo.
    ///
    /// Refused rather than stripped, exactly as for a couchdb `url`: `baseUrl` is
    /// printed verbatim by `doctor` and `print-config`, so a credential that got
    /// into it has already been written to disk in plaintext and the only useful
    /// answer is to say so.
    #[error("mount {id:?} has an algolia baseUrl containing embedded credentials; remove the 'user:password@' userinfo from the url and store the key as a secret reference in 'apiKeyRef' instead (the url is printed verbatim by 'doctor' and 'print-config')")]
    AlgoliaBaseUrlHasUserinfo { id: String },
    /// An `algolia` mount with an empty `appId` or `indexName`.
    #[error("mount {id:?} has an invalid algolia backend: {reason}")]
    InvalidAlgoliaBackend { id: String, reason: &'static str },
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

/// Directory component under a root index dir reserved for non-root mounts.
/// A literal, so the collision argument below can name it.
pub const MOUNT_INDEX_DIR_SEGMENT: &str = "mounts";

/// Where the ROOT mount's index lives when the root has no local directory to hang it
/// off — i.e. when the root mount is a couchdb or algolia backend.
///
/// # Why it must be anchored outside any vault
///
/// [`default_index_dir`] puts the index INSIDE the vault (`<vault>/.deep-obsidian-mcp`),
/// which is the right default precisely because a filesystem vault is a directory the
/// user already owns. A remote root has no such directory, so the index has to live
/// somewhere else, and the only somewhere-else this codebase already trusts is
/// [`packaged_data_dir`] — macOS Application Support, otherwise `XDG_DATA_HOME`. That
/// is the same anchor [`default_packaged_index_dir`] uses, for the same reason.
///
/// # Keyed by MOUNT ID, and why the extra `mounts/` segment is load-bearing
///
/// Id keying rather than hash keying, exactly as [`default_mount_index_dir`] chose and
/// for the identical reason: ids are unique per config
/// ([`ConfigError::DuplicateMountId`]) and constrained to `[a-z0-9][a-z0-9-]*`, so they
/// are 1:1 with the runtime that owns the directory, whereas a hash of the remote's url
/// would give two mounts naming the same database one index directory and therefore two
/// `RuntimeState`s writing one SQLite file.
///
/// But the id CANNOT go directly under `indexes/`, and this is a real collision rather
/// than a theoretical one: [`stable_vault_hash`] renders 16 lowercase hex characters,
/// and `[a-z0-9][a-z0-9-]*` accepts every one of those strings. A mount id of
/// `"abcdef0123456789"` would land on `indexes/abcdef0123456789`, which is also the
/// packaged index directory of whichever filesystem vault happens to hash to it — two
/// unrelated vaults, one `index.sqlite`. Interposing the reserved
/// [`MOUNT_INDEX_DIR_SEGMENT`] makes that structurally impossible: a 16-hex-char hash
/// can never be the literal `"mounts"`, so the two namespaces are disjoint by shape and
/// not by luck.
///
/// # Non-collision, in full
///
/// * with a filesystem root's packaged index (`indexes/<16 hex>`) — different first
///   segment under `indexes/`, as argued above;
/// * with a filesystem root's in-vault index (`<vault>/.deep-obsidian-mcp`) — a
///   different tree entirely;
/// * between two remote roots in two different configs — distinct ids give distinct
///   directories; identical ids in two configs pointing at two different remotes DO
///   collide, which is the same exposure a hand-written `indexDir` has always had and
///   the same one [`default_mount_index_dir`] carries. An operator running two services
///   over two remotes sets an explicit `indexDir` on one, exactly as they must today for
///   two mounts that share an id across configs;
/// * with this config's NON-root mounts — their default is
///   `<root index_dir>/mounts/<id>`, i.e. nested INSIDE the directory this function
///   returns, and their ids differ from the root's. So the root's `index.sqlite` and
///   each non-root mount's subdirectory are siblings under one anchor, which is exactly
///   the arrangement a filesystem root already produces. One rule to remember, not two.
pub fn default_remote_root_index_dir(mount_id: &str) -> PathBuf {
    packaged_data_dir()
        .join("indexes")
        .join(MOUNT_INDEX_DIR_SEGMENT)
        .join(mount_id)
}

/// Where a NON-ROOT mount's index lives when its own `indexDir` is unset.
///
/// Derived from the ROOT mount's already-resolved `index_dir` rather than from the
/// mount's vault path, for one decisive reason: **packaged mode is only recorded in
/// the resolved root `index_dir`**. Nothing at serve time can tell a packaged
/// install (whose indexes must live under Application Support / `XDG_DATA_HOME`,
/// outside every vault) from a source install; the installer expresses it by
/// writing an explicit `indexDir`. Nesting under that path therefore inherits
/// packaged-ness for free, and inherits an operator's explicit `indexDir` too.
///
/// Keyed by MOUNT ID, not by a vault-path hash like
/// [`default_packaged_index_dir`]. Hash keying would give two mounts that name the
/// same `vaultPath` the same index directory, i.e. two independent
/// `RuntimeState`s writing one SQLite file. Mount ids are unique per config
/// ([`ConfigError::DuplicateMountId`]) and constrained to `[a-z0-9][a-z0-9-]*`, so
/// id keying is strictly 1:1 with the runtime that owns it, is a single safe path
/// segment, and cannot collide on a case-insensitive filesystem.
///
/// # Non-collision
///
/// * with the ROOT mount: the root's index is `<index_dir>/index.sqlite` and the
///   index crate creates nothing else in that directory, so the
///   `<index_dir>/mounts/` subtree is unreachable from it;
/// * between non-root mounts: distinct ids, one segment each, no separators or
///   `.`/`..` (validated), so distinct sibling directories.
pub fn default_mount_index_dir(root_index_dir: &Path, mount_id: &str) -> PathBuf {
    root_index_dir.join(MOUNT_INDEX_DIR_SEGMENT).join(mount_id)
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
        // `url` and `database` are not paths. `sidecarPath` and `indexDir` are, and
        // both are plausible `~`-relative values (a checked-out sidecar bundle, an
        // index outside the vault), so both are expanded.
        MountBackendConfig::Couchdb {
            url,
            database,
            username,
            password_ref,
            e2ee,
            sidecar_path,
            index_dir,
            options,
            writable,
        } => MountBackendConfig::Couchdb {
            url,
            database,
            username,
            password_ref,
            e2ee,
            sidecar_path: sidecar_path.map(expand_home_path),
            index_dir: index_dir.map(expand_home_path),
            options,
            writable,
        },
        // `indexDir` is the only path here: an Algolia mount has no local corpus,
        // and `indexDir` holds nothing but its hydrated-note cache — which is
        // exactly the sort of thing an operator points at `~/Library/Caches/...`.
        MountBackendConfig::Algolia {
            app_id,
            index_name,
            api_key_ref,
            base_url,
            writable,
            participant_id,
            cache,
            retention,
            index_dir,
        } => MountBackendConfig::Algolia {
            app_id,
            index_name,
            api_key_ref,
            base_url,
            writable,
            participant_id,
            cache,
            retention,
            index_dir: index_dir.map(expand_home_path),
        },
    }
}

/// True when `url`'s authority carries `user[:password]@` userinfo.
///
/// Deliberately string-based rather than URL-parser-based: this crate has no URL
/// dependency, and the question is narrow. The authority is everything between
/// `//` and the next `/`, `?` or `#`; userinfo is an `@` inside it. An `@` later in
/// the path (legal, and not a credential) must not trip the check.
fn url_authority_has_userinfo(url: &str) -> bool {
    let Some(after_scheme) = url.split_once("//").map(|(_, rest)| rest) else {
        return false;
    };
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    after_scheme[..authority_end].contains('@')
}

/// Validate the parts of a couchdb backend that are checkable without connecting.
///
/// Reachability, credentials and remote compatibility are all deliberately NOT
/// checked here: they are the sidecar's `initialize` compatibility status, which is
/// a runtime readiness fact rather than a config error (a config must still load
/// while the CouchDB server is down).
fn validate_couchdb_backend(mount: &MountConfig) -> Result<(), ConfigError> {
    let MountBackendConfig::Couchdb { url, database, .. } = &mount.backend else {
        return Ok(());
    };
    if url.trim().is_empty() {
        return Err(ConfigError::InvalidCouchdbBackend {
            id: mount.id.clone(),
            reason:
                "'url' is empty; give the CouchDB server origin, e.g. \"https://couch.example\"",
        });
    }
    if database.trim().is_empty() {
        return Err(ConfigError::InvalidCouchdbBackend {
            id: mount.id.clone(),
            reason: "'database' is empty; give the LiveSync database name",
        });
    }
    if url_authority_has_userinfo(url) {
        return Err(ConfigError::CouchdbUrlHasUserinfo {
            id: mount.id.clone(),
        });
    }
    Ok(())
}

/// Validate the parts of an algolia backend that are checkable without connecting.
///
/// Reachability, key validity and key ACLs are deliberately NOT checked here: they
/// are runtime readiness facts (the mount's `get_settings` probe), and a config must
/// still load while Algolia is unreachable or the key has been rotated.
///
/// `participantId` is validated for SHAPE rather than existence. It is absent by
/// default and defaulted at construction, but when a user does set it, it goes into
/// every record's audit trail and into `filters` expressions, so a value containing
/// a quote or a newline would be an injection into a filter string rather than a
/// name. Rejecting it here is the only place that can.
fn validate_algolia_backend(mount: &MountConfig) -> Result<(), ConfigError> {
    let MountBackendConfig::Algolia {
        app_id,
        index_name,
        base_url,
        participant_id,
        ..
    } = &mount.backend
    else {
        return Ok(());
    };
    if app_id.trim().is_empty() {
        return Err(ConfigError::InvalidAlgoliaBackend {
            id: mount.id.clone(),
            reason: "'appId' is empty; give the Algolia application id, e.g. \"ABC1234XYZ\"",
        });
    }
    if index_name.trim().is_empty() {
        return Err(ConfigError::InvalidAlgoliaBackend {
            id: mount.id.clone(),
            reason: "'indexName' is empty; give the index holding the shared corpus",
        });
    }
    if let Some(base_url) = base_url {
        if base_url.trim().is_empty() {
            return Err(ConfigError::InvalidAlgoliaBackend {
                id: mount.id.clone(),
                reason: "'baseUrl' is present but empty; omit it to use \
                         https://{appId}.algolia.net",
            });
        }
        if url_authority_has_userinfo(base_url) {
            return Err(ConfigError::AlgoliaBaseUrlHasUserinfo {
                id: mount.id.clone(),
            });
        }
    }
    if let Some(participant_id) = participant_id {
        if participant_id.trim().is_empty() {
            return Err(ConfigError::InvalidAlgoliaBackend {
                id: mount.id.clone(),
                reason: "'participantId' is present but empty; omit it to default to \
                         \"<user>@unknown\"",
            });
        }
        if participant_id
            .chars()
            .any(|character| character == '"' || character == '\\' || character.is_control())
        {
            return Err(ConfigError::InvalidAlgoliaBackend {
                id: mount.id.clone(),
                reason: "'participantId' must not contain quotes, backslashes or control \
                         characters: it is written into every record and into index filter \
                         expressions",
            });
        }
    }
    Ok(())
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
        // Validated here rather than at the fusion call site: a weight that cannot
        // produce a meaningful ordering is a CONFIG mistake, and discovering it on the
        // first federated query — after the server has come up reporting every mount
        // healthy — would surface it as a search bug.
        if let Some(weight) = mount.recall_weight {
            if !weight.is_finite() || weight <= 0.0 {
                return Err(ConfigError::InvalidRecallWeight { id, weight });
            }
        }
        normalized.push(MountConfig {
            id,
            mount_at,
            backend: expand_mount_backend_paths(mount.backend),
            recall_weight: mount.recall_weight,
            // Carried, not defaulted: this is the one place a mount is rebuilt field
            // by field, so dropping the retained keys here would make the retention on
            // `MountConfig` a no-op for every config that actually goes through the
            // loader — i.e. all of them.
            unknown: mount.unknown,
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
            // Per-backend gates first. A remote backend can now be the ROOT mount, so
            // it no longer forces the table multi-mount and `MultiVaultNotEnabled` no
            // longer necessarily also applies — but where both do apply the
            // backend-specific error still wins, because it is the more actionable one.
            // See `ConfigError::CouchdbVaultsNotEnabled`.
            for mount in &mounts {
                match &mount.backend {
                    MountBackendConfig::Filesystem { .. } => continue,
                    MountBackendConfig::Couchdb { .. } => {
                        if !experimental.couchdb_vaults {
                            return Err(ConfigError::CouchdbVaultsNotEnabled {
                                id: mount.id.clone(),
                            });
                        }
                        validate_couchdb_backend(mount)?;
                    }
                    MountBackendConfig::Algolia { .. } => {
                        if !experimental.algolia_vaults {
                            return Err(ConfigError::AlgoliaVaultsNotEnabled {
                                id: mount.id.clone(),
                            });
                        }
                        validate_algolia_backend(mount)?;
                    }
                }
            }
            // A single explicit root mount is exactly the legacy shape spelled out
            // longhand, so it needs no flag. Anything else does.
            if mounts.len() > 1 && !experimental.multi_vault {
                return Err(ConfigError::MultiVaultNotEnabled {
                    count: mounts.len(),
                });
            }
            let root_mount = &mounts[root];
            // A filesystem root resolves EXACTLY as it always has: its `vaultPath`
            // becomes the top-level one and its `indexDir` becomes the fallback for the
            // top-level `indexDir`. A remote root has no vault path at all, and its
            // index-dir fallback is the XDG-anchored, id-keyed directory — see
            // `default_remote_root_index_dir`.
            let (vault_path, mount_index_dir) = match &root_mount.backend {
                MountBackendConfig::Filesystem {
                    vault_path,
                    index_dir,
                } => (Some(vault_path.clone()), index_dir.clone()),
                MountBackendConfig::Couchdb { index_dir, .. }
                | MountBackendConfig::Algolia { index_dir, .. } => (
                    None,
                    Some(
                        index_dir
                            .clone()
                            .unwrap_or_else(|| default_remote_root_index_dir(&root_mount.id)),
                    ),
                ),
            };
            (mounts, vault_path, mount_index_dir)
        }
        // Legacy: one implicit root mount. `mounts` stays empty so saving the
        // config back cannot invent a mount table the user never wrote.
        None => (
            Vec::new(),
            Some(expand_home_path(
                input.vault_path.ok_or(ConfigError::MissingVaultPath)?,
            )),
            None,
        ),
    };

    let index_dir = input
        .index_dir
        .map(expand_home_path)
        .or(mount_index_dir)
        // Infallible: `mount_index_dir` above is `Some` for every root that has no
        // `vault_path`, so this fallback is only reached with a filesystem root — where
        // it is the same in-vault derivation it always was.
        .unwrap_or_else(|| {
            default_index_dir(
                vault_path
                    .as_deref()
                    .expect("a root mount with no local vault path to have resolved an index dir"),
            )
        });
    let transport = input.transport.unwrap_or(TransportMode::Http);
    let stdio_mode = input.stdio_mode.unwrap_or(StdioMode::Auto);
    let http = normalize_http_input(input.http);
    let auto_reindex = normalize_auto_reindex_input(input.auto_reindex);
    let embedding = normalize_embedding_input(input.embedding);
    let artifact_embedding = normalize_embedding_input(input.artifact_embedding);
    let auth = normalize_auth_input(input.auth);

    Ok(ResolvedServiceConfig {
        // Absent means enabled: the rerank is what makes a federated answer RANKED rather
        // than merely merged, so a config that never mentions it gets it.
        federated_rerank: input.federated_rerank.unwrap_or(true),
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
    // Persisted in, persisted out: this is the one transform where retention is
    // unambiguous, so it is applied here rather than left to the caller.
    let retained = input.unknown.clone();
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
        federated_rerank: input.federated_rerank,
        config_file_path: None,
    })?;

    let mut persisted = to_persisted_config(&resolved);
    persisted.unknown = retained;
    Ok(persisted)
}

pub fn to_persisted_config(config: &ResolvedServiceConfig) -> PersistedServiceConfig {
    // A legacy config round-trips as legacy: `mounts` is empty exactly when the
    // user never wrote one, and `vaultPath` is written back as it always was. A
    // config that DID declare mounts round-trips the other way -- `mounts` is
    // emitted and `vaultPath` is omitted, because emitting both would produce a
    // file this same function's input validation rejects as ambiguous.
    let declared_mounts = !config.mounts.is_empty();
    PersistedServiceConfig {
        // A declared mount table never writes a top-level `vaultPath` (the two are
        // mutually exclusive on input), and a legacy config always has one — see the
        // invariant on `ResolvedServiceConfig::vault_path`.
        vault_path: if declared_mounts {
            None
        } else {
            config.vault_path.clone()
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
        // Only written back when it was TURNED OFF. Emitting `true` would add a key to every
        // config that never mentioned the flag, which is the opposite of a round trip.
        federated_rerank: if config.federated_rerank {
            None
        } else {
            Some(false)
        },
        // Empty here because a `ResolvedServiceConfig` does not carry them: the
        // resolved form is the SERVER's view, and threading a bag of keys it cannot
        // interpret through every one of its construction sites would buy nothing.
        // The writer restores them from the file it is about to replace — see
        // [`carry_unknown_fields`], which every config writer must call.
        unknown: UnknownFields::new(),
    }
}

/// Restore into `config` the unknown keys `previous` carried, so writing `config`
/// back does not delete them.
///
/// # Why this is a separate step rather than part of `to_persisted_config`
///
/// [`to_persisted_config`] takes a [`ResolvedServiceConfig`], which is the server's
/// interpreted view and deliberately holds no uninterpretable keys. The retained keys
/// only exist on the [`PersistedServiceConfig`] that `read_config_file` produced, so
/// only a caller holding BOTH — i.e. a writer that loaded the file it is replacing —
/// can reunite them. `setup-service` and the wizard are those callers.
///
/// # Precedence
///
/// A key this build DOES understand always wins: `config` was built from the resolved
/// configuration, so anything already present there is the user's current intent.
/// Only keys absent from `config` are taken from `previous`. Mount-level unknowns need
/// no help here — [`MountConfig`] carries its own through the loader.
///
/// A no-op when `previous` is `None` (first write) or carries nothing unknown.
pub fn carry_unknown_fields(
    config: &mut PersistedServiceConfig,
    previous: Option<&PersistedServiceConfig>,
) {
    let Some(previous) = previous else { return };
    for (key, value) in &previous.unknown {
        if !config.unknown.contains_key(key) {
            config.unknown.insert(key.clone(), value.clone());
        }
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
        carry_unknown_fields, default_mount_index_dir, default_packaged_index_dir,
        default_remote_root_index_dir, expand_home_path, is_loopback_host, is_valid_mount_id,
        normalize_persisted_config, normalize_service_config, read_config_file,
        to_persisted_config, write_config_file, ConfigError, DEFAULT_CONFIG_APP_DIR,
        MOUNT_INDEX_DIR_SEGMENT,
    };
    use deep_obsidian_types::{
        AuthConfigInput, CouchdbE2eeConfig, CouchdbOptions, ExperimentalConfig, MountBackendConfig,
        MountConfig, PersistedServiceConfig, SecretRef, ServiceConfigInput,
    };
    use std::path::{Path, PathBuf};

    /// A process- and test-unique scratch directory. The config crate has no
    /// `tempfile` dependency and does not need one: nothing here races on a name that
    /// carries both the pid and a per-call counter.
    fn temp_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "deep-obsidian-config-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // -----------------------------------------------------------------------
    // Mount table helpers
    // -----------------------------------------------------------------------

    fn filesystem_mount(id: &str, mount_at: &str, vault_path: &str) -> MountConfig {
        MountConfig {
            unknown: Default::default(),
            recall_weight: None,
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
            experimental: Some(ExperimentalConfig {
                multi_vault,
                ..ExperimentalConfig::default()
            }),
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
        assert_eq!(resolved.vault_path, Some(PathBuf::from("/tmp/vault")));
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
        assert_eq!(resolved.vault_path, Some(PathBuf::from("/tmp/vault")));
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
                    unknown: Default::default(),
                    recall_weight: None,
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: PathBuf::from("/tmp/vault"),
                        index_dir: Some(PathBuf::from("/tmp/root-index")),
                    },
                },
                // A non-root mount's indexDir is accepted but not consumed yet.
                MountConfig {
                    unknown: Default::default(),
                    recall_weight: None,
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
        assert_eq!(resolved.vault_path, Some(PathBuf::from("/tmp/vault")));
        assert_eq!(resolved.index_dir, PathBuf::from("/tmp/root-index"));

        // An explicit top-level indexDir still wins over the root mount's.
        let resolved = normalize_service_config(ServiceConfigInput {
            index_dir: Some(PathBuf::from("/tmp/top-level")),
            ..mounts_input(
                vec![MountConfig {
                    unknown: Default::default(),
                    recall_weight: None,
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
            Some(ExperimentalConfig {
                multi_vault: true,
                ..ExperimentalConfig::default()
            })
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
        assert_eq!(resolved.vault_path, Some(home.join("Vault")));
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

    /// The whole point of the derivation: no two mounts, and no mount and the
    /// root, can ever name the same index directory.
    #[test]
    fn default_mount_index_dir_cannot_collide() {
        let root = std::path::Path::new("/data/index");
        let team = default_mount_index_dir(root, "team");
        let other = default_mount_index_dir(root, "other-team");

        assert_eq!(team, root.join("mounts").join("team"));
        // Distinct ids -> distinct sibling directories...
        assert_ne!(team, other);
        // ...and neither is the root's own index directory, nor an ancestor or
        // descendant of the root's index FILE (`<root>/index.sqlite`).
        assert_ne!(team, root);
        assert!(team.starts_with(root));
        assert!(!team.starts_with(root.join("index.sqlite")));
        assert!(!root.join("index.sqlite").starts_with(&team));
    }

    /// Packaged mode is inherited rather than re-derived: the root index dir is
    /// the only place it is recorded.
    #[test]
    fn default_mount_index_dir_inherits_packaged_root() {
        let packaged_root = default_packaged_index_dir(std::path::Path::new("~/Vault"));
        let mount = default_mount_index_dir(&packaged_root, "team");
        assert!(mount.starts_with(&packaged_root));
        assert!(mount.to_string_lossy().contains(DEFAULT_CONFIG_APP_DIR));
    }

    // -----------------------------------------------------------------------
    // couchdb mounts
    // -----------------------------------------------------------------------

    fn couchdb_mount(id: &str, mount_at: &str) -> MountConfig {
        MountConfig {
            unknown: Default::default(),
            recall_weight: None,
            id: id.to_string(),
            mount_at: mount_at.to_string(),
            backend: MountBackendConfig::Couchdb {
                url: "https://couch.example".to_string(),
                database: "vault".to_string(),
                username: Some("vaultuser".to_string()),
                password_ref: SecretRef::EncryptedFile {
                    id: "livesync-password".to_string(),
                },
                e2ee: None,
                sidecar_path: None,
                index_dir: None,
                options: None,
                writable: false,
            },
        }
    }

    /// The input a working couchdb config needs: a filesystem root plus the mount,
    /// with BOTH experimental flags set.
    fn couchdb_input(
        mount: MountConfig,
        multi_vault: bool,
        couchdb_vaults: bool,
    ) -> ServiceConfigInput {
        ServiceConfigInput {
            mounts: Some(vec![
                filesystem_mount("vault", "", "/tmp/root-vault"),
                mount,
            ]),
            experimental: Some(ExperimentalConfig {
                multi_vault,
                couchdb_vaults,
                ..ExperimentalConfig::default()
            }),
            ..ServiceConfigInput::default()
        }
    }

    /// The happy path: both flags set, non-root mount, and the ROOT mount still
    /// supplies `vault_path`.
    #[test]
    fn a_gated_non_root_couchdb_mount_resolves() {
        let resolved =
            normalize_service_config(couchdb_input(couchdb_mount("live", "LiveSync"), true, true))
                .expect("a gated couchdb mount resolves");

        assert_eq!(resolved.vault_path, Some(PathBuf::from("/tmp/root-vault")));
        assert_eq!(resolved.mounts.len(), 2);
        assert_eq!(resolved.mounts[1].backend.kind_name(), "couchdb");
        assert!(resolved.experimental.couchdb_vaults);
    }

    /// Without `couchdbVaults` the mount is refused — and the COUCHDB error wins over
    /// `MultiVaultNotEnabled`, even though a couchdb mount always makes the table
    /// multi-mount and so trips both gates. The couchdb message names the flag the
    /// user has actually not set for the feature they were using.
    #[test]
    fn the_couchdb_gate_is_required_and_wins_over_the_multi_vault_gate() {
        let error = normalize_service_config(couchdb_input(
            couchdb_mount("live", "LiveSync"),
            true,
            false,
        ))
        .expect_err("an ungated couchdb mount must be refused");
        assert!(matches!(
            error,
            ConfigError::CouchdbVaultsNotEnabled { ref id } if id == "live"
        ));
        assert!(error.to_string().contains("couchdbVaults"));
        assert!(error.to_string().contains("EXPERIMENTAL"));
        // This gate is about the mount EXISTING, so it must not claim the mount can
        // only ever be read-only — `writable` is what decides that, and telling a user
        // otherwise would say the flag they are being asked to set cannot get them
        // what they want.
        assert!(
            error.to_string().contains("\"writable\": true"),
            "the gate must point at how writes are enabled: {error}"
        );

        // Neither flag set: still the couchdb error, not the multi-vault one.
        let error = normalize_service_config(couchdb_input(
            couchdb_mount("live", "LiveSync"),
            false,
            false,
        ))
        .expect_err("an ungated couchdb mount must be refused");
        assert!(matches!(error, ConfigError::CouchdbVaultsNotEnabled { .. }));

        // ...and with `couchdbVaults` alone, the multi-vault gate is what fires,
        // because the table really is multi-mount.
        let error = normalize_service_config(couchdb_input(
            couchdb_mount("live", "LiveSync"),
            false,
            true,
        ))
        .expect_err("a multi-mount table still needs multiVault");
        assert!(matches!(
            error,
            ConfigError::MultiVaultNotEnabled { count: 2 }
        ));
    }

    /// A couchdb mount CAN be the root mount, and a table consisting of nothing but
    /// one is a fully-remote vault with no filesystem anywhere in it.
    ///
    /// Two things are asserted beyond acceptance, and both are the point of the slice:
    /// `vault_path` resolves to `None` rather than to some invented placeholder, and
    /// the index dir lands on the XDG-anchored id-keyed default rather than inside a
    /// vault that does not exist. Note that only `couchdbVaults` is needed here —
    /// a single-mount table is the legacy shape spelled out longhand, so `multiVault`
    /// does not apply.
    #[test]
    fn a_couchdb_root_mount_resolves_with_no_vault_path() {
        let input = ServiceConfigInput {
            mounts: Some(vec![couchdb_mount("live", "")]),
            experimental: Some(ExperimentalConfig {
                couchdb_vaults: true,
                ..ExperimentalConfig::default()
            }),
            ..ServiceConfigInput::default()
        };
        let resolved = normalize_service_config(input).expect("a couchdb root resolves");
        assert_eq!(resolved.vault_path, None);
        assert_eq!(resolved.mounts.len(), 1);
        assert_eq!(resolved.index_dir, default_remote_root_index_dir("live"));
        // The root's location is nameable without a directory, which is what lets
        // `doctor` and the health payload keep reporting a `vaultPath` at all.
        assert_eq!(resolved.root_location(), "https://couch.example/vault");
    }

    /// An explicit top-level `indexDir` still wins over the remote-root default, and
    /// so does the root mount's own `indexDir`. Both matter: the packaged installer
    /// expresses packaged mode by writing the top-level one.
    #[test]
    fn an_explicit_index_dir_overrides_the_remote_root_default() {
        let mut mount = couchdb_mount("live", "");
        if let MountBackendConfig::Couchdb { index_dir, .. } = &mut mount.backend {
            *index_dir = Some(PathBuf::from("/var/lib/mount-index"));
        }
        let resolved = normalize_service_config(ServiceConfigInput {
            mounts: Some(vec![mount.clone()]),
            experimental: Some(ExperimentalConfig {
                couchdb_vaults: true,
                ..ExperimentalConfig::default()
            }),
            ..ServiceConfigInput::default()
        })
        .expect("a couchdb root resolves");
        assert_eq!(resolved.index_dir, PathBuf::from("/var/lib/mount-index"));

        let resolved = normalize_service_config(ServiceConfigInput {
            mounts: Some(vec![mount]),
            index_dir: Some(PathBuf::from("/var/lib/top-level")),
            experimental: Some(ExperimentalConfig {
                couchdb_vaults: true,
                ..ExperimentalConfig::default()
            }),
            ..ServiceConfigInput::default()
        })
        .expect("a couchdb root resolves");
        assert_eq!(resolved.index_dir, PathBuf::from("/var/lib/top-level"));
    }

    /// The collision `default_remote_root_index_dir`'s reserved `mounts/` segment
    /// exists to prevent, asserted on the exact shape that would collide without it.
    ///
    /// `stable_vault_hash` renders 16 lowercase hex characters and every such string is
    /// a legal mount id, so a remote root keyed directly under `indexes/` could land on
    /// a filesystem vault's packaged index directory. Nothing about that would be
    /// visible: two unrelated vaults would share one `index.sqlite`.
    #[test]
    fn a_hex_shaped_mount_id_cannot_collide_with_a_packaged_vault_index() {
        // A real hash, so the test cannot pass by picking a string no hash produces.
        let hashed = default_packaged_index_dir(Path::new("/tmp/some-vault"));
        let hash_segment = hashed
            .file_name()
            .expect("a hash segment")
            .to_string_lossy()
            .to_string();
        assert_eq!(hash_segment.len(), 16);
        assert!(hash_segment.chars().all(|c| c.is_ascii_hexdigit()));
        // ...and it is a legal mount id, which is precisely why the segment is needed.
        assert!(is_valid_mount_id(&hash_segment));

        let remote = default_remote_root_index_dir(&hash_segment);
        assert_ne!(remote, hashed);
        assert_eq!(
            remote,
            hashed
                .parent()
                .expect("indexes/")
                .join(MOUNT_INDEX_DIR_SEGMENT)
                .join(&hash_segment)
        );
        // Two remote roots with different ids stay apart, and a non-root mount's
        // default nests inside the root's rather than beside it.
        assert_ne!(
            default_remote_root_index_dir("live"),
            default_remote_root_index_dir("archive")
        );
        assert_eq!(
            default_mount_index_dir(&default_remote_root_index_dir("live"), "shared"),
            default_remote_root_index_dir("live")
                .join(MOUNT_INDEX_DIR_SEGMENT)
                .join("shared")
        );
    }

    /// A rootless table stays refused. The reason changed — it is the ROUTER's floor,
    /// not `vaultPath`'s definition — but the answer did not.
    #[test]
    fn a_rootless_mount_table_is_still_refused_now_that_the_root_may_be_remote() {
        let input = ServiceConfigInput {
            mounts: Some(vec![couchdb_mount("live", "LiveSync")]),
            experimental: Some(ExperimentalConfig {
                couchdb_vaults: true,
                ..ExperimentalConfig::default()
            }),
            ..ServiceConfigInput::default()
        };
        let error = normalize_service_config(input).expect_err("a rootless table must be refused");
        assert!(matches!(error, ConfigError::MissingRootMount));
    }

    /// Embedded `user:password@` credentials in the url are refused, because the url
    /// is printed verbatim by `doctor` and `print-config`.
    #[test]
    fn a_couchdb_url_with_userinfo_is_refused() {
        for url in [
            "https://admin:hunter2@couch.example",
            "http://admin@couch.example:5984",
        ] {
            let mut mount = couchdb_mount("live", "LiveSync");
            if let MountBackendConfig::Couchdb { url: slot, .. } = &mut mount.backend {
                *slot = url.to_string();
            }
            let error = normalize_service_config(couchdb_input(mount, true, true))
                .expect_err("userinfo must be refused");
            assert!(
                matches!(error, ConfigError::CouchdbUrlHasUserinfo { .. }),
                "{url} produced {error}"
            );
            assert!(error.to_string().contains("passwordRef"));
        }

        // An `@` in the PATH is legal and must not trip the check.
        let mut mount = couchdb_mount("live", "LiveSync");
        if let MountBackendConfig::Couchdb { url: slot, .. } = &mut mount.backend {
            *slot = "https://couch.example/prefix@v1".to_string();
        }
        assert!(normalize_service_config(couchdb_input(mount, true, true)).is_ok());
    }

    #[test]
    fn an_empty_couchdb_url_or_database_is_refused() {
        for (url, database) in [("", "vault"), ("https://couch.example", "")] {
            let mut mount = couchdb_mount("live", "LiveSync");
            if let MountBackendConfig::Couchdb {
                url: url_slot,
                database: database_slot,
                ..
            } = &mut mount.backend
            {
                *url_slot = url.to_string();
                *database_slot = database.to_string();
            }
            let error = normalize_service_config(couchdb_input(mount, true, true))
                .expect_err("an empty url/database must be refused");
            assert!(matches!(error, ConfigError::InvalidCouchdbBackend { .. }));
        }
    }

    // -----------------------------------------------------------------------
    // Algolia mounts
    // -----------------------------------------------------------------------

    fn algolia_mount(id: &str, mount_at: &str) -> MountConfig {
        MountConfig {
            unknown: Default::default(),
            recall_weight: None,
            id: id.to_string(),
            mount_at: mount_at.to_string(),
            backend: MountBackendConfig::Algolia {
                app_id: "ABC1234XYZ".to_string(),
                index_name: "team-wiki".to_string(),
                api_key_ref: SecretRef::EncryptedFile {
                    id: "algolia-api-key".to_string(),
                },
                base_url: None,
                writable: false,
                participant_id: None,
                cache: None,
                retention: None,
                index_dir: None,
            },
        }
    }

    /// The input a working algolia config needs: a filesystem root plus the mount,
    /// with `multiVault` and `algoliaVaults` set.
    fn algolia_input(
        mount: MountConfig,
        multi_vault: bool,
        algolia_vaults: bool,
    ) -> ServiceConfigInput {
        ServiceConfigInput {
            mounts: Some(vec![
                filesystem_mount("vault", "", "/tmp/root-vault"),
                mount,
            ]),
            experimental: Some(ExperimentalConfig {
                multi_vault,
                algolia_vaults,
                ..ExperimentalConfig::default()
            }),
            ..ServiceConfigInput::default()
        }
    }

    /// The happy path, and the gate.
    #[test]
    fn an_algolia_mount_needs_its_own_flag_and_then_resolves() {
        assert!(normalize_service_config(algolia_input(
            algolia_mount("shared", "_Shared"),
            true,
            true
        ))
        .is_ok());

        // Without the flag: refused, and by the ALGOLIA error rather than the
        // multi-vault one — the mount makes the table multi-mount too, so both gates
        // would otherwise fire and the less actionable one would win.
        let error = normalize_service_config(algolia_input(
            algolia_mount("shared", "_Shared"),
            true,
            false,
        ))
        .expect_err("an ungated algolia mount must be refused");
        assert!(
            matches!(error, ConfigError::AlgoliaVaultsNotEnabled { ref id } if id == "shared"),
            "{error}"
        );
        assert!(error.to_string().contains("algoliaVaults"), "{error}");
        // ...and it says the mount is read-only unless `writable` is also set, so a
        // user does not enable the flag and then wonder why writes fail.
        assert!(error.to_string().contains("\"writable\": true"), "{error}");

        // The multi-vault flag alone is not enough either, and the algolia error still
        // wins when NEITHER is set.
        let error = normalize_service_config(algolia_input(
            algolia_mount("shared", "_Shared"),
            false,
            false,
        ))
        .expect_err("neither flag set");
        assert!(
            matches!(error, ConfigError::AlgoliaVaultsNotEnabled { .. }),
            "{error}"
        );
    }

    /// An algolia mount can be the ROOT mount too, and resolves the same way a couchdb
    /// root does: no `vault_path`, an XDG-anchored index dir, a nameable location.
    ///
    /// Such a mount has no LOCAL index by design (the remote index is the corpus), so
    /// the directory this resolves is where its hydrated-note cache lives rather than a
    /// SQLite index — the derivation is deliberately the same one either way, so an
    /// operator has one rule to remember. See `default_remote_root_index_dir`.
    #[test]
    fn an_algolia_root_mount_resolves_with_no_vault_path() {
        let input = ServiceConfigInput {
            mounts: Some(vec![algolia_mount("shared", "")]),
            experimental: Some(ExperimentalConfig {
                algolia_vaults: true,
                ..ExperimentalConfig::default()
            }),
            ..ServiceConfigInput::default()
        };
        let resolved = normalize_service_config(input).expect("an algolia root resolves");
        assert_eq!(resolved.vault_path, None);
        assert_eq!(resolved.index_dir, default_remote_root_index_dir("shared"));
        assert_eq!(resolved.root_location(), "ABC1234XYZ/team-wiki");
    }

    /// A fully-remote TWO-mount table: a couchdb root with an algolia mount grafted
    /// under it. The shape the multi-backend docs now describe, and the one that proves
    /// the non-root default still nests under the root's own resolved index dir even
    /// when that dir was itself derived rather than declared.
    #[test]
    fn a_fully_remote_two_mount_table_resolves() {
        let resolved = normalize_service_config(ServiceConfigInput {
            mounts: Some(vec![
                couchdb_mount("live", ""),
                algolia_mount("shared", "_Shared"),
            ]),
            // Every field named, so no `..default()` — which clippy would flag as
            // having no effect.
            experimental: Some(ExperimentalConfig {
                multi_vault: true,
                couchdb_vaults: true,
                algolia_vaults: true,
            }),
            ..ServiceConfigInput::default()
        })
        .expect("a fully-remote table resolves");
        assert_eq!(resolved.vault_path, None);
        assert_eq!(resolved.index_dir, default_remote_root_index_dir("live"));
        assert_eq!(resolved.mounts.len(), 2);
        assert!(resolved.is_multi_mount());
    }

    #[test]
    fn an_empty_algolia_app_id_or_index_name_is_refused() {
        for (app_id, index_name) in [("", "team-wiki"), ("ABC1234XYZ", ""), ("  ", "team-wiki")] {
            let mut mount = algolia_mount("shared", "_Shared");
            if let MountBackendConfig::Algolia {
                app_id: app_slot,
                index_name: index_slot,
                ..
            } = &mut mount.backend
            {
                *app_slot = app_id.to_string();
                *index_slot = index_name.to_string();
            }
            let error = normalize_service_config(algolia_input(mount, true, true))
                .expect_err("an empty appId/indexName must be refused");
            assert!(
                matches!(error, ConfigError::InvalidAlgoliaBackend { .. }),
                "({app_id:?}, {index_name:?}) produced {error}"
            );
        }
    }

    /// Embedded `user:password@` credentials in `baseUrl` are refused, because the url
    /// is printed verbatim by `doctor` and `print-config`.
    #[test]
    fn an_algolia_base_url_with_userinfo_is_refused() {
        for base_url in [
            "https://admin:hunter2@proxy.example",
            "http://admin@proxy.example:8080",
        ] {
            let mut mount = algolia_mount("shared", "_Shared");
            if let MountBackendConfig::Algolia { base_url: slot, .. } = &mut mount.backend {
                *slot = Some(base_url.to_string());
            }
            let error = normalize_service_config(algolia_input(mount, true, true))
                .expect_err("userinfo must be refused");
            assert!(
                matches!(error, ConfigError::AlgoliaBaseUrlHasUserinfo { .. }),
                "{base_url} produced {error}"
            );
            assert!(error.to_string().contains("apiKeyRef"), "{error}");
        }

        // An `@` in the PATH is legal and must not trip the check.
        let mut mount = algolia_mount("shared", "_Shared");
        if let MountBackendConfig::Algolia { base_url: slot, .. } = &mut mount.backend {
            *slot = Some("https://proxy.example/algolia@v1".to_string());
        }
        assert!(normalize_service_config(algolia_input(mount, true, true)).is_ok());

        // A present-but-empty `baseUrl` is a mistake rather than "use the default".
        let mut mount = algolia_mount("shared", "_Shared");
        if let MountBackendConfig::Algolia { base_url: slot, .. } = &mut mount.backend {
            *slot = Some("   ".to_string());
        }
        assert!(matches!(
            normalize_service_config(algolia_input(mount, true, true))
                .expect_err("an empty baseUrl is refused"),
            ConfigError::InvalidAlgoliaBackend { .. }
        ));
    }

    /// `participantId` lands in every record AND in index filter expressions, so a
    /// value carrying a quote, a backslash or a control character is refused here —
    /// nowhere downstream can tell it apart from filter syntax.
    #[test]
    fn a_malformed_participant_id_is_refused() {
        for participant in ["", "  ", "paul\"name\"", "paul\\test", "paul\nnewline"] {
            let mut mount = algolia_mount("shared", "_Shared");
            if let MountBackendConfig::Algolia {
                participant_id: slot,
                ..
            } = &mut mount.backend
            {
                *slot = Some(participant.to_string());
            }
            let error = normalize_service_config(algolia_input(mount, true, true))
                .expect_err("a malformed participantId must be refused");
            assert!(
                matches!(error, ConfigError::InvalidAlgoliaBackend { .. }),
                "{participant:?} produced {error}"
            );
        }

        // An ordinary identifier is fine.
        let mut mount = algolia_mount("shared", "_Shared");
        if let MountBackendConfig::Algolia {
            participant_id: slot,
            ..
        } = &mut mount.backend
        {
            *slot = Some("paul@laptop".to_string());
        }
        assert!(normalize_service_config(algolia_input(mount, true, true)).is_ok());
    }

    /// `indexDir` is the ONLY path on an algolia mount, and it is home-expanded;
    /// `appId`, `indexName` and `baseUrl` are not paths and must be left alone.
    #[test]
    fn the_algolia_index_dir_is_home_expanded_and_nothing_else_is() {
        let mut mount = algolia_mount("shared", "_Shared");
        if let MountBackendConfig::Algolia {
            index_dir,
            base_url,
            ..
        } = &mut mount.backend
        {
            *index_dir = Some(PathBuf::from("~/caches/shared"));
            *base_url = Some("https://proxy.example".to_string());
        }
        let resolved =
            normalize_service_config(algolia_input(mount, true, true)).expect("resolves");
        let MountBackendConfig::Algolia {
            index_dir,
            base_url,
            app_id,
            ..
        } = &resolved.mounts[1].backend
        else {
            panic!("the algolia mount survives normalization");
        };
        let expanded = index_dir.as_ref().expect("an index dir");
        assert!(!expanded.to_string_lossy().starts_with('~'), "{expanded:?}");
        assert!(expanded.ends_with("caches/shared"), "{expanded:?}");
        assert_eq!(base_url.as_deref(), Some("https://proxy.example"));
        assert_eq!(app_id, "ABC1234XYZ");
    }

    /// `~` is expanded in `sidecarPath` and `indexDir` (both are real paths) and NOT
    /// in `url`/`database` (neither is).
    #[test]
    fn couchdb_paths_are_home_expanded_and_the_url_is_not() {
        let mut mount = couchdb_mount("live", "LiveSync");
        if let MountBackendConfig::Couchdb {
            sidecar_path,
            index_dir,
            ..
        } = &mut mount.backend
        {
            *sidecar_path = Some(PathBuf::from("~/sidecar/dist/sidecar.mjs"));
            *index_dir = Some(PathBuf::from("~/indexes/live"));
        }
        let resolved =
            normalize_service_config(couchdb_input(mount, true, true)).expect("resolves");
        let MountBackendConfig::Couchdb {
            url,
            sidecar_path,
            index_dir,
            ..
        } = &resolved.mounts[1].backend
        else {
            panic!("expected a couchdb mount");
        };
        assert_eq!(url, "https://couch.example");
        assert!(!sidecar_path
            .as_ref()
            .expect("sidecar path")
            .to_string_lossy()
            .starts_with('~'));
        assert!(!index_dir
            .as_ref()
            .expect("index dir")
            .to_string_lossy()
            .starts_with('~'));
    }

    /// A couchdb mount round-trips through `to_persisted_config` unchanged, and the
    /// persisted JSON carries only secret REFERENCES — there is no plaintext password
    /// field for `redact_config` to have to strip.
    #[test]
    fn a_couchdb_mount_round_trips_and_persists_only_secret_references() {
        let mut mount = couchdb_mount("live", "LiveSync");
        if let MountBackendConfig::Couchdb { e2ee, options, .. } = &mut mount.backend {
            *e2ee = Some(CouchdbE2eeConfig {
                passphrase_ref: SecretRef::OsKeyring {
                    service: "deep-obsidian-mcp".to_string(),
                    account: "livesync-e2ee".to_string(),
                },
                obfuscate_passphrase_ref: Some(SecretRef::EncryptedFile {
                    id: "livesync-obfuscate".to_string(),
                }),
            });
            *options = Some(CouchdbOptions {
                request_timeout_ms: Some(45_000),
                ..CouchdbOptions::default()
            });
        }
        let resolved =
            normalize_service_config(couchdb_input(mount, true, true)).expect("resolves");
        let persisted = to_persisted_config(&resolved);

        // Round trip: re-normalizing the persisted form yields the same mount table.
        let reresolved = normalize_service_config(ServiceConfigInput {
            mounts: persisted.mounts.clone(),
            experimental: persisted.experimental.clone(),
            ..ServiceConfigInput::default()
        })
        .expect("the persisted form re-resolves");
        assert_eq!(reresolved.mounts, resolved.mounts);

        let json = serde_json::to_string(&persisted).expect("serialize");
        // References are present...
        assert!(json.contains("passwordRef"), "{json}");
        assert!(json.contains("passphraseRef"), "{json}");
        assert!(json.contains("obfuscatePassphraseRef"), "{json}");
        assert!(json.contains("livesync-password"), "{json}");
        // ...and there is no plaintext secret FIELD at all, which is what makes
        // `redact_config` an identity function rather than a stripper.
        for forbidden in ["\"password\"", "\"passphrase\"", "\"obfuscatePassphrase\""] {
            assert!(!json.contains(forbidden), "{forbidden} present in {json}");
        }
        // camelCase throughout: the per-variant `rename_all` is required on a tagged
        // enum, because the container attribute renames VARIANTS, not their fields.
        assert!(
            json.contains("sidecarPath") || !json.contains("sidecar_path"),
            "{json}"
        );
        assert!(json.contains("requestTimeoutMs"), "{json}");
        assert!(!json.contains("request_timeout_ms"), "{json}");
    }

    /// A plaintext `password` in the config is rejected rather than silently ignored:
    /// `MountBackendConfig` has no such field, so serde reports an unknown one only
    /// if the variant denies them. It does not — so this asserts the honest thing
    /// instead: the field is DROPPED and never reaches the sidecar.
    #[test]
    fn a_plaintext_password_in_a_couchdb_mount_is_not_a_field() {
        let json = r#"{
            "kind": "couchdb",
            "url": "https://couch.example",
            "database": "vault",
            "username": "vaultuser",
            "passwordRef": {"kind": "encryptedFile", "id": "livesync-password"},
            "password": "hunter2"
        }"#;
        let parsed: MountBackendConfig = serde_json::from_str(json).expect("parse");
        let reserialized = serde_json::to_string(&parsed).expect("serialize");
        assert!(!reserialized.contains("hunter2"), "{reserialized}");
        assert!(reserialized.contains("passwordRef"), "{reserialized}");
    }

    /// The serde tag is `couchdb`, matching `kind_name`, and an unknown provider is
    /// an "unknown variant" error rather than a silent mis-parse.
    #[test]
    fn the_couchdb_variant_is_tagged_and_unknown_providers_are_refused() {
        let mount = couchdb_mount("live", "LiveSync");
        let json = serde_json::to_string(&mount.backend).expect("serialize");
        assert!(json.contains("\"kind\":\"couchdb\""), "{json}");
        assert_eq!(mount.backend.kind_name(), "couchdb");

        let error = serde_json::from_str::<MountBackendConfig>(
            r#"{"kind": "postgres", "url": "x", "database": "y"}"#,
        )
        .expect_err("an unknown provider must be refused");
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }

    // -----------------------------------------------------------------------
    // Unknown-field retention across a load -> save round trip
    // -----------------------------------------------------------------------

    /// The discriminating test for config upgrade safety, and it is deliberately at
    /// FILE level rather than serde level: a `PersistedServiceConfig -> JSON ->
    /// PersistedServiceConfig` round trip would pass while the real path
    /// (`read_config_file` -> normalize -> `to_persisted_config` ->
    /// `write_config_file`) still dropped everything in the middle.
    ///
    /// The failure this pins: a config written by a NEWER build, rewritten by an OLDER
    /// binary, must not silently lose the newer build's settings.
    #[test]
    fn unknown_fields_survive_a_real_load_normalize_save_round_trip() {
        for extension in ["json", "toml"] {
            let root = temp_dir(&format!("unknown-{extension}"));
            let vault = root.join("vault");
            std::fs::create_dir_all(&vault).expect("vault dir");
            let config_path = root.join(format!("config.{extension}"));

            // Hand-written as JSON regardless of the target format, then re-rendered
            // through the crate's own writer so both formats are produced by the code
            // under test rather than by a hand-built fixture.
            let seeded: PersistedServiceConfig = serde_json::from_value(serde_json::json!({
                "indexDir": root.join("index"),
                "transport": "http",
                "futureTopLevelKnob": {"nested": {"deeper": [1, 2, 3]}, "flag": true},
                // A SCALAR unknown next to the nested one, deliberately: a flattened
                // map serializes after the struct's own fields, several of which are
                // TOML tables (`http`, `autoReindex`), and a TOML serializer rejects a
                // bare value emitted after a table (`ValueAfterTable`). This is the
                // case that would make the `.toml` branch fail while the `.json` one
                // passed.
                "futureFlag": true,
                "experimental": {"multiVault": true},
                "mounts": [
                    {
                        "id": "vault",
                        "mountAt": "",
                        "backend": {"kind": "filesystem", "vaultPath": vault},
                        "futureMountKnob": "keep me"
                    },
                    {
                        "id": "team",
                        "mountAt": "Team",
                        "backend": {"kind": "filesystem", "vaultPath": vault},
                        "recallWeight": 2.0
                    }
                ]
            }))
            .expect("seed config parses");
            // Retention on the way IN: the keys reached the struct rather than being
            // dropped by the deserializer.
            assert!(
                seeded.unknown.contains_key("futureTopLevelKnob"),
                "{extension}: top-level unknown must be captured on read"
            );
            write_config_file(&config_path, &seeded).expect("seed written");

            // ...and it is still in the FILE, not just in the struct.
            let text = std::fs::read_to_string(&config_path).expect("seed text");
            assert!(
                text.contains("futureTopLevelKnob") && text.contains("futureMountKnob"),
                "{extension}: seeded file must carry both unknown keys: {text}"
            );

            // The real load path.
            let loaded = read_config_file(&config_path)
                .expect("load")
                .expect("config exists");
            let resolved = normalize_service_config(ServiceConfigInput {
                mounts: loaded.mounts.clone(),
                experimental: loaded.experimental.clone(),
                index_dir: loaded.index_dir.clone(),
                transport: loaded.transport,
                ..ServiceConfigInput::default()
            })
            .expect("normalize");

            // The real save path, exactly as a writer performs it.
            let mut persisted = to_persisted_config(&resolved);
            carry_unknown_fields(&mut persisted, Some(&loaded));
            write_config_file(&config_path, &persisted).expect("rewrite");

            // Re-read the FILE: both keys are still there, with their values intact.
            let reloaded = read_config_file(&config_path)
                .expect("reload")
                .expect("config exists");
            assert_eq!(
                reloaded.unknown.get("futureTopLevelKnob"),
                Some(&serde_json::json!({"nested": {"deeper": [1, 2, 3]}, "flag": true})),
                "{extension}: a nested top-level unknown must survive verbatim"
            );
            assert_eq!(
                reloaded.unknown.get("futureFlag"),
                Some(&serde_json::json!(true)),
                "{extension}: a SCALAR top-level unknown must survive too — this is the \
                 value-after-table case for TOML"
            );
            let mounts = reloaded.mounts.expect("mounts round-tripped");
            assert_eq!(
                mounts[0].unknown.get("futureMountKnob"),
                Some(&serde_json::json!("keep me")),
                "{extension}: a mount-level unknown must survive verbatim"
            );
            // A mount that had none must not acquire one.
            assert!(
                mounts[1].unknown.is_empty(),
                "{extension}: retention must not invent keys: {:?}",
                mounts[1].unknown
            );
            // Known fields are untouched by the retention machinery.
            assert_eq!(mounts[1].recall_weight, Some(2.0));

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// A config with no unknown keys serializes byte-identically to before retention
    /// existed: the flattened map is skipped when empty, so no `"unknown": {}` key and
    /// no reordering appears in anybody's file.
    #[test]
    fn retention_adds_nothing_to_a_config_that_has_no_unknown_keys() {
        // The label deliberately avoids the word this test greps for: it lands in the
        // temp path, which is written into the file as `vaultPath`.
        let root = temp_dir("no-extra-keys");
        let vault = root.join("vault");
        std::fs::create_dir_all(&vault).expect("vault dir");
        let config_path = root.join("config.json");

        let mut config = PersistedServiceConfig {
            vault_path: Some(vault.clone()),
            ..PersistedServiceConfig::default()
        };
        carry_unknown_fields(&mut config, None);
        write_config_file(&config_path, &config).expect("write");

        // Asserted on the parsed KEY SET rather than on the text, so a path or value
        // that happens to contain the word cannot make this pass or fail by accident.
        // (`serde_json::Map` is a `BTreeMap` here — no `preserve_order` feature — so
        // the set comes back sorted; the set is what matters, not the file's order.)
        let keys: Vec<String> = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
            &std::fs::read_to_string(&config_path).expect("read"),
        )
        .expect("valid json object")
        .keys()
        .cloned()
        .collect();
        assert_eq!(
            keys,
            vec![
                "artifactEmbedding",
                "autoReindex",
                "embedding",
                "http",
                "indexDir",
                "stdioMode",
                "transport",
                "vaultPath"
            ],
            "retention must add no key"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Pins the KNOWN LIMIT of the retention policy rather than a desirable property:
    /// a key inside a backend variant is still dropped.
    ///
    /// `MountBackendConfig` is internally tagged (`kind`) and every variant has a
    /// `#[serde(default)]` field, and serde supports neither `flatten` nor
    /// `deny_unknown_fields` on such a variant — verified, not assumed: adding
    /// `deny_unknown_fields` to the `filesystem` variant fails to compile
    /// (`missing_field` requires `Deserialize`, which the generated content path
    /// cannot satisfy). So this level can be neither retained nor failed closed
    /// without making the enum externally tagged, which is a breaking config change.
    ///
    /// The test exists so the gap is recorded where someone extending a backend's
    /// options will see it, instead of being discovered by a user whose downgraded
    /// install quietly deleted a `couchdb` setting.
    #[test]
    fn a_backend_level_unknown_key_is_still_dropped_a_documented_gap() {
        let mount: MountConfig = serde_json::from_value(serde_json::json!({
            "id": "vault",
            "mountAt": "",
            "backend": {"kind": "filesystem", "vaultPath": "/vault", "futureBackendKnob": 1}
        }))
        .expect("a backend-level unknown parses (it is dropped, not refused)");
        // Not hoisted into the mount's retained map either: the key belonged to the
        // nested object, and inventing a top-level home for it would write a config
        // that means something different from the one that was read.
        assert!(mount.unknown.is_empty());
        let text = serde_json::to_string(&mount).expect("serialize");
        assert!(
            !text.contains("futureBackendKnob"),
            "if this now round-trips, the gap closed and this test should assert that \
             instead: {text}"
        );
    }
}
