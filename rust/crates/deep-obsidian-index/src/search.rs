use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::graph::resolve_wiki_link_target;
use crate::index::{
    artifact_embedding_runtime_config, average, bm25_score, cosine_similarity, count_terms,
    embedding_runtime_config, enclosing_heading_section, matches_pattern, normalize_dense_vector,
    open_index_connection_for_index, query_vector_blob, vector_norm, IndexError,
    Result, SearchIndex, SearchNote, SemanticBackend,
};
use rusqlite::{params, params_from_iter, OptionalExtension};

const HYBRID_SEARCH_OVERSAMPLE_FACTOR: usize = 8;
const HYBRID_SEARCH_MIN_CANDIDATES: usize = 50;
const HYBRID_SEARCH_CANDIDATE_CAP: usize = 512;

/// Reciprocal Rank Fusion smoothing constant. The standard value from Cormack et al.
/// (2009); larger `k` flattens the contribution of top ranks, smaller `k` sharpens it.
const RRF_K: f64 = 60.0;

/// Graph-aware re-rank (issue #6 item #5). After RRF fusion, candidate chunks whose note
/// is one wikilink hop from one of the top-ranked notes get a small score bonus, lightly
/// promoting vault-linked context. Conservative by design: it breaks ties and lifts
/// near-misses without overriding strong direct matches. The bonus is ~half a single
/// list's rank-1 RRF contribution. A bonus of 0 reproduces pure-RRF order exactly.
const GRAPH_RERANK_TOP_N: usize = 5;
const GRAPH_PROXIMITY_BONUS: f64 = 0.5 / RRF_K;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePathMatch {
    pub path: String,
    pub matched_on: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GrepSubmatch {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchMatch {
    pub path: String,
    pub title: String,
    pub chunk_index: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f64,
    pub semantic_score: f64,
    pub bm25_score: f64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactSearchMatch {
    pub path: String,
    pub title: String,
    pub kind: String,
    pub mime_type: String,
    pub size: u64,
    pub score: f64,
    pub metadata_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelatedNoteMatch {
    pub path: String,
    pub title: String,
    pub score: f64,
    pub shared_links: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSearchMode {
    Substring,
    Regex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindFilesOptions {
    pub mode: FileSearchMode,
    pub limit: usize,
}

impl Default for FindFilesOptions {
    fn default() -> Self {
        Self {
            mode: FileSearchMode::Substring,
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankingOptions {
    pub limit: usize,
    /// Per-retriever weight for the dense/semantic list in Reciprocal Rank Fusion.
    /// Multiplies that list's `1/(k + rank)` contribution. Defaults to 1.0 (unweighted).
    ///
    /// Historically this was the dense weight in a max-normalized weighted sum; the
    /// hybrid fusion is now rank-based (RRF), so this is the RRF dense weight. The
    /// field name is retained so existing callers keep compiling. For the non-hybrid
    /// `semantic_search` / `bm25_search` paths this field is inert.
    pub semantic_weight: f64,
    /// Per-retriever weight for the BM25 list in Reciprocal Rank Fusion. See
    /// [`RankingOptions::semantic_weight`]; defaults to 1.0 (unweighted).
    pub bm25_weight: f64,
}

impl Default for RankingOptions {
    fn default() -> Self {
        Self {
            limit: 8,
            // RRF is rank-based and scale-free, so default to UNWEIGHTED fusion.
            semantic_weight: 1.0,
            bm25_weight: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedNoteOptions {
    pub limit: usize,
}

impl Default for RelatedNoteOptions {
    fn default() -> Self {
        Self { limit: 8 }
    }
}

/// Reciprocal Rank Fusion contribution of a single ranked list: `weight / (k + rank)`,
/// with `rank` 1-based. A document absent from a list never calls this, so it
/// contributes 0 to its fused score (the issue's requirement).
fn rrf_contribution(rank_one_based: usize, weight: f64, k: f64) -> f64 {
    weight / (k + rank_one_based as f64)
}

/// Graph-aware re-rank applied in place over the fused hybrid candidates. `matches` must
/// already be sorted by score (descending) so the top `top_n` distinct notes can be read
/// off the front as anchors. For each candidate whose note is one wikilink hop (outgoing
/// or incoming) from an anchor, add `bonus` to its score. Builds the link adjacency once
/// (not per candidate). No-op when `bonus <= 0` (pure RRF is then recoverable), when
/// `top_n == 0`, or when no candidate is link-adjacent. The caller re-sorts afterward.
fn apply_graph_proximity_rerank(
    index: &SearchIndex,
    matches: &mut [SearchMatch],
    top_n: usize,
    bonus: f64,
) {
    if bonus <= 0.0 || top_n == 0 || matches.is_empty() {
        return;
    }
    let mut anchors: BTreeSet<String> = BTreeSet::new();
    for match_item in matches.iter() {
        anchors.insert(match_item.path.clone());
        if anchors.len() >= top_n {
            break;
        }
    }
    let neighbors = crate::graph::one_hop_neighbor_notes(index, &anchors);
    if neighbors.is_empty() {
        return;
    }
    for match_item in matches.iter_mut() {
        if neighbors.contains(&match_item.path) {
            match_item.score += bonus;
        }
    }
}

fn sql_distance_score(distance: f64) -> f64 {
    1.0 / (1.0 + distance)
}

fn hybrid_candidate_limit(chunk_count: usize, requested_limit: usize) -> usize {
    let requested_limit = requested_limit.max(1);
    let candidate_limit = requested_limit
        .saturating_mul(HYBRID_SEARCH_OVERSAMPLE_FACTOR)
        .max(HYBRID_SEARCH_MIN_CANDIDATES)
        .min(HYBRID_SEARCH_CANDIDATE_CAP.max(requested_limit));
    candidate_limit.min(chunk_count.max(1))
}

fn semantic_search_with_query_vector_sql(
    index: &SearchIndex,
    query_embedding: &[f64],
    limit: usize,
) -> Result<Vec<SearchMatch>> {
    let connection = open_index_connection_for_index(index, true)?;
    let mut statement = connection
        .prepare(
            r#"
            SELECT
              c.path,
              c.title,
              c.chunk_index,
              c.start_line,
              c.end_line,
              c.text,
              matches.distance
            FROM (
              SELECT rowid, distance
              FROM chunk_embeddings_vec
              WHERE embedding MATCH ?1 AND k = ?2
            ) matches
            JOIN chunks c ON c.id = matches.rowid
            ORDER BY matches.distance
            "#,
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map(
            params![query_vector_blob(query_embedding), limit.max(1) as i64],
            |row| {
                let distance = row.get::<_, f64>(6)?;
                let score = sql_distance_score(distance);
                Ok(SearchMatch {
                    path: row.get::<_, String>(0)?,
                    title: row.get::<_, String>(1)?,
                    chunk_index: row.get::<_, i64>(2)? as usize,
                    start_line: row.get::<_, i64>(3)? as usize,
                    end_line: row.get::<_, i64>(4)? as usize,
                    score,
                    semantic_score: score,
                    bm25_score: 0.0,
                    text: row.get::<_, String>(5)?,
                })
            },
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| IndexError::Embedding(error.to_string()))
}

/// Bounded per-source-chunk KNN used for late-interaction (max-sim) note relatedness.
/// Each source chunk contributes its top-K nearest chunks vault-wide; K trades recall
/// for cost. On a small vault K covers most chunks (near-exact); on a large vault it
/// caps per-query work. 64 is a reasonable balance for average hardware. Clamped to the
/// chunk count so sqlite-vec never receives `k` larger than the corpus.
const RELATED_NOTES_MAXSIM_K: usize = 64;

/// Note-to-note relatedness via late interaction (max-sim) over the persisted CHUNK
/// vectors. For source note X with chunks {x_i}, candidate note Y scores
/// `sum_i max_{y in Y} cos(x_i, y)`: one sqlite-vec KNN per source chunk (sqlite-vec
/// does the nearest-chunk lookup) followed by a max-then-sum aggregation by note. No
/// note-level vector is stored -- this reads only `chunk_embeddings_vec`.
///
/// Returns `MissingNoteEmbedding` when the source note has no chunk embeddings (a note
/// that failed to embed in a partial index, or a sparse backend) so the caller degrades
/// to the sparse term-overlap path.
fn related_notes_with_embeddings_sql(
    index: &SearchIndex,
    note_path: &str,
    limit: usize,
) -> Result<Vec<RelatedNoteMatch>> {
    let note = index
        .note(note_path)
        .ok_or_else(|| IndexError::NoteNotFound(note_path.to_string()))?;
    let note_links: BTreeSet<_> = note.links.iter().cloned().collect();
    let connection = open_index_connection_for_index(index, true)?;

    // The source note's own chunk vectors are the "queries" for late interaction.
    let source_vectors: Vec<Vec<u8>> = {
        let mut statement = connection
            .prepare(
                r#"
                SELECT v.embedding
                FROM chunk_embeddings_vec v
                JOIN chunks c ON c.id = v.rowid
                WHERE c.path = ?1
                "#,
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map(params![note_path], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| IndexError::Embedding(error.to_string()))?
    };
    if source_vectors.is_empty() {
        return Err(IndexError::MissingNoteEmbedding(note_path.to_string()));
    }

    // Clamp k to the corpus so sqlite-vec never sees k > number of stored vectors.
    let k = RELATED_NOTES_MAXSIM_K.min(index.chunks.len().max(1)) as i64;

    let mut statement = connection
        .prepare(
            r#"
            SELECT
              c.path,
              n.title,
              n.links_json,
              matches.distance
            FROM (
              SELECT rowid, distance
              FROM chunk_embeddings_vec
              WHERE embedding MATCH ?1 AND k = ?2
            ) matches
            JOIN chunks c ON c.id = matches.rowid
            JOIN notes n ON n.path = c.path
            WHERE c.path <> ?3
            "#,
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    // note path -> (title, links_json, summed late-interaction score)
    let mut scored: HashMap<String, (String, String, f64)> = HashMap::new();

    for source_vector in &source_vectors {
        // This source chunk's best (max) similarity to each candidate note.
        let hits: Vec<(String, String, String, f64)> = statement
            .query_map(params![source_vector, k, note_path], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    sql_distance_score(row.get::<_, f64>(3)?),
                ))
            })
            .map_err(|error| IndexError::Embedding(error.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| IndexError::Embedding(error.to_string()))?;

        let mut best_this_chunk: HashMap<String, (String, String, f64)> = HashMap::new();
        for (path, title, links_json, sim) in hits {
            best_this_chunk
                .entry(path)
                .and_modify(|entry| {
                    if sim > entry.2 {
                        entry.2 = sim;
                    }
                })
                .or_insert((title, links_json, sim));
        }
        for (path, (title, links_json, sim)) in best_this_chunk {
            scored
                .entry(path)
                .and_modify(|entry| entry.2 += sim)
                .or_insert((title, links_json, sim));
        }
    }

    let mut matches: Vec<RelatedNoteMatch> = scored
        .into_iter()
        .map(|(path, (title, links_json, score))| {
            let links: Vec<String> = serde_json::from_str(&links_json).unwrap_or_default();
            RelatedNoteMatch {
                path,
                title,
                score,
                shared_links: links
                    .into_iter()
                    .filter(|link| note_links.contains(link))
                    .take(10)
                    .collect(),
            }
        })
        .collect();
    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    matches.truncate(limit.max(1));
    Ok(matches)
}

/// Build the exact string sent to the embedding backend for a query.
///
/// Query-side only: for instruction-tuned models the config carries a
/// `query_instruction` and the query is wrapped in the qwen3 instruction format
/// (`Instruct: {instruction}\nQuery: {query}`). When no instruction is set the raw
/// query is returned unchanged. Documents were indexed PLAIN and the document/index
/// embedding path never applies this, so no reindex is required.
fn query_embedding_input(config: &crate::embeddings::EmbeddingConfig, query: &str) -> String {
    match config.query_instruction.as_deref() {
        Some(instruction) => crate::embeddings::format_query_with_instruction(instruction, query),
        None => query.to_string(),
    }
}

/// Map a typed embedding error from a QUERY-time embed into an `IndexError`, preserving
/// the transient/backend-unavailable distinction. Transient failures (connection refused,
/// timeout, 5xx, a crashed llama-server worker) become `EmbeddingBackendUnavailable` so the
/// query layer can degrade to a lexical fallback; deterministic failures keep mapping to
/// `Embedding`. This is the single point where the `EmbeddingError` type is still available
/// at the query boundary, per the issue's "handle it as close to the call as possible".
fn map_query_embedding_error(error: crate::embeddings::EmbeddingError) -> IndexError {
    if error.is_transient() {
        IndexError::EmbeddingBackendUnavailable(error.to_string())
    } else {
        IndexError::Embedding(error.to_string())
    }
}

fn embed_query(index: &SearchIndex, query: &str) -> Result<Vec<f64>> {
    let config = embedding_runtime_config(index).ok_or(IndexError::MissingEmbeddingConfig)?;
    let input = query_embedding_input(&config, query);
    let result =
        crate::embeddings::embed_texts(&[input], &config).map_err(map_query_embedding_error)?;
    let vector = result.vectors.into_iter().next().unwrap_or_default();
    if let Some(expected) = index.embedding_dimensions {
        if vector.len() != expected {
            return Err(IndexError::EmbeddingDimensionsMismatch {
                expected,
                actual: vector.len(),
            });
        }
    }
    Ok(normalize_dense_vector(&vector))
}

/// Outcome of a non-fatal embedding-backend health probe (see [`probe_embedding_backend`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingBackendHealth {
    /// The index is not on the Embedding backend (sparse), so there is no backend to probe.
    NotApplicable,
    /// A tiny embed round-trip succeeded: the backend is reachable.
    Reachable,
    /// The backend was unreachable / errored. Carries a short cause for status reporting.
    Unreachable(String),
}

/// Bounded, NON-fatal health probe for the note embedding backend, used by `vault_info` to
/// surface backend availability without ever erroring. Issues a single tiny embed against a
/// SHORT-timeout client (so a hung/dead backend resolves quickly rather than blocking on the
/// 60s default). Returns `NotApplicable` for sparse indexes (nothing to probe), `Reachable`
/// on success, and `Unreachable` on any failure. This never returns `Err`.
pub fn probe_embedding_backend(index: &SearchIndex) -> EmbeddingBackendHealth {
    let Some(config) = embedding_runtime_config(index) else {
        return EmbeddingBackendHealth::NotApplicable;
    };
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(client) => client,
        Err(error) => return EmbeddingBackendHealth::Unreachable(error.to_string()),
    };
    match crate::embeddings::embed_texts_with_client(&["ping".to_string()], &config, &client) {
        Ok(_) => EmbeddingBackendHealth::Reachable,
        Err(error) => EmbeddingBackendHealth::Unreachable(error.to_string()),
    }
}

fn embed_artifact_query(index: &SearchIndex, query: &str) -> Result<Vec<f64>> {
    if let Some(error) = &index.artifact_embedding_error {
        return Err(IndexError::Embedding(format!(
            "artifact embedding unavailable: {error}"
        )));
    }
    let config =
        artifact_embedding_runtime_config(index).ok_or(IndexError::MissingEmbeddingConfig)?;
    let result = crate::embeddings::embed_texts(&[query.to_string()], &config)
        .map_err(map_query_embedding_error)?;
    let vector = result.vectors.into_iter().next().unwrap_or_default();
    if let Some(expected) = index.artifact_embedding_dimensions {
        if vector.len() != expected {
            return Err(IndexError::EmbeddingDimensionsMismatch {
                expected,
                actual: vector.len(),
            });
        }
    }
    Ok(normalize_dense_vector(&vector))
}

fn artifact_search_with_query_vector_sql(
    index: &SearchIndex,
    query_embedding: &[f64],
    limit: usize,
) -> Result<Vec<ArtifactSearchMatch>> {
    if index.artifact_embedding_dimensions.is_none() {
        return Err(IndexError::MissingEmbeddingConfig);
    }
    let connection = open_index_connection_for_index(index, true)?;
    let mut statement = connection
        .prepare(
            r#"
            SELECT
              a.path,
              a.title,
              a.kind,
              a.mime_type,
              a.size,
              a.metadata_json,
              matches.distance
            FROM (
              SELECT rowid, distance
              FROM artifact_embeddings_vec
              WHERE embedding MATCH ?1 AND k = ?2
            ) matches
            JOIN artifacts a ON a.id = matches.rowid
            ORDER BY matches.distance
            "#,
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map(
            params![query_vector_blob(query_embedding), limit.max(1) as i64],
            |row| {
                let distance = row.get::<_, f64>(6)?;
                Ok(ArtifactSearchMatch {
                    path: row.get::<_, String>(0)?,
                    title: row.get::<_, String>(1)?,
                    kind: row.get::<_, String>(2)?,
                    mime_type: row.get::<_, String>(3)?,
                    size: row.get::<_, i64>(4)? as u64,
                    metadata_json: row.get::<_, String>(5)?,
                    score: sql_distance_score(distance),
                })
            },
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| IndexError::Embedding(error.to_string()))
}

pub fn find_files(index: &SearchIndex, query: &str) -> Result<Vec<FilePathMatch>> {
    find_files_with_options(index, query, FindFilesOptions::default())
}

pub fn find_files_with_options(
    index: &SearchIndex,
    query: &str,
    options: FindFilesOptions,
) -> Result<Vec<FilePathMatch>> {
    let limit = options.limit.max(1);
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();

    match options.mode {
        FileSearchMode::Substring => {
            let lowered = query.to_lowercase();
            for snapshot in &index.file_snapshots {
                if snapshot.path.to_lowercase().contains(&lowered) {
                    files.push(FilePathMatch {
                        path: snapshot.path.clone(),
                        matched_on: "substring".to_string(),
                    });
                }
                if files.len() >= limit {
                    break;
                }
            }
        }
        FileSearchMode::Regex => {
            for snapshot in &index.file_snapshots {
                if matches_pattern(&snapshot.path, query, false)? {
                    files.push(FilePathMatch {
                        path: snapshot.path.clone(),
                        matched_on: "regex".to_string(),
                    });
                }
                if files.len() >= limit {
                    break;
                }
            }
        }
    }

    Ok(files)
}

/// "Small-to-big" retrieval (issue #6 item #4, query-time / presentation only). Matching
/// and ranking happen at CHUNK granularity; this grows the RETURNED `text` (and line range)
/// of each chunk hit to its enclosing heading SECTION so the caller sees a coherent unit
/// instead of a sub-chunk window. Scores are untouched — expansion never re-ranks.
///
/// For each match, the enclosing section is reconstructed from the parent note's in-memory
/// `content` using `enclosing_heading_section` (the same fence-aware, flat tiling the
/// chunker uses). A chunk in the preamble / a heading-less note has no enclosing section and
/// keeps its own text/range (no expansion). Per-note section lookups are memoized so a note
/// is parsed at most once per call even when several of its sub-chunks survive into the
/// candidate pool.
///
/// Call this AFTER sort + truncate so it runs only over returned results and cannot perturb
/// ordering. The grown `text` still flows through the server's per-result snippet cap and
/// aggregate budget (issue #6 item #6), so large sections truncate-with-continuation as usual.
fn expand_chunk_matches_to_sections(index: &SearchIndex, matches: &mut [SearchMatch]) {
    // Cache the resolved section per (path, chunk start_line). Sub-chunks have distinct
    // start_lines, so this only collapses the rare case where the same chunk appears twice;
    // the dominant cost is `enclosing_heading_section`, which re-scans the note's content
    // each call. The candidate pool is bounded (<= the search limit at these call sites), so
    // a re-scan per match is acceptable; a note is small relative to that bound.
    let mut section_cache: HashMap<(String, usize), Option<(usize, usize, String)>> =
        HashMap::new();
    for match_item in matches.iter_mut() {
        let key = (match_item.path.clone(), match_item.start_line);
        let section = section_cache
            .entry(key)
            .or_insert_with(|| {
                index
                    .note(&match_item.path)
                    .and_then(|note| {
                        enclosing_heading_section(&note.content, match_item.start_line)
                    })
            })
            .clone();
        if let Some((start_line, end_line, text)) = section {
            match_item.start_line = start_line;
            match_item.end_line = end_line;
            match_item.text = text;
        }
    }
}

pub fn bm25_search(index: &SearchIndex, query: &str) -> Result<Vec<SearchMatch>> {
    bm25_search_with_options(index, query, RankingOptions::default())
}

pub fn bm25_search_with_options(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
) -> Result<Vec<SearchMatch>> {
    let query_terms = count_terms(query);
    let query_terms = query_terms.keys().cloned().collect::<Vec<_>>();
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut matches =
        if let Ok(Some(matches)) = bm25_search_with_sql_candidates(index, &query_terms, &options) {
            matches
        } else {
            bm25_search_in_memory(index, &query_terms, options)?
        };
    // Small-to-big: grow each chunk hit's returned text to its enclosing section. Runs after
    // the inner helpers sorted + truncated, so ranking/order is untouched (presentation only).
    expand_chunk_matches_to_sections(index, &mut matches);
    Ok(matches)
}

fn bm25_search_in_memory(
    index: &SearchIndex,
    query_terms: &[String],
    options: RankingOptions,
) -> Result<Vec<SearchMatch>> {
    let chunk_lengths = index
        .chunks
        .iter()
        .map(|chunk| chunk.token_count as f64)
        .collect::<Vec<_>>();
    let average_chunk_length = average(&chunk_lengths);
    let mut matches = index
        .chunks
        .iter()
        .map(|chunk| {
            let score = bm25_score(
                &query_terms,
                &chunk.term_counts,
                &index.document_frequencies,
                index.chunk_count,
                chunk.token_count,
                average_chunk_length,
            );
            SearchMatch {
                path: chunk.path.clone(),
                title: chunk.title.clone(),
                chunk_index: chunk.chunk_index,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                score,
                semantic_score: 0.0,
                bm25_score: score,
                text: chunk.text.clone(),
            }
        })
        .filter(|match_item| match_item.score > 0.0)
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.chunk_index.cmp(&right.chunk_index))
    });
    matches.truncate(options.limit.max(1));
    Ok(matches)
}

fn bm25_search_with_sql_candidates(
    index: &SearchIndex,
    query_terms: &[String],
    options: &RankingOptions,
) -> Result<Option<Vec<SearchMatch>>> {
    let connection = match open_index_connection_for_index(index, true) {
        Ok(connection) => connection,
        Err(_) => return Ok(None),
    };
    let has_chunk_terms = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'chunk_terms'",
            [],
            |_row| Ok(()),
        )
        .optional()
        .map_err(|error| IndexError::Embedding(error.to_string()))?
        .is_some();
    if !has_chunk_terms {
        return Ok(None);
    }

    let placeholders = std::iter::repeat("?")
        .take(query_terms.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        r#"
        SELECT DISTINCT
          c.path,
          c.title,
          c.chunk_index,
          c.start_line,
          c.end_line,
          c.text,
          c.term_counts_json,
          c.token_count
        FROM chunk_terms AS ct
        JOIN chunks AS c ON c.id = ct.chunk_id
        WHERE ct.term IN ({placeholders})
        "#
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map(params_from_iter(query_terms.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as usize,
                row.get::<_, i64>(3)? as usize,
                row.get::<_, i64>(4)? as usize,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)? as usize,
            ))
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    let chunk_lengths = index
        .chunks
        .iter()
        .map(|chunk| chunk.token_count as f64)
        .collect::<Vec<_>>();
    let average_chunk_length = average(&chunk_lengths);
    let mut matches = Vec::new();
    for row in rows {
        let (path, title, chunk_index, start_line, end_line, text, term_counts_json, token_count) =
            row.map_err(|error| IndexError::Embedding(error.to_string()))?;
        let term_counts: BTreeMap<String, usize> = serde_json::from_str(&term_counts_json)
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let score = bm25_score(
            query_terms,
            &term_counts,
            &index.document_frequencies,
            index.chunk_count,
            token_count,
            average_chunk_length,
        );
        if score <= 0.0 {
            continue;
        }
        matches.push(SearchMatch {
            path,
            title,
            chunk_index,
            start_line,
            end_line,
            score,
            semantic_score: 0.0,
            bm25_score: score,
            text,
        });
    }

    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.chunk_index.cmp(&right.chunk_index))
    });
    matches.truncate(options.limit.max(1));
    Ok(Some(matches))
}

pub fn semantic_search(index: &SearchIndex, query: &str) -> Result<Vec<SearchMatch>> {
    semantic_search_with_options(index, query, RankingOptions::default())
}

pub fn semantic_search_with_options(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
) -> Result<Vec<SearchMatch>> {
    let mut matches = if index.semantic_backend == SemanticBackend::Embedding {
        let query_embedding = embed_query(index, query)?;
        semantic_search_with_query_vector_sql(index, &query_embedding, options.limit.max(1))?
    } else {
        let query_term_counts = count_terms(query);
        let query_norm = vector_norm(&query_term_counts);
        index
            .chunks
            .iter()
            .map(|chunk| {
                let score = cosine_similarity(
                    &query_term_counts,
                    query_norm,
                    &chunk.term_counts,
                    chunk.norm,
                );
                SearchMatch {
                    path: chunk.path.clone(),
                    title: chunk.title.clone(),
                    chunk_index: chunk.chunk_index,
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    score,
                    semantic_score: score,
                    bm25_score: 0.0,
                    text: chunk.text.clone(),
                }
            })
            .filter(|match_item| match_item.score > 0.0)
            .collect::<Vec<_>>()
    };

    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.chunk_index.cmp(&right.chunk_index))
    });
    matches.truncate(options.limit.max(1));
    // Small-to-big: grow each chunk hit's returned text to its enclosing section. Runs after
    // sort + truncate, so ranking/order is untouched (presentation only).
    expand_chunk_matches_to_sections(index, &mut matches);
    Ok(matches)
}

pub fn artifact_semantic_search_with_options(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
) -> Result<Vec<ArtifactSearchMatch>> {
    let query_embedding = embed_artifact_query(index, query)?;
    artifact_search_with_query_vector_sql(index, &query_embedding, options.limit.max(1))
}

/// Outcome of a hybrid search that may have degraded to a lexical-only fallback.
///
/// When the embedding backend is unavailable at query time (`EmbeddingBackendUnavailable`
/// from the semantic-candidate step), `hybrid_search_with_options_degradable` returns the
/// BM25-only ranking with `degraded = true` and a short, human-readable
/// `degradation_reason` instead of erroring. On the healthy path `degraded` is `false`,
/// `degradation_reason` is `None`, and `matches` are byte-identical to the pre-existing
/// hybrid output.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridSearchOutcome {
    pub matches: Vec<SearchMatch>,
    pub degraded: bool,
    pub degradation_reason: Option<String>,
}

/// Short, user-facing explanation set on `degraded` hybrid responses.
pub const HYBRID_DEGRADATION_REASON: &str =
    "embedding backend unavailable; returned lexical (BM25) results";

pub fn hybrid_search(index: &SearchIndex, query: &str) -> Result<Vec<SearchMatch>> {
    hybrid_search_with_options(index, query, RankingOptions::default())
}

/// Backward-compatible hybrid search that discards the degradation signal. Still degrades
/// internally (never errors on a backend-unavailable failure), so callers that don't report
/// the flag (e.g. `find_similar_notes` subject mode, `recommend_folder`) stop leaking the
/// raw upstream error automatically.
pub fn hybrid_search_with_options(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
) -> Result<Vec<SearchMatch>> {
    Ok(hybrid_search_with_options_degradable(index, query, options)?.matches)
}

/// Hybrid search that surfaces whether it degraded to BM25-only. See [`HybridSearchOutcome`].
pub fn hybrid_search_with_options_degradable(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
) -> Result<HybridSearchOutcome> {
    let requested_limit = options.limit.max(1);
    let candidate_limit = hybrid_candidate_limit(index.chunk_count, requested_limit);
    hybrid_search_with_candidate_limit(index, query, options, candidate_limit)
}

/// BM25-only fallback used when the embedding backend is unavailable. Returns results at the
/// REQUESTED limit (not the oversampled candidate limit) since there is no second list to
/// fuse. The graph-proximity rerank is intentionally skipped here: with no semantic signal
/// the fused-candidate-pool rationale for it no longer holds, and lexical results stand alone.
fn hybrid_degraded_to_bm25(
    index: &SearchIndex,
    query: &str,
    options: &RankingOptions,
    requested_limit: usize,
) -> Result<HybridSearchOutcome> {
    let bm25_matches = bm25_search_with_options(
        index,
        query,
        RankingOptions {
            limit: requested_limit,
            semantic_weight: options.semantic_weight,
            bm25_weight: options.bm25_weight,
        },
    )?;
    Ok(HybridSearchOutcome {
        matches: bm25_matches,
        degraded: true,
        degradation_reason: Some(HYBRID_DEGRADATION_REASON.to_string()),
    })
}

fn hybrid_search_with_candidate_limit(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
    candidate_limit: usize,
) -> Result<HybridSearchOutcome> {
    let requested_limit = options.limit.max(1);
    let candidate_limit = candidate_limit.max(1);
    let semantic_matches = match semantic_search_with_options(
        index,
        query,
        RankingOptions {
            limit: candidate_limit,
            semantic_weight: options.semantic_weight,
            bm25_weight: options.bm25_weight,
        },
    ) {
        Ok(matches) => matches,
        // The embedding backend died mid-query: fall back to BM25-only ranking and flag
        // the response as degraded rather than surfacing the raw upstream error.
        Err(IndexError::EmbeddingBackendUnavailable(_)) => {
            return hybrid_degraded_to_bm25(index, query, &options, requested_limit);
        }
        Err(error) => return Err(error),
    };
    let bm25_matches = bm25_search_with_options(
        index,
        query,
        RankingOptions {
            limit: candidate_limit,
            semantic_weight: options.semantic_weight,
            bm25_weight: options.bm25_weight,
        },
    )?;

    // Reciprocal Rank Fusion: rank each retriever independently by its own score
    // (the lists arrive already sorted, so enumeration position is the 0-based rank),
    // then fuse by `score(doc) = Σ_retrievers weight_i / (k + rank_i(doc))`. Rank-based
    // fusion drops the dependence on incomparable cosine vs BM25 scales. The per-list
    // weights default to 1.0 (unweighted); the retained `semantic_weight`/`bm25_weight`
    // fields supply them when a caller wants to tilt the fusion.
    let fused = rrf_fuse(
        &semantic_matches,
        options.semantic_weight,
        &bm25_matches,
        options.bm25_weight,
    );

    // Materialize fused results: carry each retriever's real component score (handy for
    // the public API / display) and set `score` to the fused RRF value.
    let mut semantic_by_key: HashMap<(String, usize), SearchMatch> = HashMap::new();
    for match_item in semantic_matches {
        semantic_by_key.insert((match_item.path.clone(), match_item.chunk_index), match_item);
    }
    let mut bm25_by_key: HashMap<(String, usize), SearchMatch> = HashMap::new();
    for match_item in bm25_matches {
        bm25_by_key.insert((match_item.path.clone(), match_item.chunk_index), match_item);
    }

    let mut matches = fused
        .into_iter()
        .filter_map(|(key, fused_score)| {
            let semantic = semantic_by_key.remove(&key);
            let bm25 = bm25_by_key.remove(&key);
            let semantic_score = semantic.as_ref().map(|item| item.score).unwrap_or(0.0);
            let bm25_score = bm25.as_ref().map(|item| item.score).unwrap_or(0.0);
            // Prefer the semantic record for the displayed text/metadata, falling back
            // to the BM25 record; one of the two must exist for the key to appear.
            let base = semantic.or(bm25)?;
            Some(SearchMatch {
                score: fused_score,
                semantic_score,
                bm25_score,
                ..base
            })
        })
        .filter(|match_item| match_item.score > 0.0)
        .collect::<Vec<_>>();

    let sort_by_fused_score = |matches: &mut Vec<SearchMatch>| {
        matches.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.chunk_index.cmp(&right.chunk_index))
        });
    };
    // Sort by fused score, then lightly re-rank link-adjacent candidates and re-sort.
    // The re-rank runs over the full oversampled candidate set BEFORE truncation, so a
    // 1-hop neighbor that fusion left just outside `requested_limit` can still surface.
    sort_by_fused_score(&mut matches);
    apply_graph_proximity_rerank(index, &mut matches, GRAPH_RERANK_TOP_N, GRAPH_PROXIMITY_BONUS);
    sort_by_fused_score(&mut matches);
    matches.truncate(requested_limit);
    Ok(HybridSearchOutcome {
        matches,
        degraded: false,
        degradation_reason: None,
    })
}

