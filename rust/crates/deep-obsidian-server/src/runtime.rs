use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deep_obsidian_backend::watch::watch_reason;
use deep_obsidian_config::default_mount_index_dir;
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_index::embeddings::{
    EmbeddingConfig as IndexEmbeddingConfig, EmbeddingProvider as IndexEmbeddingProvider,
    DEFAULT_CHARS_PER_TOKEN, DEFAULT_EMBEDDING_BATCH_SIZE, DEFAULT_EMBEDDING_CONTEXT_TOKENS,
    DEFAULT_EMBEDDING_MAX_CHARS, DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
};
use deep_obsidian_index::index::{
    build_index_with_artifacts, collect_artifact_snapshots, collect_snapshots,
    get_search_index_with_artifacts, same_artifact_embedding_config, same_artifact_snapshots,
    same_semantic_config, SearchIndex, SemanticBackend,
};
use deep_obsidian_types::{MountBackendConfig, MountConfig, ResolvedServiceConfig};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use secrecy::ExposeSecret;
use std::path::PathBuf;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub const DEFAULT_FRESH_SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(2);

/// How long the startup vault scan may run before the watchdog warns that it
/// may be blocked (e.g. on a macOS permission prompt). A healthy scan of even
/// a large vault finishes well under this.
const STARTUP_SCAN_WARN_AFTER: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct RuntimeIndexSnapshot {
    pub index: Arc<SearchIndex>,
    pub rebuilt: bool,
    pub reason: String,
}

impl RuntimeIndexSnapshot {
    pub fn semantic_backend(&self) -> &'static str {
        self.index.semantic_backend.as_str()
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeRefreshSummary {
    pub reason: String,
    pub rebuilt: bool,
    pub generated_at: String,
    pub finished_at_unix_ms: u128,
}

#[derive(Debug, Clone)]
pub struct RuntimeRefreshError {
    pub reason: String,
    pub message: String,
    pub finished_at_unix_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeReadiness {
    Loading,
    Ready,
    Degraded,
}

impl RuntimeReadiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
        }
    }

    /// Severity, for aggregating several mounts into one answer.
    ///
    /// `Degraded` outranks `Loading`: a mount that has already failed is a
    /// harder fact about the server than a mount that has not finished yet, and a
    /// reader who sees only the aggregate must not be told "loading" while an
    /// index is known broken.
    fn severity(self) -> u8 {
        match self {
            Self::Ready => 0,
            Self::Loading => 1,
            Self::Degraded => 2,
        }
    }

    /// The worse of two readiness states.
    fn worst(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeDiagnostics {
    pub status: RuntimeReadiness,
    pub refresh_in_flight: bool,
    pub snapshot: Option<RuntimeIndexSnapshot>,
    pub last_success: Option<RuntimeRefreshSummary>,
    pub last_error: Option<RuntimeRefreshError>,
}

#[derive(Debug, Clone)]
pub struct RuntimeFreshnessDiagnostics {
    pub snapshot_stale: bool,
    pub snapshot_age_ms: Option<u128>,
    pub stale_reason: Option<String>,
    pub last_watch_signal_unix_ms: Option<u64>,
}

fn index_embedding_config(config: &ResolvedServiceConfig) -> Result<IndexEmbeddingConfig, String> {
    let provider = match config.embedding.provider {
        Some(deep_obsidian_types::EmbeddingProvider::OpenAiCompatible) => {
            Some(IndexEmbeddingProvider::OpenAiCompatible)
        }
        None => None,
    };
    let api_key = SecretResolver::new()
        .resolve_embedding_api_key(&config.embedding)
        .map_err(|error| error.to_string())?
        .map(|secret| secret.expose_secret().to_string());

    Ok(IndexEmbeddingConfig {
        provider,
        model: config.embedding.model.clone(),
        base_url: config.embedding.base_url.clone(),
        api_key,
        max_chars: config
            .embedding
            .max_chars
            .unwrap_or(DEFAULT_EMBEDDING_MAX_CHARS),
        batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
        max_input_tokens: config
            .embedding
            .max_input_tokens
            .unwrap_or(DEFAULT_EMBEDDING_MAX_INPUT_TOKENS),
        context_tokens: config
            .embedding
            .context_tokens
            .unwrap_or(DEFAULT_EMBEDDING_CONTEXT_TOKENS),
        chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        query_instruction: config.embedding.query_instruction.clone(),
    }
    .normalize())
}

fn index_artifact_embedding_config(
    config: &ResolvedServiceConfig,
) -> Result<IndexEmbeddingConfig, String> {
    let provider = match config.artifact_embedding.provider {
        Some(deep_obsidian_types::EmbeddingProvider::OpenAiCompatible) => {
            Some(IndexEmbeddingProvider::OpenAiCompatible)
        }
        None => None,
    };
    let api_key = SecretResolver::new()
        .resolve_embedding_api_key(&config.artifact_embedding)
        .map_err(|error| error.to_string())?
        .map(|secret| secret.expose_secret().to_string());

    Ok(IndexEmbeddingConfig {
        provider,
        model: config.artifact_embedding.model.clone(),
        base_url: config.artifact_embedding.base_url.clone(),
        api_key,
        max_chars: config
            .artifact_embedding
            .max_chars
            .unwrap_or(DEFAULT_EMBEDDING_MAX_CHARS),
        batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
        max_input_tokens: config
            .artifact_embedding
            .max_input_tokens
            .unwrap_or(DEFAULT_EMBEDDING_MAX_INPUT_TOKENS),
        context_tokens: config
            .artifact_embedding
            .context_tokens
            .unwrap_or(DEFAULT_EMBEDDING_CONTEXT_TOKENS),
        chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        // Artifacts are intentionally out of scope for query-instruction wrapping.
        query_instruction: None,
    }
    .normalize())
}

#[derive(Debug)]
pub struct RuntimeState {
    config: Arc<ResolvedServiceConfig>,
    snapshot: RwLock<Option<RuntimeIndexSnapshot>>,
    refresh_lock: Mutex<()>,
    refresh_in_flight: AtomicBool,
    refresh_required: AtomicBool,
    generation: AtomicU64,
    last_watch_signal_unix_ms: AtomicU64,
    last_success: RwLock<Option<RuntimeRefreshSummary>>,
    last_error: RwLock<Option<RuntimeRefreshError>>,
    stale_reason: RwLock<Option<String>>,
}

pub struct AutoReindexHandle {
    stopped: Arc<AtomicBool>,
    join_handle: JoinHandle<()>,
}

impl Drop for AutoReindexHandle {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.join_handle.abort();
    }
}

