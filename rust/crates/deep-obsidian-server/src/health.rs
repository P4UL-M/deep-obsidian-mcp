use deep_obsidian_types::{ResolvedServiceConfig, SecretRef};
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
    insert_secret_status(
        &mut payload,
        "embeddingApiKey",
        config.embedding.api_key_ref.as_ref(),
        diagnostics,
    );
    insert_secret_status(
        &mut payload,
        "artifactEmbeddingApiKey",
        config.artifact_embedding.api_key_ref.as_ref(),
        diagnostics,
    );
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
    insert_secret_status(
        &mut payload,
        "embeddingApiKey",
        config.embedding.api_key_ref.as_ref(),
        diagnostics,
    );
    insert_secret_status(
        &mut payload,
        "artifactEmbeddingApiKey",
        config.artifact_embedding.api_key_ref.as_ref(),
        diagnostics,
    );
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

fn insert_secret_status(
    payload: &mut Map<String, Value>,
    key: &str,
    reference: Option<&SecretRef>,
    diagnostics: &RuntimeDiagnostics,
) {
    let Some(reference) = reference else {
        return;
    };
    let kind = match reference {
        SecretRef::OsKeyring { .. } => "osKeyring",
        SecretRef::EncryptedFile { .. } => "encryptedFile",
    };
    payload.insert(
        key.to_string(),
        json!({
            "kind": kind,
            "configured": true,
            "resolved": matches!(diagnostics.status, RuntimeReadiness::Ready),
        }),
    );
}