/// Pure Reciprocal Rank Fusion over two ranked lists.
///
/// Each list is assumed already sorted best-first (position 0 is rank 1). A document
/// keyed by `(path, chunk_index)` accumulates `weight_i / (RRF_K + rank_i)` from every
/// list it appears in; a document absent from a list contributes nothing from that list
/// (it never gets a term). Returns `(key, fused_score)` pairs (unordered — the caller
/// sorts), so the math is testable without building an index.
fn rrf_fuse(
    semantic_matches: &[SearchMatch],
    semantic_weight: f64,
    bm25_matches: &[SearchMatch],
    bm25_weight: f64,
) -> HashMap<(String, usize), f64> {
    let mut fused: HashMap<(String, usize), f64> = HashMap::new();
    for (position, match_item) in semantic_matches.iter().enumerate() {
        let contribution = rrf_contribution(position + 1, semantic_weight, RRF_K);
        *fused
            .entry((match_item.path.clone(), match_item.chunk_index))
            .or_insert(0.0) += contribution;
    }
    for (position, match_item) in bm25_matches.iter().enumerate() {
        let contribution = rrf_contribution(position + 1, bm25_weight, RRF_K);
        *fused
            .entry((match_item.path.clone(), match_item.chunk_index))
            .or_insert(0.0) += contribution;
    }
    fused
}

