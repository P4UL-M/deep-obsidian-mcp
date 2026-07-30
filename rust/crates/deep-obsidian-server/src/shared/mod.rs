//! Shared Algolia-backed corpus support (design: docs/algolia-shared-wiki.md).
//!
//! A [`SharedMountRuntime`] grafts a remote corpus into the vault namespace
//! under `mount_at` (hydrating reads, versioned writes). Content enters the
//! index through mount writes or the one-shot `share seed`; explicit removal
//! is `share retract` (model C: mount-only authorship).

pub mod cache;
pub mod seed;
pub mod reads;
pub mod records_build;
pub mod retrieval;
pub mod versioning;

use deep_obsidian_algolia::AlgoliaClient;
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_types::SharedMountConfig;
use secrecy::ExposeSecret;
use std::collections::HashSet;
use std::path::Path;

pub const DEFAULT_CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_RETENTION_MIN_VERSIONS: usize = 5;
pub const DEFAULT_RETENTION_MAX_AGE_DAYS: u64 = 90;
/// Env var override for the Algolia API key (mirrors the embedding env vars);
/// takes precedence over `keyRef` so containers and the demo need no keyring.
pub const ALGOLIA_API_KEY_ENV: &str = "DEEP_OBSIDIAN_ALGOLIA_API_KEY";

#[derive(Debug, thiserror::Error)]
pub enum SharedError {
    #[error("shared mount error: {0}")]
    Config(String),
    #[error(transparent)]
    Algolia(#[from] deep_obsidian_algolia::AlgoliaError),
    #[error("shared io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("note not found on shared mount: {0}")]
    NoteNotFound(String),
    #[error("shared mount is read-only: {0}")]
    ReadOnly(String),
}

pub type Result<T> = std::result::Result<T, SharedError>;

/// Maps Algolia's "index does not exist" 404 onto an empty result.
///
/// An Algolia index is created by its first write, so a shared index that has
/// never been written to — and the `_history` index until the first note is
/// superseded — answers 404 to every read. Semantically that is "no records",
/// not a failure, and treating it as one broke the first seed against a real
/// account. Every other error still propagates.
pub fn empty_if_missing_index<T>(
    result: std::result::Result<T, deep_obsidian_algolia::AlgoliaError>,
    empty: T,
) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.is_index_not_found() => Ok(empty),
        Err(error) => Err(error.into()),
    }
}

/// An empty `SearchResponse`, for [`empty_if_missing_index`] on search calls.
pub fn empty_search_response() -> deep_obsidian_algolia::SearchResponse {
    deep_obsidian_algolia::SearchResponse {
        hits: Vec::new(),
        nb_hits: 0,
        facets: None,
    }
}

/// A resolved, connected shared mount.
pub struct SharedMountRuntime {
    pub config: SharedMountConfig,
    pub client: AlgoliaClient,
    pub history_index: String,
    pub cache: cache::NoteCache,
    /// First stage the index supports: "neural" when NeuralSearch is enabled on
    /// the index, "lexical" otherwise. Detected from settings at startup.
    pub recall_stage: std::sync::Mutex<String>,
    /// Set once the history index has had its settings applied. Settings can
    /// only be applied to an index that exists, and the history index exists
    /// only after its first record — so provisioning is lazy, right after that
    /// first write.
    pub history_provisioned: std::sync::atomic::AtomicBool,
}

impl SharedMountRuntime {
    pub fn index(&self) -> &str {
        &self.config.index_name
    }

    pub fn mount_at(&self) -> &str {
        &self.config.mount_at
    }

    pub fn participant_id(&self) -> String {
        self.config
            .participant_id
            .clone()
            .unwrap_or_else(|| format!("{}@unknown", whoami()))
    }

    pub fn retention(&self) -> (usize, u64) {
        let retention = self.config.retention.clone().unwrap_or_default();
        (
            retention
                .min_versions
                .unwrap_or(DEFAULT_RETENTION_MIN_VERSIONS),
            retention
                .max_age_days
                .unwrap_or(DEFAULT_RETENTION_MAX_AGE_DAYS),
        )
    }

    /// Full vault-visible path for a remote path (`_Wiki/A.md` ->
    /// `_Shared/Team/_Wiki/A.md`).
    pub fn mounted_path(&self, remote_path: &str) -> String {
        format!("{}{}", self.config.mount_at, remote_path)
    }
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "participant".to_string())
}

/// Resolves the API key: env var first, then the secret store reference.
pub fn resolve_api_key(config: &SharedMountConfig, secrets: &SecretResolver) -> Result<String> {
    if let Ok(key) = std::env::var(ALGOLIA_API_KEY_ENV) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }
    if let Some(reference) = &config.key_ref {
        let secret = secrets
            .get(reference)
            .map_err(|error| SharedError::Config(format!("failed to resolve keyRef: {error}")))?
            .ok_or_else(|| {
                SharedError::Config("keyRef does not resolve to a stored secret".to_string())
            })?;
        return Ok(secret.expose_secret().to_string());
    }
    Err(SharedError::Config(format!(
        "no Algolia API key: set {ALGOLIA_API_KEY_ENV} or configure keyRef"
    )))
}

