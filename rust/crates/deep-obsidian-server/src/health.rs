use deep_obsidian_types::ResolvedServiceConfig;
use serde_json::{json, Map, Value};

use crate::runtime::{
    storage_backend_name, vector_search_backend_name, RuntimeDiagnostics, RuntimeIndexSnapshot,
    RuntimeReadiness,
};

fn insert_optional_value<T>(map: &mut Map<String, Value>, key: &str, value: &Option<T>)
where
    T: serde::Serialize,
{
    if let Some(value) = value {
        if let Ok(json) = serde_json::to_value(value) {
            map.insert(key.to_string(), json);
        }
    }
}

pub fn build_health_payload(
    config: &ResolvedServiceConfig,
    diagnostics: &RuntimeDiagnostics,
) -> Value {
    let mut payload = Map::new();
    payload.insert("status".to_string(), Value::String("ok".to_string()));
    payload.insert(
        "vaultPath".to_string(),
        Value::String(config.vault_path.to_string_lossy().to_string()),
    );
    payload.insert(
        "ready".to_string(),
        Value::Bool(diagnostics.snapshot.is_some()),
    );
    payload.insert(
        "indexStatus".to_string(),
        Value::String(diagnostics.status.as_str().to_string()),
    );
    payload.insert(
        "refreshInFlight".to_string(),
        Value::Bool(diagnostics.refresh_in_flight),
    );
    payload.insert(
        "autoReindex".to_string(),
        Value::Bool(config.auto_reindex.enabled),
    );
    insert_runtime_diagnostics(&mut payload, diagnostics);
    if let Some(snapshot) = &diagnostics.snapshot {
        insert_index_snapshot(&mut payload, snapshot);
    }
    Value::Object(payload)
}

pub fn build_readiness_payload(
    config: &ResolvedServiceConfig,
    diagnostics: &RuntimeDiagnostics,
) -> Value {
    let mut payload = Map::new();
    payload.insert(
        "status".to_string(),
        Value::String(diagnostics.status.as_str().to_string()),
    );
    payload.insert(
        "ready".to_string(),
        Value::Bool(diagnostics.snapshot.is_some()),
    );
    payload.insert(
        "vaultPath".to_string(),
        Value::String(config.vault_path.to_string_lossy().to_string()),
    );
    payload.insert(
        "refreshInFlight".to_string(),
        Value::Bool(diagnostics.refresh_in_flight),
    );
    payload.insert(
        "autoReindex".to_string(),
        Value::Bool(config.auto_reindex.enabled),
    );
    insert_runtime_diagnostics(&mut payload, diagnostics);
    if let Some(snapshot) = &diagnostics.snapshot {
        insert_index_snapshot(&mut payload, snapshot);
    }
    Value::Object(payload)
}

