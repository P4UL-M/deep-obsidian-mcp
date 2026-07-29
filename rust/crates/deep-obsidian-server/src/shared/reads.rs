//! Hydrating reads over a shared mount (design §4.1, §7).
//!
//! `read_file` fetches the head note record (one small request — also the
//! freshness check), then serves the body from cache or reassembles it from
//! chunk records. Section chunks tile exactly; the line-based fallback for
//! heading-less notes overlaps by 12 lines, so reassembly walks sorted chunks
//! and appends only lines past the previous chunk's end.

use super::versioning::fetch_head;
use super::{Result, SharedError, SharedMountRuntime};
use deep_obsidian_algolia::records::NoteRecord;
use deep_obsidian_algolia::SearchRequest;
use serde_json::Value;

pub struct HydratedNote {
    pub content: String,
    pub note: NoteRecord,
}

/// Reassembles a note body from its chunk records, de-duplicating overlap by
/// line range. Chunks must all belong to one (note, version).
pub fn reassemble_chunks(mut chunks: Vec<(usize, usize, String)>) -> String {
    chunks.sort_by_key(|(start_line, _, _)| *start_line);
    let mut lines: Vec<String> = Vec::new();
    let mut covered_through = 0usize; // last 1-based line already emitted
    for (start_line, end_line, text) in chunks {
        let chunk_lines: Vec<&str> = text.split('\n').collect();
        // Skip lines the previous chunk already emitted (overlap dedup).
        let skip = covered_through.saturating_sub(start_line - 1);
        for (offset, line) in chunk_lines.iter().enumerate().skip(skip) {
            let line_number = start_line + offset;
            if line_number > covered_through {
                lines.push((*line).to_string());
                covered_through = line_number;
            }
        }
        let _ = end_line;
    }
    lines.join("\n")
}

/// Fetches all chunk records for one (note, version) — distinct disabled so
/// every chunk comes back, not one per path.
pub async fn fetch_version_chunks(
    mount: &SharedMountRuntime,
    index: &str,
    remote_path: &str,
    version_id: &str,
) -> Result<Vec<(usize, usize, String)>> {
    let response = mount
        .client
        .search(
            index,
            &SearchRequest {
                query: String::new(),
                filters: Some(format!(
                    "recordType:chunk AND noteId:\"{remote_path}\" AND versionId:\"{version_id}\""
                )),
                hits_per_page: Some(1000),
                distinct: Some(false),
                ..SearchRequest::default()
            },
        )
        .await?;
    Ok(response
        .hits
        .iter()
        .filter_map(|hit| {
            Some((
                hit.get("startLine")?.as_u64()? as usize,
                hit.get("endLine")?.as_u64()? as usize,
                hit.get("text")?.as_str()?.to_string(),
            ))
        })
        .collect())
}

/// Hydrates a note: head lookup (freshness check), cache hit or chunk
/// reassembly, cache fill.
pub async fn read_note(mount: &SharedMountRuntime, remote_path: &str) -> Result<HydratedNote> {
    let note = fetch_head(mount, remote_path)
        .await?
        .ok_or_else(|| SharedError::NoteNotFound(mount.mounted_path(remote_path)))?;
    if let Some(content) = mount.cache.get(remote_path, &note.version_id) {
        return Ok(HydratedNote { content, note });
    }
    let chunks =
        fetch_version_chunks(mount, mount.index(), remote_path, &note.version_id).await?;
    if chunks.is_empty() && note.chunk_count > 0 {
        return Err(SharedError::Config(format!(
            "note {remote_path} head {} has no chunk records (mid-cutover or corrupt)",
            note.version_id
        )));
    }
    let content = reassemble_chunks(chunks);
    mount
        .cache
        .put(remote_path, &note.version_id, &note.content_hash, &content);
    Ok(HydratedNote { content, note })
}

pub struct RemoteChildEntry {
    pub name: String,
    pub path: String, // mounted (vault-visible) path
    pub is_dir: bool,
    pub size_bytes: Option<u64>,
}

