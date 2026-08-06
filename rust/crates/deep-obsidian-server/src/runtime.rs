use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deep_obsidian_backend::watch::{watch_reason, ChangeEvent};
use deep_obsidian_backend::VaultBackend;
use deep_obsidian_config::default_mount_index_dir;
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_index::embeddings::{
    EmbeddingConfig as IndexEmbeddingConfig, EmbeddingProvider as IndexEmbeddingProvider,
    DEFAULT_CHARS_PER_TOKEN, DEFAULT_EMBEDDING_BATCH_SIZE, DEFAULT_EMBEDDING_CONTEXT_TOKENS,
    DEFAULT_EMBEDDING_MAX_CHARS, DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
};
use deep_obsidian_index::index::{
    build_index_from_source, get_search_index_from_source, same_artifact_embedding_config,
    same_artifact_snapshots, same_semantic_config, SearchIndex, SemanticBackend,
};
use deep_obsidian_index::source::{FilesystemSource, NoteSource};
use deep_obsidian_index::sqlite::index_file_path;
use deep_obsidian_types::{MountBackendConfig, MountConfig, ResolvedServiceConfig};

use crate::mounts::MountBackends;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use secrecy::ExposeSecret;
use std::path::{Path, PathBuf};
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

/// The vault one [`RuntimeState`] indexes, and where its SQLite file lives.
///
/// Introduced so a mount whose notes are not on disk can be indexed. The
/// filesystem case is constructed from `(vault_path, index_dir)` exactly as before
/// — `FilesystemSource::new(vault_path)` paired with
/// `sqlite::index_file_path(vault_path, Some(index_dir))` is *literally* what
/// `build_index_with_artifacts` did internally, so the filesystem index build is
/// byte-for-byte unchanged rather than merely intended to be.
///
/// # Why this is a FACTORY and not a source
///
/// A remote source caches its manifest, because one refresh asks for it up to four
/// times and each ask is a network conversation (see [`crate::couchdb_source`]). That
/// cache must be scoped to ONE refresh. Holding a single long-lived source instance
/// here would scope it to the process instead, and then the second refresh would read
/// the first refresh's manifest, compare it against the index built from that same
/// manifest, conclude "unchanged", and clear the stale flag — so a couchdb mount
/// would serve its startup snapshot forever and no change feed could ever move it.
///
/// So [`RuntimeState`] mints one source per refresh and threads it through both the
/// reuse check and the build. That gets both properties at once: one consistent
/// manifest *within* a refresh, and a fresh one *between* refreshes. A
/// [`FilesystemSource`] is stateless, so for a filesystem mount the factory is
/// nothing but a constructor call and behaviour is unchanged.
#[derive(Clone)]
pub struct IndexTarget {
    source_factory: Arc<dyn Fn() -> Arc<dyn NoteSource> + Send + Sync>,
    index_file: PathBuf,
    /// Only for `Debug`; the sources themselves are minted on demand.
    describes: String,
}

impl std::fmt::Debug for IndexTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexTarget")
            .field("index_file", &self.index_file)
            .field("source", &self.describes)
            .finish()
    }
}

impl IndexTarget {
    /// A local vault directory indexed into `index_dir`.
    pub fn filesystem(vault_path: &Path, index_dir: &Path) -> Self {
        let vault_path = vault_path.to_path_buf();
        Self {
            index_file: index_file_path(&vault_path, Some(index_dir)),
            describes: format!("filesystem({})", vault_path.display()),
            source_factory: Arc::new(move || Arc::new(FilesystemSource::new(vault_path.clone()))),
        }
    }

    /// Any other source, minted afresh for each refresh by `factory`.
    pub fn from_factory(
        describes: impl Into<String>,
        index_dir: &Path,
        factory: impl Fn() -> Arc<dyn NoteSource> + Send + Sync + 'static,
    ) -> Self {
        Self {
            // `index_file_path` falls back to a vault-relative default only when
            // `index_dir` is `None`; passing `Some` makes the first argument unused,
            // so a source with no local directory is fine here.
            index_file: index_file_path(Path::new(""), Some(index_dir)),
            describes: describes.into(),
            source_factory: Arc::new(factory),
        }
    }

    /// A source for exactly one refresh.
    fn source(&self) -> Arc<dyn NoteSource> {
        (self.source_factory)()
    }

    pub fn index_file(&self) -> &Path {
        &self.index_file
    }
}

pub struct RuntimeState {
    config: Arc<ResolvedServiceConfig>,
    /// What this runtime indexes. For the ROOT mount of any config this is
    /// `IndexTarget::filesystem(&config.vault_path, &config.index_dir)`, i.e. the
    /// pre-existing behaviour spelled out.
    target: IndexTarget,
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

/// Owns a change-feed pump task, aborting it on drop.
///
/// The mirror of what the `notify` watcher does for a filesystem mount: dropping the
/// handle stops delivery. Without this, a dropped `AutoReindexHandle` would leave a
/// task forwarding events into a channel nobody reads.
struct WatchPump(JoinHandle<()>);

impl Drop for WatchPump {
    fn drop(&mut self) {
        self.0.abort();
    }
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

impl std::fmt::Debug for RuntimeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeState")
            .field("target", &self.target)
            .field("refresh_in_flight", &self.refresh_in_flight)
            .finish_non_exhaustive()
    }
}

