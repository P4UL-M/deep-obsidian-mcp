use std::{thread, time::Duration};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::index::SemanticBackend;

pub const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 32;
#[allow(dead_code)]
pub(crate) const DEFAULT_EMBEDDING_MAX_CONCURRENCY: usize = 4;
/// Hard character ceiling on a single embedding input. Acts as a backstop on top
/// of the token budget below; the effective per-input cap is the smaller of the two.
pub const DEFAULT_EMBEDDING_MAX_CHARS: usize = 8_000;
/// Backend context window (tokens) we size inputs against. Defaults to Ollama's
/// out-of-box `num_ctx` (4096) so the fix works without changing the embedding
/// server. An input exceeding the worker's window crashes llama.cpp rather than
/// erroring, so this MUST stay <= the server's actual `num_ctx`
/// (`OLLAMA_CONTEXT_LENGTH`). Raise via config only after raising the server window.
pub const DEFAULT_EMBEDDING_CONTEXT_TOKENS: usize = 4_096;
/// Per-input token budget, kept with margin (~30%) under the context window.
pub const DEFAULT_EMBEDDING_MAX_INPUT_TOKENS: usize = 2_800;
/// Conservative chars-per-token estimate. Dense markdown / code pack ~2.7-2.9
/// tokens per char (observed on technical vaults), so we keep this low to
/// over-estimate token counts and stay safe.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 2.5;
#[allow(dead_code)]
pub(crate) const DEFAULT_EMBEDDING_TIMEOUT: Duration = Duration::from_secs(60);

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub provider: Option<EmbeddingProvider>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub max_chars: usize,
    pub batch_size: usize,
    /// Per-input token budget. Each embedding input is clamped to at most
    /// `max_input_tokens * chars_per_token` characters (and `max_chars`).
    pub max_input_tokens: usize,
    /// Backend context window in tokens; bounds the per-request token total.
    pub context_tokens: usize,
    /// Conservative chars-per-token estimate used to convert token budgets to chars.
    pub chars_per_token: f64,
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
            max_chars: DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        }
    }

    pub fn normalize(mut self) -> Self {
        self.batch_size = self.batch_size.max(1);
        self.max_chars = self.max_chars.max(1);
        self.max_input_tokens = self.max_input_tokens.max(1);
        self.context_tokens = self.context_tokens.max(self.max_input_tokens);
        if !(self.chars_per_token.is_finite() && self.chars_per_token >= 1.0) {
            self.chars_per_token = DEFAULT_CHARS_PER_TOKEN;
        }
        self
    }

    /// Effective per-input character cap: the smaller of the hard char ceiling and
    /// the char-equivalent of the per-input token budget.
    pub fn effective_max_chars(&self) -> usize {
        let token_chars = (self.max_input_tokens as f64 * self.chars_per_token).floor();
        let token_chars = if token_chars.is_finite() && token_chars >= 1.0 {
            token_chars as usize
        } else {
            self.max_chars
        };
        self.max_chars.min(token_chars).max(1)
    }

    /// Estimated token count of a string under the configured chars/token ratio.
    pub fn estimate_tokens(&self, text: &str) -> usize {
        let ratio = if self.chars_per_token >= 1.0 {
            self.chars_per_token
        } else {
            DEFAULT_CHARS_PER_TOKEN
        };
        ((text.chars().count() as f64) / ratio).ceil() as usize
    }

    pub fn is_sparse(&self) -> bool {
        self.provider.is_none()
            || self
                .model
                .as_ref()
                .map(|model| model.trim().is_empty())
                .unwrap_or(true)
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
        self.base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    }

    pub fn resolve_api_key(&self) -> Option<String> {
        self.api_key.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResult {
    pub vectors: Vec<Vec<f64>>,
    pub dimensions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactEmbeddingInput {
    pub path: String,
    pub kind: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub(crate) struct EmbeddingBatchMetrics {
    pub text_count: usize,
    pub batch_count: usize,
    pub dimensions: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)]
pub(crate) struct EmbeddingBatchResult {
    pub vectors: Vec<Vec<f64>>,
    pub dimensions: usize,
    pub metrics: EmbeddingBatchMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct EmbeddingBatchOptions {
    pub batch_size: usize,
    pub max_concurrency: usize,
    pub timeout: Duration,
    /// Upper bound on the estimated total tokens packed into a single HTTP request.
    /// A batch is closed when EITHER `batch_size` inputs OR this token budget is hit.
    pub max_request_tokens: usize,
}

impl EmbeddingBatchOptions {
    #[allow(dead_code)]
    pub(crate) fn from_config(config: &EmbeddingConfig) -> Self {
        Self {
            batch_size: config.batch_size.max(1),
            max_concurrency: DEFAULT_EMBEDDING_MAX_CONCURRENCY,
            timeout: DEFAULT_EMBEDDING_TIMEOUT,
            // Cap the per-request token sum at the per-input budget so a batch never
            // decodes far more than a single chunk's worth of tokens against the
            // worker's `num_ctx`. llama.cpp can crash (heap corruption) when a
            // request exceeds the window, so we keep request totals modest rather
            // than packing up to the full context.
            max_request_tokens: config.max_input_tokens.max(1),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn normalized(mut self) -> Self {
        self.batch_size = self.batch_size.max(1);
        self.max_concurrency = self.max_concurrency.max(1);
        self.max_request_tokens = self.max_request_tokens.max(1);
        self
    }
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
    #[error(
        "embedding provider returned inconsistent dimensions: expected {expected}, actual {actual}"
    )]
    DimensionsMismatch { expected: usize, actual: usize },
}

fn clamp_chars(text: &str, max_chars: usize) -> String {
    let safe_max = max_chars.max(1);
    if text.chars().count() <= safe_max {
        return text.to_string();
    }
    text.chars().take(safe_max).collect()
}

/// Clamp a single embedding input to the config's effective per-input budget
/// (the smaller of the hard char ceiling and the token-derived char cap). This is
/// the guarantee that no input sent to the backend can exceed its context window.
fn clamp_input(text: &str, config: &EmbeddingConfig) -> String {
    clamp_chars(text, config.effective_max_chars())
}

pub fn normalize_dense_vector(vector: &[f64]) -> Vec<f64> {
    let norm = vector.iter().map(|value| value * value).sum::<f64>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return vector.to_vec();
    }
    vector.iter().map(|value| value / norm).collect()
}

pub fn embed_texts(
    texts: &[String],
    config: &EmbeddingConfig,
) -> Result<EmbeddingResult, EmbeddingError> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|error| EmbeddingError::RequestFailed {
            status: 0,
            body: error.to_string(),
        })?;
    embed_texts_with_client(texts, config, &client)
}

