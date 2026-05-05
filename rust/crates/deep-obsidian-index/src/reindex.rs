use std::path::Path;

use crate::index::{collect_snapshots, same_snapshots, Result, SearchIndex, SemanticBackend};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexOptions {
    pub debounce_ms: u64,
    pub interval_ms: u64,
}

impl Default for ReindexOptions {
    fn default() -> Self {
        Self {
            debounce_ms: 1500,
            interval_ms: 30000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexPlan {
    pub should_rebuild: bool,
    pub reason: String,
    pub snapshot_count: usize,
}

pub fn plan_reindex(vault_path: &Path, current_index: Option<&SearchIndex>) -> Result<ReindexPlan> {
    let snapshots = collect_snapshots(vault_path)?;
    let should_rebuild = match current_index {
        Some(index) => {
            !same_snapshots(&index.file_snapshots, &snapshots)
                || index.semantic_backend != SemanticBackend::Sparse
        }
        None => true,
    };

    let reason = match current_index {
        None => "no existing index".to_string(),
        Some(index) if index.semantic_backend != SemanticBackend::Sparse => {
            "semantic backend changed".to_string()
        }
        Some(index) if !same_snapshots(&index.file_snapshots, &snapshots) => {
            "file snapshots changed".to_string()
        }
        Some(_) => "index is current".to_string(),
    };

    Ok(ReindexPlan {
        should_rebuild,
        reason,
        snapshot_count: snapshots.len(),
    })
}

pub fn needs_reindex(vault_path: &Path, current_index: Option<&SearchIndex>) -> Result<bool> {
    Ok(plan_reindex(vault_path, current_index)?.should_rebuild)
}
