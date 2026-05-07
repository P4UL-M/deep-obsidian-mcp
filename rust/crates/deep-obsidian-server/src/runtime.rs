use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_index::embeddings::{
    EmbeddingConfig as IndexEmbeddingConfig, EmbeddingProvider as IndexEmbeddingProvider,
    DEFAULT_EMBEDDING_BATCH_SIZE, DEFAULT_EMBEDDING_MAX_CHARS,
};
use deep_obsidian_index::index::{
    build_index_with_artifacts, collect_artifact_snapshots, collect_snapshots,
    get_search_index_with_artifacts, same_artifact_embedding_config, same_artifact_snapshots,
    same_semantic_config, SearchIndex, SemanticBackend,
};
use deep_obsidian_types::ResolvedServiceConfig;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use secrecy::ExposeSecret;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub const DEFAULT_FRESH_SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(2);

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
        max_chars: DEFAULT_EMBEDDING_MAX_CHARS,
        batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
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
        max_chars: DEFAULT_EMBEDDING_MAX_CHARS,
        batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
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

fn should_ignore_watch_path(relative_path: Option<&str>) -> bool {
    let Some(relative_path) = relative_path else {
        return false;
    };

    let normalized = relative_path.replace('\\', "/");
    let segments = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return false;
    }

    if segments.iter().any(|segment| segment.starts_with('.')) {
        return true;
    }
    if segments.iter().any(|segment| *segment == "node_modules") {
        return true;
    }

    let basename = segments.last().copied().unwrap_or_default();
    if basename.ends_with(".md") {
        return false;
    }

    !basename.contains('.')
}

fn watch_reason(vault_path: &Path, event: &Event) -> Option<String> {
    if event.paths.is_empty() {
        return Some("watch:unknown".to_string());
    }

    for path in &event.paths {
        let relative = path
            .strip_prefix(vault_path)
            .ok()
            .map(|value| value.to_string_lossy().replace('\\', "/"));
        if should_ignore_watch_path(relative.as_deref()) {
            continue;
        }
        return Some(match relative {
            Some(value) if !value.is_empty() => format!("watch:{value}"),
            _ => "watch:unknown".to_string(),
        });
    }

    None
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

    pub async fn bootstrap(
        config: ResolvedServiceConfig,
    ) -> Result<(Arc<Self>, Option<AutoReindexHandle>), String> {
        let runtime = Self::new(config);
        runtime.refresh("startup").await?;

        let handle = if runtime.config.auto_reindex.enabled {
            Some(start_auto_reindex_tasks(runtime.clone()))
        } else {
            None
        };

        Ok((runtime, handle))
    }

    pub fn start_initial_refresh(self: &Arc<Self>) -> JoinHandle<()> {
        let runtime = self.clone();
        tokio::spawn(async move {
            match runtime.refresh("startup").await {
                Ok(snapshot) => {
                    info!(
                        "initial index {} at {}",
                        if snapshot.rebuilt {
                            "rebuilt"
                        } else {
                            "loaded"
                        },
                        snapshot.index.generated_at,
                    );
                }
                Err(error) => warn!("initial index refresh failed: {error}"),
            }
        })
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
        let mut sync_interval = tokio::time::interval(Duration::from_millis(sync_interval_ms));
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
                            info!(
                                "index {} ({}) at {}",
                                if snapshot.rebuilt { "rebuilt" } else { "checked" },
                                snapshot.reason,
                                snapshot.index.generated_at,
                            );
                        }
                        Err(error) => warn!("auto-reindex watch refresh failed: {error}"),
                    }
                }
                _ = sync_interval.tick() => {
                    if task_stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    match runtime.refresh("periodic-sync").await {
                        Ok(snapshot) => {
                            info!(
                                "index {} ({}) at {}",
                                if snapshot.rebuilt { "rebuilt" } else { "checked" },
                                snapshot.reason,
                                snapshot.index.generated_at,
                            );
                        }
                        Err(error) => warn!("auto-reindex periodic sync failed: {error}"),
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
            config_file_path: None,
        }
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