#[allow(dead_code)]
pub(crate) fn embed_text_batches(
    texts: &[String],
    config: &EmbeddingConfig,
    options: Option<EmbeddingBatchOptions>,
) -> Result<EmbeddingBatchResult, EmbeddingError> {
    let options = options
        .unwrap_or_else(|| EmbeddingBatchOptions::from_config(config))
        .normalized();
    let client = reqwest::blocking::Client::builder()
        .timeout(options.timeout)
        .build()
        .map_err(|error| EmbeddingError::RequestFailed {
            status: 0,
            body: error.to_string(),
        })?;
    embed_text_batches_with_client(texts, config, &client, &options)
}

#[allow(dead_code)]
pub(crate) fn embed_text_batches_with_client(
    texts: &[String],
    config: &EmbeddingConfig,
    client: &reqwest::blocking::Client,
    options: &EmbeddingBatchOptions,
) -> Result<EmbeddingBatchResult, EmbeddingError> {
    if texts.is_empty() {
        return Ok(EmbeddingBatchResult {
            vectors: Vec::new(),
            dimensions: 0,
            metrics: EmbeddingBatchMetrics {
                text_count: 0,
                batch_count: 0,
                dimensions: 0,
            },
        });
    }

    let options = options.clone().normalized();
    let batches = pack_batches(texts, config, &options);
    let batch_count = batches.len();
    let mut batch_results = Vec::with_capacity(batch_count);

    for window in batches.chunks(options.max_concurrency) {
        let window_results = thread::scope(|scope| {
            let handles = window
                .iter()
                .enumerate()
                .map(|(offset, batch)| {
                    let client = client.clone();
                    let batch = batch.clone();
                    let batch_index = batch_results.len() + offset;
                    scope.spawn(move || {
                        (
                            batch_index,
                            embed_batch_with_bisect(&batch, config, &client),
                        )
                    })
                })
                .collect::<Vec<_>>();

            let mut results = Vec::with_capacity(handles.len());
            for handle in handles {
                results.push(handle.join().map_err(|_| EmbeddingError::RequestFailed {
                    status: 0,
                    body: "embedding worker panicked".to_string(),
                })?);
            }
            Ok::<_, EmbeddingError>(results)
        })?;
        batch_results.extend(window_results);
    }

    batch_results.sort_by_key(|(batch_index, _)| *batch_index);
    let mut vectors = Vec::with_capacity(texts.len());
    let mut dimensions = None;
    for (_, result) in batch_results {
        let result = result?;
        if let Some(expected) = dimensions {
            if result.dimensions != expected {
                return Err(EmbeddingError::DimensionsMismatch {
                    expected,
                    actual: result.dimensions,
                });
            }
        } else {
            dimensions = Some(result.dimensions);
        }
        vectors.extend(result.vectors);
    }

    if vectors.len() != texts.len() {
        return Err(EmbeddingError::VectorCountMismatch);
    }

    let dimensions = dimensions.unwrap_or(0);
    Ok(EmbeddingBatchResult {
        vectors,
        dimensions,
        metrics: EmbeddingBatchMetrics {
            text_count: texts.len(),
            batch_count,
            dimensions,
        },
    })
}

