//! Export (`share push`): publish local prefixes into the shared index
//! (design §9 — explicit publication and active retraction).
//!
//! A push pass reconciles the FULL intended set against the index, not just a
//! file diff: notes newly excluded (deleted, `share: false`, config narrowed)
//! are retracted with a history purge. Retraction is conservative in the
//! multi-writer case: a remote note whose last version was written by someone
//! else is never deleted by our push — it is reported instead.

use super::records_build::parse_frontmatter_fields;
use super::versioning::{push_note_version, retract_note};
use super::{Result, SharedError, SharedMountRuntime};
use deep_obsidian_algolia::records::{history_index_settings, main_index_settings};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushAction {
    Create,
    Update,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct PushItem {
    pub path: String,
    pub action: PushAction,
}

#[derive(Debug, Default)]
pub struct PushPlan {
    pub first_push: bool,
    pub items: Vec<PushItem>,
    /// Remote paths to retract (ours, no longer in the export set).
    pub retract: Vec<String>,
    /// Remote paths absent locally but last written by another participant —
    /// left alone, surfaced for visibility.
    pub foreign_orphans: Vec<String>,
}

impl PushPlan {
    pub fn changed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.action != PushAction::Unchanged)
            .count()
    }
}

#[derive(Debug, Default)]
pub struct PushReport {
    pub pushed: usize,
    pub unchanged: usize,
    pub retracted: usize,
    pub foreign_orphans: Vec<String>,
}

/// The local file set covered by the mount's export rule, with `share: false`
/// notes dropped. Returns (path, content) pairs plus the full vault file list
/// (for link resolution).
fn collect_export_set(
    vault_path: &Path,
    mount: &SharedMountRuntime,
) -> Result<(Vec<(String, String)>, Vec<String>)> {
    let export = mount
        .config
        .export
        .clone()
        .ok_or_else(|| SharedError::Config("mount has no export rule".to_string()))?;
    let all_files = deep_obsidian_core::vault::list_markdown_files(vault_path)
        .map_err(|error| SharedError::Config(error.to_string()))?;
    let mut set = Vec::new();
    for path in &all_files {
        let included = export.prefixes.iter().any(|prefix| path.starts_with(prefix));
        let excluded = export.exclude.iter().any(|prefix| path.starts_with(prefix));
        if !included || excluded {
            continue;
        }
        let content = deep_obsidian_core::vault::read_text_file(vault_path, path)
            .map_err(|error| SharedError::Config(error.to_string()))?
            .text;
        if parse_frontmatter_fields(&content).share == Some(false) {
            continue;
        }
        set.push((path.clone(), content));
    }
    Ok((set, all_files))
}

