use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportMode {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StdioMode {
    Auto,
    Newline,
    Framed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub mcp_path: String,
    pub health_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HttpConfigInput {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub mcp_path: Option<String>,
    pub health_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoReindexConfig {
    pub enabled: bool,
    pub debounce_ms: u64,
    pub interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoReindexConfigInput {
    pub enabled: Option<bool>,
    pub debounce_ms: Option<u64>,
    pub interval_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EmbeddingProvider {
    #[default]
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SecretRef {
    OsKeyring { service: String, account: String },
    EncryptedFile { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<EmbeddingProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<SecretRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddingConfigInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<EmbeddingProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<SecretRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceConfigInput {
    pub vault_path: Option<PathBuf>,
    pub index_dir: Option<PathBuf>,
    pub transport: Option<TransportMode>,
    pub stdio_mode: Option<StdioMode>,
    pub http: Option<HttpConfigInput>,
    pub auto_reindex: Option<AutoReindexConfigInput>,
    pub embedding: Option<EmbeddingConfigInput>,
    pub artifact_embedding: Option<EmbeddingConfigInput>,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PersistedServiceConfig {
    pub vault_path: Option<PathBuf>,
    pub index_dir: Option<PathBuf>,
    pub transport: Option<TransportMode>,
    pub stdio_mode: Option<StdioMode>,
    pub http: Option<HttpConfigInput>,
    pub auto_reindex: Option<AutoReindexConfigInput>,
    pub embedding: Option<EmbeddingConfigInput>,
    pub artifact_embedding: Option<EmbeddingConfigInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedServiceConfig {
    pub vault_path: PathBuf,
    pub index_dir: PathBuf,
    pub transport: TransportMode,
    pub stdio_mode: StdioMode,
    pub http: HttpConfig,
    pub auto_reindex: AutoReindexConfig,
    pub embedding: EmbeddingConfig,
    pub artifact_embedding: EmbeddingConfig,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEndpoints {
    pub mcp: String,
    pub health: String,
}

impl ResolvedServiceConfig {
    pub fn service_endpoints(&self) -> ServiceEndpoints {
        ServiceEndpoints {
            mcp: format!(
                "http://{}:{}{}",
                self.http.host, self.http.port, self.http.mcp_path
            ),
            health: format!(
                "http://{}:{}{}",
                self.http.host, self.http.port, self.http.health_path
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes_api_key_ref() {
        let config: PersistedServiceConfig = serde_json::from_str(
            r#"{
                "embedding": {
                    "provider": "openai-compatible",
                    "model": "nomic-embed-text",
                    "baseUrl": "http://localhost:11434/v1",
                    "apiKeyRef": {
                        "kind": "encryptedFile",
                        "id": "openai-embedding"
                    }
                }
            }"#,
        )
        .expect("parse config");

        let reference = config
            .embedding
            .as_ref()
            .and_then(|embedding| embedding.api_key_ref.as_ref())
            .expect("api key ref");
        assert!(matches!(
            reference,
            SecretRef::EncryptedFile { id } if id == "openai-embedding"
        ));

        let serialized = serde_json::to_string(&config).expect("serialize config");
        assert!(serialized.contains("apiKeyRef"));
        assert!(serialized.contains("encryptedFile"));
        assert!(!serialized.contains("apiKey\""));
        assert!(!serialized.contains("apiKeyEnv"));
    }

    #[test]
    fn rejects_old_plaintext_secret_fields() {
        let error = serde_json::from_str::<PersistedServiceConfig>(
            r#"{
                "embedding": {
                    "provider": "openai-compatible",
                    "model": "text-embedding-3-small",
                    "apiKey": "secret"
                }
            }"#,
        )
        .expect_err("old apiKey should be rejected");

        assert!(error.to_string().contains("apiKey"));
    }
}
