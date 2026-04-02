use std::env;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::index::SemanticBackend;

const DEFAULT_EMBEDDING_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 32;
pub const DEFAULT_EMBEDDING_MAX_CHARS: usize = 12_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingProvider {
    OpenAiCompatible,
}

impl EmbeddingProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbeddingProvider::OpenAiCompatible => "openai-compatible",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub provider: Option<EmbeddingProvider>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub max_chars: usize,
    pub batch_size: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self::sparse()
    }
}

impl EmbeddingConfig {
    pub fn sparse() -> Self {
        Self {
            provider: None,
            model: None,
            base_url: None,
            api_key: None,
            api_key_env: None,
            max_chars: DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
        }
    }

    pub fn normalize(mut self) -> Self {
        self.batch_size = self.batch_size.max(1);
        self.max_chars = self.max_chars.max(1);
        if self.base_url.is_none() {
            self.base_url = Some(DEFAULT_EMBEDDING_BASE_URL.to_string());
        }
        self
    }

    pub fn is_sparse(&self) -> bool {
        self.provider.is_none() || self.model.as_ref().map(|model| model.trim().is_empty()).unwrap_or(true)
    }

    pub fn semantic_backend(&self) -> SemanticBackend {
        if self.is_sparse() {
            SemanticBackend::Sparse
        } else {
            SemanticBackend::Embedding
        }
    }

    pub fn supports_embeddings(&self) -> bool {
        matches!(self.provider, Some(EmbeddingProvider::OpenAiCompatible)) && !self.is_sparse()
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref().filter(|value| !value.trim().is_empty())
    }

    pub fn resolve_api_key(&self) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| self.api_key_env.as_deref().and_then(|key| env::var(key).ok()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResult {
    pub vectors: Vec<Vec<f64>>,
    pub dimensions: usize,
}

#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error("embedding backend is not configured")]
    NotConfigured,
    #[error("unsupported embedding provider: {0}")]
    UnsupportedProvider(String),
    #[error("embedding request failed ({status}): {body}")]
    RequestFailed { status: u16, body: String },
    #[error("embedding provider returned an unexpected number of vectors")]
    VectorCountMismatch,
    #[error("embedding provider returned an empty base URL")]
    MissingBaseUrl,
}

fn clamp_text(text: &str, max_chars: usize) -> String {
    let safe_max = max_chars.max(1);
    if text.chars().count() <= safe_max {
        return text.to_string();
    }
    text.chars().take(safe_max).collect()
}

pub fn normalize_dense_vector(vector: &[f64]) -> Vec<f64> {
    let norm = vector.iter().map(|value| value * value).sum::<f64>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return vector.to_vec();
    }
    vector.iter().map(|value| value / norm).collect()
}

pub fn embed_texts(texts: &[String], config: &EmbeddingConfig) -> Result<EmbeddingResult, EmbeddingError> {
    if texts.is_empty() {
        return Ok(EmbeddingResult {
            vectors: Vec::new(),
            dimensions: 0,
        });
    }

    if config.is_sparse() {
        return Err(EmbeddingError::NotConfigured);
    }

    if !matches!(config.provider.as_ref(), Some(EmbeddingProvider::OpenAiCompatible)) {
        return Err(match config.provider.as_ref() {
            Some(other) => EmbeddingError::UnsupportedProvider(other.as_str().to_string()),
            None => EmbeddingError::NotConfigured,
        });
    }

    let base_url = config.base_url().ok_or(EmbeddingError::MissingBaseUrl)?;
    let model = config.model.as_deref().ok_or(EmbeddingError::NotConfigured)?;
    let api_key = config.resolve_api_key();

    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|error| EmbeddingError::RequestFailed {
            status: 0,
            body: error.to_string(),
        })?;

    let request_body = serde_json::json!({
        "model": model,
        "input": texts
            .iter()
            .map(|text| clamp_text(text, config.max_chars))
            .collect::<Vec<_>>(),
    });

    let mut request = client
        .post(format!("{}/embeddings", base_url.trim_end_matches('/')))
        .header("content-type", "application/json");
    if let Some(api_key) = api_key.as_deref() {
        request = request.header("authorization", format!("Bearer {api_key}"));
    }

    let response = request
        .json(&request_body)
        .send()
        .map_err(|error| EmbeddingError::RequestFailed {
            status: 0,
            body: error.to_string(),
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        return Err(EmbeddingError::RequestFailed {
            status: status.as_u16(),
            body,
        });
    }

    let payload: serde_json::Value = response.json().map_err(|error| EmbeddingError::RequestFailed {
        status: status.as_u16(),
        body: error.to_string(),
    })?;
    let mut items = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    items.sort_by(|left, right| {
        let left_index = left.get("index").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let right_index = right.get("index").and_then(serde_json::Value::as_u64).unwrap_or(0);
        left_index.cmp(&right_index)
    });

    let mut vectors = Vec::with_capacity(items.len());
    for item in items {
        let Some(vector) = item.get("embedding").and_then(serde_json::Value::as_array) else {
            return Err(EmbeddingError::VectorCountMismatch);
        };
        let parsed = vector
            .iter()
            .map(|value| value.as_f64().ok_or(EmbeddingError::VectorCountMismatch))
            .collect::<Result<Vec<_>, _>>()?;
        vectors.push(parsed);
    }

    if vectors.len() != texts.len() {
        return Err(EmbeddingError::VectorCountMismatch);
    }

    let dimensions = vectors.first().map(|vector| vector.len()).unwrap_or(0);
    Ok(EmbeddingResult { vectors, dimensions })
}
