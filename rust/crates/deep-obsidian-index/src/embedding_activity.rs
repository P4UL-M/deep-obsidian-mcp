//! Historical successes observed during real embedding requests, never health probes.
//! Process-local and shared by calls to the same provider, endpoint and model.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::embeddings::{EmbeddingConfig, EmbeddingResult};

const MAX_BACKENDS: usize = 64;

#[derive(Clone, PartialEq, Eq, Hash)]
struct BackendIdentity {
    provider: String,
    endpoint: String,
    model: String,
}

impl BackendIdentity {
    fn new(provider: &str, endpoint: &str, model: &str) -> Self {
        Self {
            provider: provider.to_string(),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
        }
    }
}

fn successes() -> &'static Mutex<HashMap<BackendIdentity, DateTime<Utc>>> {
    static SUCCESSES: OnceLock<Mutex<HashMap<BackendIdentity, DateTime<Utc>>>> = OnceLock::new();
    SUCCESSES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The last successful real request for this backend, as a UTC RFC3339 timestamp.
/// None means no success observed in this process, or an evicted historical entry.
/// This is not a claim about current reachability. It never resolves credentials
/// or performs IO; credentials and input texts are not stored in the registry.
pub fn last_successful_embedding_at(provider: &str, endpoint: &str, model: &str) -> Option<String> {
    successes()
        .lock()
        .ok()?
        .get(&BackendIdentity::new(provider, endpoint, model))
        .map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true))
}

pub(crate) fn record_success(config: &EmbeddingConfig, result: &EmbeddingResult) {
    // Empty inputs return without a request. Malformed vectors are not successes.
    if result.vectors.is_empty()
        || result.dimensions == 0
        || result.vectors.iter().any(|vector| {
            vector.len() != result.dimensions || vector.iter().any(|value| !value.is_finite())
        })
    {
        return;
    }
    let (Some(provider), Some(endpoint), Some(model)) = (
        config.provider.as_ref(),
        config.base_url(),
        config.model.as_deref(),
    ) else {
        return;
    };
    let key = BackendIdentity::new(provider.as_str(), endpoint, model);
    let Ok(mut successes) = successes().lock() else {
        return;
    };
    if !successes.contains_key(&key) && successes.len() >= MAX_BACKENDS {
        if let Some(oldest) = successes
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(key, _)| key.clone())
        {
            successes.remove(&oldest);
        }
    }
    successes.insert(key, Utc::now());
}
