//! Best-effort context discovery with a bounded, expiring cache. The OpenAI-compatible
//! embeddings API does not advertise a standard context field; Ollama exposes it
//! through metadata endpoints. Discovery never loads a model or changes its options.
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::embeddings::EmbeddingConfig;

pub const FALLBACK_CONTEXT_TOKENS: usize = 4096;
const UNLOADED_CONTEXT_CEILING: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextResolution {
    pub tokens: usize,
    pub source: &'static str,
}

const CONTEXT_CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const FALLBACK_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_CACHE_ENTRIES: usize = 64;

#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    base_url: String,
    model: String,
    // Partition authenticated backends without storing a credential in the cache.
    credential_scope: u64,
}

impl CacheKey {
    fn from_config(config: &EmbeddingConfig) -> Option<Self> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        config.api_key.hash(&mut hash);
        Some(Self {
            base_url: config.base_url()?.trim_end_matches('/').to_string(),
            model: config.model.clone()?,
            credential_scope: hash.finish(),
        })
    }
}

struct CachedContext {
    resolution: ContextResolution,
    expires_at: Instant,
}

#[derive(Default)]
struct ContextCache {
    entries: HashMap<CacheKey, CachedContext>,
}

impl ContextCache {
    fn get(&mut self, key: &CacheKey, now: Instant) -> Option<ContextResolution> {
        // Retire expired models even when the current model changes permanently.
        self.entries.retain(|_, entry| entry.expires_at > now);
        self.entries.get(key).map(|entry| entry.resolution.clone())
    }

    fn insert(&mut self, key: CacheKey, resolution: ContextResolution, now: Instant) {
        self.entries.retain(|_, entry| entry.expires_at > now);
        if self.entries.len() >= MAX_CACHE_ENTRIES {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        let ttl = if resolution.source == "fallback" {
            FALLBACK_CACHE_TTL
        } else {
            CONTEXT_CACHE_TTL
        };
        self.entries.insert(
            key,
            CachedContext {
                resolution,
                expires_at: now + ttl,
            },
        );
    }
}

fn positive(value: &Value) -> Option<usize> {
    usize::try_from(value.as_u64()?)
        .ok()
        .filter(|value| *value > 0)
}

fn parameter_context(show: &Value) -> Option<usize> {
    show.get("parameters")?.as_str()?.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "num_ctx").then(|| {
            words
                .next()?
                .parse::<usize>()
                .ok()
                .filter(|value| *value > 0)
        })?
    })
}

/// Distinguish the native model capacity from its actually configured/loaded
/// window. In particular, never mistake `embedding_length` for context length.
fn metadata_context(
    show: &Value,
    running: Option<&Value>,
    model: &str,
) -> Option<ContextResolution> {
    let info = show.get("model_info").and_then(Value::as_object);
    let native = info.and_then(|info| {
        let architecture = info.get("general.architecture")?.as_str()?;
        positive(info.get(&format!("{architecture}.context_length"))?)
    });
    let configured = parameter_context(show);
    let matches_model = |name: &str| {
        name == model
            || name.strip_suffix(":latest").unwrap_or(name)
                == model.strip_suffix(":latest").unwrap_or(model)
    };
    let loaded = running
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().find_map(|entry| {
                let name = entry.get("model").or_else(|| entry.get("name"))?.as_str()?;
                matches_model(name).then(|| positive(entry.get("context_length")?))?
            })
        });
    // A loaded context may be smaller than either the Modelfile or the native
    // capacity. Take the most restrictive known bound.
    let effective = [native, configured, loaded].into_iter().flatten().min()?;
    Some(ContextResolution {
        tokens: if configured.is_none() && loaded.is_none() {
            effective.min(UNLOADED_CONTEXT_CEILING)
        } else {
            effective
        },
        source: if loaded.is_some() {
            "ollama-loaded"
        } else if configured.is_some() {
            "ollama-parameters"
        } else {
            "ollama-model-conservative"
        },
    })
}