/// Lists a directory on the mount: subfolders via folder-level facet values,
/// files via note records with `dir` equal to the remote dir.
pub async fn list_children(
    mount: &SharedMountRuntime,
    remote_dir: &str,
) -> Result<Vec<RemoteChildEntry>> {
    let remote_dir = remote_dir.trim_matches('/');
    let mut entries: Vec<RemoteChildEntry> = Vec::new();

    // Subfolders: facet level = number of segments in remote_dir.
    let depth = if remote_dir.is_empty() {
        0
    } else {
        remote_dir.split('/').count()
    };
    let facet = format!("folders.lvl{depth}");
    let filters = if remote_dir.is_empty() {
        "recordType:note".to_string()
    } else {
        format!(
            "recordType:note AND folders.lvl{}:\"{remote_dir}\"",
            depth - 1
        )
    };
    let facet_hits = mount
        .client
        .search_facet_values(mount.index(), &facet, "", Some(&filters), 1000)
        .await?;
    for hit in facet_hits {
        let name = hit
            .value
            .rsplit('/')
            .next()
            .unwrap_or(hit.value.as_str())
            .to_string();
        entries.push(RemoteChildEntry {
            path: mount.mounted_path(&hit.value),
            name,
            is_dir: true,
            size_bytes: None,
        });
    }

    // Files directly in this dir.
    let response = mount
        .client
        .search(
            mount.index(),
            &SearchRequest {
                query: String::new(),
                filters: Some(format!("recordType:note AND dir:\"{remote_dir}\"")),
                hits_per_page: Some(1000),
                distinct: Some(false),
                ..SearchRequest::default()
            },
        )
        .await?;
    for hit in response.hits {
        let Some(path) = hit.get("path").and_then(Value::as_str) else {
            continue;
        };
        entries.push(RemoteChildEntry {
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            path: mount.mounted_path(path),
            is_dir: false,
            size_bytes: hit.get("sizeBytes").and_then(Value::as_u64),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

/// All folder paths on the mount up to `max_depth` levels, via facet values.
pub async fn list_folders(mount: &SharedMountRuntime, max_depth: usize) -> Result<Vec<String>> {
    let mut folders = Vec::new();
    for level in 0..max_depth.clamp(1, 3) {
        let hits = mount
            .client
            .search_facet_values(
                mount.index(),
                &format!("folders.lvl{level}"),
                "",
                Some("recordType:note"),
                1000,
            )
            .await?;
        for hit in hits {
            folders.push(mount.mounted_path(&hit.value));
        }
    }
    folders.sort();
    folders.dedup();
    Ok(folders)
}

/// Path-restricted search for `find_files` on the mount.
pub async fn find_paths(
    mount: &SharedMountRuntime,
    query: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let response = mount
        .client
        .search(
            mount.index(),
            &SearchRequest {
                query: query.to_string(),
                filters: Some("recordType:note".to_string()),
                restrict_searchable_attributes: vec!["path".to_string()],
                hits_per_page: Some(limit),
                distinct: Some(false),
                ..SearchRequest::default()
            },
        )
        .await?;
    Ok(response
        .hits
        .iter()
        .filter_map(|hit| hit.get("path").and_then(Value::as_str))
        .map(|path| mount.mounted_path(path))
        .collect())
}

/// Reverse-link lookup: one filter query (design §7 — the case where the
/// shared index is genuinely better than the local graph walk).
pub async fn backlinks(mount: &SharedMountRuntime, remote_path: &str) -> Result<Vec<String>> {
    let response = mount
        .client
        .search(
            mount.index(),
            &SearchRequest {
                query: String::new(),
                filters: Some(format!(
                    "recordType:note AND links:\"{remote_path}\""
                )),
                hits_per_page: Some(1000),
                distinct: Some(false),
                ..SearchRequest::default()
            },
        )
        .await?;
    Ok(response
        .hits
        .iter()
        .filter_map(|hit| hit.get("path").and_then(Value::as_str))
        .map(|path| mount.mounted_path(path))
        .collect())
}

/// Outgoing links from the head note record, kept as raw remote targets
/// (mount-prefixed when they resolve inside the mount).
pub async fn outgoing_links(
    mount: &SharedMountRuntime,
    remote_path: &str,
) -> Result<Vec<String>> {
    let note = fetch_head(mount, remote_path)
        .await?
        .ok_or_else(|| SharedError::NoteNotFound(mount.mounted_path(remote_path)))?;
    Ok(note
        .links
        .iter()
        .map(|target| mount.mounted_path(target))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassemble_exact_tiling_round_trips() {
        let source = "# T\n\n## A\nalpha\n\n## B\nbeta\n";
        let lines: Vec<&str> = source.split('\n').collect();
        // Simulated exact tiling: [1..=2], [3..=5], [6..=8].
        let chunks = vec![
            (1usize, 2usize, lines[0..2].join("\n")),
            (3, 5, lines[2..5].join("\n")),
            (6, 8, lines[5..8].join("\n")),
        ];
        assert_eq!(reassemble_chunks(chunks), source);
    }

    #[test]
    fn reassemble_dedups_overlapping_fallback_chunks() {
        // Lines 1..=6 with a 2-line overlap between chunks.
        let all: Vec<String> = (1..=6).map(|n| format!("line{n}")).collect();
        let chunks = vec![
            (1usize, 4usize, all[0..4].join("\n")),
            (3, 6, all[2..6].join("\n")), // overlaps lines 3-4
        ];
        assert_eq!(reassemble_chunks(chunks), all.join("\n"));
    }

    #[test]
    fn reassemble_handles_out_of_order_chunks() {
        let all: Vec<String> = (1..=4).map(|n| format!("l{n}")).collect();
        let chunks = vec![
            (3usize, 4usize, all[2..4].join("\n")),
            (1, 2, all[0..2].join("\n")),
        ];
        assert_eq!(reassemble_chunks(chunks), all.join("\n"));
    }
}

pub struct DumpReport {
    pub notes: usize,
    pub bytes: u64,
    pub hash_mismatches: Vec<String>,
}

/// Materializes every live note of the mount's index (head versions) into
/// `target_dir` — the backup / exit strategy for model C, where the index is
/// the only copy of the shared wiki. Paths come from remote records, so each
/// one is revalidated against the target directory (no `..` escape). A
/// `deep-obsidian-dump.json` manifest records index, app, count, and time.
pub async fn dump_all(
    mount: &SharedMountRuntime,
    target_dir: &std::path::Path,
) -> Result<DumpReport> {
    std::fs::create_dir_all(target_dir)?;
    let records = mount
        .client
        .browse_all(mount.index(), Some("recordType:note"))
        .await?;
    let mut report = DumpReport {
        notes: 0,
        bytes: 0,
        hash_mismatches: Vec::new(),
    };
    for record in records {
        let (Some(path), Some(version_id)) = (
            record.get("path").and_then(Value::as_str),
            record.get("versionId").and_then(Value::as_str),
        ) else {
            continue;
        };
        if record.get("deleted").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let chunks = fetch_version_chunks(mount, mount.index(), path, version_id).await?;
        let content = reassemble_chunks(chunks);
        if let Some(expected) = record.get("contentHash").and_then(Value::as_str) {
            if crate::tools::content_hash(content.as_bytes()) != expected {
                report.hash_mismatches.push(path.to_string());
            }
        }
        let absolute = deep_obsidian_core::vault::ensure_inside_vault(target_dir, path)
            .map_err(|error| SharedError::Config(format!("unsafe dump path {path}: {error}")))?;
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&absolute, &content)?;
        report.notes += 1;
        report.bytes += content.len() as u64;
    }
    let manifest = serde_json::json!({
        "indexName": mount.index(),
        "appId": mount.config.app_id,
        "noteCount": report.notes,
        "dumpedAtMs": super::now_ms(),
    });
    std::fs::write(
        target_dir.join("deep-obsidian-dump.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )?;
    Ok(report)
}