#[cfg(test)]
fn hybrid_search_exhaustive_with_options(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
) -> Result<Vec<SearchMatch>> {
    Ok(
        hybrid_search_with_candidate_limit(index, query, options, index.chunk_count.max(1))?
            .matches,
    )
}

/// Sparse term-overlap related-notes computation: cosine similarity over term counts,
/// plus shared outbound links. Shared by the sparse backend and by the
/// graceful-degradation path when an embedding-backend note has no dense vector.
/// The caller is responsible for sorting and truncating to the requested limit.
fn related_notes_sparse(index: &SearchIndex, note: &SearchNote) -> Vec<RelatedNoteMatch> {
    let note_links: BTreeSet<_> = note.links.iter().cloned().collect();
    index
        .notes
        .iter()
        .filter(|candidate| candidate.path != note.path)
        .map(|candidate| RelatedNoteMatch {
            path: candidate.path.clone(),
            title: candidate.title.clone(),
            score: cosine_similarity(
                &note.term_counts,
                note.norm,
                &candidate.term_counts,
                candidate.norm,
            ),
            shared_links: candidate
                .links
                .iter()
                .filter(|link| note_links.contains(*link))
                .cloned()
                .take(10)
                .collect(),
        })
        .filter(|candidate| candidate.score > 0.0)
        .collect::<Vec<_>>()
}