/// Pack inputs into request batches, closing a batch when EITHER the input count
/// reaches `batch_size` OR the estimated token total reaches `max_request_tokens`.
/// A single oversized input still gets its own batch (it is clamped at send time).
#[allow(dead_code)]
fn pack_batches(
    texts: &[String],
    config: &EmbeddingConfig,
    options: &EmbeddingBatchOptions,
) -> Vec<Vec<String>> {
    let mut batches: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_tokens = 0usize;

    for text in texts {
        // Tokens actually sent are bounded by the per-input clamp.
        let est = config.estimate_tokens(&clamp_input(text, config)).max(1);
        if !current.is_empty()
            && (current.len() >= options.batch_size
                || current_tokens + est > options.max_request_tokens)
        {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current.push(text.clone());
        current_tokens += est;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Embed a batch, bisecting the INPUT LIST on failure so one bad input can't fail
/// the whole batch. Splits in half and retries each half down to a single input;
/// a size-1 input that still fails propagates its error. The 1:1 input→vector
/// contract is preserved (we never split an input's text here).
#[allow(dead_code)]
fn embed_batch_with_bisect(
    texts: &[String],
    config: &EmbeddingConfig,
    client: &reqwest::blocking::Client,
) -> Result<EmbeddingResult, EmbeddingError> {
    match embed_texts_with_client(texts, config, client) {
        Ok(result) => Ok(result),
        Err(error) => {
            if texts.len() <= 1 {
                return Err(error);
            }
            let mid = texts.len() / 2;
            let left = embed_batch_with_bisect(&texts[..mid], config, client)?;
            let right = embed_batch_with_bisect(&texts[mid..], config, client)?;
            if !left.vectors.is_empty()
                && !right.vectors.is_empty()
                && left.dimensions != right.dimensions
            {
                return Err(EmbeddingError::DimensionsMismatch {
                    expected: left.dimensions,
                    actual: right.dimensions,
                });
            }
            let dimensions = if left.vectors.is_empty() {
                right.dimensions
            } else {
                left.dimensions
            };
            let mut vectors = left.vectors;
            vectors.extend(right.vectors);
            Ok(EmbeddingResult {
                vectors,
                dimensions,
            })
        }
    }
}

pub fn embed_texts_with_client(
    texts: &[String],
    config: &EmbeddingConfig,
    client: &reqwest::blocking::Client,
) -> Result<EmbeddingResult, EmbeddingError> {
    if texts.is_empty() {
        return Ok(EmbeddingResult {
            vectors: Vec::new(),
            dimensions: 0,
        });
    }

    if config.is_sparse() {
        return Err(EmbeddingError::NotConfigured);
    }

    if !matches!(
        config.provider.as_ref(),
        Some(EmbeddingProvider::OpenAiCompatible)
    ) {
        return Err(match config.provider.as_ref() {
            Some(other) => EmbeddingError::UnsupportedProvider(other.as_str().to_string()),
            None => EmbeddingError::NotConfigured,
        });
    }

    let base_url = config.base_url().ok_or(EmbeddingError::MissingBaseUrl)?;
    let model = config
        .model
        .as_deref()
        .ok_or(EmbeddingError::NotConfigured)?;
    let api_key = config.resolve_api_key();

    let request_body = serde_json::json!({
        "model": model,
        "input": texts
            .iter()
            .map(|text| clamp_input(text, config))
            .collect::<Vec<_>>(),
    });

    let mut request = client
        .post(format!("{}/embeddings", base_url.trim_end_matches('/')))
        .header("content-type", "application/json");
    if let Some(api_key) = api_key.as_deref() {
        request = request.header("authorization", format!("Bearer {api_key}"));
    }

    let response =
        request
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

    let payload: serde_json::Value =
        response
            .json()
            .map_err(|error| EmbeddingError::RequestFailed {
                status: status.as_u16(),
                body: error.to_string(),
            })?;
    let mut items = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    items.sort_by(|left, right| {
        let left_index = left
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let right_index = right
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
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
    Ok(EmbeddingResult {
        vectors,
        dimensions,
    })
}

pub fn embed_artifacts(
    artifacts: &[ArtifactEmbeddingInput],
    config: &EmbeddingConfig,
) -> Result<EmbeddingResult, EmbeddingError> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|error| EmbeddingError::RequestFailed {
            status: 0,
            body: error.to_string(),
        })?;
    embed_artifacts_with_client(artifacts, config, &client)
}

pub fn embed_artifacts_with_client(
    artifacts: &[ArtifactEmbeddingInput],
    config: &EmbeddingConfig,
    client: &reqwest::blocking::Client,
) -> Result<EmbeddingResult, EmbeddingError> {
    if artifacts.is_empty() {
        return Ok(EmbeddingResult {
            vectors: Vec::new(),
            dimensions: 0,
        });
    }

    if config.is_sparse() {
        return Err(EmbeddingError::NotConfigured);
    }

    if !matches!(
        config.provider.as_ref(),
        Some(EmbeddingProvider::OpenAiCompatible)
    ) {
        return Err(match config.provider.as_ref() {
            Some(other) => EmbeddingError::UnsupportedProvider(other.as_str().to_string()),
            None => EmbeddingError::NotConfigured,
        });
    }

    let base_url = config.base_url().ok_or(EmbeddingError::MissingBaseUrl)?;
    let model = config
        .model
        .as_deref()
        .ok_or(EmbeddingError::NotConfigured)?;
    let api_key = config.resolve_api_key();

    let request_body = serde_json::json!({
        "model": model,
        "input": artifacts
            .iter()
            .map(|artifact| serde_json::json!({
                "type": "file",
                "path": artifact.path,
                "kind": artifact.kind,
                "mime_type": artifact.mime_type,
                "data": BASE64_STANDARD.encode(&artifact.bytes),
            }))
            .collect::<Vec<_>>(),
    });

    let mut request = client
        .post(format!("{}/embeddings", base_url.trim_end_matches('/')))
        .header("content-type", "application/json");
    if let Some(api_key) = api_key.as_deref() {
        request = request.header("authorization", format!("Bearer {api_key}"));
    }

    let response =
        request
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

    let payload: serde_json::Value =
        response
            .json()
            .map_err(|error| EmbeddingError::RequestFailed {
                status: status.as_u16(),
                body: error.to_string(),
            })?;
    parse_embedding_payload(payload, artifacts.len())
}

fn parse_embedding_payload(
    payload: serde_json::Value,
    expected_count: usize,
) -> Result<EmbeddingResult, EmbeddingError> {
    let mut items = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    items.sort_by(|left, right| {
        let left_index = left
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let right_index = right
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
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

    if vectors.len() != expected_count {
        return Err(EmbeddingError::VectorCountMismatch);
    }

    let dimensions = vectors.first().map(|vector| vector.len()).unwrap_or(0);
    Ok(EmbeddingResult {
        vectors,
        dimensions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{Arc, Mutex},
        thread::JoinHandle,
    };

    fn test_config(base_url: String) -> EmbeddingConfig {
        EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("test-embedding-model".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: 2,
            max_input_tokens: DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        }
    }

    fn spawn_embedding_server(
        dimensions_by_request: Vec<usize>,
    ) -> (String, Arc<Mutex<Vec<Vec<String>>>>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test embedding server");
        let address = listener.local_addr().expect("read listener address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for (request_index, stream) in listener
                .incoming()
                .take(dimensions_by_request.len())
                .enumerate()
            {
                let stream = stream.expect("accept test embedding request");
                handle_embedding_request(
                    stream,
                    dimensions_by_request
                        .get(request_index)
                        .copied()
                        .unwrap_or(2),
                    &captured_requests,
                );
            }
        });
        (format!("http://{address}"), requests, handle)
    }

    fn handle_embedding_request(
        mut stream: TcpStream,
        dimensions: usize,
        requests: &Arc<Mutex<Vec<Vec<String>>>>,
    ) {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let bytes_read = stream.read(&mut buffer).expect("read request");
            if bytes_read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..bytes_read]);
            if request_body(&request).is_some() {
                break;
            }
        }

        let body = request_body(&request).expect("request body");
        let payload: serde_json::Value = serde_json::from_slice(body).expect("parse request body");
        let inputs = payload
            .get("input")
            .and_then(serde_json::Value::as_array)
            .expect("input array")
            .iter()
            .map(|value| value.as_str().expect("input string").to_string())
            .collect::<Vec<_>>();
        requests.lock().expect("lock requests").push(inputs.clone());

        let data = inputs
            .iter()
            .enumerate()
            .rev()
            .map(|(index, text)| {
                let embedding = (0..dimensions)
                    .map(|offset| text.len() as f64 + offset as f64)
                    .collect::<Vec<_>>();
                json!({
                    "index": index,
                    "embedding": embedding,
                })
            })
            .collect::<Vec<_>>();
        let response_body = json!({ "data": data }).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    }

    fn request_body(request: &[u8]) -> Option<&[u8]> {
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")?;
        let headers = std::str::from_utf8(&request[..header_end]).ok()?;
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        (request.len() >= body_start + content_length)
            .then(|| &request[body_start..body_start + content_length])
    }

    #[test]
    fn embed_text_batches_preserves_order_and_reports_metrics() {
        let texts = ["a", "bb", "ccc", "dddd", "eeeee"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let (base_url, requests, handle) = spawn_embedding_server(vec![2, 2, 2]);
        let config = test_config(base_url);

        let result = embed_text_batches(
            &texts,
            &config,
            Some(EmbeddingBatchOptions {
                batch_size: 2,
                max_concurrency: 2,
                timeout: Duration::from_secs(5),
                max_request_tokens: usize::MAX,
            }),
        )
        .expect("embed batches");
        handle.join().expect("join test embedding server");

        assert_eq!(result.dimensions, 2);
        assert_eq!(
            result.metrics,
            EmbeddingBatchMetrics {
                text_count: 5,
                batch_count: 3,
                dimensions: 2,
            }
        );
        assert_eq!(
            result
                .vectors
                .iter()
                .map(|vector| vector[0])
                .collect::<Vec<_>>(),
            vec![1.0, 2.0, 3.0, 4.0, 5.0]
        );

        let mut request_sizes = requests
            .lock()
            .expect("lock requests")
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>();
        request_sizes.sort_unstable();
        assert_eq!(request_sizes, vec![1, 2, 2]);
    }

    #[test]
    fn embed_text_batches_rejects_inconsistent_dimensions() {
        let texts = ["a", "bb", "ccc"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let (base_url, _requests, handle) = spawn_embedding_server(vec![2, 3]);
        let config = test_config(base_url);

        let error = embed_text_batches(
            &texts,
            &config,
            Some(EmbeddingBatchOptions {
                batch_size: 2,
                max_concurrency: 1,
                timeout: Duration::from_secs(5),
                max_request_tokens: usize::MAX,
            }),
        )
        .expect_err("dimension mismatch");
        handle.join().expect("join test embedding server");

        assert!(matches!(
            error,
            EmbeddingError::DimensionsMismatch {
                expected: 2,
                actual: 3
            }
        ));
    }

    #[test]
    fn clamp_input_caps_by_token_estimate() {
        // Token budget binds even though the hard char ceiling is far larger.
        let mut config = test_config("http://unused".to_string());
        config.max_chars = 100_000;
        config.max_input_tokens = 10;
        config.chars_per_token = 2.5;
        // effective cap = min(100_000, floor(10 * 2.5)) = 25 chars.
        assert_eq!(config.effective_max_chars(), 25);
        let long = "x".repeat(1_000);
        assert_eq!(clamp_input(&long, &config).chars().count(), 25);

        // Hard char ceiling binds when it is the smaller of the two.
        config.max_chars = 5;
        config.max_input_tokens = 10_000;
        assert_eq!(config.effective_max_chars(), 5);
        assert_eq!(clamp_input(&long, &config).chars().count(), 5);
    }

    #[test]
    fn pack_batches_splits_by_request_token_budget() {
        let mut config = test_config("http://unused".to_string());
        config.max_chars = 100_000; // no per-input clamping in this test
        config.max_input_tokens = 100_000;
        config.chars_per_token = 2.5;
        // Each input: 25 chars -> ceil(25 / 2.5) = 10 estimated tokens.
        let texts = vec!["x".repeat(25), "y".repeat(25), "z".repeat(25)];

        // Token budget of 15 forces one input per request despite a high count cap.
        let tight = EmbeddingBatchOptions {
            batch_size: 100,
            max_concurrency: 1,
            timeout: Duration::from_secs(5),
            max_request_tokens: 15,
        };
        let batches = pack_batches(&texts, &config, &tight);
        assert_eq!(batches.len(), 3);
        assert!(batches.iter().all(|batch| batch.len() == 1));

        // Token budget of 25 packs two inputs (20 tokens) before the third spills.
        let loose = EmbeddingBatchOptions {
            max_request_tokens: 25,
            ..tight
        };
        let batches = pack_batches(&texts, &config, &loose);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1].len(), 1);
    }

    /// Mock server that returns HTTP 500 for any request whose `input` array has
    /// more than one element, and a valid 1-dim embedding otherwise. Used to drive
    /// the bisect path: a multi-input batch fails, then each half retries down to 1.
    fn spawn_bisecting_server(expected_requests: usize) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind bisect server");
        let address = listener.local_addr().expect("read listener address");
        let handle = thread::spawn(move || {
            for stream in listener.incoming().take(expected_requests) {
                let mut stream = stream.expect("accept bisect request");
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let read = stream.read(&mut buffer).expect("read request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request_body(&request).is_some() {
                        break;
                    }
                }
                let body = request_body(&request).expect("request body");
                let payload: serde_json::Value =
                    serde_json::from_slice(body).expect("parse request body");
                let inputs = payload
                    .get("input")
                    .and_then(serde_json::Value::as_array)
                    .expect("input array")
                    .clone();

                let response = if inputs.len() > 1 {
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                } else {
                    let data = inputs
                        .iter()
                        .enumerate()
                        .map(|(index, text)| {
                            let len = text.as_str().map(|value| value.len()).unwrap_or(0);
                            json!({ "index": index, "embedding": [len as f64] })
                        })
                        .collect::<Vec<_>>();
                    let response_body = json!({ "data": data }).to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    )
                };
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn embed_batch_with_bisect_isolates_failing_input() {
        // texts of len 2: request [alpha, beta] -> 500, then [alpha] -> 200,
        // [beta] -> 200 = 3 connections, preserving 1:1 input order.
        let (base_url, handle) = spawn_bisecting_server(3);
        let config = test_config(base_url);
        let client = reqwest::blocking::Client::builder()
            .build()
            .expect("client");
        let texts = vec!["alpha".to_string(), "beta".to_string()];

        let result = embed_batch_with_bisect(&texts, &config, &client).expect("bisect");
        handle.join().expect("join bisect server");

        assert_eq!(result.dimensions, 1);
        assert_eq!(result.vectors.len(), 2);
        // Embedding value encodes input length, so order is verifiable: alpha=5, beta=4.
        assert_eq!(result.vectors[0][0], 5.0);
        assert_eq!(result.vectors[1][0], 4.0);
    }
}
