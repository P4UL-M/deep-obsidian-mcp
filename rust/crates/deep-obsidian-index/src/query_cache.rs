//! Process-local query vectors, shared across mounts and final reranking.
//!
//! Keys contain runtime configuration (including credential identity) and effective
//! instructed input. They are deliberately neither Debug nor logged nor persisted.
use crate::embedding_scheduler::{self, CancelCheck};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::embeddings::EmbeddingConfig;
use crate::index::{normalize_dense_vector, IndexError, Result};

const QUERY_CACHE_CAPACITY: usize = 128;
const QUERY_CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(PartialEq, Eq, Hash)]
struct QueryKey {
    configuration: String,
    input: String,
    dimensions: Option<usize>,
}

#[derive(Clone)]
enum QueryFailure {
    Backend(String),
    Embedding(String),
    Dimensions { expected: usize, actual: usize },
}

impl From<IndexError> for QueryFailure {
    fn from(error: IndexError) -> Self {
        match error {
            IndexError::EmbeddingBackendUnavailable(message) => Self::Backend(message),
            IndexError::EmbeddingDimensionsMismatch { expected, actual } => {
                Self::Dimensions { expected, actual }
            }
            IndexError::Embedding(message) => Self::Embedding(message),
            error => Self::Embedding(error.to_string()),
        }
    }
}

impl From<QueryFailure> for IndexError {
    fn from(error: QueryFailure) -> Self {
        match error {
            QueryFailure::Backend(message) => Self::EmbeddingBackendUnavailable(message),
            QueryFailure::Embedding(message) => Self::Embedding(message),
            QueryFailure::Dimensions { expected, actual } => {
                Self::EmbeddingDimensionsMismatch { expected, actual }
            }
        }
    }
}

#[derive(Clone)]
struct CachedQuery {
    result: std::result::Result<Vec<f64>, QueryFailure>,
    completed_at: Instant,
}

struct Waiter {
    cancellation: Option<CancelCheck>,
}
impl Waiter {
    fn abandoned(&self) -> bool {
        self.cancellation.as_ref().is_some_and(|check| check())
    }
}
#[derive(Default)]
struct QueryValue {
    result: Mutex<Option<CachedQuery>>,
    ready: Condvar,
    waiters: Mutex<Vec<Weak<Waiter>>>,
}
impl QueryValue {
    fn is_complete(&self) -> bool {
        self.result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    fn retain(&self, now: Instant, ttl: Duration) -> bool {
        self.result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|cached| {
                cached.result.is_ok() && now.duration_since(cached.completed_at) < ttl
            })
    }
    fn all_abandoned(&self) -> bool {
        let mut waiters = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        waiters.retain(|waiter| waiter.strong_count() > 0);
        !waiters
            .iter()
            .filter_map(Weak::upgrade)
            .any(|waiter| !waiter.abandoned())
    }
    #[cfg(test)]
    fn get_mut(&mut self) -> Option<&mut CachedQuery> {
        self.result.get_mut().unwrap().as_mut()
    }
}

struct Entry {
    value: Arc<QueryValue>,
    accessed_at: Instant,
}

pub(crate) struct QueryCache {
    entries: Mutex<HashMap<QueryKey, Entry>>,
    capacity: usize,
    ttl: Duration,
}