pub fn resolve_context(
    config: &EmbeddingConfig,
    override_tokens: Option<usize>,
) -> ContextResolution {
    if let Some(tokens) = override_tokens {
        return ContextResolution {
            tokens,
            source: "config",
        };
    }
    let Some(key) = CacheKey::from_config(config) else {
        return ContextResolution {
            tokens: FALLBACK_CONTEXT_TOKENS,
            source: "fallback",
        };
    };
    static CACHE: OnceLock<Mutex<ContextCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(ContextCache::default()));
    if let Some(resolution) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key, Instant::now())
    {
        return resolution;
    }
    // Serialize cache fills and check again: concurrent mount refreshes for the
    // same model must share one probe. Cache hits never wait for network I/O.
    static DISCOVERY: Mutex<()> = Mutex::new(());
    let _discovery = DISCOVERY.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(resolution) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key, Instant::now())
    {
        return resolution;
    }
    let resolution = discover_context(config);
    cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key, resolution.clone(), Instant::now());
    resolution
}

fn discover_context(config: &EmbeddingConfig) -> ContextResolution {
    let fallback = || ContextResolution {
        tokens: FALLBACK_CONTEXT_TOKENS,
        source: "fallback",
    };
    let (Some(base_url), Some(model)) = (config.base_url(), config.model.as_deref()) else {
        return fallback();
    };
    // Preserve reverse-proxy prefixes: /ollama/v1 -> /ollama/api/show. Never
    // probe another host or follow a redirect with provider credentials.
    let Some(root) = base_url.trim_end_matches('/').strip_suffix("/v1") else {
        return fallback();
    };
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return fallback();
    };
    let authorize = |request: reqwest::blocking::RequestBuilder| {
        if let Some(key) = config.api_key.as_deref() {
            request.bearer_auth(key)
        } else {
            request
        }
    };
    let show = authorize(client.post(format!("{root}/api/show")))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(|response| response.json::<Value>());
    let Ok(show) = show else {
        return fallback();
    };
    // An OpenAI-compatible endpoint may return unrelated JSON for unknown routes.
    if metadata_context(&show, None, model).is_none() {
        return fallback();
    }
    let running = authorize(client.get(format!("{root}/api/ps")))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(|response| response.json::<Value>())
        .ok();
    metadata_context(&show, running.as_ref(), model).unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn metadata_server(
        responses: Vec<(u16, Value)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, response) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let body = response.to_string();
                write!(stream, "HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        (format!("http://{address}/proxy/v1/"), handle)
    }

    #[test]
    fn discovers_context_through_authenticated_proxy_without_embedding_or_loading() {
        let (url, handle) = metadata_server(vec![
            (
                200,
                json!({"model_info": {"general.architecture": "bert", "bert.context_length": 1024}}),
            ),
            (200, json!({"models": []})),
        ]);
        let mut config = EmbeddingConfig::sparse();
        config.base_url = Some(url);
        config.model = Some("model".into());
        config.api_key = Some("test-secret".into());
        assert_eq!(resolve_context(&config, None).tokens, 1024);
        // The second lookup must not contact the server, which accepts only two requests.
        assert_eq!(resolve_context(&config, None).tokens, 1024);
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("POST /proxy/api/show "));
        assert!(requests[0].contains("\"model\":\"model\""));
        assert!(requests[1].starts_with("GET /proxy/api/ps "));
        assert!(requests.iter().all(|request| request
            .to_lowercase()
            .contains("authorization: bearer test-secret")));
    }

    #[test]
    fn unsupported_metadata_endpoint_uses_4096_fallback() {
        let (url, handle) = metadata_server(vec![(404, json!({"error":"not supported"}))]);
        let mut config = EmbeddingConfig::sparse();
        config.base_url = Some(url);
        config.model = Some("model".into());
        assert_eq!(
            resolve_context(&config, None),
            ContextResolution {
                tokens: 4096,
                source: "fallback"
            }
        );
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn cache_expires_and_model_endpoint_or_credentials_get_independent_entries() {
        let now = Instant::now();
        let mut config = EmbeddingConfig::sparse();
        config.base_url = Some("http://localhost:11434/v1".into());
        config.model = Some("model-A".into());
        let key = CacheKey::from_config(&config).unwrap();
        let value = ContextResolution {
            tokens: 1024,
            source: "ollama-model-conservative",
        };
        let mut cache = ContextCache::default();
        cache.insert(key.clone(), value.clone(), now);
        assert_eq!(
            cache.get(&key, now + CONTEXT_CACHE_TTL - Duration::from_secs(1)),
            Some(value.clone())
        );
        config.model = Some("model-B".into());
        assert!(cache
            .get(&CacheKey::from_config(&config).unwrap(), now)
            .is_none());
        config.model = Some("model-A".into());
        config.base_url = Some("http://other:11434/v1".into());
        assert!(cache
            .get(&CacheKey::from_config(&config).unwrap(), now)
            .is_none());
        config.base_url = Some("http://localhost:11434/v1/".into());
        assert_eq!(
            cache.get(&CacheKey::from_config(&config).unwrap(), now),
            Some(value)
        );
        config.api_key = Some("another-credential".into());
        assert!(cache
            .get(&CacheKey::from_config(&config).unwrap(), now)
            .is_none());
        assert!(cache.get(&key, now + CONTEXT_CACHE_TTL).is_none());
        assert!(cache.entries.is_empty());
        let fallback = ContextResolution {
            tokens: 4096,
            source: "fallback",
        };
        cache.insert(key.clone(), fallback.clone(), now);
        assert_eq!(
            cache.get(&key, now + FALLBACK_CACHE_TTL - Duration::from_secs(1)),
            Some(fallback)
        );
        assert!(cache.get(&key, now + FALLBACK_CACHE_TTL).is_none());
    }

    #[test]
    fn changed_model_triggers_discovery_immediately_and_override_bypasses_cache() {
        let (url, handle) = metadata_server(vec![
            (
                200,
                json!({"model_info":{"general.architecture":"bert", "bert.context_length":1024}}),
            ),
            (200, json!({"models":[]})),
            (
                200,
                json!({"model_info":{"general.architecture":"bert", "bert.context_length":2048}}),
            ),
            (200, json!({"models":[]})),
        ]);
        let mut config = EmbeddingConfig::sparse();
        config.base_url = Some(url);
        config.model = Some("model-A".into());
        assert_eq!(resolve_context(&config, None).tokens, 1024);
        assert_eq!(resolve_context(&config, Some(512)).tokens, 512);
        config.model = Some("model-B".into());
        assert_eq!(resolve_context(&config, None).tokens, 2048);
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].contains("model-A"));
        assert!(requests[2].contains("model-B"));
    }

    #[test]
    fn native_1024_is_respected_and_dimensions_are_ignored() {
        let show = json!({"model_info": {"general.architecture": "bert", "bert.context_length": 1024, "bert.embedding_length": 4096}});
        assert_eq!(metadata_context(&show, None, "model").unwrap().tokens, 1024);
        assert!(metadata_context(
            &json!({"model_info": {"bert.embedding_length": 1024}}),
            None,
            "model"
        )
        .is_none());
    }

    #[test]
    fn loaded_window_and_modelfile_limit_native_capacity() {
        let show = json!({"parameters": "temperature 0.7\nnum_ctx 4096", "model_info": {"general.architecture": "qwen3", "qwen3.context_length": 32768}});
        let running = json!({"models": [{"name": "other", "context_length": 512}, {"model": "model:latest", "context_length": 1024}]});
        assert_eq!(
            metadata_context(&show, Some(&running), "model").unwrap(),
            ContextResolution {
                tokens: 1024,
                source: "ollama-loaded"
            }
        );
        assert_eq!(metadata_context(&show, None, "model").unwrap().tokens, 4096);
        assert_eq!(metadata_context(&json!({"parameters":"num_ctx 99999", "model_info":{"general.architecture":"bert", "bert.context_length":1024}}), None, "model").unwrap().tokens, 1024);
    }

    #[test]
    fn unloaded_large_model_does_not_assume_a_large_server_window() {
        let show =
            json!({"model_info":{"general.architecture":"qwen3", "qwen3.context_length":32768}});
        assert_eq!(metadata_context(&show, None, "model").unwrap().tokens, 4096);
    }

    #[test]
    fn explicit_override_skips_discovery_and_fallback_is_visible() {
        let config = EmbeddingConfig::sparse();
        assert_eq!(
            resolve_context(&config, Some(1024)),
            ContextResolution {
                tokens: 1024,
                source: "config"
            }
        );
        assert_eq!(
            resolve_context(&config, None),
            ContextResolution {
                tokens: 4096,
                source: "fallback"
            }
        );
    }
}