/// Remote note records: path -> (contentHash, participantId).
async fn remote_note_map(
    mount: &SharedMountRuntime,
) -> Result<std::collections::HashMap<String, (String, String)>> {
    let records = mount
        .client
        .browse_all(mount.index(), Some("recordType:note"))
        .await?;
    Ok(records
        .iter()
        .filter_map(|record| {
            Some((
                record.get("path")?.as_str()?.to_string(),
                (
                    record
                        .get("contentHash")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    record
                        .get("participantId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
            ))
        })
        .collect())
}

/// Computes what a push would do, without writing anything.
pub async fn plan_push(vault_path: &Path, mount: &SharedMountRuntime) -> Result<PushPlan> {
    let (local_set, _all_files) = collect_export_set(vault_path, mount)?;
    let remote = remote_note_map(mount).await?;
    let mut plan = PushPlan {
        first_push: remote.is_empty(),
        ..PushPlan::default()
    };

    let export = mount.config.export.clone().unwrap_or_default();
    let participant = mount.participant_id();

    for (path, content) in &local_set {
        let action = match remote.get(path) {
            None => PushAction::Create,
            Some((remote_hash, _))
                if *remote_hash == crate::tools::content_hash(content.as_bytes()) =>
            {
                PushAction::Unchanged
            }
            Some(_) => PushAction::Update,
        };
        plan.items.push(PushItem {
            path: path.clone(),
            action,
        });
    }

    // Reconciliation: remote notes under OUR export prefixes that are absent
    // from the local set. Ours -> retract; someone else's -> report only.
    let local_paths: std::collections::HashSet<&String> =
        local_set.iter().map(|(path, _)| path).collect();
    for (remote_path, (_, remote_participant)) in &remote {
        let under_export = export
            .prefixes
            .iter()
            .any(|prefix| remote_path.starts_with(prefix));
        if !under_export || local_paths.contains(remote_path) {
            continue;
        }
        if *remote_participant == participant {
            plan.retract.push(remote_path.clone());
        } else {
            plan.foreign_orphans.push(remote_path.clone());
        }
    }
    plan.retract.sort();
    plan.foreign_orphans.sort();
    Ok(plan)
}

/// Applies a push plan: provisions settings on first push, writes changed
/// notes as new versions, retracts de-selected notes (with history purge).
pub async fn apply_push(
    vault_path: &Path,
    mount: &SharedMountRuntime,
    plan: &PushPlan,
) -> Result<PushReport> {
    if plan.first_push {
        mount
            .client
            .set_settings(mount.index(), main_index_settings())
            .await?;
        mount
            .client
            .set_settings(&mount.history_index, history_index_settings())
            .await?;
    }

    let (local_set, all_files) = collect_export_set(vault_path, mount)?;
    let content_by_path: std::collections::HashMap<&String, &String> =
        local_set.iter().map(|(path, content)| (path, content)).collect();

    let mut report = PushReport {
        foreign_orphans: plan.foreign_orphans.clone(),
        ..PushReport::default()
    };
    for item in &plan.items {
        match item.action {
            PushAction::Unchanged => report.unchanged += 1,
            PushAction::Create | PushAction::Update => {
                let Some(content) = content_by_path.get(&item.path) else {
                    continue; // deleted between plan and apply; next push reconciles
                };
                // Export pushes base on the current head (no fork): the local
                // file is the exporter's source of truth.
                let head = super::versioning::fetch_head(mount, &item.path).await?;
                push_note_version(
                    mount,
                    &item.path,
                    content,
                    &all_files,
                    head.as_ref().map(|note| note.version_id.as_str()),
                    false,
                )
                .await?;
                report.pushed += 1;
            }
        }
    }
    for path in &plan.retract {
        retract_note(mount, path).await?;
        report.retracted += 1;
    }
    Ok(report)
}

/// After a verified seed, deletes the local copies of exported notes so the
/// index holds the only copy (`share seed --move`). Per-file guard: a file is
/// removed ONLY when a fresh plan classifies it as `Unchanged`, i.e. the
/// remote head hash equals the local content hash — anything that drifted
/// between push and deletion is skipped and reported, never dropped.
/// Empty parent directories are pruned best-effort.
pub async fn remove_seeded_local_files(
    vault_path: &Path,
    mount: &SharedMountRuntime,
) -> Result<(Vec<String>, Vec<String>)> {
    let plan = plan_push(vault_path, mount).await?;
    let mut deleted = Vec::new();
    let mut skipped = Vec::new();
    for item in &plan.items {
        if item.action != PushAction::Unchanged {
            skipped.push(item.path.clone());
            continue;
        }
        let absolute = deep_obsidian_core::vault::ensure_inside_vault(vault_path, &item.path)
            .map_err(|error| SharedError::Config(error.to_string()))?;
        match std::fs::remove_file(&absolute) {
            Ok(()) => {
                deleted.push(item.path.clone());
                // Prune now-empty parents up to the vault root (stops at the
                // first non-empty directory).
                let mut parent = absolute.parent();
                while let Some(dir) = parent {
                    if !dir.starts_with(vault_path) || dir == vault_path {
                        break;
                    }
                    if std::fs::remove_dir(dir).is_err() {
                        break;
                    }
                    parent = dir.parent();
                }
            }
            Err(error) => {
                skipped.push(format!("{} ({error})", item.path));
            }
        }
    }
    Ok((deleted, skipped))
}