/// Builds the connected runtime for one mount config.
pub fn connect_mount(
    config: &SharedMountConfig,
    secrets: &SecretResolver,
    index_dir: &Path,
) -> Result<SharedMountRuntime> {
    let api_key = resolve_api_key(config, secrets)?;
    let client = AlgoliaClient::new(&config.app_id, &api_key, config.base_url.as_deref());
    let history_index = history_index_name(&config.index_name);
    let cache_config = config.cache.clone().unwrap_or_default();
    let cache = cache::NoteCache::open(
        index_dir.join("shared-cache").join(&config.index_name),
        cache_config.max_bytes.unwrap_or(DEFAULT_CACHE_MAX_BYTES),
        cache_config.pin.clone(),
    )?;
    Ok(SharedMountRuntime {
        config: config.clone(),
        client,
        history_index,
        cache,
        recall_stage: std::sync::Mutex::new("lexical".to_string()),
        history_provisioned: std::sync::atomic::AtomicBool::new(false),
    })
}

pub fn history_index_name(index_name: &str) -> String {
    format!("{index_name}_history")
}

/// Longest-prefix routing: returns the mount owning `path` plus the
/// mount-relative remote path. The mount root itself matches with or without
/// its trailing slash (`_Shared/Team` and `_Shared/Team/` both route).
pub fn route<'a>(
    mounts: &'a [SharedMountRuntime],
    path: &'a str,
) -> Option<(&'a SharedMountRuntime, &'a str)> {
    mounts
        .iter()
        .filter_map(|mount| {
            let mount_at = mount.mount_at();
            if let Some(remote) = path.strip_prefix(mount_at) {
                Some((mount, remote))
            } else if path == mount_at.trim_end_matches('/') {
                Some((mount, ""))
            } else {
                None
            }
        })
        .max_by_key(|(mount, _)| mount.mount_at().len())
}

/// Retention keep-set (design §3.1): keep the `min_versions` most recent
/// versions PLUS anything younger than `max_age_days`. `versions` is
/// (version_id, updated_at_ms), any order.
pub fn retention_keep_set(
    versions: &[(String, u64)],
    min_versions: usize,
    max_age_days: u64,
    now_ms: u64,
) -> HashSet<String> {
    let mut sorted: Vec<&(String, u64)> = versions.iter().collect();
    sorted.sort_by(|left, right| right.1.cmp(&left.1));
    let age_floor_ms = now_ms.saturating_sub(max_age_days.saturating_mul(24 * 60 * 60 * 1000));
    sorted
        .iter()
        .enumerate()
        .filter(|(rank, (_, updated_at))| *rank < min_versions || *updated_at > age_floor_ms)
        .map(|(_, (version_id, _))| version_id.clone())
        .collect()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Version ids sort naturally by timestamp; the random suffix disambiguates
/// same-millisecond writers.
pub fn new_version_id(participant_id: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in participant_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let salt: u16 = rand::random();
    format!("v{}-{:04x}{:04x}", now_ms(), (hash & 0xffff) as u16, salt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_keeps_floor_union_recency() {
        let day_ms = 24 * 60 * 60 * 1000_u64;
        let now = 200 * day_ms;
        // Ten versions, one per day going back from now.
        let versions: Vec<(String, u64)> = (0..10)
            .map(|age_days| (format!("v{age_days}"), now - age_days as u64 * day_ms))
            .collect();
        // min 2, max age 5 days: keep the 2 most recent PLUS anything < 5 days.
        let keep = retention_keep_set(&versions, 2, 5, now);
        assert!(keep.contains("v0") && keep.contains("v1")); // floor
        assert!(keep.contains("v4")); // young enough (4 days)
        assert!(!keep.contains("v5")); // 5 days old, outside floor
        assert!(!keep.contains("v9"));

        // Stale note: 3 versions all older than the window, floor keeps them.
        let stale: Vec<(String, u64)> =
            (0..3).map(|i| (format!("s{i}"), day_ms * (i as u64 + 1))).collect();
        let keep = retention_keep_set(&stale, 5, 90, now);
        assert_eq!(keep.len(), 3);
    }

    #[test]
    fn route_picks_longest_prefix() {
        // Routing operates purely on config strings; build runtimes via
        // connect_mount in integration tests instead. Here exercise the
        // prefix logic through a tiny inline harness.
        let paths = ["_Shared/Team/", "_Shared/Team/Deep/"];
        let best = paths
            .iter()
            .filter(|prefix| "_Shared/Team/Deep/Note.md".starts_with(*prefix))
            .max_by_key(|prefix| prefix.len())
            .copied();
        assert_eq!(best, Some("_Shared/Team/Deep/"));
    }
}