fn insert_index_snapshot(payload: &mut Map<String, Value>, snapshot: &RuntimeIndexSnapshot) {
    let index = snapshot.index.as_ref();
    payload.insert(
        "markdownFileCount".to_string(),
        json!(index.file_snapshots.len()),
    );
    payload.insert(
        "artifactFileCount".to_string(),
        json!(index.artifact_snapshots.len()),
    );
    payload.insert("artifactCount".to_string(), json!(index.artifact_count));
    payload.insert(
        "vectorizedArtifactCount".to_string(),
        json!(index.vectorized_artifact_count),
    );
    payload.insert(
        "skippedArtifactCount".to_string(),
        json!(index.skipped_artifact_count),
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
    insert_optional_value(
        payload,
        "artifactEmbeddingProvider",
        &index.artifact_embedding_provider,
    );
    insert_optional_value(
        payload,
        "artifactEmbeddingModel",
        &index.artifact_embedding_model,
    );
    insert_optional_value(
        payload,
        "artifactEmbeddingDimensions",
        &index.artifact_embedding_dimensions,
    );
    insert_optional_value(
        payload,
        "artifactEmbeddingError",
        &index.artifact_embedding_error,
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

// ---------------------------------------------------------------------------
// Additive multi-mount detail
// ---------------------------------------------------------------------------

/// One mount's index state, as the payload builders need it.
///
/// Joins what the router knows (id, prefix, backend kind) with what that mount's
/// [`RuntimeState`](crate::runtime::RuntimeState) knows (readiness, snapshot,
/// failure). Built by [`AppState::mount_index_summaries`](crate::mcp::AppState::mount_index_summaries).
#[derive(Debug, Clone)]
pub struct MountIndexSummary {
    pub id: String,
    pub mount_at: String,
    pub backend_kind: &'static str,
    /// `None` when the mount has NO LOCAL INDEX BY DESIGN — an Algolia-backed corpus,
    /// whose remote index is the corpus itself.
    ///
    /// Distinct from `Some(diagnostics)` carrying a failure, and the distinction is the
    /// whole point: a mount with no local index is not broken, so it must not report an
    /// index status of `degraded` nor appear in `degradedMounts`. Reporting it as
    /// degraded would make `/readyz` permanently red for a correctly configured mount
    /// and destroy the signal for the mounts that genuinely are failing.
    pub diagnostics: Option<RuntimeDiagnostics>,
}

impl MountIndexSummary {
    /// True when this mount is serving what it advertises.
    ///
    /// For an indexed mount that means its index has a snapshot. For a mount with no
    /// local index there is nothing to be ready for, and the honest answer is that the
    /// mount is fine — an index it never had cannot be missing.
    fn ready(&self) -> bool {
        match &self.diagnostics {
            Some(diagnostics) => diagnostics.snapshot.is_some(),
            None => true,
        }
    }

    /// The `indexStatus` string: the runtime's own, or `"none"`.
    fn index_status(&self) -> &'static str {
        match &self.diagnostics {
            Some(diagnostics) => diagnostics.status.as_str(),
            None => "none",
        }
    }
}

/// What `mounts[].indexNote` says for a mount with no local index.
///
/// A note rather than a `lastError`: nothing went wrong, and putting it in an error
/// field would make every `vault_info` on such a config look like a report of a
/// problem.
const NO_LOCAL_INDEX_NOTE: &str = "this mount has no local search index by design: its \
backend serves its own content, so index-backed recall tools (hybrid_search, related_notes, \
graph_traverse, search_artifacts) cannot be scoped to it";

/// Whole-vault counts that are meaningful as a SUM over mounts.
///
/// Each is a count of things in the index, so adding them across mounts answers
/// the same question for the whole logical vault. Only keys already present in a
/// payload are replaced, so no payload grows a field it never had.
const AGGREGATE_COUNT_KEYS: [&str; 7] = [
    "markdownFileCount",
    "artifactFileCount",
    "artifactCount",
    "vectorizedArtifactCount",
    "skippedArtifactCount",
    "noteCount",
    "chunkCount",
];

fn mount_count(summary: &MountIndexSummary, key: &str) -> u64 {
    let Some(snapshot) = summary
        .diagnostics
        .as_ref()
        .and_then(|diagnostics| diagnostics.snapshot.as_ref())
    else {
        return 0;
    };
    let index = snapshot.index.as_ref();
    match key {
        "markdownFileCount" => index.file_snapshots.len() as u64,
        "artifactFileCount" => index.artifact_snapshots.len() as u64,
        "artifactCount" => index.artifact_count as u64,
        "vectorizedArtifactCount" => index.vectorized_artifact_count as u64,
        "skippedArtifactCount" => index.skipped_artifact_count as u64,
        "noteCount" => index.note_count as u64,
        "chunkCount" => index.chunk_count as u64,
        _ => 0,
    }
}

/// Add per-mount index detail to a health, readiness or vault-overview payload,
/// and make its whole-vault counts cover every mount.
///
/// **Additive, and multi-mount only.** A single-mount config returns immediately,
/// so every payload it produces — and every golden that freezes one — is
/// untouched. This is the same discipline as `vault_info.mounts`: the multi-mount
/// shape is a superset, never a reshaping.
///
/// What it does on a multi-mount config:
///
/// * replaces each already-present count in [`AGGREGATE_COUNT_KEYS`] with the sum
///   over mounts, so `markdownFileCount` means the whole logical vault rather than
///   the root mount's share of it;
/// * adds `mounts`, one entry per mount, carrying that mount's own index status,
///   counts, timestamp and failure;
/// * adds `degradedMounts` when any mount's index failed. That is what makes the
///   aggregate `status: "degraded"` actionable: the top-level wording is frozen and
///   says nothing about mounts, so the mount is NAMED here instead of another
///   mount's message being laundered into `lastError`.
pub fn insert_mount_index_detail(payload: &mut Value, summaries: &[MountIndexSummary]) {
    if summaries.len() < 2 {
        return;
    }
    let Some(object) = payload.as_object_mut() else {
        return;
    };

    for key in AGGREGATE_COUNT_KEYS {
        if object.contains_key(key) {
            let total: u64 = summaries
                .iter()
                .map(|summary| mount_count(summary, key))
                .sum();
            object.insert(key.to_string(), json!(total));
        }
    }

    let mounts = summaries
        .iter()
        .map(|summary| {
            let mut entry = Map::from_iter([
                ("id".to_string(), json!(summary.id)),
                ("mountAt".to_string(), json!(summary.mount_at)),
                ("backendKind".to_string(), json!(summary.backend_kind)),
                ("indexStatus".to_string(), json!(summary.index_status())),
                ("ready".to_string(), json!(summary.ready())),
                // Stated explicitly rather than left to be inferred from
                // `indexStatus: "none"`: a client deciding whether to send a scoped
                // recall call needs a boolean, not a string it has to know the
                // vocabulary of.
                (
                    "localIndex".to_string(),
                    json!(summary.diagnostics.is_some()),
                ),
            ]);
            let Some(diagnostics) = &summary.diagnostics else {
                entry.insert("indexNote".to_string(), json!(NO_LOCAL_INDEX_NOTE));
                return Value::Object(entry);
            };
            if let Some(snapshot) = &diagnostics.snapshot {
                let index = snapshot.index.as_ref();
                entry.insert(
                    "markdownFileCount".to_string(),
                    json!(index.file_snapshots.len()),
                );
                entry.insert("noteCount".to_string(), json!(index.note_count));
                entry.insert("chunkCount".to_string(), json!(index.chunk_count));
                entry.insert("generatedAt".to_string(), json!(index.generated_at));
            }
            if let Some(error) = &diagnostics.last_error {
                entry.insert(
                    "lastError".to_string(),
                    json!({
                        "reason": error.reason,
                        "message": error.message,
                        "finishedAtUnixMs": error.finished_at_unix_ms,
                    }),
                );
            }
            Value::Object(entry)
        })
        .collect::<Vec<_>>();
    object.insert("mounts".to_string(), json!(mounts));

    let degraded = summaries
        .iter()
        .filter(|summary| !summary.ready())
        .map(|summary| json!(summary.id))
        .collect::<Vec<_>>();
    if !degraded.is_empty() {
        object.insert("degradedMounts".to_string(), json!(degraded));
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
    insert_optional_value(
        &mut payload,
        "artifactEmbeddingProvider",
        &index.artifact_embedding_provider,
    );
    insert_optional_value(
        &mut payload,
        "artifactEmbeddingModel",
        &index.artifact_embedding_model,
    );
    insert_optional_value(
        &mut payload,
        "artifactEmbeddingDimensions",
        &index.artifact_embedding_dimensions,
    );
    insert_optional_value(
        &mut payload,
        "artifactEmbeddingError",
        &index.artifact_embedding_error,
    );
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
            federated_rerank: true,
            vault_path: PathBuf::from("/tmp/deep-obsidian-test-vault"),
            index_dir: PathBuf::from("/tmp/deep-obsidian-test-index"),
            mounts: Vec::new(),
            experimental: Default::default(),
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
            artifact_embedding: EmbeddingConfig::default(),
            auth: deep_obsidian_types::AuthConfig::default(),
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