#[derive(Debug)]
enum WatchSignal {
    Change(String),
    Error(String),
}

fn start_recursive_watcher(
    vault_path: PathBuf,
    sender: mpsc::UnboundedSender<WatchSignal>,
) -> notify::Result<RecommendedWatcher> {
    let watched_root = vault_path.clone();
    let mut watcher =
        notify::recommended_watcher(move |result: notify::Result<Event>| match result {
            Ok(event) => {
                if let Some(reason) = watch_reason(&watched_root, &event) {
                    let _ = sender.send(WatchSignal::Change(reason));
                }
            }
            Err(error) => {
                let _ = sender.send(WatchSignal::Error(error.to_string()));
            }
        })?;
    watcher.watch(&vault_path, RecursiveMode::Recursive)?;
    Ok(watcher)
}

impl RuntimeState {
    pub fn new(config: ResolvedServiceConfig) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(config),
            snapshot: RwLock::new(None),
            refresh_lock: Mutex::new(()),
            refresh_in_flight: AtomicBool::new(false),
            refresh_required: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            last_watch_signal_unix_ms: AtomicU64::new(0),
            last_success: RwLock::new(None),
            last_error: RwLock::new(None),
            stale_reason: RwLock::new(None),
        })
    }

    /// Runs the startup refresh with a watchdog: if the vault scan has not
    /// completed after `STARTUP_SCAN_WARN_AFTER`, log what is happening and the
    /// most likely cause. A macOS TCC consent dialog suspends the scan's
    /// `open()` syscall inside the kernel, so without this warning the server
    /// hangs with no output at all — nothing else can observe the stall.
    async fn startup_refresh_with_watchdog(&self) -> Result<RuntimeIndexSnapshot, String> {
        let refresh = self.refresh("startup");
        tokio::pin!(refresh);
        match tokio::time::timeout(STARTUP_SCAN_WARN_AFTER, &mut refresh).await {
            Ok(result) => result,
            Err(_) => {
                warn!(
                    "initial vault scan of {} still running after {}s — if it never completes, \
the process is likely blocked on a macOS permission prompt for the vault folder; approve the \
dialog, or grant the deep-obsidian-mcp binary Full Disk Access in System Settings → Privacy & \
Security and restart the service",
                    self.config.vault_path.display(),
                    STARTUP_SCAN_WARN_AFTER.as_secs(),
                );
                refresh.await
            }
        }
    }

    pub fn config(&self) -> &ResolvedServiceConfig {
        self.config.as_ref()
    }

    pub fn config_arc(&self) -> Arc<ResolvedServiceConfig> {
        self.config.clone()
    }

    pub fn snapshot(&self) -> Result<RuntimeIndexSnapshot, String> {
        self.snapshot
            .read()
            .map_err(|_| "runtime index lock poisoned".to_string())
            .and_then(|guard| {
                guard
                    .clone()
                    .ok_or_else(|| "runtime index is not ready".to_string())
            })
    }

    pub fn index(&self) -> Result<Arc<SearchIndex>, String> {
        Ok(self.snapshot()?.index)
    }

    pub fn diagnostics(&self) -> RuntimeDiagnostics {
        let snapshot = self.snapshot.read().ok().and_then(|guard| guard.clone());
        let last_success = self
            .last_success
            .read()
            .ok()
            .and_then(|guard| guard.clone());
        let last_error = self.last_error.read().ok().and_then(|guard| guard.clone());
        let refresh_in_flight = self.refresh_in_flight.load(Ordering::SeqCst);
        let status = if snapshot.is_some() {
            RuntimeReadiness::Ready
        } else if last_error.is_some() {
            RuntimeReadiness::Degraded
        } else {
            RuntimeReadiness::Loading
        };

        RuntimeDiagnostics {
            status,
            refresh_in_flight,
            snapshot,
            last_success,
            last_error,
        }
    }

    pub fn freshness_diagnostics(&self) -> RuntimeFreshnessDiagnostics {
        RuntimeFreshnessDiagnostics {
            snapshot_stale: self.refresh_required.load(Ordering::SeqCst),
            snapshot_age_ms: self.snapshot_age_ms(),
            stale_reason: self
                .stale_reason
                .read()
                .ok()
                .and_then(|guard| guard.clone()),
            last_watch_signal_unix_ms: match self.last_watch_signal_unix_ms.load(Ordering::SeqCst) {
                0 => None,
                value => Some(value),
            },
        }
    }

    pub fn snapshot_age_ms(&self) -> Option<u128> {
        self.last_success
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|success| success.finished_at_unix_ms))
            .map(|finished_at| unix_time_ms().saturating_sub(finished_at))
    }

    pub async fn rebuild(&self, reason: impl Into<String>) -> Result<RuntimeIndexSnapshot, String> {
        self.run_index_operation(reason.into(), true).await
    }

    pub async fn refresh(&self, reason: impl Into<String>) -> Result<RuntimeIndexSnapshot, String> {
        self.run_index_operation(reason.into(), false).await
    }

    pub async fn fresh_snapshot(
        &self,
        reason: impl Into<String>,
    ) -> Result<RuntimeIndexSnapshot, String> {
        self.snapshot_or_refresh(reason, DEFAULT_FRESH_SNAPSHOT_MAX_AGE)
            .await
    }

    pub async fn snapshot_or_refresh(
        &self,
        reason: impl Into<String>,
        max_age: Duration,
    ) -> Result<RuntimeIndexSnapshot, String> {
        let reason = reason.into();
        if !self.refresh_required.load(Ordering::SeqCst) {
            if let Some(snapshot) = self.cached_snapshot_within(max_age)? {
                return Ok(snapshot);
            }
        }

        self.refresh(reason).await
    }

    pub fn mark_stale(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.refresh_required.store(true, Ordering::SeqCst);
        self.last_watch_signal_unix_ms
            .store(unix_time_ms() as u64, Ordering::SeqCst);
        if let Ok(mut guard) = self.stale_reason.write() {
            *guard = Some(reason);
        }
    }

    fn cached_snapshot_within(
        &self,
        max_age: Duration,
    ) -> Result<Option<RuntimeIndexSnapshot>, String> {
        let Some(snapshot) = self
            .snapshot
            .read()
            .map_err(|_| "runtime index lock poisoned".to_string())?
            .clone()
        else {
            return Ok(None);
        };

        let Some(last_success) = self
            .last_success
            .read()
            .map_err(|_| "runtime index lock poisoned".to_string())?
            .clone()
        else {
            return Ok(None);
        };

        let max_age_ms = max_age.as_millis();
        let age_ms = unix_time_ms().saturating_sub(last_success.finished_at_unix_ms);
        if age_ms <= max_age_ms {
            Ok(Some(snapshot))
        } else {
            Ok(None)
        }
    }

    async fn run_index_operation(
        &self,
        reason: String,
        force_rebuild: bool,
    ) -> Result<RuntimeIndexSnapshot, String> {
        let observed_generation = self.generation.load(Ordering::SeqCst);
        let _guard = self.refresh_lock.lock().await;

        if !force_rebuild && self.generation.load(Ordering::SeqCst) != observed_generation {
            return self.snapshot();
        }

        if !force_rebuild {
            if let Some(snapshot) = self.reuse_current_snapshot_if_unchanged(&reason).await? {
                return Ok(snapshot);
            }
        }

        let config = self.config.clone();
        self.refresh_in_flight.store(true, Ordering::SeqCst);
        let operation_result = if force_rebuild {
            tokio::task::spawn_blocking(move || {
                let embedding_config = index_embedding_config(&config)?;
                let artifact_embedding_config = index_artifact_embedding_config(&config)?;
                build_index_with_artifacts(
                    &config.vault_path,
                    Some(config.index_dir.as_path()),
                    Some(&embedding_config),
                    Some(&artifact_embedding_config),
                )
                .map(|index| (index, true))
                .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result)
        } else {
            tokio::task::spawn_blocking(move || {
                let embedding_config = index_embedding_config(&config)?;
                let artifact_embedding_config = index_artifact_embedding_config(&config)?;
                get_search_index_with_artifacts(
                    &config.vault_path,
                    Some(config.index_dir.as_path()),
                    Some(&embedding_config),
                    Some(&artifact_embedding_config),
                )
                .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result)
        };
        self.refresh_in_flight.store(false, Ordering::SeqCst);

        match operation_result {
            Ok((index, rebuilt)) => {
                let snapshot = RuntimeIndexSnapshot {
                    index: Arc::new(index),
                    rebuilt,
                    reason: reason.clone(),
                };
                {
                    let mut guard = self
                        .snapshot
                        .write()
                        .map_err(|_| "runtime index lock poisoned".to_string())?;
                    *guard = Some(snapshot.clone());
                }
                {
                    let mut guard = self
                        .last_success
                        .write()
                        .map_err(|_| "runtime index lock poisoned".to_string())?;
                    *guard = Some(RuntimeRefreshSummary {
                        reason,
                        rebuilt,
                        generated_at: snapshot.index.generated_at.clone(),
                        finished_at_unix_ms: unix_time_ms(),
                    });
                }
                self.refresh_required.store(false, Ordering::SeqCst);
                if let Ok(mut guard) = self.stale_reason.write() {
                    *guard = None;
                }
                if let Ok(mut guard) = self.last_error.write() {
                    *guard = None;
                }
                self.generation.fetch_add(1, Ordering::SeqCst);
                Ok(snapshot)
            }
            Err(error) => {
                if let Ok(mut guard) = self.last_error.write() {
                    *guard = Some(RuntimeRefreshError {
                        reason,
                        message: error.clone(),
                        finished_at_unix_ms: unix_time_ms(),
                    });
                }
                Err(error)
            }
        }
    }

    async fn reuse_current_snapshot_if_unchanged(
        &self,
        reason: &str,
    ) -> Result<Option<RuntimeIndexSnapshot>, String> {
        let Some(current) = self
            .snapshot
            .read()
            .map_err(|_| "runtime index lock poisoned".to_string())?
            .clone()
        else {
            return Ok(None);
        };
        let config = self.config.clone();
        let current_index = current.index.clone();
        let unchanged = tokio::task::spawn_blocking(move || {
            let snapshots =
                collect_snapshots(&config.vault_path).map_err(|error| error.to_string())?;
            let artifact_snapshots = collect_artifact_snapshots(&config.vault_path)
                .map_err(|error| error.to_string())?;
            let embedding_config = index_embedding_config(&config)?;
            let artifact_embedding_config = index_artifact_embedding_config(&config)?;
            Ok::<_, String>(
                snapshots == current_index.file_snapshots
                    && same_artifact_snapshots(
                        &current_index.artifact_snapshots,
                        &artifact_snapshots,
                    )
                    && same_semantic_config(current_index.as_ref(), Some(&embedding_config))
                    && same_artifact_embedding_config(
                        current_index.as_ref(),
                        Some(&artifact_embedding_config),
                    ),
            )
        })
        .await
        .map_err(|error| error.to_string())??;
        if !unchanged {
            return Ok(None);
        }

        let snapshot = RuntimeIndexSnapshot {
            index: current.index,
            rebuilt: false,
            reason: reason.to_string(),
        };
        if let Ok(mut guard) = self.last_success.write() {
            *guard = Some(RuntimeRefreshSummary {
                reason: reason.to_string(),
                rebuilt: false,
                generated_at: snapshot.index.generated_at.clone(),
                finished_at_unix_ms: unix_time_ms(),
            });
        }
        self.refresh_required.store(false, Ordering::SeqCst);
        Ok(Some(snapshot))
    }
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

// ---------------------------------------------------------------------------
// One index runtime per mount
// ---------------------------------------------------------------------------

/// The vault path and index directory one mount's [`RuntimeState`] must use.
///
/// For the ROOT mount this is the resolved config verbatim — see
/// [`mount_runtime_config`].
fn filesystem_mount_paths(
    config: &ResolvedServiceConfig,
    mount: &MountConfig,
) -> (PathBuf, PathBuf) {
    match &mount.backend {
        MountBackendConfig::Filesystem {
            vault_path,
            index_dir,
        } => (
            vault_path.clone(),
            index_dir
                .clone()
                .unwrap_or_else(|| default_mount_index_dir(&config.index_dir, &mount.id)),
        ),
    }
}

/// The config a single mount's [`RuntimeState`] runs against.
///
/// [`RuntimeState`] reads exactly four things out of its config — `vault_path`,
/// `index_dir`, `embedding`/`artifact_embedding`, and `auto_reindex` — so giving
/// each mount a clone with the first two rewritten is enough to give it its own
/// index, its own watcher and its own refresh lifecycle, with no change to the
/// index crate (which stays path-based, one vault at a time).
///
/// # Why the root mount gets the config UNCHANGED
///
/// `ResolvedServiceConfig::vault_path` *is* the root mount's vault path and
/// `index_dir` already resolves the root mount's `indexDir` (see
/// `normalize_service_config`). Returning the config verbatim for the root
/// therefore is not an optimization — it is what makes "a single-mount config
/// behaves exactly as before" a structural property: the one runtime a
/// single-mount server builds is constructed from the identical config value it
/// was constructed from before this slice existed.
fn mount_runtime_config(
    config: &ResolvedServiceConfig,
    mount: &MountConfig,
) -> ResolvedServiceConfig {
    if mount.mount_at.is_empty() {
        return config.clone();
    }
    let (vault_path, index_dir) = filesystem_mount_paths(config, mount);
    ResolvedServiceConfig {
        vault_path,
        index_dir,
        ..config.clone()
    }
}

/// One mount's index runtime, with the logical prefix its index paths sit under.
#[derive(Debug)]
pub struct MountRuntime {
    /// The mount id, matching `VaultRouter`'s mount of the same name.
    pub id: String,
    /// The logical folder prefix; `""` for the root mount. Index paths are
    /// MOUNT-RELATIVE, so this is what turns them into logical vault paths.
    pub mount_at: String,
    pub runtime: Arc<RuntimeState>,
}

impl MountRuntime {
    pub fn is_root(&self) -> bool {
        self.mount_at.is_empty()
    }
}

/// One [`RuntimeState`] per mount: per-mount index, watcher and refresh lifecycle.
///
/// # Concurrency
///
/// Each mount's refresh serializes on ITS OWN `RuntimeState::refresh_lock`, so a
/// slow rebuild of one vault never blocks a read served from another's index.
/// There is deliberately no table-wide lock.
///
/// # Failure isolation
///
/// The ROOT mount is load-bearing (it holds the vault root, and every legacy
/// config is exactly one root mount), so a root index failure at startup stays
/// fatal — unchanged from before. A NON-ROOT mount that fails to index is logged
/// and left `Degraded`: the root mount keeps serving, and
/// [`MountRuntimes::aggregate_diagnostics`] reports the server as degraded so
/// `/readyz` cannot claim everything is fine.
///
/// # Where the next two slices attach
///
/// * **A backend that brings its OWN index** (one fed by a change feed rather than
///   by scanning a directory) replaces the [`RuntimeState`] in a
///   [`MountRuntime`], not this table: every consumer only ever asks a mount for a
///   snapshot and translates the mount-relative paths in it. The one thing that has
///   to generalize is [`filesystem_mount_paths`], which is the only place that
///   assumes a mount's backend config carries a vault directory — a non-filesystem
///   variant would supply its own runtime instead of a `(vault_path, index_dir)`
///   pair.
/// * **Federated recall** needs every mount's index at once, which is exactly what
///   [`MountRuntimes::entries`] hands back — the same enumeration
///   `resources/list` already uses to list every mount's notes. What is missing is
///   not access, it is comparable scores across independently built indexes.
#[derive(Debug)]
pub struct MountRuntimes {
    entries: Vec<MountRuntime>,
    /// Index of the root mount in `entries`. Always valid: a resolved config
    /// always declares exactly one root mount.
    root: usize,
}

impl MountRuntimes {
    /// Construct one runtime per mount. Pure construction: no IO, no index build.
    pub fn new(config: &ResolvedServiceConfig) -> Arc<Self> {
        let entries: Vec<MountRuntime> = config
            .mount_table()
            .into_iter()
            .map(|mount| MountRuntime {
                runtime: RuntimeState::new(mount_runtime_config(config, &mount)),
                id: mount.id,
                mount_at: mount.mount_at,
            })
            .collect();
        let root = entries
            .iter()
            .position(MountRuntime::is_root)
            // `mount_table` synthesizes the implicit root mount and
            // `normalize_service_config` rejects a table without one, so a config
            // reaching here without a root mount is a programming error.
            .expect("resolved config to declare a root mount");
        Arc::new(Self { entries, root })
    }

    /// Construct every mount's runtime and run each one's startup index refresh.
    ///
    /// Sequential on purpose: the refreshes are CPU- and IO-heavy vault scans, and
    /// running one vault at a time keeps startup cost identical to the
    /// single-mount case for the common single-mount config.
    pub async fn bootstrap(
        config: &ResolvedServiceConfig,
    ) -> Result<(Arc<Self>, Vec<AutoReindexHandle>), String> {
        let runtimes = Self::new(config);
        for entry in &runtimes.entries {
            if let Err(error) = entry.runtime.startup_refresh_with_watchdog().await {
                if entry.is_root() {
                    // Fatal, exactly as a single-mount startup failure has always been.
                    return Err(error);
                }
                warn!(
                    "mount '{}' failed its initial index refresh: {error}; the vault root keeps \
serving and readiness reports the server as degraded",
                    entry.id,
                );
            }
        }
        let handles = runtimes.start_auto_reindex();
        Ok((runtimes, handles))
    }

    /// Start the background watcher/periodic-sync task for every mount, when
    /// auto-reindex is enabled. One watcher per mount, each on its own vault.
    pub fn start_auto_reindex(&self) -> Vec<AutoReindexHandle> {
        if !self.root().config().auto_reindex.enabled {
            return Vec::new();
        }
        self.entries
            .iter()
            .map(|entry| start_auto_reindex_tasks(entry.runtime.clone()))
            .collect()
    }

    /// Refresh every mount's index in the background, logging per mount.
    pub fn start_initial_refresh(self: &Arc<Self>) -> JoinHandle<()> {
        let runtimes = self.clone();
        tokio::spawn(async move {
            for entry in &runtimes.entries {
                match entry.runtime.startup_refresh_with_watchdog().await {
                    Ok(snapshot) => {
                        info!(
                            "initial index for mount '{}' {} at {}",
                            entry.id,
                            if snapshot.rebuilt {
                                "rebuilt"
                            } else {
                                "loaded"
                            },
                            snapshot.index.generated_at,
                        );
                    }
                    Err(error) => {
                        warn!(
                            "initial index refresh failed for mount '{}': {error}",
                            entry.id
                        )
                    }
                }
            }
        })
    }

    pub fn entries(&self) -> &[MountRuntime] {
        &self.entries
    }

    /// The ROOT mount's runtime: the one every non-federated caller means.
    pub fn root(&self) -> &Arc<RuntimeState> {
        &self.entries[self.root].runtime
    }

    /// The runtime serving `mount_id`, or `None` when that mount has no index of
    /// its own.
    pub fn for_mount(&self, mount_id: &str) -> Option<&Arc<RuntimeState>> {
        self.entries
            .iter()
            .find(|entry| entry.id == mount_id)
            .map(|entry| &entry.runtime)
    }

    pub fn is_multi_mount(&self) -> bool {
        self.entries.len() > 1
    }

    /// Per-mount diagnostics, in config order.
    pub fn mount_diagnostics(&self) -> Vec<(&MountRuntime, RuntimeDiagnostics)> {
        self.entries
            .iter()
            .map(|entry| (entry, entry.runtime.diagnostics()))
            .collect()
    }

    /// The whole server's index diagnostics.
    ///
    /// A single-mount config returns the root runtime's diagnostics VERBATIM, so
    /// every health and readiness payload it produces is unchanged.
    ///
    /// For several mounts the answer is the honest conjunction: the worst status
    /// wins, a refresh anywhere counts as in flight, and the snapshot (which
    /// carries the index statistics and drives `ready`) is only offered when EVERY
    /// mount has one. `last_success`/`last_error` stay the root mount's — another
    /// mount's message must not be laundered into a field whose wording says
    /// nothing about mounts; the additive per-mount detail names it instead.
    pub fn aggregate_diagnostics(&self) -> RuntimeDiagnostics {
        let root = self.root().diagnostics();
        if !self.is_multi_mount() {
            return root;
        }
        let mut status = root.status;
        let mut refresh_in_flight = root.refresh_in_flight;
        let mut all_ready = root.snapshot.is_some();
        for entry in &self.entries {
            if entry.is_root() {
                continue;
            }
            let diagnostics = entry.runtime.diagnostics();
            status = status.worst(diagnostics.status);
            refresh_in_flight |= diagnostics.refresh_in_flight;
            all_ready &= diagnostics.snapshot.is_some();
        }
        RuntimeDiagnostics {
            status,
            refresh_in_flight,
            snapshot: if all_ready { root.snapshot } else { None },
            last_success: root.last_success,
            last_error: root.last_error,
        }
    }
}

/// Upper bound on the auto-reindex periodic interval while backing off.
const AUTO_REINDEX_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Exponential backoff for the periodic sync interval: `base * 2^(failures-1)`,
/// capped at [`AUTO_REINDEX_BACKOFF_MAX`]. Returns `base` when there are no
/// consecutive failures. Keeps a crashing backend from being hammered every tick.
fn auto_reindex_interval(base: Duration, consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return base;
    }
    let shift = consecutive_failures.saturating_sub(1).min(9);
    base.saturating_mul(1u32 << shift)
        .min(AUTO_REINDEX_BACKOFF_MAX)
        .max(base)
}

pub fn start_auto_reindex_tasks(runtime: Arc<RuntimeState>) -> AutoReindexHandle {
    let stopped = Arc::new(AtomicBool::new(false));
    let task_stopped = stopped.clone();
    let config = runtime.config.clone();
    let debounce_ms = config.auto_reindex.debounce_ms.max(100);
    let sync_interval_ms = config.auto_reindex.interval_ms.max(1000);

    let join_handle = tokio::spawn(async move {
        let (watch_tx, mut watch_rx) = mpsc::unbounded_channel();
        let mut watcher = match start_recursive_watcher(config.vault_path.clone(), watch_tx) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                warn!("watch setup failed: {error}");
                None
            }
        };
        let base_interval = Duration::from_millis(sync_interval_ms);
        let mut sync_interval = tokio::time::interval(base_interval);
        let mut consecutive_failures: u32 = 0;
        let mut pending_watch_reason: Option<String> = None;
        let mut pending_watch_at: Option<tokio::time::Instant> = None;

        loop {
            if task_stopped.load(Ordering::Relaxed) {
                break;
            }

            tokio::select! {
                Some(signal) = watch_rx.recv(), if watcher.is_some() => {
                    match signal {
                        WatchSignal::Change(reason) => {
                            runtime.mark_stale(reason.clone());
                            pending_watch_reason = Some(reason);
                            pending_watch_at = Some(tokio::time::Instant::now() + Duration::from_millis(debounce_ms));
                        }
                        WatchSignal::Error(error) => {
                            warn!("watch runtime failed: {error}; continuing with periodic sync only");
                            watcher = None;
                            pending_watch_reason = None;
                            pending_watch_at = None;
                        }
                    }
                }
                _ = async {
                    if let Some(deadline) = pending_watch_at {
                        tokio::time::sleep_until(deadline).await;
                    }
                }, if pending_watch_at.is_some() => {
                    let reason = pending_watch_reason
                        .take()
                        .unwrap_or_else(|| "watch:unknown".to_string());
                    pending_watch_at = None;
                    match runtime.refresh(reason.clone()).await {
                        Ok(snapshot) => {
                            if consecutive_failures != 0 {
                                consecutive_failures = 0;
                                sync_interval = tokio::time::interval(base_interval);
                            }
                            info!(
                                "index {} ({}) at {}",
                                if snapshot.rebuilt { "rebuilt" } else { "checked" },
                                snapshot.reason,
                                snapshot.index.generated_at,
                            );
                        }
                        Err(error) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            let delay = auto_reindex_interval(base_interval, consecutive_failures);
                            sync_interval =
                                tokio::time::interval_at(tokio::time::Instant::now() + delay, delay);
                            warn!(
                                "auto-reindex watch refresh failed (attempt {consecutive_failures}, backing off {delay:?}): {error}"
                            );
                        }
                    }
                }
                _ = sync_interval.tick() => {
                    if task_stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    match runtime.refresh("periodic-sync").await {
                        Ok(snapshot) => {
                            if consecutive_failures != 0 {
                                consecutive_failures = 0;
                                sync_interval = tokio::time::interval(base_interval);
                            }
                            info!(
                                "index {} ({}) at {}",
                                if snapshot.rebuilt { "rebuilt" } else { "checked" },
                                snapshot.reason,
                                snapshot.index.generated_at,
                            );
                        }
                        Err(error) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            let delay = auto_reindex_interval(base_interval, consecutive_failures);
                            sync_interval =
                                tokio::time::interval_at(tokio::time::Instant::now() + delay, delay);
                            warn!(
                                "auto-reindex periodic sync failed (attempt {consecutive_failures}, backing off {delay:?}): {error}"
                            );
                        }
                    }
                }
            }
        }
    });

    AutoReindexHandle {
        stopped,
        join_handle,
    }
}