impl QueryCache {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
            ttl,
        }
    }

    pub(crate) fn get_or_embed(
        &self,
        config: &EmbeddingConfig,
        input: &str,
        dimensions: Option<usize>,
        compute: impl FnOnce() -> Result<Vec<f64>>,
    ) -> Result<Vec<f64>> {
        // Config includes provider, endpoint, model, credential, all truncation and
        // context budgets, instruction and transport settings. Serializing it is
        // only an in-memory identity; never expose it in diagnostics.
        let cancellation = embedding_scheduler::current_cancellation();
        if cancellation.as_ref().is_some_and(|check| check()) {
            return Err(IndexError::EmbeddingBackendUnavailable(
                "query abandoned before embedding".into(),
            ));
        }
        // Admission policy changes do not alter a vector's identity.
        let mut identity = config.clone();
        identity.scheduling = None;
        let key = QueryKey {
            configuration: serde_json::to_string(&identity).map_err(|_| {
                IndexError::Embedding("invalid query embedding configuration".into())
            })?,
            input: input.chars().take(config.effective_max_chars()).collect(),
            dimensions,
        };
        let now = Instant::now();
        let (value, owner) = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            entries.retain(|_, entry| entry.value.retain(now, self.ttl));
            if let Some(entry) = entries.get_mut(&key) {
                entry.accessed_at = now;
                (Arc::clone(&entry.value), false)
            } else {
                if entries.len() >= self.capacity {
                    // Do not evict in-flight requests: identical callers must keep
                    // joining the same computation while it is in progress.
                    let oldest = entries
                        .iter()
                        .filter(|(_, entry)| entry.value.is_complete())
                        .min_by_key(|(_, entry)| entry.accessed_at)
                        .map(|(key, _)| QueryKey {
                            configuration: key.configuration.clone(),
                            input: key.input.clone(),
                            dimensions: key.dimensions,
                        });
                    if let Some(oldest) = oldest {
                        entries.remove(&oldest);
                    }
                }
                let value = Arc::new(QueryValue::default());
                if entries.len() < self.capacity {
                    entries.insert(
                        QueryKey {
                            configuration: key.configuration.clone(),
                            input: key.input.clone(),
                            dimensions: key.dimensions,
                        },
                        Entry {
                            value: Arc::clone(&value),
                            accessed_at: now,
                        },
                    );
                }
                // If every slot is in flight, compute this unrelated query without
                // retaining it rather than growing the cache or blocking all users.
                (value, true)
            }
        };
        let waiter = Arc::new(Waiter { cancellation });
        {
            let mut waiters = value.waiters.lock().unwrap_or_else(|e| e.into_inner());
            waiters.retain(|waiter| waiter.strong_count() > 0);
            waiters.push(Arc::downgrade(&waiter));
        }
        let result = if owner {
            // The originating client may disappear while another client still waits.
            // A queued HTTP request is abandoned only when ALL live callers are gone.
            let weak = Arc::downgrade(&value);
            let shared_cancel: CancelCheck =
                Arc::new(move || weak.upgrade().is_none_or(|value| value.all_abandoned()));
            // Wake followers even if an embedding implementation panics. Otherwise a
            // poisoned in-flight cache entry could retain callers indefinitely.
            let computed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                embedding_scheduler::with_cancellation(Some(shared_cancel), compute)
            }))
            .unwrap_or_else(|_| {
                Err(IndexError::Embedding(
                    "query embedding computation panicked".into(),
                ))
            })
            .and_then(|vector| {
                if let Some(expected) = dimensions {
                    if vector.len() != expected {
                        return Err(IndexError::EmbeddingDimensionsMismatch {
                            expected,
                            actual: vector.len(),
                        });
                    }
                }
                if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
                    return Err(IndexError::Embedding(
                        "invalid query embedding vector".into(),
                    ));
                }
                Ok(normalize_dense_vector(&vector))
            })
            .map_err(QueryFailure::from);
            *value.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedQuery {
                result: computed.clone(),
                completed_at: Instant::now(),
            });
            value.ready.notify_all();
            computed.map_err(IndexError::from)
        } else {
            let mut cached = value.result.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if waiter.abandoned() {
                    return Err(IndexError::EmbeddingBackendUnavailable(
                        "query abandoned while waiting for shared embedding".into(),
                    ));
                }
                if let Some(cached) = cached.as_ref() {
                    break cached.result.clone().map_err(IndexError::from);
                }
                cached = value
                    .ready
                    .wait_timeout(cached, Duration::from_millis(20))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        if result.is_err() {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            // Existing waiters retain their Arc and receive this same failure.
            // A later request retries, and must not be removed by an older waiter.
            if entries
                .get(&key)
                .is_some_and(|entry| Arc::ptr_eq(&entry.value, &value))
            {
                entries.remove(&key);
            }
        }
        result
    }
}

