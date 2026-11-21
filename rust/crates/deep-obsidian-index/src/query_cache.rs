//! Process-local query vectors, shared across mounts and final reranking.
//!
//! Keys contain runtime configuration (including credential identity) and effective
//! instructed input. They are deliberately neither Debug nor logged nor persisted.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
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

struct CachedQuery {
    result: std::result::Result<Vec<f64>, QueryFailure>,
    completed_at: Instant,
}

struct Entry {
    value: Arc<OnceLock<CachedQuery>>,
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
        let key = QueryKey {
            configuration: serde_json::to_string(config).map_err(|_| {
                IndexError::Embedding("invalid query embedding configuration".into())
            })?,
            input: input.chars().take(config.effective_max_chars()).collect(),
            dimensions,
        };
        let now = Instant::now();
        let value = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            entries.retain(|_, entry| {
                entry.value.get().is_none_or(|cached| {
                    cached.result.is_ok() && now.duration_since(cached.completed_at) < self.ttl
                })
            });
            if let Some(entry) = entries.get_mut(&key) {
                entry.accessed_at = now;
                Arc::clone(&entry.value)
            } else {
                if entries.len() >= self.capacity {
                    // Do not evict in-flight requests: identical callers must keep
                    // joining the same computation while it is in progress.
                    let oldest = entries
                        .iter()
                        .filter(|(_, entry)| entry.value.get().is_some())
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
                let value = Arc::new(OnceLock::new());
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
                value
            }
        };
        // OnceLock joins concurrent requests for this key. No map lock survives
        // across the remote embedding call; other queries can progress independently.
        let cached = value.get_or_init(|| CachedQuery {
            result: compute()
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
                .map_err(QueryFailure::from),
            completed_at: Instant::now(),
        });
        let result = cached.result.clone().map_err(IndexError::from);
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
}
