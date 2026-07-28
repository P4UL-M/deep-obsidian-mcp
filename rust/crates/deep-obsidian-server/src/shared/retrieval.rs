//! Federated retrieval over shared mounts (design §4.2, §4.3, §8).
//!
//! The shared index answers first-stage recall over the whole corpus; results
//! fuse into the local ranking with rank-based RRF (scale-free, so Algolia's
//! scores never need normalizing against BM25/cosine). `recallStage` reports
//! whether the index's first stage is neural or lexical.

use super::{Result, SharedMountRuntime};
use deep_obsidian_algolia::SearchRequest;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct RemoteSearchHit {
    pub mounted_path: String,
    pub remote_path: String,
    pub title: String,
    pub text: String,
    pub chunk_index: usize,
    pub start_line: usize,
    pub end_line: usize,
}

/// One ranked query against the mount's main index. Chunk records only (note
/// records carry no body); index-level `distinct` on `path` returns the best
/// chunk per note.
pub async fn search_mount(
    mount: &SharedMountRuntime,
    query: &str,
    limit: usize,
) -> Result<Vec<RemoteSearchHit>> {
    let response = mount
        .client
        .search(
            mount.index(),
            &SearchRequest {
                query: query.to_string(),
                filters: Some("recordType:chunk".to_string()),
                hits_per_page: Some(limit),
                ..SearchRequest::default()
            },
        )
        .await?;
    Ok(response
        .hits
        .iter()
        .filter_map(|hit| {
            let remote_path = hit.get("path")?.as_str()?.to_string();
            Some(RemoteSearchHit {
                mounted_path: mount.mounted_path(&remote_path),
                remote_path,
                title: hit
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                text: hit
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                chunk_index: hit.get("chunkIndex").and_then(Value::as_u64).unwrap_or(0) as usize,
                start_line: hit.get("startLine").and_then(Value::as_u64).unwrap_or(1) as usize,
                end_line: hit.get("endLine").and_then(Value::as_u64).unwrap_or(1) as usize,
            })
        })
        .collect())
}

/// Detects the index's first-stage capability from its settings: Algolia
/// NeuralSearch is enabled per-index via `mode: "neuralSearch"`. Falls back to
/// "lexical" on any error — reporting a weaker stage is safe, claiming a
/// stronger one is not.
pub async fn detect_recall_stage(mount: &SharedMountRuntime) -> String {
    match mount.client.get_settings(mount.index()).await {
        Ok(settings) => match settings.get("mode").and_then(Value::as_str) {
            Some("neuralSearch") => "neural".to_string(),
            _ => "lexical".to_string(),
        },
        Err(_) => "lexical".to_string(),
    }
}

/// Generalized Reciprocal Rank Fusion over N ranked lists of keys (design §8):
/// `score(key) = Σ_lists weight_i / (k + rank_i(key))`, rank 0-based. Returns
/// keys sorted by fused score, descending. `k = 60` is the conventional
/// constant, matching the local hybrid fusion.
pub fn rrf_fuse_many<K: Clone + Eq + std::hash::Hash>(
    lists: &[(Vec<K>, f64)],
    k: f64,
) -> Vec<(K, f64)> {
    let mut scores: std::collections::HashMap<K, f64> = std::collections::HashMap::new();
    for (list, weight) in lists {
        for (rank, key) in list.iter().enumerate() {
            *scores.entry(key.clone()).or_default() += weight / (k + rank as f64);
        }
    }
    let mut fused: Vec<(K, f64)> = scores.into_iter().collect();
    fused.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    fused
}

/// Extracts a lexical anchor from a regex pattern for the shared-grep
/// prefilter (design §4.3): the longest run of plain literal characters
/// outside any regex metasyntax. Returns `None` when the pattern has no
/// usable anchor — the caller must refuse, never silently under-report.
pub fn extract_literal_anchor(pattern: &str) -> Option<String> {
    let mut runs: Vec<String> = vec![String::new()];
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                // Escaped char: literal only for escaped metachars like \. \-
                if let Some(next) = chars.next() {
                    if !next.is_alphanumeric() {
                        runs.last_mut().unwrap().push(next);
                    } else {
                        // \b, \d, \w, ... — class shorthand, breaks the run.
                        runs.push(String::new());
                    }
                }
            }
            '[' => {
                in_class = true;
                runs.push(String::new());
            }
            ']' if in_class => {
                in_class = false;
            }
            _ if in_class => {}
            '*' | '+' | '?' => {
                // Quantifier makes the PRECEDING char optional/repeated: drop it.
                let last = runs.last_mut().unwrap();
                last.pop();
                runs.push(String::new());
            }
            '.' | '^' | '$' | '(' | ')' | '|' | '{' | '}' => {
                runs.push(String::new());
            }
            _ => runs.last_mut().unwrap().push(ch),
        }
    }
    runs.into_iter()
        .max_by_key(String::len)
        .filter(|run| run.trim().len() >= 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_fuses_ranked_lists_scale_free() {
        let local = vec!["a", "b", "c"];
        let remote = vec!["b", "d"];
        let fused = rrf_fuse_many(&[(local, 1.0), (remote, 1.0)], 60.0);
        // "b" appears in both lists -> highest fused score.
        assert_eq!(fused[0].0, "b");
        let keys: Vec<&str> = fused.iter().map(|(key, _)| *key).collect();
        assert!(keys.contains(&"a") && keys.contains(&"d"));
    }

    #[test]
    fn literal_anchor_extraction_and_refusal() {
        assert_eq!(
            extract_literal_anchor(r"\bSpacelift\b").as_deref(),
            Some("Spacelift")
        );
        assert_eq!(
            extract_literal_anchor("retention.*policy").as_deref(),
            Some("retention")
        );
        assert_eq!(
            extract_literal_anchor(r"foo\.bar baz").as_deref(),
            Some("foo.bar baz")
        );
        // No lexical anchor: must refuse.
        assert_eq!(extract_literal_anchor(r"^\s*$"), None);
        assert_eq!(extract_literal_anchor("[a-z]+"), None);
        assert_eq!(extract_literal_anchor("a?b?"), None);
    }
}
