//! One-shot seeding of local notes into the shared index (`share seed`).
//!
//! Model C: the shared wiki LIVES in the index and is authored through the
//! mount. Seeding is the only local->index flow — an explicit, one-shot import
//! with no standing export rule. It only creates or updates; it never
//! reconciles deletions (removal is the explicit `share retract`, which also
//! purges history — the deliberate exception to non-destruction, design §8).

use super::records_build::parse_frontmatter_fields;
use super::versioning::push_note_version;
use super::{Result, SharedError, SharedMountRuntime};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedAction {
    Create,
    Update,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct SeedItem {
    pub path: String,
    pub action: SeedAction,
}

#[derive(Debug, Default)]
pub struct SeedPlan {
    /// True when the index holds no note records yet — triggers the explicit
    /// first-publish confirmation with the full note list.
    pub first_push: bool,
    pub items: Vec<SeedItem>,
}

impl SeedPlan {
    pub fn changed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.action != SeedAction::Unchanged)
            .count()
    }
}

#[derive(Debug, Default)]
pub struct SeedReport {
    pub seeded: usize,
    pub unchanged: usize,
}

/// The local file set covered by the given prefixes, with `share: false`
/// notes dropped. Returns (path, content) pairs plus the full vault file list
/// (for wiki-link resolution).
fn collect_seed_set(
    vault_path: &Path,
    prefixes: &[String],
) -> Result<(Vec<(String, String)>, Vec<String>)> {
    let all_files = deep_obsidian_core::vault::list_markdown_files(vault_path)
        .map_err(|error| SharedError::Config(error.to_string()))?;
    let mut set = Vec::new();
    for path in &all_files {
        if !prefixes.iter().any(|prefix| path.starts_with(prefix)) {
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

/// Remote note records: path -> contentHash.
async fn remote_hash_map(
    mount: &SharedMountRuntime,
) -> Result<std::collections::HashMap<String, String>> {
    let records = super::empty_if_missing_index(
        mount
            .client
            .browse_all(mount.index(), Some(super::reads::LIVE_NOTES))
            .await,
        Vec::new(),
    )?;
    Ok(records
        .iter()
        .filter_map(|record| {
            Some((
                record.get("path")?.as_str()?.to_string(),
                record
                    .get("contentHash")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect())
}

/// Computes what a seed would import, without writing anything.
pub async fn plan_seed(
    vault_path: &Path,
    mount: &SharedMountRuntime,
    prefixes: &[String],
) -> Result<SeedPlan> {
    let (local_set, _all_files) = collect_seed_set(vault_path, prefixes)?;
    let remote = remote_hash_map(mount).await?;
    let mut plan = SeedPlan {
        first_push: remote.is_empty(),
        ..SeedPlan::default()
    };
    for (path, content) in &local_set {
        let action = match remote.get(path) {
            None => SeedAction::Create,
            Some(remote_hash)
                if *remote_hash == crate::tools::content_hash(content.as_bytes()) =>
            {
                SeedAction::Unchanged
            }
            Some(_) => SeedAction::Update,
        };
        plan.items.push(SeedItem {
            path: path.clone(),
            action,
        });
    }
    Ok(plan)
}

/// Applies a seed plan: provisions index settings on first push, then writes
/// each changed note as a new version (base = current head, so a re-seed over
/// remote edits supersedes them into history, never destroys them).
pub async fn apply_seed(
    vault_path: &Path,
    mount: &SharedMountRuntime,
    prefixes: &[String],
    plan: &SeedPlan,
) -> Result<SeedReport> {
    let (local_set, all_files) = collect_seed_set(vault_path, prefixes)?;
    let content_by_path: std::collections::HashMap<&String, &String> =
        local_set.iter().map(|(path, content)| (path, content)).collect();

    let mut report = SeedReport::default();
    for item in &plan.items {
        match item.action {
            SeedAction::Unchanged => report.unchanged += 1,
            SeedAction::Create | SeedAction::Update => {
                let Some(content) = content_by_path.get(&item.path) else {
                    continue; // deleted between plan and apply
                };
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
                report.seeded += 1;
            }
        }
    }
    Ok(report)
}

/// After a verified seed, deletes the local copies of seeded notes so the
/// index holds the only copy (`share seed --move`). Per-file guard: a file is
/// removed ONLY when a fresh plan classifies it as `Unchanged`, i.e. the
/// remote head hash equals the local content hash — anything that drifted
/// between push and deletion is skipped and reported, never dropped.
/// Empty parent directories are pruned best-effort.
pub async fn remove_seeded_local_files(
    vault_path: &Path,
    mount: &SharedMountRuntime,
    prefixes: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let plan = plan_seed(vault_path, mount, prefixes).await?;
    let mut deleted = Vec::new();
    let mut skipped = Vec::new();
    for item in &plan.items {
        if item.action != SeedAction::Unchanged {
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
