//! Tool-layer glue between `call_tool` handlers and shared mounts.
//!
//! Each helper either answers for a mounted path or returns `None` so the
//! caller falls through to the local implementation. Remote failures on
//! *additive* surfaces (federated search, grep scope) degrade into explicit
//! per-mount error entries rather than failing the whole tool.

use crate::mcp::AppState;
use crate::shared::{self, reads, retrieval, versioning, SharedError, SharedMountRuntime};
use serde_json::{json, Map, Value};

/// Reads a note's text through the router. `Ok(None)` = the note does not
/// exist (mounted or local). The second tuple field is the shared version id
/// (`None` for local paths) — writers pass it back as their base version.
pub async fn routed_read(
    state: &AppState,
    path: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    if let Some((mount, remote)) = shared::route(&state.mounts, path) {
        match reads::read_note(mount, remote).await {
            Ok(hydrated) => Ok(Some((hydrated.content, Some(hydrated.note.version_id)))),
            Err(SharedError::NoteNotFound(_)) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    } else {
        Ok(
            deep_obsidian_core::vault::read_text_file(&state.config.vault_path, path)
                .ok()
                .map(|file| (file.text, None)),
        )
    }
}

/// Writes note text through the router. Returns `Ok(None)` when the path is
/// local (caller performs the disk write); `Ok(Some(fields))` when the write
/// went to a mount — the fields merge into the tool payload.
pub async fn routed_write(
    state: &AppState,
    path: &str,
    content: &str,
    base_version: Option<&str>,
    resolve_divergence: bool,
    dry_run: bool,
) -> Result<Option<Map<String, Value>>, String> {
    let Some((mount, remote)) = shared::route(&state.mounts, path) else {
        return Ok(None);
    };
    if !mount.config.writable {
        return Err(format!(
            "shared mount {} is read-only (writable: false)",
            mount.mount_at()
        ));
    }
    let mut fields = Map::new();
    fields.insert("shared".to_string(), json!(true));
    fields.insert("indexName".to_string(), json!(mount.index()));
    if dry_run {
        return Ok(Some(fields));
    }
    let known_files = known_remote_files(mount).await;
    let outcome = versioning::push_note_version(
        mount,
        remote,
        content,
        &known_files,
        base_version,
        resolve_divergence,
    )
    .await
    .map_err(|error| error.to_string())?;
    // Keep the local cache coherent immediately (never a write buffer: the
    // push already succeeded upstream).
    mount.cache.put(
        remote,
        &outcome.version_id,
        &crate::tools::content_hash(content.as_bytes()),
        content,
    );
    fields.insert("versionId".to_string(), json!(outcome.version_id));
    if let Some(parent) = &outcome.parent_version_id {
        fields.insert("parentVersionId".to_string(), json!(parent));
    }
    if let Some(forked) = &outcome.forked_from {
        fields.insert("forkedFrom".to_string(), json!(forked));
        fields.insert(
            "forkNote".to_string(),
            json!(
                "This write was based on a superseded version; the overtaken head is \
preserved in history. Use resolve_divergence to reconcile."
            ),
        );
    }
    fields.insert("hasDivergence".to_string(), json!(outcome.has_divergence));
    Ok(Some(fields))
}

/// Remote note paths (for link resolution of consumer-side writes) — one
/// facet query, capped at 1000.
async fn known_remote_files(mount: &SharedMountRuntime) -> Vec<String> {
    mount
        .client
        .search_facet_values(mount.index(), "path", "", Some("recordType:note"), 1000)
        .await
        .map(|hits| hits.into_iter().map(|hit| hit.value).collect())
        .unwrap_or_default()
}

/// `list_children` on a mounted directory.
pub async fn list_children_payload(
    state: &AppState,
    path: &str,
    folders_only: bool,
) -> Result<Option<Value>, String> {
    let Some((mount, remote_dir)) = shared::route(&state.mounts, path) else {
        return Ok(None);
    };
    let entries = reads::list_children(mount, remote_dir)
        .await
        .map_err(|error| error.to_string())?;
    let entries_json: Vec<Value> = entries
        .iter()
        .filter(|entry| entry.is_dir || !folders_only)
        .map(|entry| {
            json!({
                "name": entry.name,
                "path": entry.path,
                "kind": if entry.is_dir { "directory" } else { "file" },
                "isMarkdown": !entry.is_dir,
                "sizeBytes": entry.size_bytes,
                "shared": true,
            })
        })
        .collect();
    Ok(Some(json!({
        "path": path,
        "shared": true,
        "indexName": mount.index(),
        "count": entries_json.len(),
        "entries": entries_json,
    })))
}

/// Synthetic directory entries for mount roots whose parent is `dir` (so
/// mounted namespaces are discoverable while walking the local tree).
pub fn mount_root_entries(state: &AppState, dir: &str) -> Vec<Value> {
    let normalized = dir.trim_matches('/');
    let mut entries = Vec::new();
    for mount in state.mounts.iter() {
        let mount_at = mount.mount_at().trim_end_matches('/');
        let Some(rest) = (if normalized.is_empty() {
            Some(mount_at)
        } else {
            mount_at
                .strip_prefix(normalized)
                .and_then(|rest| rest.strip_prefix('/'))
        }) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let next_segment = rest.split('/').next().unwrap_or(rest);
        let path = if normalized.is_empty() {
            next_segment.to_string()
        } else {
            format!("{normalized}/{next_segment}")
        };
        entries.push(json!({
            "name": next_segment,
            "path": path,
            "kind": "directory",
            "isMarkdown": false,
            "shared": true,
        }));
    }
    entries
}

/// Remote path matches appended to `find_files` results.
pub async fn find_files_remote(state: &AppState, query: &str, limit: usize) -> Vec<Value> {
    let mut matches = Vec::new();
    for mount in state.mounts.iter() {
        match reads::find_paths(mount, query, limit).await {
            Ok(paths) => {
                for path in paths {
                    matches.push(json!({
                        "path": path,
                        "shared": true,
                        "indexName": mount.index(),
                    }));
                }
            }
            Err(error) => {
                matches.push(json!({
                    "shared": true,
                    "indexName": mount.index(),
                    "error": error.to_string(),
                }));
            }
        }
    }
    matches
}

/// Candidate-bounded remote grep (design §4.3). Returns (matches, scope):
/// scope always carries one entry per mount stating exhaustive:false and the
/// anchor used, or the refusal reason for anchor-less patterns.
pub async fn grep_remote(
    state: &AppState,
    pattern: &str,
    case_sensitive: bool,
    limit: usize,
) -> (Vec<Value>, Vec<Value>) {
    let mut matches = Vec::new();
    let mut scope = Vec::new();
    if state.mounts.is_empty() {
        return (matches, scope);
    }
    let anchor = retrieval::extract_literal_anchor(pattern);
    let regex = regex::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build();
    for mount in state.mounts.iter() {
        let Some(anchor) = anchor.as_deref() else {
            scope.push(json!({
                "mountAt": mount.mount_at(),
                "searched": false,
                "reason": "pattern has no literal anchor of >= 3 chars; shared content cannot \
be prefiltered lexically and silent under-reporting is refused",
            }));
            continue;
        };
        let Ok(regex) = regex.as_ref() else {
            scope.push(json!({
                "mountAt": mount.mount_at(),
                "searched": false,
                "reason": "pattern is not a valid Rust regex",
            }));
            continue;
        };
        match retrieval::search_mount_with_distinct(mount, anchor, 200, Some(false)).await {
            Ok(hits) => {
                let candidate_count = hits.len();
                let mut mount_matches = 0usize;
                for hit in hits {
                    if matches.len() >= limit {
                        break;
                    }
                    for (offset, line) in hit.text.split('\n').enumerate() {
                        if regex.is_match(line) {
                            matches.push(json!({
                                "path": hit.mounted_path,
                                "line": hit.start_line + offset,
                                "text": line,
                                "shared": true,
                            }));
                            mount_matches += 1;
                            if matches.len() >= limit {
                                break;
                            }
                        }
                    }
                }
                scope.push(json!({
                    "mountAt": mount.mount_at(),
                    "searched": true,
                    "exhaustive": false,
                    "anchor": anchor,
                    "candidateCount": candidate_count,
                    "matchCount": mount_matches,
                }));
            }
            Err(error) => {
                scope.push(json!({
                    "mountAt": mount.mount_at(),
                    "searched": false,
                    "reason": error.to_string(),
                }));
            }
        }
    }
    (matches, scope)
}

/// Cached-recall-stage lookup: detected once per process per mount.
async fn recall_stage(mount: &SharedMountRuntime) -> String {
    {
        let cached = mount.recall_stage.lock().expect("recall stage lock");
        if cached.as_str() != "lexical" {
            return cached.clone();
        }
    }
    let detected = retrieval::detect_recall_stage(mount).await;
    *mount.recall_stage.lock().expect("recall stage lock") = detected.clone();
    detected
}

/// Federates shared-mount results into a locally-ranked match list via
/// N-list RRF (design §8). `local` is the already-ranked local match JSON
/// list; returns (fused matches, shared metadata block).
pub async fn federate_matches(
    state: &AppState,
    query: &str,
    local: Vec<Value>,
    limit: usize,
) -> (Vec<Value>, Option<Value>) {
    if state.mounts.is_empty() {
        return (local, None);
    }
    let mut lists: Vec<(Vec<String>, f64)> = Vec::new();
    let mut by_key: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let local_keys: Vec<String> = local
        .iter()
        .enumerate()
        .map(|(position, item)| {
            let key = format!(
                "local:{}#{}",
                item.get("path").and_then(Value::as_str).unwrap_or(""),
                item.get("chunkIndex").and_then(Value::as_u64).unwrap_or(0)
            );
            by_key.insert(key.clone(), item.clone());
            let _ = position;
            key
        })
        .collect();
    lists.push((local_keys, 1.0));

    let mut mounts_meta = Vec::new();
    for mount in state.mounts.iter() {
        match retrieval::search_mount(mount, query, limit).await {
            Ok(hits) => {
                let stage = recall_stage(mount).await;
                mounts_meta.push(json!({
                    "mountAt": mount.mount_at(),
                    "indexName": mount.index(),
                    "recallStage": stage,
                    "hitCount": hits.len(),
                }));
                let keys: Vec<String> = hits
                    .iter()
                    .map(|hit| {
                        let key = format!("shared:{}#{}", hit.mounted_path, hit.chunk_index);
                        by_key.insert(
                            key.clone(),
                            json!({
                                "path": hit.mounted_path,
                                "title": hit.title,
                                "chunkIndex": hit.chunk_index,
                                "startLine": hit.start_line,
                                "endLine": hit.end_line,
                                "text": hit.text,
                                "shared": true,
                                "indexName": mount.index(),
                            }),
                        );
                        key
                    })
                    .collect();
                lists.push((keys, 1.0));
            }
            Err(error) => {
                mounts_meta.push(json!({
                    "mountAt": mount.mount_at(),
                    "indexName": mount.index(),
                    "error": error.to_string(),
                }));
            }
        }
    }

    let fused = retrieval::rrf_fuse_many(&lists, 60.0);
    let matches: Vec<Value> = fused
        .into_iter()
        .take(limit)
        .filter_map(|(key, score)| {
            let mut item = by_key.remove(&key)?;
            if let Some(object) = item.as_object_mut() {
                object.insert("score".to_string(), json!(score));
            }
            Some(item)
        })
        .collect();
    (matches, Some(json!({ "mounts": mounts_meta })))
}

/// Graph traversal on a mounted path: BFS over `links[]` (outgoing) or the
/// `links:"<path>"` reverse filter (incoming).
pub async fn traverse_remote(
    state: &AppState,
    start_path: &str,
    direction: &str,
    depth: usize,
) -> Result<Option<Value>, String> {
    let Some((mount, remote_start)) = shared::route(&state.mounts, start_path) else {
        return Ok(None);
    };
    let mut visited: Vec<String> = vec![remote_start.to_string()];
    let mut frontier: Vec<String> = vec![remote_start.to_string()];
    let mut edges = Vec::new();
    for _ in 0..depth.clamp(1, 3) {
        let mut next = Vec::new();
        for node in &frontier {
            let neighbours = match direction {
                "incoming" => reads::backlinks(mount, node)
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(|mounted| mounted.trim_start_matches(mount.mount_at()).to_string())
                    .collect::<Vec<_>>(),
                _ => match versioning::fetch_head(mount, node).await {
                    Ok(Some(note)) => note.links,
                    _ => Vec::new(),
                },
            };
            for neighbour in neighbours {
                let (from, to) = if direction == "incoming" {
                    (neighbour.clone(), node.clone())
                } else {
                    (node.clone(), neighbour.clone())
                };
                edges.push(json!({
                    "from": mount.mounted_path(&from),
                    "to": mount.mounted_path(&to),
                }));
                if !visited.contains(&neighbour) {
                    visited.push(neighbour.clone());
                    next.push(neighbour);
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    let nodes: Vec<Value> = visited
        .iter()
        .map(|node| json!({ "path": mount.mounted_path(node), "shared": true }))
        .collect();
    Ok(Some(json!({
        "start": start_path,
        "direction": direction,
        "depth": depth,
        "shared": true,
        "indexName": mount.index(),
        "nodes": nodes,
        "edges": edges,
    })))
}

/// `vault_info` block describing the configured mounts.
pub async fn vault_info_mounts(state: &AppState) -> Option<Value> {
    if state.mounts.is_empty() {
        return None;
    }
    let mut mounts = Vec::new();
    for mount in state.mounts.iter() {
        let (cache_entries, cache_bytes) = mount.cache.stats();
        mounts.push(json!({
            "mountAt": mount.mount_at(),
            "appId": mount.config.app_id,
            "indexName": mount.index(),
            "writable": mount.config.writable,
            "recallStage": recall_stage(mount).await,
            "cacheEntries": cache_entries,
            "cacheBytes": cache_bytes,
        }));
    }
    Some(json!(mounts))
}

/// `note_history` tool payload.
pub async fn note_history_payload(state: &AppState, path: &str) -> Result<Value, String> {
    let Some((mount, remote)) = shared::route(&state.mounts, path) else {
        return Err(format!("{path} is not on a shared mount"));
    };
    let head = versioning::fetch_head(mount, remote)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("note not found on shared mount: {path}"))?;
    let history = mount
        .client
        .browse_all(
            &mount.history_index,
            Some(&format!("recordType:note AND noteId:\"{remote}\"")),
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut versions: Vec<Value> = history
        .iter()
        .map(|record| {
            json!({
                "versionId": record.get("versionId"),
                "participantId": record.get("participantId"),
                "updatedAtMs": record.get("updatedAtMs"),
                "supersededBy": record.get("supersededBy"),
                "forkedFrom": record.get("forkedFrom"),
                "current": false,
            })
        })
        .collect();
    versions.push(json!({
        "versionId": head.version_id,
        "participantId": head.participant_id,
        "updatedAtMs": head.updated_at_ms,
        "parentVersionId": head.parent_version_id,
        "forkedFrom": head.forked_from,
        "current": true,
    }));
    versions.sort_by_key(|version| {
        std::cmp::Reverse(version.get("updatedAtMs").and_then(Value::as_u64).unwrap_or(0))
    });
    Ok(json!({
        "path": path,
        "hasDivergence": head.has_divergence,
        "count": versions.len(),
        "versions": versions,
    }))
}

/// `read_version` tool payload: reassembles a superseded (or the current)
/// version's text.
pub async fn read_version_payload(
    state: &AppState,
    path: &str,
    version_id: &str,
) -> Result<Value, String> {
    let Some((mount, remote)) = shared::route(&state.mounts, path) else {
        return Err(format!("{path} is not on a shared mount"));
    };
    // Try main first (current version), then history.
    for index in [mount.index(), mount.history_index.as_str()] {
        let chunks = reads::fetch_version_chunks(mount, index, remote, version_id)
            .await
            .map_err(|error| error.to_string())?;
        if !chunks.is_empty() {
            let text = reads::reassemble_chunks(chunks);
            return Ok(json!({
                "path": path,
                "versionId": version_id,
                "hash": crate::tools::content_hash(text.as_bytes()),
                "text": text,
            }));
        }
    }
    Err(format!(
        "version {version_id} of {path} not found (purged by retention, or wrong id)"
    ))
}

/// `resolve_divergence` tool payload: the current head, the overtaken head it
/// forked past, and their common ancestor — everything an agent needs for a
/// real three-way merge. The server never merges (design: a wrong automatic
/// merge produces plausible text and is nearly undetectable).
pub async fn resolve_divergence_payload(state: &AppState, path: &str) -> Result<Value, String> {
    let Some((mount, remote)) = shared::route(&state.mounts, path) else {
        return Err(format!("{path} is not on a shared mount"));
    };
    let head = versioning::fetch_head(mount, remote)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("note not found on shared mount: {path}"))?;
    if !head.has_divergence {
        return Ok(json!({
            "path": path,
            "hasDivergence": false,
            "note": "no divergence recorded on this note",
        }));
    }
    let head_chunks = reads::fetch_version_chunks(mount, mount.index(), remote, &head.version_id)
        .await
        .map_err(|error| error.to_string())?;
    let head_text = reads::reassemble_chunks(head_chunks);

    let mut payload = Map::new();
    payload.insert("path".to_string(), json!(path));
    payload.insert("hasDivergence".to_string(), json!(true));
    payload.insert(
        "head".to_string(),
        json!({
            "versionId": head.version_id,
            "participantId": head.participant_id,
            "text": head_text,
        }),
    );
    if let Some(forked_from) = &head.forked_from {
        if let Ok(overtaken) = read_version_payload(state, path, forked_from).await {
            payload.insert("overtaken".to_string(), overtaken);
        }
    }
    if let Some(parent) = &head.parent_version_id {
        if let Ok(ancestor) = read_version_payload(state, path, parent).await {
            payload.insert("commonAncestor".to_string(), ancestor);
        }
    }
    payload.insert(
        "howToResolve".to_string(),
        json!(
            "Merge head and overtaken against commonAncestor, then upsert_note with \
resolveDivergence: true to clear the flag."
        ),
    );
    Ok(Value::Object(payload))
}
