//! Ranked search served by the shared index itself.
//!
//! The one recall path in this repository that does not go through the server's local
//! SQLite index, and the reason [`Capability::NativeRecall`](crate::Capability::NativeRecall)
//! exists: this corpus has no local copy, so there is nothing local to rank over. The
//! index IS the vault, and it already ranks.
//!
//! Ported from PR #40's `shared/retrieval.rs` (`search_mount`, `drop_superseded_hits`,
//! `detect_recall_stage`). What changed:
//!
//! * #40 fused these hits into the local ranking with rank-based RRF inside the tool
//!   layer. That fusion is FEDERATION and does not belong to one backend, so it is not
//!   here: this returns one mount's own ranked list, and the server serves it as that
//!   mount's answer. Cross-mount fusion is a later slice, and it will consume exactly
//!   this response.
//! * hits are paginated, so a caller can ask for more instead of silently receiving a
//!   truncated list. See [`page_of`] for why the cursor is a page number.
//! * `recall_stage: String` became [`RecallMode`], and the detection result is cached
//!   ASYMMETRICALLY — see [`recall_mode`].

use deep_obsidian_algolia::SearchRequest as AlgoliaSearchRequest;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use super::{empty_if_missing_index, empty_search_response, AlgoliaVaultBackend};
use crate::{
    BackendError, OpaqueCursor, RecallHit, RecallMode, RecallSearchResponse, SearchRequest,
};

/// Run one ranked query against the mount's own index.
///
/// Chunk records only: a note record carries no body, so it could never produce a
/// snippet. The index-level `distinct` on `path` is left ON (unlike the grep prefilter,
/// which turns it off) so what comes back is the best chunk per NOTE — which is what a
/// note-level result list wants, and what makes `limit` count hits a caller will see
/// rather than chunks.
pub async fn search(
    backend: &AlgoliaVaultBackend,
    request: &SearchRequest,
) -> Result<RecallSearchResponse, BackendError> {
    let page = page_of(request.cursor.as_ref())?;
    // A zero limit would ask Algolia for `hitsPerPage=0` and get an empty page back,
    // which is indistinguishable from "no matches". Clamped to one so an empty result
    // always means an empty result.
    let hits_per_page = request.limit.max(1);
    let response = empty_if_missing_index(
        backend
            .client()
            .search(
                backend.index(),
                &AlgoliaSearchRequest {
                    query: request.query.clone(),
                    // No `deleted` guard: chunk records carry no such attribute, and a
                    // soft delete removes a note's chunks from the main index outright,
                    // so a tombstoned note has no chunks left to match. Filtering an
                    // absent attribute would only make the query depend on Algolia's
                    // missing-value semantics.
                    filters: Some("recordType:chunk".to_string()),
                    hits_per_page: Some(hits_per_page),
                    page: Some(page),
                    ..AlgoliaSearchRequest::default()
                },
            )
            .await,
        empty_search_response(),
    )?;

    // Computed from the RAW page, before the superseded filter below removes anything: a
    // page that came back full means the index has more to give, whether or not this
    // server chose to serve all of it. Deriving it from the filtered list would report a
    // page as final purely because its hits were orphans.
    let page_was_full = response.hits.len() >= hits_per_page;
    let hits: Vec<RawHit> = response
        .hits
        .iter()
        .enumerate()
        .filter_map(|(rank, hit)| {
            let path = hit.get("path")?.as_str()?.to_string();
            Some(RawHit {
                // Rank is taken over the RAW page, so dropping an orphan leaves a gap in
                // the scores rather than promoting the hit below it. That is deliberate:
                // the score reflects where the provider actually ranked the hit, and
                // renumbering would invent an ordering the provider did not produce.
                score: 1.0 / (page * hits_per_page + rank + 1) as f64,
                version_id: hit
                    .get("versionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                hit: RecallHit {
                    path,
                    title: hit
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    score: 0.0,
                    snippet: hit
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    chunk_index: hit.get("chunkIndex").and_then(Value::as_u64).unwrap_or(0)
                        as usize,
                    start_line: hit.get("startLine").and_then(Value::as_u64).unwrap_or(1) as usize,
                    end_line: hit.get("endLine").and_then(Value::as_u64).unwrap_or(1) as usize,
                },
            })
        })
        .collect();
    let hits = drop_superseded(backend, hits).await?;

    Ok(RecallSearchResponse {
        hits,
        // Minted only when there is a next page to fetch. A cursor pointing past the end
        // would make a caller's pagination loop run one pointless round trip and then
        // conclude, from an empty page, that the corpus had shrunk.
        next_cursor: page_was_full.then(|| OpaqueCursor::new((page + 1).to_string())),
        exhausted: !page_was_full,
        recall_mode: recall_mode(backend).await,
    })
}

/// A hit plus the two things needed to finish it: its provider rank (already turned into
/// a score) and the version its chunk belongs to.
struct RawHit {
    hit: RecallHit,
    score: f64,
    version_id: String,
}