pub fn readiness_status_code(diagnostics: &RuntimeDiagnostics) -> axum::http::StatusCode {
    match diagnostics.status {
        RuntimeReadiness::Ready => axum::http::StatusCode::OK,
        RuntimeReadiness::Loading | RuntimeReadiness::Degraded => {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

fn insert_index_snapshot(payload: &mut Map<String, Value>, snapshot: &RuntimeIndexSnapshot) {
    let index = snapshot.index.as_ref();
    payload.insert(
        "markdownFileCount".to_string(),
        json!(index.file_snapshots.len()),
    );
    payload.insert("rebuilt".to_string(), Value::Bool(snapshot.rebuilt));
    payload.insert(
        "generatedAt".to_string(),
        Value::String(index.generated_at.clone()),
    );
    payload.insert(
        "semanticBackend".to_string(),
        Value::String(index.semantic_backend.as_str().to_string()),
    );
}

fn insert_runtime_diagnostics(payload: &mut Map<String, Value>, diagnostics: &RuntimeDiagnostics) {
    if let Some(last_success) = &diagnostics.last_success {
        payload.insert(
            "lastRefresh".to_string(),
            json!({
                "reason": last_success.reason,
                "rebuilt": last_success.rebuilt,
                "generatedAt": last_success.generated_at,
                "finishedAtUnixMs": last_success.finished_at_unix_ms,
            }),
        );
    }
    if let Some(last_error) = &diagnostics.last_error {
        payload.insert(
            "lastError".to_string(),
            json!({
                "reason": last_error.reason,
                "message": last_error.message,
                "finishedAtUnixMs": last_error.finished_at_unix_ms,
            }),
        );
    }
}

pub fn build_vault_overview_payload(
    config: &ResolvedServiceConfig,
    snapshot: &RuntimeIndexSnapshot,
) -> Value {
    let index = snapshot.index.as_ref();
    let mut payload = Map::new();
    payload.insert(
        "vaultPath".to_string(),
        Value::String(config.vault_path.to_string_lossy().to_string()),
    );
    payload.insert(
        "markdownFileCount".to_string(),
        json!(index.file_snapshots.len()),
    );
    payload.insert(
        "indexGeneratedAt".to_string(),
        Value::String(index.generated_at.clone()),
    );
    payload.insert("chunkCount".to_string(), json!(index.chunk_count));
    payload.insert("noteCount".to_string(), json!(index.note_count));
    payload.insert(
        "storageBackend".to_string(),
        Value::String(storage_backend_name().to_string()),
    );
    payload.insert(
        "vectorSearchBackend".to_string(),
        Value::String(vector_search_backend_name(index).to_string()),
    );
    payload.insert(
        "semanticBackend".to_string(),
        Value::String(index.semantic_backend.as_str().to_string()),
    );
    insert_optional_value(&mut payload, "embeddingProvider", &index.embedding_provider);
    insert_optional_value(&mut payload, "embeddingModel", &index.embedding_model);
    payload.insert("rebuilt".to_string(), Value::Bool(snapshot.rebuilt));
    payload.insert(
        "autoReindex".to_string(),
        Value::Bool(config.auto_reindex.enabled),
    );
    payload.insert(
        "reindexDebounceMs".to_string(),
        json!(config.auto_reindex.debounce_ms),
    );
    payload.insert(
        "reindexIntervalMs".to_string(),
        json!(config.auto_reindex.interval_ms),
    );
    Value::Object(payload)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use deep_obsidian_types::{
        AutoReindexConfig, EmbeddingConfig, HttpConfig, StdioMode, TransportMode,
    };

    use super::*;

    fn test_config() -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            vault_path: PathBuf::from("/tmp/deep-obsidian-test-vault"),
            index_dir: PathBuf::from("/tmp/deep-obsidian-test-index"),
            transport: TransportMode::Http,
            stdio_mode: StdioMode::Newline,
            http: HttpConfig {
                host: "127.0.0.1".to_string(),
                port: 4100,
                mcp_path: "/mcp".to_string(),
                health_path: "/healthz".to_string(),
            },
            auto_reindex: AutoReindexConfig {
                enabled: true,
                debounce_ms: 250,
                interval_ms: 30_000,
            },
            embedding: EmbeddingConfig::default(),
            config_file_path: None,
        }
    }

    #[test]
    fn health_payload_does_not_require_ready_index() {
        let diagnostics = RuntimeDiagnostics {
            status: RuntimeReadiness::Loading,
            refresh_in_flight: true,
            snapshot: None,
            last_success: None,
            last_error: None,
        };

        let payload = build_health_payload(&test_config(), &diagnostics);

        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["ready"], false);
        assert_eq!(payload["indexStatus"], "loading");
        assert_eq!(payload["refreshInFlight"], true);
        assert!(payload.get("generatedAt").is_none());
    }

    #[test]
    fn readiness_returns_unavailable_until_index_is_ready() {
        let diagnostics = RuntimeDiagnostics {
            status: RuntimeReadiness::Degraded,
            refresh_in_flight: false,
            snapshot: None,
            last_success: None,
            last_error: None,
        };

        assert_eq!(
            readiness_status_code(&diagnostics),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
