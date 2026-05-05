use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::graph::resolve_wiki_link_target;
use crate::index::{
    average, bm25_score, cosine_similarity, count_terms, embedding_runtime_config,
    find_pattern_spans, matches_pattern, normalize_dense_vector, open_index_connection_for_index,
    path_matches_glob, query_vector_blob, vector_norm, IndexError, Result, SearchIndex,
    SemanticBackend,
};
use rusqlite::{params, params_from_iter, OptionalExtension};

const HYBRID_SEARCH_OVERSAMPLE_FACTOR: usize = 8;
const HYBRID_SEARCH_MIN_CANDIDATES: usize = 50;
const HYBRID_SEARCH_CANDIDATE_CAP: usize = 512;

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
pub struct GrepMatch {
    pub path: String,
    pub line_number: usize,
    pub submatches: Vec<GrepSubmatch>,
    pub line_text: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepOptions {
    pub regex: bool,
    pub case_sensitive: bool,
    pub glob: Option<String>,
    pub context_lines: usize,
    pub limit: usize,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
            limit: 50,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankingOptions {
    pub limit: usize,
    pub semantic_weight: f64,
    pub bm25_weight: f64,
}

impl Default for RankingOptions {
    fn default() -> Self {
        Self {
            limit: 8,
            semantic_weight: 0.6,
            bm25_weight: 0.4,
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

fn normalize_scores(scores: &[f64]) -> Vec<f64> {
    let max_score = scores.iter().copied().fold(0.0_f64, f64::max);
    scores
        .iter()
        .copied()
        .map(|score| {
            if max_score > 0.0 {
                score / max_score
            } else {
                0.0
            }
        })
        .collect()
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

pub fn grep_search(index: &SearchIndex, query: &str) -> Result<Vec<GrepMatch>> {
    grep_search_with_options(index, query, GrepOptions::default())
}

pub fn grep_search_with_options(
    index: &SearchIndex,
    query: &str,
    options: GrepOptions,
) -> Result<Vec<GrepMatch>> {
    let limit = options.limit.max(1);
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let mut matches = Vec::new();

    for note in &index.notes {
        if let Some(glob) = options.glob.as_deref() {
            if !path_matches_glob(&note.path, glob)? {
                continue;
            }
        }

        for (line_number, line) in note.content.split('\n').enumerate() {
            let mut submatches = Vec::new();
            if options.regex {
                for (start, end) in find_pattern_spans(line, query, options.case_sensitive)? {
                    submatches.push(GrepSubmatch {
                        start,
                        end,
                        text: line[start..end].to_string(),
                    });
                }
            } else {
                let query_text = if options.case_sensitive {
                    query.to_string()
                } else {
                    query.to_lowercase()
                };
                let line_text = if options.case_sensitive {
                    line.to_string()
                } else {
                    line.to_lowercase()
                };
                let mut search_start = 0;
                while let Some(relative_start) = line_text[search_start..].find(&query_text) {
                    let start = search_start + relative_start;
                    let end = start + query_text.len();
                    submatches.push(GrepSubmatch {
                        start,
                        end,
                        text: line[start..end].to_string(),
                    });
                    search_start = end;
                }
            }

            if submatches.is_empty() {
                continue;
            }

            matches.push(GrepMatch {
                path: note.path.clone(),
                line_number: line_number + 1,
                submatches,
                line_text: line.to_string(),
            });

            if matches.len() >= limit {
                return Ok(matches);
            }
        }
    }

    Ok(matches)
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

    let semantic_scores = normalize_scores(
        &semantic_matches
            .iter()
            .map(|item| item.score)
            .collect::<Vec<_>>(),
    );
    let bm25_scores = normalize_scores(
        &bm25_matches
            .iter()
            .map(|item| item.score)
            .collect::<Vec<_>>(),
    );

    let mut combined: HashMap<(String, usize), SearchMatch> = HashMap::new();
    for (index_position, match_item) in semantic_matches.into_iter().enumerate() {
        combined.insert(
            (match_item.path.clone(), match_item.chunk_index),
            SearchMatch {
                semantic_score: semantic_scores.get(index_position).copied().unwrap_or(0.0),
                score: 0.0,
                bm25_score: 0.0,
                ..match_item
            },
        );
    }

    for (index_position, match_item) in bm25_matches.into_iter().enumerate() {
        let normalized = bm25_scores.get(index_position).copied().unwrap_or(0.0);
        let key = (match_item.path.clone(), match_item.chunk_index);
        if let Some(existing) = combined.get_mut(&key) {
            existing.bm25_score = normalized;
            continue;
        }
        combined.insert(
            key,
            SearchMatch {
                semantic_score: 0.0,
                bm25_score: normalized,
                score: 0.0,
                ..match_item
            },
        );
    }

    let mut matches = combined
        .into_values()
        .map(|mut match_item| {
            match_item.score = options.semantic_weight * match_item.semantic_score
                + options.bm25_weight * match_item.bm25_score;
            match_item
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

#[cfg(test)]
fn hybrid_search_exhaustive_with_options(
    index: &SearchIndex,
    query: &str,
    options: RankingOptions,
) -> Result<Vec<SearchMatch>> {
    hybrid_search_with_candidate_limit(index, query, options, index.chunk_count.max(1))
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
    let note_links: BTreeSet<_> = note.links.iter().cloned().collect();
    let mut matches = if index.semantic_backend == SemanticBackend::Embedding {
        related_notes_with_embeddings_sql(index, note_path, options.limit.max(1))?
    } else {
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
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    fn grep_search_returns_line_matches() {
        let index = sample_index();
        let matches = grep_search(&index, "install").expect("grep");
        assert!(!matches.is_empty());
        assert!(matches.iter().any(|entry| entry.path == "Home.md"));
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
}
