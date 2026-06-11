use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::graph::resolve_wiki_link_target;
use crate::index::{
    artifact_embedding_runtime_config, average, bm25_score, cosine_similarity, count_terms,
    embedding_runtime_config, matches_pattern, normalize_dense_vector,
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
    let source_embedding = connection
        .query_row(
            r#"
            SELECT v.embedding
            FROM note_embeddings_vec v
            JOIN notes n ON n.id = v.rowid
            WHERE n.path = ?1
            "#,
            params![note_path],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map_err(|_| IndexError::MissingNoteEmbedding(note_path.to_string()))?;

    let mut statement = connection
        .prepare(
            r#"
            SELECT
              n.path,
              n.title,
              matches.distance,
              n.links_json
            FROM (
              SELECT rowid, distance
              FROM note_embeddings_vec
              WHERE embedding MATCH ?1 AND k = ?2
            ) matches
            JOIN notes n ON n.id = matches.rowid
            WHERE n.path <> ?3
            ORDER BY matches.distance
            LIMIT ?4
            "#,
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map(
            params![
                source_embedding,
                (limit.max(1) + 1) as i64,
                note_path,
                limit.max(1) as i64
            ],
            |row| {
                let links: Vec<String> =
                    serde_json::from_str(&row.get::<_, String>(3)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                Ok(RelatedNoteMatch {
                    path: row.get::<_, String>(0)?,
                    title: row.get::<_, String>(1)?,
                    score: sql_distance_score(row.get::<_, f64>(2)?),
                    shared_links: links
                        .into_iter()
                        .filter(|link| note_links.contains(link))
                        .take(10)
                        .collect(),
                })
            },
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| IndexError::Embedding(error.to_string()))
}

fn embed_query(index: &SearchIndex, query: &str) -> Result<Vec<f64>> {
    let config = embedding_runtime_config(index).ok_or(IndexError::MissingEmbeddingConfig)?;
    let result = crate::embeddings::embed_texts(&[query.to_string()], &config)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
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

fn embed_artifact_query(index: &SearchIndex, query: &str) -> Result<Vec<f64>> {
    if let Some(error) = &index.artifact_embedding_error {
        return Err(IndexError::Embedding(format!(
            "artifact embedding unavailable: {error}"
        )));
    }
    let config =
        artifact_embedding_runtime_config(index).ok_or(IndexError::MissingEmbeddingConfig)?;
    let result = crate::embeddings::embed_texts(&[query.to_string()], &config)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
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
    if let Ok(Some(matches)) = bm25_search_with_sql_candidates(index, &query_terms, &options) {
        return Ok(matches);
    }
    bm25_search_in_memory(index, &query_terms, options)
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

pub fn hybrid_search(index: &SearchIndex, query: &str) -> Result<Vec<SearchMatch>> {
    hybrid_search_with_options(index, query, RankingOptions::default())
}

pub fn hybrid_search_with_options(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
) -> Result<Vec<SearchMatch>> {
    let requested_limit = options.limit.max(1);
    let candidate_limit = hybrid_candidate_limit(index.chunk_count, requested_limit);
    hybrid_search_with_candidate_limit(index, query, options, candidate_limit)
}

fn hybrid_search_with_candidate_limit(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
    candidate_limit: usize,
) -> Result<Vec<SearchMatch>> {
    let requested_limit = options.limit.max(1);
    let candidate_limit = candidate_limit.max(1);
    let semantic_matches = semantic_search_with_options(
        index,
        query,
        RankingOptions {
            limit: candidate_limit,
            semantic_weight: options.semantic_weight,
            bm25_weight: options.bm25_weight,
        },
    )?;
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

    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.chunk_index.cmp(&right.chunk_index))
    });
    matches.truncate(requested_limit);
    Ok(matches)
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
    hybrid_search_with_candidate_limit(index, query, options, index.chunk_count.max(1))
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

pub fn backlinks(
    index: &SearchIndex,
    note_path: &str,
    limit: usize,
) -> Result<Vec<crate::graph::BacklinkMatch>> {
    crate::graph::backlinks(index, note_path, limit)
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
}