pub(crate) fn query_cache() -> &'static QueryCache {
    static CACHE: OnceLock<QueryCache> = OnceLock::new();
    CACHE.get_or_init(|| QueryCache::new(QUERY_CACHE_CAPACITY, QUERY_CACHE_TTL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;

    fn config() -> EmbeddingConfig {
        EmbeddingConfig {
            provider: Some(crate::embeddings::EmbeddingProvider::OpenAiCompatible),
            model: Some("test-embedding".into()),
            base_url: Some("http://test.invalid/v1".into()),
            ..EmbeddingConfig::default()
        }
        .normalize()
    }

    #[test]
    fn identical_queries_coalesce_and_unrelated_queries_progress() {
        let cache = Arc::new(QueryCache::new(4, QUERY_CACHE_TTL));
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (cache, calls, barrier) = (cache.clone(), calls.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    cache
                        .get_or_embed(&config(), "query", Some(2), || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            // A distinct key can compute while this one is in flight:
                            // calling under the cache's global lock would deadlock here.
                            cache
                                .get_or_embed(&config(), "another", Some(2), || Ok(vec![1., 0.]))?;
                            thread::sleep(Duration::from_millis(20));
                            Ok(vec![3., 4.])
                        })
                        .unwrap()
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), vec![0.6, 0.8]);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn complete_runtime_identity_isolates_vectors() {
        let cache = QueryCache::new(128, QUERY_CACHE_TTL);
        let calls = AtomicUsize::new(0);
        let original = config();
        let mut configurations = vec![original.clone(); 10];
        configurations[1].base_url = Some("http://another.invalid/v1".into());
        configurations[2].model = Some("other-model".into());
        configurations[3].api_key = Some("different-credential".into());
        configurations[4].max_chars += 1;
        configurations[5].context_tokens += 1;
        configurations[6].max_input_tokens += 1;
        configurations[7].chars_per_token += 0.1;
        configurations[8].query_instruction = Some("different instruction".into());
        configurations[9].timeout_seconds += 1;
        for config in &configurations {
            for _ in 0..2 {
                cache
                    .get_or_embed(config, "effective input", Some(2), || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(vec![1., 0.])
                    })
                    .unwrap();
            }
        }
        cache
            .get_or_embed(&original, "different input", Some(2), || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![1., 0.])
            })
            .unwrap();
        cache
            .get_or_embed(&original, "effective input", Some(3), || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![1., 0., 0.])
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 12);
    }

    #[test]
    fn failures_and_invalid_dimensions_retry_without_polluting_cache() {
        let cache = QueryCache::new(4, QUERY_CACHE_TTL);
        let config = config();
        assert!(matches!(
            cache.get_or_embed(&config, "q", Some(2), || {
                Err(IndexError::EmbeddingBackendUnavailable("offline".into()))
            }),
            Err(IndexError::EmbeddingBackendUnavailable(_))
        ));
        assert!(matches!(
            cache.get_or_embed(&config, "q", Some(2), || Ok(vec![1.])),
            Err(IndexError::EmbeddingDimensionsMismatch {
                expected: 2,
                actual: 1
            })
        ));
        assert!(cache
            .get_or_embed(&config, "q", Some(2), || Ok(vec![f64::NAN, 0.]))
            .is_err());
        assert_eq!(
            cache
                .get_or_embed(&config, "q", Some(2), || Ok(vec![1., 0.]))
                .unwrap(),
            vec![1., 0.]
        );
    }

    #[test]
    fn ttl_and_lru_bound_retention() {
        let cache = QueryCache::new(2, Duration::from_secs(60));
        let calls = AtomicUsize::new(0);
        let config = config();
        let run = |query| {
            cache
                .get_or_embed(&config, query, Some(2), || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![1., 0.])
                })
                .unwrap()
        };
        run("a");
        run("b");
        run("a");
        run("c");
        run("b");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(cache.entries.lock().unwrap().len(), 2);
        {
            let mut entries = cache.entries.lock().unwrap();
            let entry = entries
                .iter_mut()
                .find(|(key, _)| key.input == "b")
                .unwrap()
                .1;
            Arc::get_mut(&mut entry.value)
                .unwrap()
                .get_mut()
                .unwrap()
                .completed_at = Instant::now() - Duration::from_secs(61);
        }
        run("b");
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(cache.entries.lock().unwrap().len(), 2);
    }
    fn wait_for_waiters(cache: &QueryCache, count: usize) {
        let began = Instant::now();
        loop {
            let ready = cache.entries.lock().unwrap().values().any(|entry| {
                entry
                    .value
                    .waiters
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|w| w.strong_count() > 0)
                    .count()
                    == count
            });
            if ready {
                break;
            }
            assert!(began.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn cancelling_owner_keeps_shared_computation_for_live_follower() {
        use crate::embedding_scheduler::{with_cancellation, CancellationToken};
        let cache = Arc::new(QueryCache::new(4, QUERY_CACHE_TTL));
        let token = CancellationToken::default();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let (owner_cache, check) = (cache.clone(), token.check());
        let owner = thread::spawn(move || {
            with_cancellation(Some(check), || {
                owner_cache.get_or_embed(&config(), "shared", Some(2), || {
                    started_tx.send(()).unwrap();
                    continue_rx.recv().unwrap();
                    assert!(!embedding_scheduler::current_cancellation().unwrap()());
                    Ok(vec![3., 4.])
                })
            })
        });
        started_rx.recv().unwrap();
        let follower_cache = cache.clone();
        let follower = thread::spawn(move || {
            follower_cache.get_or_embed(&config(), "shared", Some(2), || panic!("must coalesce"))
        });
        wait_for_waiters(&cache, 2);
        token.cancel();
        continue_tx.send(()).unwrap();
        assert_eq!(follower.join().unwrap().unwrap(), vec![0.6, 0.8]);
        assert_eq!(owner.join().unwrap().unwrap(), vec![0.6, 0.8]);
    }

    #[test]
    fn all_callers_abandoning_cancels_pending_computation_and_allows_retry() {
        use crate::embedding_scheduler::{with_cancellation, CancellationToken};
        let cache = Arc::new(QueryCache::new(4, QUERY_CACHE_TTL));
        let token = CancellationToken::default();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let (owner_cache, check) = (cache.clone(), token.check());
        let owner = thread::spawn(move || {
            with_cancellation(Some(check), || {
                owner_cache.get_or_embed(&config(), "abandon", Some(2), || {
                    started_tx.send(()).unwrap();
                    continue_rx.recv().unwrap();
                    assert!(embedding_scheduler::current_cancellation().unwrap()());
                    Err(IndexError::Embedding("abandoned".into()))
                })
            })
        });
        started_rx.recv().unwrap();
        token.cancel();
        continue_tx.send(()).unwrap();
        assert!(owner.join().unwrap().is_err());
        cache
            .get_or_embed(&config(), "abandon", Some(2), || Ok(vec![1., 0.]))
            .unwrap();
    }

    #[test]
    fn cancelling_follower_returns_before_shared_embedding_finishes() {
        use crate::embedding_scheduler::{with_cancellation, CancellationToken};
        let cache = Arc::new(QueryCache::new(4, QUERY_CACHE_TTL));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let owner_cache = cache.clone();
        let owner = thread::spawn(move || {
            owner_cache.get_or_embed(&config(), "follower", Some(2), || {
                started_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                Ok(vec![1., 0.])
            })
        });
        started_rx.recv().unwrap();
        let token = CancellationToken::default();
        let (follower_cache, check) = (cache.clone(), token.check());
        let follower = thread::spawn(move || {
            with_cancellation(Some(check), || {
                follower_cache
                    .get_or_embed(&config(), "follower", Some(2), || panic!("must coalesce"))
            })
        });
        wait_for_waiters(&cache, 2);
        token.cancel();
        assert!(follower.join().unwrap().is_err());
        continue_tx.send(()).unwrap();
        owner.join().unwrap().unwrap();
        cache
            .get_or_embed(&config(), "follower", Some(2), || {
                panic!("must retain successful result")
            })
            .unwrap();
    }

    #[test]
    fn cache_hits_ignore_admission_policy_and_do_not_retain_dead_waiters() {
        let cache = QueryCache::new(4, QUERY_CACHE_TTL);
        let mut cfg = config();
        cfg.base_url = Some("https://cached-scheduler-test.invalid/v1".into());
        cache
            .get_or_embed(&cfg, "cached", Some(2), || Ok(vec![1., 0.]))
            .unwrap();
        cfg.scheduling = Some(deep_obsidian_types::EmbeddingSchedulingConfig {
            max_in_flight: Some(1),
            ..Default::default()
        });
        let _held =
            embedding_scheduler::acquire(&cfg, embedding_scheduler::Priority::Background).unwrap();
        for _ in 0..500 {
            cache
                .get_or_embed(&cfg, "cached", Some(2), || {
                    panic!("cache precedes scheduler")
                })
                .unwrap();
        }
        assert_eq!(cache.entries.lock().unwrap().len(), 1);
        assert_eq!(
            cache
                .entries
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .value
                .waiters
                .lock()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn panicking_owner_wakes_followers_and_does_not_poison_key() {
        let cache = Arc::new(QueryCache::new(4, QUERY_CACHE_TTL));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let owner_cache = cache.clone();
        let owner = thread::spawn(move || {
            owner_cache.get_or_embed(&config(), "panic", Some(2), || {
                started_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                panic!("broken backend implementation");
            })
        });
        started_rx.recv().unwrap();
        let follower_cache = cache.clone();
        let follower = thread::spawn(move || {
            follower_cache.get_or_embed(&config(), "panic", Some(2), || panic!("must coalesce"))
        });
        wait_for_waiters(&cache, 2);
        continue_tx.send(()).unwrap();
        assert!(owner.join().unwrap().is_err());
        assert!(follower.join().unwrap().is_err());
        cache
            .get_or_embed(&config(), "panic", Some(2), || Ok(vec![1., 0.]))
            .unwrap();
    }
}