impl RuntimeState {
    /// A runtime indexing the config's own vault directory.
    ///
    /// Behaviour unchanged for a config with a local root: the target it derives is
    /// exactly what the path-based index entry points constructed internally before this
    /// slice. Panics on a config whose root is REMOTE, and that is the right shape rather
    /// than an oversight — this constructor's whole contract is "index the directory this
    /// config names", and a remote-rooted config names none. The serve path never reaches
    /// it: [`MountRuntimes::new`] builds every runtime from its mount's own
    /// [`IndexTarget`] via [`RuntimeState::with_target`], which is how a couchdb mount
    /// gets a `CouchDbSource` instead of a directory scan.
    pub fn new(config: ResolvedServiceConfig) -> Arc<Self> {
        let vault_path = config
            .vault_path
            .clone()
            .expect("RuntimeState::new to be given a config with a local vault root");
        let target = IndexTarget::filesystem(&vault_path, &config.index_dir);
        Self::with_target(config, target)
    }

    /// A runtime indexing an arbitrary [`NoteSource`].
    pub fn with_target(config: ResolvedServiceConfig, target: IndexTarget) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(config),
            target,
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
            // The macOS-TCC wording is emitted ONLY for a mount that actually reads a
            // local directory. A remote-backed mount cannot be blocked on a permission
            // prompt for a folder it never opens, so pointing an operator at System
            // Settings would send them to the wrong place — the stall is a slow or
            // unresponsive remote. Both branches say what is still running and for how
            // long; only the diagnosis differs.
            Err(_) => {
                match self.config.vault_path.as_deref() {
                    Some(vault_path) => warn!(
                        "initial vault scan of {} still running after {}s — if it never completes, \
the process is likely blocked on a macOS permission prompt for the vault folder; approve the \
dialog, or grant the deep-obsidian-mcp binary Full Disk Access in System Settings → Privacy & \
Security and restart the service",
                        vault_path.display(),
                        STARTUP_SCAN_WARN_AFTER.as_secs(),
                    ),
                    None => warn!(
                        "initial index build of remote vault {} still running after {}s — if it \
never completes, the remote is likely slow or unresponsive rather than the local machine being \
busy; check the mount's url and the service log for the backend's own diagnostics",
                        self.target.describes,
                        STARTUP_SCAN_WARN_AFTER.as_secs(),
                    ),
                }
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

        // ONE source for this whole refresh: the reuse check and the build below both
        // read through it, so they cannot disagree about what the vault contains, and
        // the next refresh gets a fresh one. See `IndexTarget`.
        let source = self.target.source();

        if !force_rebuild {
            if let Some(snapshot) = self
                .reuse_current_snapshot_if_unchanged(&reason, source.clone())
                .await?
            {
                return Ok(snapshot);
            }
        }

        let config = self.config.clone();
        let index_file = self.target.index_file.clone();
        self.refresh_in_flight.store(true, Ordering::SeqCst);
        let operation_result = if force_rebuild {
            let source = source.clone();
            tokio::task::spawn_blocking(move || {
                let embedding_config = index_embedding_config(&config)?;
                let artifact_embedding_config = index_artifact_embedding_config(&config)?;
                build_index_from_source(
                    source.as_ref(),
                    &index_file,
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
            let source = source.clone();
            tokio::task::spawn_blocking(move || {
                let embedding_config = index_embedding_config(&config)?;
                let artifact_embedding_config = index_artifact_embedding_config(&config)?;
                get_search_index_from_source(
                    source.as_ref(),
                    &index_file,
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

    /// Takes the refresh's source rather than minting one, so it reads the SAME
    /// manifest the build below will.
    async fn reuse_current_snapshot_if_unchanged(
        &self,
        reason: &str,
        source: Arc<dyn NoteSource>,
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
            // Through the source, not the path collectors: a remote source pins ONE
            // manifest for the refresh that owns it (see the couchdb source), so
            // these two calls plus the two inside `get_search_index_from_source`
            // become one remote walk instead of four.
            let snapshots = source.note_snapshots().map_err(|error| error.to_string())?;
            let artifact_snapshots = source
                .artifact_snapshots()
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

/// The vault path and index directory one mount's [`RuntimeState`] config must
/// carry.
///
/// For the ROOT mount this is the resolved config verbatim — see
/// [`mount_runtime_config`].
///
/// Note that this no longer decides where the index is BUILT: that is
/// [`IndexTarget`], derived from the mount's backend by
/// [`crate::mounts::MountBackendEntry::index_target`]. What survives here is the
/// config a runtime reports about itself (`vault_path` for diagnostics and the
/// startup watchdog message, `index_dir` for the health payload), which for a
/// backend with no local directory is the mount's index directory and NO vault path.
///
/// The absent vault path is an [`Option`] rather than an empty `PathBuf` because the
/// difference is observable: an empty path is normalized against the process working
/// directory by `ensure_vault_path`, so anything that treated it as a directory would
/// silently address the server's CWD. `None` cannot be mistaken for a directory by
/// construction.
fn mount_runtime_paths(
    config: &ResolvedServiceConfig,
    mount: &MountConfig,
) -> (Option<PathBuf>, PathBuf) {
    let declared_index_dir = match &mount.backend {
        MountBackendConfig::Filesystem { index_dir, .. }
        | MountBackendConfig::Couchdb { index_dir, .. }
        | MountBackendConfig::Algolia { index_dir, .. } => index_dir.clone(),
    };
    (
        // `None` for couchdb (whose `IndexTarget` carries a `CouchDbSource`, not a
        // `FilesystemSource`) and for algolia (which gets no `RuntimeState` at all — see
        // [`mount_has_local_index`] — so this arm is belt and braces for a future caller
        // that builds one anyway, answered honestly rather than with a panic).
        mount.backend.local_vault_path().map(Path::to_path_buf),
        declared_index_dir.unwrap_or_else(|| default_mount_index_dir(&config.index_dir, &mount.id)),
    )
}

/// Whether a mount failing at startup must take the whole service down.
///
/// True for a mount that is BOTH the vault root AND backed by a local directory;
/// false for everything else. The one predicate both startup gates consult — this
/// module's index bootstrap and the HTTP transport's backend reachability gate — so the
/// two cannot drift.
///
/// # Why the asymmetry is the right shape and not an inconsistency
///
/// The two failures are different KINDS of failure, and fail-fast is right for exactly
/// one of them:
///
/// * A filesystem root that cannot be read is a **permanent configuration error**. The
///   directory is missing, or misspelled, or unreadable; nothing about waiting improves
///   it, and a server that came up serving errors for the entire vault would hide the
///   mistake behind a green process. Failing at startup is what puts the message in
///   front of the operator who just changed the config. This is unchanged behaviour, and
///   it is unchanged for stdio too, where a client shows a startup failure but has
///   nowhere to show a readiness probe.
/// * A remote root that cannot be reached is a **transient outage**. The url is right,
///   the credentials are right, and the remote is down or slow or mid-rebuild. Making
///   that fatal would mean a network blip at the wrong moment bricks a service that a
///   supervisor then restart-loops, and — worse — it would take down the mounts that ARE
///   healthy along with it. So the server starts DEGRADED: readiness answers 503 naming
///   the mount, health says so honestly, vault paths on the failed mount refuse with the
///   backend's own reason, and every other mount serves normally. A couchdb mount then
///   re-handshakes in the background until the remote returns, with no process restart —
///   see [`deep_obsidian_backend::sidecar::SidecarSupervisor`]'s readiness-recovery loop,
///   which is what makes the degraded start recoverable rather than merely survivable.
///   An algolia mount needs no such loop: it probes the remote on every call, so its
///   readiness is never cached.
///
/// The rule is therefore about the FAILURE MODE, not about being the root: "a permanent
/// local misconfiguration fails fast; an unreachable remote degrades".
///
/// # The honest caveat: not every remote-root failure is transient
///
/// A remote root also degrades when its failure is NOT an outage — a `passwordRef` that
/// resolves to nothing, a sidecar bundle that is not where it should be. Those are
/// permanent configuration errors, so by the argument above they would seem to deserve
/// fail-fast. They still degrade, for three reasons and deliberately:
///
/// * **The failure cannot be classified from here.** This predicate sees a mount config;
///   the gates see a `BackendError` from `health_overview`. "Secret missing" and "remote
///   down" arrive as the same shape, and guessing wrong in the fatal direction is the
///   expensive way to be wrong.
/// * **Degrading says strictly more.** The server comes up, `/readyz` answers 503 naming
///   the mount, and every path on it refuses with the backend's own message — which
///   states which mount, which backend and which secret. A refusal to start prints one
///   line to a log the operator may not be watching and leaves nothing to interrogate.
/// * **It is what a non-root remote mount has always done.** A missing secret on a
///   couchdb mount at `LiveSync` has degraded that mount since the mount existed. Making
///   the same misconfiguration fatal purely because the mount sits at `""` would be a
///   rule about position, which is exactly what this function is not.
pub fn root_failure_is_fatal(mount: &MountConfig) -> bool {
    mount.mount_at.is_empty() && mount.backend.local_vault_path().is_some()
}

/// True when a mount is served by a LOCAL search index of its own.
///
/// # Why an algolia mount is `false`, and why that is not a degradation
///
/// A filesystem or couchdb mount is indexed locally: the server scans or walks its
/// content into a SQLite index and every recall tool is answered from it. An
/// Algolia-backed mount is the opposite arrangement — the remote index IS the corpus,
/// several participants share it, and materializing a local copy would (a) duplicate
/// the whole corpus on every participant's disk and (b) serve one participant's stale
/// snapshot to a tool that looks authoritative.
///
/// So such a mount gets NO [`RuntimeState`], which is deliberate and load-bearing.
/// The alternative — registering a runtime whose source fails — was rejected: a failed
/// refresh is what marks a mount `Degraded`, and
/// [`MountRuntimes::aggregate_diagnostics`] would then report the whole server
/// degraded forever for a mount that is working exactly as designed. `/readyz` would
/// be permanently red and the signal would be worthless.
///
/// What consumers see instead: [`MountRuntimes::for_mount`] answers `None`, and the
/// tool layer's `mount_index` turns that into a refusal that says the mount has no
/// local index. That refusal is the DESIGNED path here, not an error path.
pub fn mount_has_local_index(mount: &MountConfig) -> bool {
    !matches!(mount.backend, MountBackendConfig::Algolia { .. })
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
/// `ResolvedServiceConfig::vault_path` *is* the root mount's vault path (or `None`
/// when the root is a remote backend, which is exactly what the root's runtime should
/// then report about itself) and `index_dir` already resolves the root mount's
/// `indexDir` (see `normalize_service_config`). Returning the config verbatim for the
/// root therefore is not an optimization — it is what makes "a single-mount config
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
    let (vault_path, index_dir) = mount_runtime_paths(config, mount);
    ResolvedServiceConfig {
        federated_rerank: true,
        vault_path,
        index_dir,
        ..config.clone()
    }
}

/// How a mount learns that its content changed.
///
/// The two arms are the reason `notify` did not simply generalize: a filesystem
/// mount's watcher is constructed FROM A PATH and owned by the watching task, while
/// a couchdb mount's feed is owned by the backend (whose supervisor re-arms it
/// across sidecar restarts) and merely subscribed to. Modelling that as one thing
/// would have meant either giving `notify` a fake path or giving the backend a fake
/// watcher.
pub enum ChangeSource {
    /// Watch a local directory with `notify`, exactly as before.
    LocalDirectory(PathBuf),
    /// Subscribe to a backend's change stream.
    Backend(Arc<dyn VaultBackend>),
}

impl std::fmt::Debug for ChangeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeSource::LocalDirectory(path) => {
                f.debug_tuple("LocalDirectory").field(path).finish()
            }
            ChangeSource::Backend(_) => f.write_str("Backend(..)"),
        }
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
    /// What drives this mount's staleness signal.
    pub change_source: ChangeSource,
    /// Whether this mount failing its startup index build must abort the service.
    /// Computed once, by [`root_failure_is_fatal`], which is where the reasoning lives.
    fatal_on_startup_failure: bool,
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
/// A LOCAL root mount is load-bearing and its failure is a permanent configuration
/// error, so a filesystem root's index failure at startup stays fatal — unchanged from
/// before, and unchanged for every legacy config, which is exactly one filesystem root
/// mount. Everything else is logged and left `Degraded`: a non-root mount of any kind,
/// and now also a REMOTE root, whose failure is an outage rather than a mistake. The
/// server then starts and [`MountRuntimes::aggregate_diagnostics`] reports it degraded so
/// `/readyz` cannot claim everything is fine. See [`root_failure_is_fatal`] for the full
/// argument.
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
/// # Not every mount is in here
///
/// A mount whose backend serves its own recall has NO local index and therefore no
/// entry — see [`mount_has_local_index`]. So `entries()` is a subset of the router's
/// mount table, and anything that must enumerate ALL mounts (capabilities,
/// `vault_info.mounts[]`, the conflict report) reads the ROUTER instead.
#[derive(Debug)]
pub struct MountRuntimes {
    entries: Vec<MountRuntime>,
    /// Index of the root mount in `entries`, when the root mount has a runtime here.
    ///
    /// `None` for exactly one shape: an ALGOLIA root mount, which by design has no
    /// local index and therefore no entry (see [`mount_has_local_index`]). A
    /// single-mount algolia-rooted config makes `entries` empty outright. Every other
    /// root — filesystem or couchdb — is present.
    root: Option<usize>,
    /// The resolved config, kept so table-wide questions (is auto-reindex on?) can be
    /// answered without a root entry to read them off.
    config: Arc<ResolvedServiceConfig>,
}

impl MountRuntimes {
    /// Construct one runtime per mount. Pure construction: no IO, no index build.
    ///
    /// Takes the already-built [`MountBackends`] rather than deriving everything from
    /// the config, because a couchdb mount's index must read through THE SAME sidecar
    /// supervisor its router backend uses — see [`crate::mounts`].
    ///
    /// Must be called from inside a tokio runtime: a couchdb mount's source captures
    /// the current runtime handle for its sync→async bridge.
    pub fn new(config: &ResolvedServiceConfig, backends: &MountBackends) -> Arc<Self> {
        let handle = tokio::runtime::Handle::current();
        let entries: Vec<MountRuntime> = backends
            .entries()
            .iter()
            // A mount with no local index of its own is not represented here at all.
            // See `mount_has_local_index` for why that is right rather than a gap.
            .filter(|entry| mount_has_local_index(&entry.mount))
            .map(|entry| {
                let mount = entry.mount.clone();
                let change_source = match mount_runtime_paths(config, &mount).0 {
                    Some(vault_path) => ChangeSource::LocalDirectory(vault_path),
                    None => ChangeSource::Backend(entry.backend.clone()),
                };
                MountRuntime {
                    runtime: RuntimeState::with_target(
                        mount_runtime_config(config, &mount),
                        entry.index_target(&handle),
                    ),
                    fatal_on_startup_failure: root_failure_is_fatal(&mount),
                    id: mount.id,
                    mount_at: mount.mount_at,
                    change_source,
                }
            })
            .collect();
        // `None` only when the root mount is an algolia one, which has no local index by
        // design. Not an error, and deliberately not an `expect`: the config is valid and
        // the vault serves — what does not exist is a local index for the root, and every
        // caller that needs one refuses with that exact reason.
        let root = entries.iter().position(MountRuntime::is_root);
        Arc::new(Self {
            entries,
            root,
            config: Arc::new(config.clone()),
        })
    }

    /// Construct every mount's runtime and run each one's startup index refresh.
    ///
    /// Sequential on purpose: the refreshes are CPU- and IO-heavy vault scans, and
    /// running one vault at a time keeps startup cost identical to the
    /// single-mount case for the common single-mount config.
    pub async fn bootstrap(
        config: &ResolvedServiceConfig,
        backends: &MountBackends,
    ) -> Result<(Arc<Self>, Vec<AutoReindexHandle>), String> {
        let runtimes = Self::new(config, backends);
        for entry in &runtimes.entries {
            if let Err(error) = entry.runtime.startup_refresh_with_watchdog().await {
                if entry.fatal_on_startup_failure {
                    // A LOCAL root that cannot be indexed is fatal, exactly as a
                    // single-mount startup failure has always been. See
                    // [`root_failure_is_fatal`] for why the asymmetry with a remote root
                    // is the right shape rather than an inconsistency.
                    return Err(error);
                }
                warn!(
                    "mount '{}' failed its initial index refresh: {error}; the server starts \
DEGRADED and readiness reports it as such{}",
                    entry.id,
                    if entry.is_root() {
                        " — vault paths on the root will refuse until the remote comes back, \
which the mount retries by itself"
                    } else {
                        "; the vault root keeps serving"
                    },
                );
            }
        }
        let handles = runtimes.start_auto_reindex();
        Ok((runtimes, handles))
    }

    /// Start the background watcher/periodic-sync task for every mount, when
    /// auto-reindex is enabled. One watcher per mount, each on its own vault.
    pub fn start_auto_reindex(&self) -> Vec<AutoReindexHandle> {
        if !self.config().auto_reindex.enabled {
            return Vec::new();
        }
        self.entries
            .iter()
            .map(|entry| start_auto_reindex_tasks(entry.runtime.clone(), &entry.change_source))
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
    ///
    /// `None` when the root mount has no local index of its own — an algolia root. See
    /// the [`MountRuntimes::root`] field and [`crate::mcp::AppState::runtime`].
    pub fn root(&self) -> Option<&Arc<RuntimeState>> {
        self.root.map(|index| &self.entries[index].runtime)
    }

    /// The resolved config this table was built from.
    ///
    /// Read instead of the root runtime's own config for table-wide questions, because
    /// there may be no root runtime — and because a table-wide question was never the
    /// root's to answer in the first place.
    pub fn config(&self) -> &ResolvedServiceConfig {
        self.config.as_ref()
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
    ///
    /// # When there is no root runtime
    ///
    /// An ALGOLIA root mount has no local index, so there is no root runtime whose
    /// diagnostics could be the base. The status is then the worst over the mounts that
    /// DO have an index — `Ready` when there are none — and never `Degraded` merely
    /// because the root has no index: a mount that by design never has one is working
    /// exactly as designed, and reporting `/readyz` permanently red for it is the precise
    /// failure [`mount_has_local_index`] documents.
    ///
    /// The `snapshot` is `None` in that case, so the payload's `ready` is `false` and it
    /// carries no index statistics. That is deliberate and it is the only honest answer:
    /// those numbers describe the ROOT's index, and there is no root index to describe.
    /// Substituting another mount's counts would report one mount's corpus as the vault's.
    /// What a caller reads instead is the additive `mounts[]` detail, which states each
    /// mount's own index state including "has none".
    ///
    /// Note that `ready: false` and `status: "ready"` therefore coexist on an
    /// algolia-rooted vault, and the HTTP code is **200**:
    /// [`crate::health::readiness_status_code`] keys on `status`, never on `ready`. The
    /// two fields have always answered different questions — `status` is "can this server
    /// serve?", `ready` is "does the root index have a usable snapshot?" — and on every
    /// index-backed configuration they happen to agree, which is why the difference has
    /// not mattered before. `ready` is not redefined to mean `status == Ready`, tempting
    /// though it is: on a multi-mount vault whose root is fine and whose second mount is
    /// degraded, that would flip `ready` from `true` to `false` for configurations that
    /// exist today.
    pub fn aggregate_diagnostics(&self) -> RuntimeDiagnostics {
        let Some(root) = self.root() else {
            return RuntimeDiagnostics {
                status: self
                    .entries
                    .iter()
                    .fold(RuntimeReadiness::Ready, |worst, entry| {
                        worst.worst(entry.runtime.diagnostics().status)
                    }),
                refresh_in_flight: self
                    .entries
                    .iter()
                    .any(|entry| entry.runtime.diagnostics().refresh_in_flight),
                snapshot: None,
                last_success: None,
                last_error: None,
            };
        };
        let root = root.diagnostics();
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

/// Start one mount's watch + periodic-sync task.
///
/// # What replaced `notify` for a couchdb mount
///
/// The loop below is UNCHANGED: it still receives `WatchSignal`s over one channel,
/// debounces them, and falls back to the periodic sync. Only the producer differs.
/// A filesystem mount still gets `start_recursive_watcher`; a couchdb mount gets a
/// pump task that forwards its backend's [`ChangeStream`] onto the same channel and
/// translates [`ChangeEvent`] into `WatchSignal` one-for-one (which is exactly what
/// `ChangeEvent` was built to mirror). So the debounce, the backoff, and the
/// periodic-sync fallback are shared by construction rather than reimplemented, and
/// a mount whose change feed never arms still reindexes on the periodic tick.
pub fn start_auto_reindex_tasks(
    runtime: Arc<RuntimeState>,
    change_source: &ChangeSource,
) -> AutoReindexHandle {
    let stopped = Arc::new(AtomicBool::new(false));
    let task_stopped = stopped.clone();
    let config = runtime.config.clone();
    let debounce_ms = config.auto_reindex.debounce_ms.max(100);
    let sync_interval_ms = config.auto_reindex.interval_ms.max(1000);

    // Subscribing happens on THIS thread, before the task is spawned, so the
    // subscription cannot be missed between spawn and first poll.
    enum Producer {
        Directory(PathBuf),
        Backend(Arc<dyn VaultBackend>),
    }
    let producer = match change_source {
        ChangeSource::LocalDirectory(path) => Producer::Directory(path.clone()),
        ChangeSource::Backend(backend) => Producer::Backend(backend.clone()),
    };

    let join_handle = tokio::spawn(async move {
        let (watch_tx, mut watch_rx) = mpsc::unbounded_channel();
        // Holds whatever keeps the subscription alive: the `notify` watcher, or the
        // pump task's handle. Dropping it stops delivery, exactly as before.
        let mut watcher: Option<Box<dyn std::any::Any + Send>> = match producer {
            Producer::Directory(path) => match start_recursive_watcher(path, watch_tx) {
                Ok(watcher) => Some(Box::new(watcher)),
                Err(error) => {
                    warn!("watch setup failed: {error}");
                    None
                }
            },
            Producer::Backend(backend) => {
                let mut stream = backend.changes(None);
                Some(Box::new(WatchPump(tokio::spawn(async move {
                    while let Some(event) = stream.recv().await {
                        let signal = match event {
                            ChangeEvent::Change(reason) => WatchSignal::Change(reason),
                            ChangeEvent::Error(error) => WatchSignal::Error(error),
                        };
                        if watch_tx.send(signal).is_err() {
                            break;
                        }
                    }
                }))))
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
            federated_rerank: true,
            vault_path: Some(vault_path),
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
            federated_rerank: true,
            mounts: vec![
                MountConfig {
                    unknown: Default::default(),
                    recall_weight: None,
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: root_vault.clone(),
                        index_dir: None,
                    },
                },
                MountConfig {
                    unknown: Default::default(),
                    recall_weight: None,
                    id: "team".to_string(),
                    mount_at: "Team".to_string(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: team_vault,
                        index_dir: None,
                    },
                },
            ],
            experimental: deep_obsidian_types::ExperimentalConfig {
                multi_vault: true,
                ..Default::default()
            },
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

        assert_eq!(derived.vault_path, Some(team_vault));
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
    ///
    /// `#[tokio::test]` because `MountRuntimes::new` captures the current runtime
    /// handle for a source that needs to bridge sync→async.
    #[tokio::test]
    async fn every_mount_gets_a_distinct_runtime() {
        let config = two_mount_config(temp_path("root_vault"), temp_path("team_vault"));
        let backends = MountBackends::build(&config);
        let runtimes = MountRuntimes::new(&config, &backends);

        assert!(runtimes.is_multi_mount());
        assert_eq!(runtimes.entries().len(), 2);
        assert!(Arc::ptr_eq(
            runtimes.root().expect("a filesystem root has a runtime"),
            runtimes.for_mount("vault").unwrap()
        ));
        let team = runtimes.for_mount("team").expect("team runtime");
        assert!(!Arc::ptr_eq(
            runtimes.root().expect("a filesystem root has a runtime"),
            team
        ));
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
    #[tokio::test]
    async fn a_single_mount_aggregate_is_the_root_diagnostics_verbatim() {
        let config = test_config(temp_path("solo_vault"), temp_path("solo_index"));
        let backends = MountBackends::build(&config);
        let runtimes = MountRuntimes::new(&config, &backends);
        assert!(!runtimes.is_multi_mount());

        let aggregate = runtimes.aggregate_diagnostics();
        let root = runtimes
            .root()
            .expect("a filesystem root has a runtime")
            .diagnostics();
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

        let backends = MountBackends::build(&config);
        let (runtimes, _handles) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("a non-root mount failure must not fail the bootstrap");

        assert_eq!(
            runtimes
                .root()
                .expect("a filesystem root has a runtime")
                .diagnostics()
                .status,
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

    /// The fatality rule, on every combination of position and backend kind.
    ///
    /// Asserted as a table because the rule is easy to state and easy to get subtly
    /// wrong in either direction: making a remote root fatal would let a network blip
    /// brick a fully-remote vault, and making a LOCAL root non-fatal would let a typo'd
    /// `vaultPath` come up green and serve errors for the whole vault. It is also the
    /// single predicate BOTH startup gates consult — this module's index bootstrap and
    /// the HTTP transport's backend reachability gate — so a drift here would silently
    /// desynchronize them.
    #[test]
    fn only_a_local_root_mount_is_fatal_on_startup_failure() {
        let mount = |mount_at: &str, backend: MountBackendConfig| MountConfig {
            unknown: Default::default(),
            recall_weight: None,
            id: "m".to_string(),
            mount_at: mount_at.to_string(),
            backend,
        };
        let filesystem = || MountBackendConfig::Filesystem {
            vault_path: PathBuf::from("/tmp/vault"),
            index_dir: None,
        };
        let couchdb = || MountBackendConfig::Couchdb {
            url: "https://couch.example".to_string(),
            database: "vault".to_string(),
            username: None,
            password_ref: deep_obsidian_types::SecretRef::EncryptedFile {
                id: "pw".to_string(),
            },
            e2ee: None,
            sidecar_path: None,
            index_dir: None,
            options: None,
            writable: false,
        };
        let algolia = || MountBackendConfig::Algolia {
            app_id: "APP".to_string(),
            index_name: "wiki".to_string(),
            api_key_ref: deep_obsidian_types::SecretRef::EncryptedFile {
                id: "key".to_string(),
            },
            base_url: None,
            writable: false,
            participant_id: None,
            cache: None,
            retention: None,
            index_dir: None,
        };

        // The ONE fatal shape: a local directory at the vault root.
        assert!(root_failure_is_fatal(&mount("", filesystem())));
        // A remote root degrades — its failure is an outage, not a mistake.
        assert!(!root_failure_is_fatal(&mount("", couchdb())));
        assert!(!root_failure_is_fatal(&mount("", algolia())));
        // No non-root mount is ever fatal, whatever it is backed by.
        assert!(!root_failure_is_fatal(&mount("Team", filesystem())));
        assert!(!root_failure_is_fatal(&mount("LiveSync", couchdb())));
        assert!(!root_failure_is_fatal(&mount("_Shared", algolia())));
    }

    /// A failing LOCAL ROOT mount stays fatal, exactly as a single-mount startup failure
    /// has always been — asserted through `bootstrap` rather than through the predicate,
    /// so the wiring is covered and not just the rule.
    #[tokio::test]
    async fn a_failing_root_mount_still_fails_the_bootstrap() {
        let root = temp_path("root_failure");
        let vault_path = root.join("vault");
        let index_dir = root.join("index-file");
        fs::create_dir_all(&vault_path).expect("vault");
        // A FILE where the index directory must be: the index build cannot proceed.
        fs::write(&index_dir, "not a directory").expect("index file");

        let config = test_config(vault_path, index_dir);
        let backends = MountBackends::build(&config);
        assert!(MountRuntimes::bootstrap(&config, &backends).await.is_err());

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

    // -----------------------------------------------------------------------
    // The per-refresh source contract
    // -----------------------------------------------------------------------

    /// A source whose manifest CHANGES on every `note_snapshots()` call.
    ///
    /// Stands in for a remote source that caches its manifest for the lifetime of one
    /// source value: each `note_snapshots()` here reports a different note body size,
    /// so an index built from call N cannot compare equal to call N+1.
    #[derive(Debug, Default)]
    struct DriftingSource {
        /// This instance's fixed manifest generation, pinned at construction — exactly
        /// the shape `CouchDbSource` has. The FACTORY owns the construction counter;
        /// the source only needs to know which generation it is.
        generation: u64,
    }

    impl deep_obsidian_index::source::NoteSource for DriftingSource {
        fn ensure_ready(&self) -> deep_obsidian_index::index::Result<()> {
            Ok(())
        }

        fn note_snapshots(
            &self,
        ) -> deep_obsidian_index::index::Result<Vec<deep_obsidian_index::index::FileSnapshot>>
        {
            Ok(vec![deep_obsidian_index::index::FileSnapshot {
                path: "Note.md".to_string(),
                // Both move per generation, so neither `same_snapshots` nor the
                // incremental refresh can mistake one for another.
                mtime_ms: 1_700_000_000_000 + self.generation,
                size: 32 + self.generation,
            }])
        }

        fn artifact_snapshots(
            &self,
        ) -> deep_obsidian_index::index::Result<Vec<deep_obsidian_index::index::ArtifactSnapshot>>
        {
            Ok(Vec::new())
        }

        fn ensure_path(&self, _path: &str) -> deep_obsidian_index::index::Result<()> {
            Ok(())
        }

        fn read_note(&self, _path: &str) -> deep_obsidian_index::index::Result<String> {
            Ok(format!("# Note\n\ngeneration {}\n", self.generation))
        }

        fn read_artifact(
            &self,
            _path: &str,
            _max_bytes: u64,
        ) -> deep_obsidian_index::index::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    /// The regression guard for the manifest pin.
    ///
    /// A source that caches its manifest for its own lifetime is correct ONLY if a
    /// fresh one is minted per refresh. If `IndexTarget` held a single instance
    /// instead, the second refresh would read the first refresh's manifest, compare it
    /// against the index built from that same manifest, conclude "unchanged", and
    /// clear the stale flag — so a couchdb mount would serve its startup snapshot
    /// forever and no change feed could move it. This test fails in that world.
    #[tokio::test]
    async fn each_refresh_gets_a_freshly_minted_source() {
        let root = temp_path("per_refresh_source");
        let index_dir = root.join("index");
        std::fs::create_dir_all(&index_dir).expect("index dir");

        let constructions = Arc::new(AtomicU64::new(0));
        let factory_constructions = constructions.clone();
        let target = IndexTarget::from_factory("drifting", &index_dir, move || {
            let generation = factory_constructions.fetch_add(1, Ordering::SeqCst);
            Arc::new(DriftingSource { generation })
        });
        let runtime = RuntimeState::with_target(
            test_config(root.join("unused-vault"), index_dir.clone()),
            target,
        );

        let first = runtime.refresh("first").await.expect("first refresh");
        let second = runtime.refresh("second").await.expect("second refresh");

        // A source was minted per refresh...
        assert!(
            constructions.load(Ordering::SeqCst) >= 2,
            "expected a source per refresh, got {}",
            constructions.load(Ordering::SeqCst)
        );
        // ...and the second refresh actually saw the moved manifest rather than
        // reusing the first snapshot.
        assert_ne!(
            first.index.file_snapshots, second.index.file_snapshots,
            "the second refresh reused a pinned manifest: the index cannot ever update"
        );
        assert_eq!(second.reason, "second");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Within ONE refresh, the reuse check and the build must read the SAME source, or
    /// they could disagree about what the vault contains and persist an index whose
    /// snapshots do not match its content.
    #[tokio::test]
    async fn one_refresh_uses_a_single_source_for_the_reuse_check_and_the_build() {
        let root = temp_path("single_source_per_refresh");
        let index_dir = root.join("index");
        std::fs::create_dir_all(&index_dir).expect("index dir");

        let constructions = Arc::new(AtomicU64::new(0));
        let factory_constructions = constructions.clone();
        let target = IndexTarget::from_factory("drifting", &index_dir, move || {
            let generation = factory_constructions.fetch_add(1, Ordering::SeqCst);
            Arc::new(DriftingSource { generation })
        });
        let runtime = RuntimeState::with_target(
            test_config(root.join("unused-vault"), index_dir.clone()),
            target,
        );

        runtime.refresh("only").await.expect("refresh");
        // Exactly one source for the whole refresh: the reuse check (which short
        // circuits on the first refresh, having no snapshot) and the build share it.
        assert_eq!(
            constructions.load(Ordering::SeqCst),
            1,
            "a refresh must mint exactly one source"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