pub fn related_notes(index: &SearchIndex, note_path: &str) -> Result<Vec<RelatedNoteMatch>> {
    related_notes_with_options(index, note_path, RelatedNoteOptions::default())
}

pub fn related_notes_with_options(
    index: &SearchIndex,
    note_path: &str,
    options: RelatedNoteOptions,
) -> Result<Vec<RelatedNoteMatch>> {
    let note = index
        .note(note_path)
        .ok_or_else(|| IndexError::NoteNotFound(note_path.to_string()))?;
    let mut matches = if index.semantic_backend == SemanticBackend::Embedding {
        match related_notes_with_embeddings_sql(index, note_path, options.limit.max(1)) {
            Ok(matches) => matches,
            // Partial indexes (a note that failed to embed) can leave an
            // Embedding-backend index with no dense vector for this note. Rather than
            // failing the whole call, degrade to the sparse term-overlap path for this
            // note — the same graceful per-query degradation `search` already relies on.
            Err(IndexError::MissingNoteEmbedding(_)) => {
                related_notes_sparse(index, note)
            }
            Err(error) => return Err(error),
        }
    } else {
        related_notes_sparse(index, note)
    };

    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    matches.truncate(options.limit.max(1));
    Ok(matches)
}

pub fn graph_traverse(
    index: &SearchIndex,
    note_path: &str,
    direction: crate::graph::GraphDirection,
    depth: usize,
    limit: usize,
) -> Result<crate::graph::Graph> {
    crate::graph::graph_traverse(index, note_path, direction, depth, limit)
}

