use deep_obsidian_types::ResolvedServiceConfig;
use serde_json::{json, Map, Value};

use crate::runtime::{storage_backend_name, vector_search_backend_name, RuntimeIndexSnapshot};

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

pub fn build_health_payload(config: &ResolvedServiceConfig, snapshot: &RuntimeIndexSnapshot) -> Value {
    let index = snapshot.index.as_ref();
    let mut payload = Map::new();
    payload.insert("status".to_string(), Value::String("ok".to_string()));
    payload.insert(
        "vaultPath".to_string(),
        Value::String(config.vault_path.to_string_lossy().to_string()),
    );
    payload.insert("markdownFileCount".to_string(), json!(index.file_snapshots.len()));
    payload.insert("rebuilt".to_string(), Value::Bool(snapshot.rebuilt));
    payload.insert("generatedAt".to_string(), Value::String(index.generated_at.clone()));
    payload.insert(
        "semanticBackend".to_string(),
        Value::String(index.semantic_backend.as_str().to_string()),
    );
    payload.insert(
        "autoReindex".to_string(),
        Value::Bool(config.auto_reindex.enabled),
    );
    Value::Object(payload)
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
