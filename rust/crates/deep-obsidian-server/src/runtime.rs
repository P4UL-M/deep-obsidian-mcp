use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use deep_obsidian_index::embeddings::{
    EmbeddingConfig as IndexEmbeddingConfig, EmbeddingProvider as IndexEmbeddingProvider,
    DEFAULT_EMBEDDING_BATCH_SIZE, DEFAULT_EMBEDDING_MAX_CHARS,
};
use deep_obsidian_index::index::{build_index, get_search_index, SearchIndex, SemanticBackend};
use deep_obsidian_types::ResolvedServiceConfig;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

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

fn index_embedding_config(config: &ResolvedServiceConfig) -> IndexEmbeddingConfig {
    let provider = match config.embedding.provider {
        Some(deep_obsidian_types::EmbeddingProvider::OpenAiCompatible) => {
            Some(IndexEmbeddingProvider::OpenAiCompatible)
        }
        None => None,
    };

    IndexEmbeddingConfig {
        provider,
        model: config.embedding.model.clone(),
        base_url: config.embedding.base_url.clone(),
        api_key: config.embedding.api_key.clone(),
        api_key_env: config.embedding.api_key_env.clone(),
        max_chars: DEFAULT_EMBEDDING_MAX_CHARS,
        batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
    }
    .normalize()
}

#[derive(Debug)]
pub struct RuntimeState {
    config: Arc<ResolvedServiceConfig>,
    snapshot: RwLock<RuntimeIndexSnapshot>,
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
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| match result {
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
    pub async fn bootstrap(config: ResolvedServiceConfig) -> Result<(Arc<Self>, Option<AutoReindexHandle>), String> {
        let config = Arc::new(config);
        let (initial_index, rebuilt) = {
            let config = config.clone();
            tokio::task::spawn_blocking(move || {
                let embedding_config = index_embedding_config(&config);
                get_search_index(
                    &config.vault_path,
                    Some(config.index_dir.as_path()),
                    Some(&embedding_config),
                )
                .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())??
        };

        let runtime = Arc::new(Self {
            config,
            snapshot: RwLock::new(RuntimeIndexSnapshot {
                index: Arc::new(initial_index),
                rebuilt,
                reason: "startup".to_string(),
            }),
        });

        let handle = if runtime.config.auto_reindex.enabled {
            Some(start_auto_reindex_tasks(runtime.clone()))
        } else {
            None
        };

        Ok((runtime, handle))
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
            .map(|guard| guard.clone())
    }

    pub fn index(&self) -> Result<Arc<SearchIndex>, String> {
        Ok(self.snapshot()?.index)
    }

    pub async fn rebuild(&self, reason: impl Into<String>) -> Result<RuntimeIndexSnapshot, String> {
        let reason = reason.into();
        let config = self.config.clone();
        let rebuilt = tokio::task::spawn_blocking(move || {
            let embedding_config = index_embedding_config(&config);
            build_index(
                &config.vault_path,
                Some(config.index_dir.as_path()),
                Some(&embedding_config),
            )
            .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())??;

        let snapshot = RuntimeIndexSnapshot {
            index: Arc::new(rebuilt),
            rebuilt: true,
            reason,
        };

        let mut guard = self.snapshot.write().map_err(|_| "runtime index lock poisoned".to_string())?;
        *guard = snapshot.clone();
        Ok(snapshot)
    }

    pub async fn refresh(&self, reason: impl Into<String>) -> Result<RuntimeIndexSnapshot, String> {
        let reason = reason.into();
        let config = self.config.clone();
        let (index, rebuilt) = tokio::task::spawn_blocking(move || {
            let embedding_config = index_embedding_config(&config);
            get_search_index(
                &config.vault_path,
                Some(config.index_dir.as_path()),
                Some(&embedding_config),
            )
            .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())??;

        let snapshot = RuntimeIndexSnapshot {
            index: Arc::new(index),
            rebuilt,
            reason,
        };
        let mut guard = self.snapshot.write().map_err(|_| "runtime index lock poisoned".to_string())?;
        *guard = snapshot.clone();
        Ok(snapshot)
    }
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

    AutoReindexHandle { stopped, join_handle }
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