/// Decode a cursor into a page number.
///
/// The cursor is a page index, and it is opaque to callers by contract rather than by
/// encoding — obfuscating it would buy nothing and make a failure unreadable. Algolia's
/// search pagination is page-based, and the alternative (its `browse` cursor) is not
/// available on a ranked query.
///
/// A cursor that is not a page number is an ERROR rather than a silent reset to page
/// zero: a caller that mixed up two mounts' cursors would otherwise get page one of the
/// wrong corpus and no indication anything went wrong.
fn page_of(cursor: Option<&OpaqueCursor>) -> Result<usize, BackendError> {
    match cursor {
        None => Ok(0),
        Some(cursor) => cursor.as_str().parse::<usize>().map_err(|_| {
            BackendError::Message(format!(
                "this Algolia mount cannot resume from the cursor {:?}: its cursors are page \
                 numbers, and this is not one. Pass a cursor this mount minted, or omit it to \
                 start from the most relevant hit.",
                cursor.as_str()
            ))
        }),
    }
}

/// Drop hits whose chunk belongs to a version that is no longer the note's head.
///
/// Two participants writing one note concurrently each push their own chunks; only one
/// wins the head pointer, and the loser's chunks stay in the main index as ORPHANS. They
/// are unreachable from the head (a read reassembles by head version), but a plain chunk
/// query still matches them, so search would show text the note no longer contains.
///
/// Deleting them instead would mean re-running exactly the destructive race the explicit
/// `versionId:vPrev` delete filter exists to avoid, so they are filtered at QUERY time:
/// one batched `getObjects` over the hit paths, then keep only head-version chunks.
async fn drop_superseded(
    backend: &AlgoliaVaultBackend,
    hits: Vec<RawHit>,
) -> Result<Vec<RecallHit>, BackendError> {
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<String> = hits.iter().map(|hit| hit.hit.path.clone()).collect();
    paths.sort();
    paths.dedup();
    let ids: Vec<String> = paths
        .iter()
        .map(|path| deep_obsidian_algolia::note_object_id(path))
        .collect();
    let raw = backend.client().get_objects(backend.index(), &ids).await;
    // A secured key may be scoped so chunk records are visible but note records are not.
    // Failing the whole search there would be worse than skipping the head check, so an
    // unresolvable head keeps its hits.
    if raw
        .as_ref()
        .err()
        .is_some_and(|error| error.is_forbidden_by_key_scope())
    {
        return Ok(finish(hits));
    }
    let records = empty_if_missing_index(raw, Vec::new())?;
    let mut head_of: HashMap<String, String> = HashMap::new();
    for record in records.into_iter().flatten() {
        let (Some(path), Some(version)) = (
            record.get("path").and_then(Value::as_str),
            record.get("versionId").and_then(Value::as_str),
        ) else {
            continue;
        };
        // A tombstoned note has no live content at all.
        if record
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        head_of.insert(path.to_string(), version.to_string());
    }
    Ok(finish(
        hits.into_iter()
            .filter(|hit| {
                head_of
                    .get(&hit.hit.path)
                    .is_some_and(|head| *head == hit.version_id)
            })
            .collect(),
    ))
}

/// Move each hit's rank-derived score onto the hit itself.
fn finish(hits: Vec<RawHit>) -> Vec<RecallHit> {
    hits.into_iter()
        .map(|raw| RecallHit {
            score: raw.score,
            ..raw.hit
        })
        .collect()
}

/// Which retrieval stage this index uses, from its own settings.
///
/// Algolia NeuralSearch is enabled per index via `mode: "neuralSearch"`, so the index
/// itself is the only source of truth — and the answer can change under a running
/// server, when somebody enables it in the dashboard.
///
/// # Why the cache is asymmetric
///
/// [`RecallMode::Neural`] is cached for the process; [`RecallMode::Lexical`] is not. A
/// confirmed neural index does not silently become lexical in a way that would mislead
/// anyone (the claim would merely become conservative), whereas caching `Lexical`
/// forever would keep under-reporting an index that has since been upgraded. Every
/// failure — an unreachable index, a rotated key, a never-written corpus with no
/// settings — also answers `Lexical`, because reporting a weaker stage than was used is
/// harmless while claiming a stronger one makes a caller trust the ranking more than it
/// should. Ported from #40, which cached with the same asymmetry.
async fn recall_mode(backend: &AlgoliaVaultBackend) -> RecallMode {
    if backend.neural_recall_confirmed.load(Ordering::Relaxed) {
        return RecallMode::Neural;
    }
    let detected = match backend.client().get_settings(backend.index()).await {
        Ok(settings) => match settings.get("mode").and_then(Value::as_str) {
            Some("neuralSearch") => RecallMode::Neural,
            _ => RecallMode::Lexical,
        },
        Err(_) => RecallMode::Lexical,
    };
    if matches!(detected, RecallMode::Neural) {
        backend
            .neural_recall_confirmed
            .store(true, Ordering::Relaxed);
    }
    detected
}
