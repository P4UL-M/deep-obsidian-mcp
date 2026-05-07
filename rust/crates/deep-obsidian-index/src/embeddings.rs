use std::{thread, time::Duration};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::index::SemanticBackend;

pub const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 32;
#[allow(dead_code)]
pub(crate) const DEFAULT_EMBEDDING_MAX_CONCURRENCY: usize = 4;
pub const DEFAULT_EMBEDDING_MAX_CHARS: usize = 12_000;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub provider: Option<EmbeddingProvider>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
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
            max_chars: DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
        }
    }

    pub fn normalize(mut self) -> Self {
        self.batch_size = self.batch_size.max(1);
        self.max_chars = self.max_chars.max(1);
        self
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
}

impl EmbeddingBatchOptions {
    #[allow(dead_code)]
    pub(crate) fn from_config(config: &EmbeddingConfig) -> Self {
        Self {
            batch_size: config.batch_size.max(1),
            max_concurrency: DEFAULT_EMBEDDING_MAX_CONCURRENCY,
            timeout: DEFAULT_EMBEDDING_TIMEOUT,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn normalized(mut self) -> Self {
        self.batch_size = self.batch_size.max(1);
        self.max_concurrency = self.max_concurrency.max(1);
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
    let batches = texts
        .chunks(options.batch_size)
        .map(|chunk| chunk.to_vec())
        .collect::<Vec<_>>();
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
                            embed_texts_with_client(&batch, config, &client),
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
            .map(|text| clamp_text(text, config.max_chars))
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
}