pub fn resolve_note_link(index: &SearchIndex, source_path: &str, raw_link: &str) -> Option<String> {
    resolve_wiki_link_target(index, source_path, raw_link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("deep-obsidian-search-{label}-{nanos}-{suffix}"))
    }

    fn write_fixture(root: &Path, relative: &str, content: &str) {
        let absolute = root.join(relative);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&absolute, content).expect("write fixture");
    }

    fn sample_index() -> SearchIndex {
        let root = unique_temp_dir("sample");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(
            &root,
            "Home.md",
            "# Home\n\nInstall the brew service and validate the runtime.\n\nSee [[Projects/Brew Service]] and [[Research/Service Contract]].\n",
        );
        write_fixture(
            &root,
            "Projects/Brew Service.md",
            "# Brew Service\n\nInstall the service and validate the runtime.\n\nReference [[Home]].\n",
        );
        write_fixture(
            &root,
            "Research/Service Contract.md",
            "# Service Contract\n\nInstall the service and validate the runtime.\n\nReference [[Home]].\n",
        );

        let index = crate::index::build_index(&root, None, None).expect("build index");
        fs::remove_dir_all(root).ok();
        index
    }

    fn graph_rerank_index() -> SearchIndex {
        let root = unique_temp_dir("graph-rerank");
        fs::create_dir_all(&root).expect("temp dir");
        // A links to B; C is unlinked. So B is one hop from A, C is not.
        write_fixture(&root, "A.md", "# A\n\nAnchor note. See [[B]].\n");
        write_fixture(&root, "B.md", "# B\n\nLinked neighbour.\n");
        write_fixture(&root, "C.md", "# C\n\nUnlinked note.\n");
        let index = crate::index::build_index(&root, None, None).expect("build index");
        fs::remove_dir_all(root).ok();
        index
    }

    fn match_at(path: &str, score: f64) -> SearchMatch {
        SearchMatch {
            path: path.to_string(),
            title: path.to_string(),
            chunk_index: 0,
            start_line: 1,
            end_line: 1,
            score,
            semantic_score: 0.0,
            bm25_score: 0.0,
            text: String::new(),
        }
    }

    #[test]
    fn graph_rerank_promotes_link_adjacent_candidate() {
        let index = graph_rerank_index();
        // Pre-rerank fused order: A (top), C (mid), B (low). B is 1 hop from the #1 note A,
        // C is unlinked. A small bonus to the link-adjacent B should lift it above C.
        let mut matches = vec![
            match_at("A.md", 0.030),
            match_at("C.md", 0.020),
            match_at("B.md", 0.015),
        ];
        apply_graph_proximity_rerank(&index, &mut matches, 1, 0.010);
        matches.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.path.cmp(&right.path))
        });
        let order: Vec<&str> = matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(
            order,
            vec!["A.md", "B.md", "C.md"],
            "the link-adjacent note B should be promoted above the unlinked C"
        );
    }

    #[test]
    fn graph_rerank_zero_bonus_is_noop() {
        let index = graph_rerank_index();
        let original = vec![
            match_at("A.md", 0.030),
            match_at("C.md", 0.020),
            match_at("B.md", 0.015),
        ];
        let mut matches = original.clone();
        // A bonus of 0 must reproduce pure-RRF state exactly (no score change at all).
        apply_graph_proximity_rerank(&index, &mut matches, GRAPH_RERANK_TOP_N, 0.0);
        assert_eq!(matches, original);
    }

    #[test]
    fn file_search_matches_substring_and_regex() {
        let index = sample_index();
        let substring = find_files(&index, "Brew").expect("substring");
        assert!(substring
            .iter()
            .any(|entry| entry.path == "Projects/Brew Service.md"));
        let regex = find_files_with_options(
            &index,
            "Service Contract\\.md$",
            FindFilesOptions {
                mode: FileSearchMode::Regex,
                limit: 20,
            },
        )
        .expect("regex");
        assert!(regex
            .iter()
            .any(|entry| entry.path == "Research/Service Contract.md"));
    }

    #[test]
    fn bm25_and_semantic_search_rank_related_chunks() {
        let index = sample_index();
        let bm25 = bm25_search(&index, "install service runtime").expect("bm25");
        assert!(!bm25.is_empty());
        assert!(bm25
            .iter()
            .take(2)
            .any(|entry| entry.path == "Projects/Brew Service.md"));

        let semantic = semantic_search(&index, "brew runtime").expect("semantic");
        assert!(!semantic.is_empty());
        assert!(semantic
            .iter()
            .take(2)
            .any(|entry| entry.path == "Projects/Brew Service.md"));
    }

    #[test]
    fn hybrid_search_merges_scores() {
        let index = sample_index();
        let hybrid = hybrid_search(&index, "install runtime").expect("hybrid");
        assert!(!hybrid.is_empty());
        assert!(hybrid[0].score > 0.0);
    }

    /// Build an index over a single multi-section note whose `## Beta` section is oversized
    /// (past the 512-token target) so the chunker splits it; used to exercise small-to-big.
    fn small_to_big_index() -> SearchIndex {
        let root = unique_temp_dir("small-to-big");
        fs::create_dir_all(&root).expect("temp dir");
        // Each paragraph's filler alone exceeds the 512-token target, so the packer puts
        // every paragraph in its OWN sub-chunk: betafirst (with the heading) and betalater
        // (with the query term) land in DIFFERENT sub-chunks.
        let filler = "calibration telemetry harmonic resonance throughput diagnostic \
            subsystem actuator manifold turbine compressor lubrication bearing tolerance "
            .repeat(45);
        let content = format!(
            "# Handbook\n\n## Alpha\nThe alpha overview mentions widgetonium upfront.\n\n\
             ## Beta\nThe betafirst paragraph carries marker_betahead.\n{filler}\n\n\
             The betalater paragraph carries marker_betatail and the term gizmotron.\n{filler}\n\n\
             ## Gamma\nThe gamma section carries marker_gamma and stays separate.\n"
        );
        write_fixture(&root, "Handbook.md", &content);
        let index = crate::index::build_index(&root, None, None).expect("build index");
        fs::remove_dir_all(root).ok();
        index
    }

    #[test]
    fn chunk_hit_expands_to_enclosing_section() {
        let index = small_to_big_index();
        // Confirm the Beta section split into >=2 sub-chunks (otherwise the test is vacuous).
        let beta_subchunks = index
            .chunks
            .iter()
            .filter(|chunk| chunk.text.contains("marker_betahead") || chunk.text.contains("gizmotron"))
            .count();
        assert!(beta_subchunks >= 2, "Beta must split: {beta_subchunks}");

        // The query term lives in the LATER Beta sub-chunk; the returned text must grow to
        // the full Beta section: heading line + first-paragraph sibling marker present,
        // adjacent Alpha/Gamma markers absent.
        // The matched chunk (gizmotron's sub-chunk) on its OWN contains neither the heading
        // nor the first-paragraph sibling marker; only expansion to the full section adds them.
        let matched_chunk = index
            .chunks
            .iter()
            .find(|chunk| chunk.text.contains("gizmotron"))
            .expect("gizmotron chunk");
        assert!(!matched_chunk.text.contains("## Beta"));
        assert!(!matched_chunk.text.contains("marker_betahead"));

        let results = bm25_search(&index, "gizmotron marker_betatail").expect("bm25");
        let hit = results
            .iter()
            .find(|item| item.text.contains("gizmotron"))
            .expect("beta hit");
        assert!(hit.text.contains("## Beta"), "heading line included");
        assert!(hit.text.contains("marker_betahead"), "sibling sub-chunk included");
        assert!(!hit.text.contains("marker_gamma"), "adjacent section excluded");
        assert!(!hit.text.contains("## Alpha"), "previous section excluded");
        // Expansion grew the returned text beyond the matched sub-chunk's own text.
        assert!(hit.text.len() > matched_chunk.text.len(), "returned text expanded");
    }

    #[test]
    fn preamble_chunk_does_not_expand() {
        // A heading-less note falls back to chunk_lines; its chunk starts in the preamble
        // (no enclosing heading section), so the returned text equals the chunk's own text.
        let root = unique_temp_dir("preamble");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(
            &root,
            "Plain.md",
            "Just prose with the distinctive token snorklewump and no headings at all here.\n",
        );
        let index = crate::index::build_index(&root, None, None).expect("build index");
        fs::remove_dir_all(root).ok();

        let results = bm25_search(&index, "snorklewump").expect("bm25");
        let hit = results
            .iter()
            .find(|item| item.path == "Plain.md")
            .expect("plain hit");
        let chunk = index
            .chunks
            .iter()
            .find(|chunk| chunk.path == "Plain.md")
            .expect("plain chunk");
        // No expansion: text and range are exactly the chunk's own.
        assert_eq!(hit.text, chunk.text);
        assert_eq!((hit.start_line, hit.end_line), (chunk.start_line, chunk.end_line));
    }

    /// Build a minimal SearchMatch keyed only by path (chunk_index 0); the score field
    /// is irrelevant to RRF (rank-based), so it is set to 0.0.
    fn ranked(path: &str) -> SearchMatch {
        SearchMatch {
            path: path.to_string(),
            title: path.to_string(),
            chunk_index: 0,
            start_line: 0,
            end_line: 0,
            score: 0.0,
            semantic_score: 0.0,
            bm25_score: 0.0,
            text: String::new(),
        }
    }

    /// Sort an RRF result map into descending (path, score) order for assertions.
    fn fused_order(fused: HashMap<(String, usize), f64>) -> Vec<(String, f64)> {
        let mut entries = fused
            .into_iter()
            .map(|((path, _chunk), score)| (path, score))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        entries
    }

    #[test]
    fn rrf_fuses_known_lists_with_expected_scores() {
        // Two retrievers, identical single-doc lists. Doc gets 1/(60+1) from each.
        let semantic = vec![ranked("A")];
        let bm25 = vec![ranked("A")];
        let fused = rrf_fuse(&semantic, 1.0, &bm25, 1.0);
        let expected = 2.0 / (RRF_K + 1.0);
        assert!((fused[&("A".to_string(), 0)] - expected).abs() < 1e-12);
    }

    #[test]
    fn rrf_surfaces_docs_strong_in_only_one_retriever() {
        // `top_bm25` is #1 in BM25 but mid-pack (#3) in dense; `top_dense` is the
        // mirror image (#1 dense, #3 BM25). Both must survive fusion NEAR THE TOP,
        // ahead of docs that are merely mid-pack in BOTH lists. This is the exact
        // case the issue calls out for RRF.
        let semantic = vec![
            ranked("top_dense"),  // dense rank 1
            ranked("mid_both_a"), // dense rank 2
            ranked("top_bm25"),   // dense rank 3
            ranked("mid_both_b"), // dense rank 4
        ];
        let bm25 = vec![
            ranked("top_bm25"),   // bm25 rank 1
            ranked("mid_both_b"), // bm25 rank 2
            ranked("top_dense"),  // bm25 rank 3
            ranked("mid_both_a"), // bm25 rank 4
        ];

        let order = fused_order(rrf_fuse(&semantic, 1.0, &bm25, 1.0));
        let ranks: Vec<&str> = order.iter().map(|(path, _)| path.as_str()).collect();

        // Both single-retriever leaders fuse to 1/61 + 1/63; the docs that are #2/#4 in
        // both fuse to 1/62 + 1/64 (a). top_bm25/top_dense each = 1/61+1/63 > any mid pair.
        let leader_score = 1.0 / (RRF_K + 1.0) + 1.0 / (RRF_K + 3.0);
        assert!((order[0].1 - leader_score).abs() < 1e-12);
        assert!((order[1].1 - leader_score).abs() < 1e-12);
        // The top two slots are exactly the two single-retriever leaders.
        assert_eq!(&ranks[0..2].iter().copied().collect::<BTreeSet<_>>(), &BTreeSet::from(["top_bm25", "top_dense"]));
        // ...and they outrank both docs that were only ever mid-pack.
        let pos = |name: &str| ranks.iter().position(|item| *item == name).unwrap();
        assert!(pos("top_bm25") < pos("mid_both_a"));
        assert!(pos("top_bm25") < pos("mid_both_b"));
        assert!(pos("top_dense") < pos("mid_both_a"));
        assert!(pos("top_dense") < pos("mid_both_b"));
    }

    #[test]
    fn rrf_absent_doc_contributes_zero() {
        // `only_dense` appears solely in the dense list; its fused score is exactly its
        // single dense contribution (the BM25 side adds nothing).
        let semantic = vec![ranked("shared"), ranked("only_dense")];
        let bm25 = vec![ranked("shared")];
        let fused = rrf_fuse(&semantic, 1.0, &bm25, 1.0);
        let only_dense = fused[&("only_dense".to_string(), 0)];
        assert!((only_dense - 1.0 / (RRF_K + 2.0)).abs() < 1e-12);
        // `shared` is in both at rank 1, so it must outrank the single-list doc.
        let shared = fused[&("shared".to_string(), 0)];
        assert!(shared > only_dense);
    }

    #[test]
    fn rrf_weights_tilt_fusion() {
        // With BM25 weight 0, only the dense ranking matters: a BM25-only doc drops out.
        let semantic = vec![ranked("dense_doc")];
        let bm25 = vec![ranked("bm25_doc")];
        let fused = rrf_fuse(&semantic, 1.0, &bm25, 0.0);
        assert!((fused[&("bm25_doc".to_string(), 0)] - 0.0).abs() < 1e-12);
        assert!(fused[&("dense_doc".to_string(), 0)] > 0.0);
    }

    #[test]
    fn hybrid_search_candidate_pool_is_bounded() {
        assert_eq!(hybrid_candidate_limit(10_000, 8), 64);
        assert_eq!(hybrid_candidate_limit(10_000, 3), 50);
        assert_eq!(hybrid_candidate_limit(10_000, 1_000), 1_000);
        assert_eq!(hybrid_candidate_limit(20, 8), 20);
    }

    #[test]
    fn exhaustive_hybrid_path_matches_default_on_small_indexes() {
        let index = sample_index();
        let options = RankingOptions::default();
        let bounded = hybrid_search_with_options(&index, "install runtime", options.clone())
            .expect("bounded");
        let exhaustive = hybrid_search_exhaustive_with_options(&index, "install runtime", options)
            .expect("exhaustive");
        assert_eq!(bounded, exhaustive);
    }

    #[test]
    fn related_notes_uses_shared_terms_and_links() {
        let index = sample_index();
        let related = related_notes(&index, "Home.md").expect("related");
        assert!(!related.is_empty());
        assert!(related
            .iter()
            .any(|entry| entry.path == "Projects/Brew Service.md"));
    }

    /// Embedding backend returning a valid 3-dim vector for any input that does NOT
    /// contain `sentinel`, and HTTP 500 for any input that does. Lets a build embed
    /// every note except the one whose text carries the sentinel.
    fn start_sentinel_failing_embedding_server(sentinel: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind sentinel server");
        let address = listener.local_addr().expect("server address");
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                let mut buffer = Vec::new();
                let mut header_end = None;
                while header_end.is_none() {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).expect("read request");
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
                }
                let Some(header_end) = header_end.map(|end| end + 4) else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buffer[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().expect("content length"))
                    })
                    .expect("content length header");
                while buffer.len() < header_end + content_length {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).expect("read body");
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let body = &buffer[header_end..header_end + content_length];
                let payload: serde_json::Value =
                    serde_json::from_slice(body).expect("json request");
                let inputs = payload
                    .get("input")
                    .and_then(serde_json::Value::as_array)
                    .expect("input array")
                    .iter()
                    .map(|value| value.as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>();

                let response = if inputs.iter().any(|input| input.contains(sentinel)) {
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                } else {
                    let data = inputs
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            serde_json::json!({
                                "index": index,
                                "embedding": [1.0, index as f64 + 1.0, 0.5]
                            })
                        })
                        .collect::<Vec<_>>();
                    let response_body = serde_json::json!({ "data": data }).to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    )
                };
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        format!("http://{}", address)
    }

    #[test]
    fn related_notes_degrades_to_sparse_when_source_note_has_no_embedding() {
        // FIX 3: on an Embedding-backend index, a note that failed to embed has no
        // dense vector. `related_notes` for that note must degrade to the sparse
        // term-overlap path instead of erroring with MissingNoteEmbedding.
        const SENTINEL: &str = "EMBED_BREAK";
        let root = unique_temp_dir("related-no-embedding");
        fs::create_dir_all(&root).expect("temp dir");
        // The source note carries the sentinel so its embedding fails; it shares terms
        // and a link with a sibling that embeds fine, so sparse still finds a relation.
        write_fixture(
            &root,
            "Source.md",
            &format!("# Source\n\n{SENTINEL} install the service runtime.\n\nSee [[Sibling]].\n"),
        );
        write_fixture(
            &root,
            "Sibling.md",
            "# Sibling\n\nInstall the service runtime.\n\nReference [[Source]].\n",
        );

        let base_url = start_sentinel_failing_embedding_server(SENTINEL);
        let config = crate::embeddings::EmbeddingConfig {
            provider: Some(crate::embeddings::EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: crate::embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: crate::embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: crate::embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: crate::embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: crate::embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();

        let index =
            crate::index::build_index(&root, None, Some(&config)).expect("partial embedding build");

        // Precondition: embedding backend, with the source note left un-embedded.
        assert_eq!(index.semantic_backend, SemanticBackend::Embedding);

        // Must NOT error with MissingNoteEmbedding; degrades to sparse and still finds
        // the sibling via shared terms/links. (The embedding backend reads the
        // persisted sqlite at query time, so keep `root` until after the query.)
        let related = related_notes(&index, "Source.md")
            .expect("related_notes must degrade to sparse, not error on a missing embedding");
        assert!(
            related.iter().any(|entry| entry.path == "Sibling.md"),
            "sparse degradation should still surface the related sibling"
        );

        fs::remove_dir_all(root).ok();
    }

    /// A healthy embedding server that returns a fixed 3-dim vector for every input,
    /// derived from input length so distinct chunks get distinct vectors. Used to build a
    /// real Embedding-backed index; the caller then points the backend down for query-time.
    fn start_healthy_embedding_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind healthy server");
        let address = listener.local_addr().expect("server address");
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                let mut buffer = Vec::new();
                let mut header_end = None;
                while header_end.is_none() {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).expect("read request");
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
                }
                let Some(header_end) = header_end.map(|end| end + 4) else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buffer[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().expect("content length"))
                    })
                    .expect("content length header");
                while buffer.len() < header_end + content_length {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).expect("read body");
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let body = &buffer[header_end..header_end + content_length];
                let payload: serde_json::Value =
                    serde_json::from_slice(body).expect("json request");
                let inputs = payload
                    .get("input")
                    .and_then(serde_json::Value::as_array)
                    .expect("input array")
                    .iter()
                    .map(|value| value.as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>();
                let data = inputs
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        serde_json::json!({
                            "index": index,
                            "embedding": [1.0, text.len() as f64 + 1.0, 0.5],
                        })
                    })
                    .collect::<Vec<_>>();
                let response_body = serde_json::json!({ "data": data }).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        format!("http://{}", address)
    }

    fn embedding_backend_config(base_url: String) -> crate::embeddings::EmbeddingConfig {
        crate::embeddings::EmbeddingConfig {
            provider: Some(crate::embeddings::EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: crate::embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: crate::embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: crate::embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: crate::embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: crate::embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize()
    }

    /// Build an Embedding-backed index over a small fixture vault, embedding via a healthy
    /// server. Returns the index and its `root` (kept alive for query-time sqlite reads).
    fn embedding_backed_index() -> (SearchIndex, PathBuf) {
        let root = unique_temp_dir("embedding-backed");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(
            &root,
            "Home.md",
            "# Home\n\nInstall the brew service and validate the runtime.\n\nSee [[Projects/Brew Service]].\n",
        );
        write_fixture(
            &root,
            "Projects/Brew Service.md",
            "# Brew Service\n\nInstall the service and validate the runtime.\n\nReference [[Home]].\n",
        );
        let base_url = start_healthy_embedding_server();
        let config = embedding_backend_config(base_url);
        let index =
            crate::index::build_index(&root, None, Some(&config)).expect("embedding-backed build");
        assert_eq!(index.semantic_backend, SemanticBackend::Embedding);
        (index, root)
    }

    /// An unroutable base URL: the discard/zero port refuses connections immediately, so a
    /// query-time embed fails fast with a connection error (classified transient).
    const DEAD_BACKEND_URL: &str = "http://127.0.0.1:1";

    #[test]
    fn hybrid_search_degrades_to_bm25_when_backend_unavailable() {
        let (mut index, root) = embedding_backed_index();
        // Healthy baseline: identical results, NOT degraded.
        let healthy = hybrid_search_with_options_degradable(
            &index,
            "install runtime",
            RankingOptions::default(),
        )
        .expect("healthy hybrid");
        assert!(!healthy.degraded, "healthy backend must not degrade");
        assert!(healthy.degradation_reason.is_none());
        assert!(!healthy.matches.is_empty());

        // Point the embedding backend at a refused port: the query-time embed fails with a
        // connection error -> EmbeddingBackendUnavailable -> hybrid degrades to BM25-only.
        index.embedding_base_url = Some(DEAD_BACKEND_URL.to_string());
        let degraded = hybrid_search_with_options_degradable(
            &index,
            "install runtime",
            RankingOptions::default(),
        )
        .expect("hybrid must degrade, not error, when the backend is down");
        assert!(degraded.degraded, "down backend must flag degraded");
        assert_eq!(
            degraded.degradation_reason.as_deref(),
            Some(HYBRID_DEGRADATION_REASON)
        );
        assert!(
            !degraded.matches.is_empty(),
            "BM25 fallback should still return lexical matches"
        );
        // The degraded results are exactly BM25-only ranking at the requested limit.
        let bm25 = bm25_search_with_options(&index, "install runtime", RankingOptions::default())
            .expect("bm25");
        assert_eq!(degraded.matches, bm25);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn hybrid_search_backward_compat_wrapper_degrades_without_erroring() {
        // The legacy `hybrid_search_with_options` (used by find_similar_notes subject mode
        // and recommend_folder) must also degrade internally rather than leak the raw error.
        let (mut index, root) = embedding_backed_index();
        index.embedding_base_url = Some(DEAD_BACKEND_URL.to_string());
        let matches =
            hybrid_search_with_options(&index, "install runtime", RankingOptions::default())
                .expect("legacy wrapper must degrade, not error");
        assert!(!matches.is_empty());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn probe_embedding_backend_reports_reachable_then_unreachable() {
        let (mut index, root) = embedding_backed_index();
        // Healthy backend (the build server is still accepting): reachable.
        assert_eq!(
            probe_embedding_backend(&index),
            EmbeddingBackendHealth::Reachable
        );
        // Point at a refused port: unreachable, but NEVER an Err.
        index.embedding_base_url = Some(DEAD_BACKEND_URL.to_string());
        assert!(matches!(
            probe_embedding_backend(&index),
            EmbeddingBackendHealth::Unreachable(_)
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn probe_embedding_backend_is_not_applicable_for_sparse_index() {
        // A sparse index has no embedding backend to probe.
        let index = sample_index();
        assert_eq!(index.semantic_backend, SemanticBackend::Sparse);
        assert_eq!(
            probe_embedding_backend(&index),
            EmbeddingBackendHealth::NotApplicable
        );
    }

    #[test]
    fn artifact_semantic_search_surfaces_backend_unavailable_error() {
        // search_artifacts has no lexical fallback; a down artifact backend must surface a
        // clear EmbeddingBackendUnavailable error (mapped to an actionable message by the
        // tool layer) rather than leaking the raw upstream body.
        let (mut index, root) = embedding_backed_index();
        // Configure an artifact embedding backend pointed at a refused port.
        index.artifact_embedding_provider = Some("openai-compatible".to_string());
        index.artifact_embedding_model = Some("artifact-embed-test".to_string());
        index.artifact_embedding_base_url = Some(DEAD_BACKEND_URL.to_string());
        index.artifact_embedding_dimensions = Some(3);

        let error = artifact_semantic_search_with_options(
            &index,
            "diagram",
            RankingOptions::default(),
        )
        .expect_err("artifact search must error when its backend is down");
        assert!(
            matches!(error, IndexError::EmbeddingBackendUnavailable(_)),
            "expected EmbeddingBackendUnavailable, got {error:?}"
        );
        fs::remove_dir_all(root).ok();
    }

    // --- Query-side instruction wrapping (asymmetric query encoding) ---

    use crate::embeddings::{
        EmbeddingConfig, EmbeddingProvider, DEFAULT_CHARS_PER_TOKEN,
        DEFAULT_EMBEDDING_BATCH_SIZE, DEFAULT_EMBEDDING_CONTEXT_TOKENS,
        DEFAULT_EMBEDDING_MAX_CHARS, DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
        DEFAULT_SEARCH_QUERY_INSTRUCTION,
    };

    fn embedding_config_with_instruction(instruction: Option<&str>) -> EmbeddingConfig {
        EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("test-embedding-model".to_string()),
            base_url: Some("http://unused".to_string()),
            api_key: None,
            max_chars: DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            query_instruction: instruction.map(str::to_string),
        }
    }

    #[test]
    fn query_input_wraps_in_qwen3_instruction_format_when_set() {
        let config = embedding_config_with_instruction(Some(DEFAULT_SEARCH_QUERY_INSTRUCTION));
        let input = query_embedding_input(&config, "how do embeddings work");
        assert_eq!(
            input,
            format!(
                "Instruct: {DEFAULT_SEARCH_QUERY_INSTRUCTION}\nQuery: how do embeddings work"
            )
        );
    }

    #[test]
    fn query_input_is_raw_when_instruction_is_none() {
        let config = embedding_config_with_instruction(None);
        let input = query_embedding_input(&config, "how do embeddings work");
        assert_eq!(input, "how do embeddings work");
    }

    /// End-to-end query encoding: the wrapped string is what `embed_texts` actually
    /// SENDS to the backend. Spins up a mock embedding server and captures the request
    /// `input` array, asserting the qwen3 wrapper is on the wire.
    #[test]
    fn embed_texts_sends_wrapped_query_to_backend() {
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};
        use std::sync::{Arc, Mutex};

        fn request_body(request: &[u8]) -> Option<&[u8]> {
            let header_end = request.windows(4).position(|w| w == b"\r\n\r\n")?;
            let headers = std::str::from_utf8(&request[..header_end]).ok()?;
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let body_start = header_end + 4;
            (request.len() >= body_start + content_length)
                .then(|| &request[body_start..body_start + content_length])
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("addr");
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_thread = Arc::clone(&captured);
        let handle = thread::spawn(move || {
            let mut stream: TcpStream =
                listener.incoming().next().unwrap().expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).expect("read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request_body(&request).is_some() {
                    break;
                }
            }
            let body = request_body(&request).expect("body");
            let payload: serde_json::Value =
                serde_json::from_slice(body).expect("parse body");
            let inputs = payload
                .get("input")
                .and_then(serde_json::Value::as_array)
                .expect("input array")
                .iter()
                .map(|v| v.as_str().expect("string input").to_string())
                .collect::<Vec<_>>();
            *captured_for_thread.lock().unwrap() = inputs;
            let response_body = serde_json::json!({
                "data": [{ "index": 0, "embedding": [1.0, 2.0, 3.0] }]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("write");
        });

        let mut config =
            embedding_config_with_instruction(Some(DEFAULT_SEARCH_QUERY_INSTRUCTION));
        config.base_url = Some(format!("http://{address}"));
        let input = query_embedding_input(&config, "ranking signals");
        crate::embeddings::embed_texts(&[input], &config).expect("embed");
        handle.join().expect("join mock server");

        let sent = captured.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![format!(
                "Instruct: {DEFAULT_SEARCH_QUERY_INSTRUCTION}\nQuery: ranking signals"
            )]
        );
    }
}