pub fn storage_backend_name() -> &'static str {
    "sqlite"
}

pub fn vector_search_backend_name(index: &SearchIndex) -> &'static str {
    match index.semantic_backend {
        SemanticBackend::Sparse => "sparse-terms",
        SemanticBackend::Embedding => "sqlite-vec",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use deep_obsidian_types::{
        AutoReindexConfig, EmbeddingConfig, HttpConfig, StdioMode, TransportMode,
    };

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "deep_obsidian_runtime_{name}_{}_{}",
            std::process::id(),
            unix_time_ms()
        ))
    }

    fn test_config(vault_path: PathBuf, index_dir: PathBuf) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            vault_path,
            index_dir,
            mounts: Vec::new(),
            experimental: Default::default(),
            transport: TransportMode::Http,
            stdio_mode: StdioMode::Newline,
            http: HttpConfig {
                host: "127.0.0.1".to_string(),
                port: 4100,
                mcp_path: "/mcp".to_string(),
                health_path: "/healthz".to_string(),
            },
            auto_reindex: AutoReindexConfig {
                enabled: false,
                debounce_ms: 250,
                interval_ms: 30_000,
            },
            embedding: EmbeddingConfig::default(),
            artifact_embedding: EmbeddingConfig::default(),
            auth: deep_obsidian_types::AuthConfig::default(),
            config_file_path: None,
        }
    }

    /// The two-mount table the derivation tests share.
    fn two_mount_config(root_vault: PathBuf, team_vault: PathBuf) -> ResolvedServiceConfig {
        let index_dir = temp_path("two_mount_index");
        ResolvedServiceConfig {
            mounts: vec![
                MountConfig {
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: root_vault.clone(),
                        index_dir: None,
                    },
                },
                MountConfig {
                    id: "team".to_string(),
                    mount_at: "Team".to_string(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: team_vault,
                        index_dir: None,
                    },
                },
            ],
            experimental: deep_obsidian_types::ExperimentalConfig { multi_vault: true },
            ..test_config(root_vault, index_dir)
        }
    }

    /// The load-bearing identity: the ROOT mount's runtime is built from the
    /// resolved config VERBATIM, which is what makes a single-mount server behave
    /// exactly as it did before there was a mount table.
    #[test]
    fn the_root_mounts_runtime_config_is_the_resolved_config_verbatim() {
        let config = two_mount_config(temp_path("root_vault"), temp_path("team_vault"));
        let root = &config.mount_table()[0];
        assert_eq!(mount_runtime_config(&config, root), config);

        // ...and a legacy `vaultPath`-only config, whose root mount is implicit.
        let legacy = test_config(temp_path("legacy_vault"), temp_path("legacy_index"));
        let implicit_root = &legacy.mount_table()[0];
        assert_eq!(mount_runtime_config(&legacy, implicit_root), legacy);
    }

    /// A non-root mount indexes ITS OWN vault into a directory that cannot collide
    /// with the root's, and inherits everything else (embedding config, cadence).
    #[test]
    fn a_non_root_mounts_runtime_config_retargets_only_the_two_paths() {
        let team_vault = temp_path("team_vault");
        let config = two_mount_config(temp_path("root_vault"), team_vault.clone());
        let team = &config.mount_table()[1];
        let derived = mount_runtime_config(&config, team);

        assert_eq!(derived.vault_path, team_vault);
        assert_eq!(
            derived.index_dir,
            config.index_dir.join("mounts").join("team")
        );
        assert_ne!(derived.index_dir, config.index_dir);
        // Everything the index build depends on besides the paths is inherited, so a
        // mount cannot silently embed with different settings than its siblings.
        assert_eq!(derived.embedding, config.embedding);
        assert_eq!(derived.artifact_embedding, config.artifact_embedding);
        assert_eq!(derived.auto_reindex, config.auto_reindex);
    }

    /// An explicit per-mount `indexDir` wins over the derived default.
    #[test]
    fn an_explicit_mount_index_dir_is_honoured() {
        let chosen = temp_path("chosen_index");
        let mut config = two_mount_config(temp_path("root_vault"), temp_path("team_vault"));
        config.mounts[1].backend = MountBackendConfig::Filesystem {
            vault_path: temp_path("team_vault"),
            index_dir: Some(chosen.clone()),
        };
        let team = &config.mount_table()[1];
        assert_eq!(mount_runtime_config(&config, team).index_dir, chosen);
    }

    /// Each mount gets its OWN runtime, so its refresh serializes on its own lock
    /// rather than behind a single global one.
    #[test]
    fn every_mount_gets_a_distinct_runtime() {
        let config = two_mount_config(temp_path("root_vault"), temp_path("team_vault"));
        let runtimes = MountRuntimes::new(&config);

        assert!(runtimes.is_multi_mount());
        assert_eq!(runtimes.entries().len(), 2);
        assert!(Arc::ptr_eq(
            runtimes.root(),
            runtimes.for_mount("vault").unwrap()
        ));
        let team = runtimes.for_mount("team").expect("team runtime");
        assert!(!Arc::ptr_eq(runtimes.root(), team));
        assert!(runtimes.for_mount("absent").is_none());
    }

    #[test]
    fn readiness_aggregates_to_the_worst_mount() {
        assert_eq!(
            RuntimeReadiness::Ready.worst(RuntimeReadiness::Loading),
            RuntimeReadiness::Loading
        );
        // A known failure outranks "not finished yet": a reader of the aggregate must
        // not be told "loading" while an index is known broken.
        assert_eq!(
            RuntimeReadiness::Loading.worst(RuntimeReadiness::Degraded),
            RuntimeReadiness::Degraded
        );
        assert_eq!(
            RuntimeReadiness::Degraded.worst(RuntimeReadiness::Ready),
            RuntimeReadiness::Degraded
        );
        assert_eq!(
            RuntimeReadiness::Ready.worst(RuntimeReadiness::Ready),
            RuntimeReadiness::Ready
        );
    }

    /// A single-mount config's aggregate is the root runtime's diagnostics
    /// verbatim, so every health and readiness payload it produces is unchanged.
    #[test]
    fn a_single_mount_aggregate_is_the_root_diagnostics_verbatim() {
        let config = test_config(temp_path("solo_vault"), temp_path("solo_index"));
        let runtimes = MountRuntimes::new(&config);
        assert!(!runtimes.is_multi_mount());

        let aggregate = runtimes.aggregate_diagnostics();
        let root = runtimes.root().diagnostics();
        assert_eq!(aggregate.status, root.status);
        assert_eq!(aggregate.status, RuntimeReadiness::Loading);
        assert!(aggregate.snapshot.is_none());
    }

    /// A failing NON-ROOT mount is degradation, not a fatal startup error: the
    /// bootstrap succeeds, the root mount is ready, and the aggregate is degraded.
    #[tokio::test]
    async fn a_failing_non_root_mount_degrades_without_failing_the_bootstrap() {
        let root_vault = temp_path("isolation_root");
        fs::create_dir_all(&root_vault).expect("root vault");
        fs::write(root_vault.join("Note.md"), "# Note\n\nbody").expect("note");
        // Never created, so the team mount's index refresh fails.
        let team_vault = temp_path("isolation_team_missing");
        let config = two_mount_config(root_vault.clone(), team_vault);

        let (runtimes, _handles) = MountRuntimes::bootstrap(&config)
            .await
            .expect("a non-root mount failure must not fail the bootstrap");

        assert_eq!(
            runtimes.root().diagnostics().status,
            RuntimeReadiness::Ready
        );
        assert_eq!(
            runtimes.for_mount("team").unwrap().diagnostics().status,
            RuntimeReadiness::Degraded
        );
        let aggregate = runtimes.aggregate_diagnostics();
        assert_eq!(aggregate.status, RuntimeReadiness::Degraded);
        // `ready` is driven by the snapshot, which is withheld until EVERY mount has
        // one -- so `/readyz` cannot report 200 while a mount is broken.
        assert!(aggregate.snapshot.is_none());

        let _ = fs::remove_dir_all(&root_vault);
        let _ = fs::remove_dir_all(&config.index_dir);
    }

    /// A failing ROOT mount stays fatal, exactly as a single-mount startup failure
    /// has always been.
    #[tokio::test]
    async fn a_failing_root_mount_still_fails_the_bootstrap() {
        let root = temp_path("root_failure");
        let vault_path = root.join("vault");
        let index_dir = root.join("index-file");
        fs::create_dir_all(&vault_path).expect("vault");
        // A FILE where the index directory must be: the index build cannot proceed.
        fs::write(&index_dir, "not a directory").expect("index file");

        let config = test_config(vault_path, index_dir);
        assert!(MountRuntimes::bootstrap(&config).await.is_err());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn auto_reindex_interval_backs_off_and_caps() {
        let base = Duration::from_secs(30);
        // No failures -> base cadence.
        assert_eq!(auto_reindex_interval(base, 0), base);
        // Exponential growth: base * 2^(n-1).
        assert_eq!(auto_reindex_interval(base, 1), base);
        assert_eq!(auto_reindex_interval(base, 2), Duration::from_secs(60));
        assert_eq!(auto_reindex_interval(base, 3), Duration::from_secs(120));
        // Caps at AUTO_REINDEX_BACKOFF_MAX and never drops below base.
        let big = auto_reindex_interval(base, 30);
        assert_eq!(big, AUTO_REINDEX_BACKOFF_MAX);
        assert!(big >= base);
    }

    #[test]
    fn new_runtime_reports_loading_until_index_exists() {
        let runtime = RuntimeState::new(test_config(temp_path("vault"), temp_path("index")));
        let diagnostics = runtime.diagnostics();

        assert_eq!(diagnostics.status, RuntimeReadiness::Loading);
        assert!(diagnostics.snapshot.is_none());
        assert!(diagnostics.last_success.is_none());
        assert!(diagnostics.last_error.is_none());
    }

    #[tokio::test]
    async fn failed_refresh_records_degraded_diagnostics_without_snapshot() {
        let root = temp_path("failed_refresh");
        let vault_path = root.join("vault");
        let index_dir = root.join("index-file");
        fs::create_dir_all(&vault_path).expect("test vault directory");
        fs::write(&index_dir, "not a directory").expect("test index file");

        let runtime = RuntimeState::new(test_config(vault_path, index_dir));
        let error = runtime
            .refresh("test-refresh")
            .await
            .expect_err("refresh should fail");
        assert!(!error.is_empty());

        let diagnostics = runtime.diagnostics();
        assert_eq!(diagnostics.status, RuntimeReadiness::Degraded);
        assert!(diagnostics.snapshot.is_none());
        assert_eq!(
            diagnostics
                .last_error
                .as_ref()
                .map(|error| error.reason.as_str()),
            Some("test-refresh")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn snapshot_or_refresh_reuses_recent_clean_snapshot() {
        let root = temp_path("reuse_recent");
        let vault_path = root.join("vault");
        let index_dir = root.join("index");
        fs::create_dir_all(&vault_path).expect("test vault directory");
        fs::write(vault_path.join("note.md"), "# Note\n\nhello world").expect("test note");

        let runtime = RuntimeState::new(test_config(vault_path, index_dir));
        let initial = runtime.refresh("initial").await.expect("initial refresh");
        let reused = runtime
            .snapshot_or_refresh("tool-read", Duration::from_secs(60))
            .await
            .expect("cached snapshot");

        assert!(Arc::ptr_eq(&initial.index, &reused.index));
        assert_eq!(reused.reason, "initial");
        assert_eq!(
            runtime
                .diagnostics()
                .last_success
                .as_ref()
                .map(|success| success.reason.as_str()),
            Some("initial")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_signal_forces_snapshot_or_refresh_even_with_fresh_snapshot() {
        let root = temp_path("stale_forces_refresh");
        let vault_path = root.join("vault");
        let index_dir = root.join("index");
        fs::create_dir_all(&vault_path).expect("test vault directory");
        fs::write(vault_path.join("note.md"), "# Note\n\nhello world").expect("test note");

        let runtime = RuntimeState::new(test_config(vault_path.clone(), index_dir));
        runtime.refresh("initial").await.expect("initial refresh");
        runtime.mark_stale("watch:note.md");

        let stale_diagnostics = runtime.freshness_diagnostics();
        assert!(stale_diagnostics.snapshot_stale);
        assert_eq!(
            stale_diagnostics.stale_reason.as_deref(),
            Some("watch:note.md")
        );
        assert!(stale_diagnostics.last_watch_signal_unix_ms.is_some());

        fs::write(vault_path.join("note.md"), "# Note\n\nhello refreshed").expect("update note");
        let refreshed = runtime
            .snapshot_or_refresh("tool-read", Duration::from_secs(60))
            .await
            .expect("refresh after stale signal");

        assert_eq!(refreshed.reason, "tool-read");
        let diagnostics = runtime.freshness_diagnostics();
        assert!(!diagnostics.snapshot_stale);
        assert!(diagnostics.stale_reason.is_none());
        assert_eq!(
            runtime
                .diagnostics()
                .last_success
                .as_ref()
                .map(|success| success.reason.as_str()),
            Some("tool-read")
        );

        let _ = fs::remove_dir_all(root);
    }
}
