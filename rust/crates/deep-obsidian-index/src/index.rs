use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use rusqlite::{params, types::Type, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::embeddings::{self, EmbeddingBatchOptions, EmbeddingConfig, EmbeddingProvider};
use crate::sqlite;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticBackend {
    Sparse,
    Embedding,
}

impl SemanticBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            SemanticBackend::Sparse => "sparse",
            SemanticBackend::Embedding => "embedding",
        }
    }
}

impl Default for SemanticBackend {
    fn default() -> Self {
        SemanticBackend::Sparse
    }
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("vault path does not exist or is not a directory: {0}")]
    InvalidVaultPath(PathBuf),
    #[error("invalid vault-relative path: {0}")]
    InvalidVaultRelativePath(String),
    #[error("pattern error for {pattern:?}: {message}")]
    InvalidRegex { pattern: String, message: String },
    #[error("io error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("semantic backend {0} is not supported by the sparse-only index implementation")]
    UnsupportedSemanticBackend(String),
    #[error("note not found in index: {0}")]
    NoteNotFound(String),
    #[error("embedding vector dimensions mismatch: expected {expected}, got {actual}")]
    EmbeddingDimensionsMismatch { expected: usize, actual: usize },
    #[error("embedding unavailable for note: {0}")]
    MissingNoteEmbedding(String),
    #[error("embedding configuration is not available in the loaded index")]
    MissingEmbeddingConfig,
    #[error("index context is not available for SQLite-backed vector operations")]
    MissingIndexContext,
    #[error("embedding error: {0}")]
    Embedding(String),
}

pub type Result<T> = std::result::Result<T, IndexError>;

fn is_protected_template_segment(segment: &str) -> bool {
    segment.eq_ignore_ascii_case("template") || segment.eq_ignore_ascii_case("templates")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub path: String,
    pub mtime_ms: u64,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSnapshot {
    pub path: String,
    pub mtime_ms: u64,
    pub size: u64,
    pub mime_type: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchNote {
    pub path: String,
    pub title: String,
    pub content: String,
    pub term_counts: BTreeMap<String, usize>,
    pub norm: f64,
    pub token_count: usize,
    pub links: Vec<String>,
    pub embedding: Option<Vec<f64>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchChunk {
    pub path: String,
    pub title: String,
    pub chunk_index: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub term_counts: BTreeMap<String, usize>,
    pub norm: f64,
    pub token_count: usize,
    pub embedding: Option<Vec<f64>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchArtifact {
    pub path: String,
    pub kind: String,
    pub mime_type: String,
    pub size: u64,
    pub title: String,
    pub metadata_json: String,
    pub embedding: Option<Vec<f64>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchIndex {
    pub version: u32,
    pub generated_at: String,
    pub semantic_backend: SemanticBackend,
    pub embedding_provider: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: Option<usize>,
    pub embedding_base_url: Option<String>,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub runtime_embedding_api_key: Option<String>,
    /// Explicit user-configured query instruction injected at load (query-side only,
    /// not persisted). When `None`, an auto-default is derived from the model name in
    /// `embedding_runtime_config`. Mirrors `runtime_embedding_api_key`.
    #[serde(skip_serializing, skip_deserializing, default)]
    pub runtime_query_instruction: Option<String>,
    pub artifact_embedding_provider: Option<String>,
    pub artifact_embedding_model: Option<String>,
    pub artifact_embedding_dimensions: Option<usize>,
    pub artifact_embedding_base_url: Option<String>,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub runtime_artifact_embedding_api_key: Option<String>,
    pub artifact_embedding_error: Option<String>,
    pub file_snapshots: Vec<FileSnapshot>,
    pub artifact_snapshots: Vec<ArtifactSnapshot>,
    pub document_frequencies: BTreeMap<String, usize>,
    pub chunk_count: usize,
    pub note_count: usize,
    pub artifact_count: usize,
    pub vectorized_artifact_count: usize,
    pub skipped_artifact_count: usize,
    pub notes: Vec<SearchNote>,
    pub chunks: Vec<SearchChunk>,
    pub artifacts: Vec<SearchArtifact>,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub context: Option<IndexContext>,
}

impl SearchIndex {
    pub fn note(&self, note_path: &str) -> Option<&SearchNote> {
        self.notes.iter().find(|note| note.path == note_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexContext {
    pub vault_path: PathBuf,
    pub index_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct IndexDiff {
    added: Vec<FileSnapshot>,
    modified: Vec<FileSnapshot>,
    deleted: Vec<String>,
    unchanged: Vec<FileSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ArtifactDiff {
    added: Vec<ArtifactSnapshot>,
    modified: Vec<ArtifactSnapshot>,
    deleted: Vec<String>,
    unchanged: Vec<ArtifactSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
struct PreparedNote {
    snapshot: FileSnapshot,
    note: SearchNote,
    chunks: Vec<SearchChunk>,
    chunk_texts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct PreparedArtifact {
    snapshot: ArtifactSnapshot,
    artifact: SearchArtifact,
    bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedIndexHeader {
    pub version: u32,
    pub semantic_backend: SemanticBackend,
    pub embedding_provider: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: Option<usize>,
    pub artifact_embedding_provider: Option<String>,
    pub artifact_embedding_model: Option<String>,
    pub artifact_embedding_dimensions: Option<usize>,
    pub file_snapshots: Vec<FileSnapshot>,
    pub artifact_snapshots: Vec<ArtifactSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeadingSection {
    pub level: usize,
    pub title: String,
    pub slug: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockSection {
    pub id: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

const INDEX_VERSION: u32 = sqlite::CURRENT_SCHEMA_VERSION;
const DEFAULT_CHUNK_SIZE_LINES: usize = 80;
const DEFAULT_CHUNK_OVERLAP_LINES: usize = 12;
/// Character ceiling per chunk. Kept well below the per-input embedding budget so a
/// chunk plus its `"{title}\n"` prefix stays comfortably under the backend context
/// window (~2,400 estimated tokens at 2.5 chars/token, safe under a 4096 `num_ctx`).
/// Only bites on dense long-line content; typical 80-line prose chunks are smaller.
const DEFAULT_CHUNK_MAX_CHARS: usize = 6_000;
/// Target upper bound (in `tokenize` tokens) for a section-based chunk. A heading
/// section whose token count exceeds this is split (sub-heading -> paragraph ->
/// hard-wrap) until each piece fits. Chosen at the top of the common 256-512 dense
/// retrieval window so chunks carry enough context without diluting the embedding.
const SECTION_CHUNK_TARGET_TOKENS: usize = 512;
/// Floor (in `tokenize` tokens) below which adjacent sibling sections are merged so
/// the index does not fill with single-line micro-chunks. Merging stops once the
/// accumulated chunk reaches this size or a heading-hierarchy boundary is crossed.
const SECTION_CHUNK_MIN_TOKENS: usize = 100;
const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 25 * 1024 * 1024;
/// Per-note timeout used by the resilience fallback (`embed_prepared_notes_per_note`).
/// Deliberately much SHORTER than the 60s bulk timeout: the fallback runs one HTTP
/// call per note sequentially, so against a backend that accepts the connection but
/// hangs, a 60s-per-note budget would turn into hours of apparent freeze on a large
/// vault. A tight per-note budget bounds each attempt; the short-circuit below bounds
/// the total number of attempts.
const PER_NOTE_EMBEDDING_TIMEOUT: Duration = Duration::from_secs(15);
/// Consecutive per-note embedding failures tolerated in the fallback before giving up.
/// After this many notes fail back-to-back the backend is treated as unhealthy: the
/// remaining notes are recorded as failed (no dense vector -> BM25/sparse at query
/// time) without further attempts, so a dead/hung backend can't grind through all N.
const MAX_CONSECUTIVE_PER_NOTE_FAILURES: usize = 3;
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "in", "into", "is",
    "it", "of", "on", "or", "that", "the", "this", "to", "with",
];
const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".obsidian",
    ".trash",
    ".deep-obsidian-mcp",
    "node_modules",
];

pub fn ensure_vault_path(vault_path: &Path) -> Result<PathBuf> {
    let resolved = vault_path.to_path_buf();
    match fs::metadata(&resolved) {
        Ok(metadata) if metadata.is_dir() => Ok(resolved),
        Ok(_) | Err(_) => Err(IndexError::InvalidVaultPath(resolved)),
    }
}

pub fn ensure_inside_vault(vault_path: &Path, relative_path: &str) -> Result<PathBuf> {
    let normalized = relative_path.trim_start_matches('/');
    if normalized.is_empty() {
        return Err(IndexError::InvalidVaultRelativePath(
            relative_path.to_string(),
        ));
    }

    if Path::new(normalized).components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(IndexError::InvalidVaultRelativePath(
            relative_path.to_string(),
        ));
    }

    Ok(vault_path.join(normalized))
}

pub fn read_text_file(vault_path: &Path, relative_path: &str) -> Result<String> {
    let absolute = ensure_inside_vault(vault_path, relative_path)?;
    fs::read_to_string(&absolute).map_err(|source| IndexError::Io {
        path: absolute,
        source,
    })
}

pub fn write_text_file(vault_path: &Path, relative_path: &str, text: &str) -> Result<bool> {
    let normalized = relative_path.trim_start_matches('/');
    if Path::new(normalized)
        .components()
        .any(|component| match component {
            std::path::Component::Normal(part) => {
                is_protected_template_segment(&part.to_string_lossy())
            }
            _ => false,
        })
    {
        return Err(IndexError::InvalidVaultRelativePath(
            relative_path.to_string(),
        ));
    }
    let absolute = ensure_inside_vault(vault_path, relative_path)?;
    let created = !absolute.exists();
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent).map_err(|source| IndexError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&absolute, text).map_err(|source| IndexError::Io {
        path: absolute,
        source,
    })?;
    Ok(created)
}

pub fn list_markdown_files(vault_path: &Path) -> Result<Vec<String>> {
    let resolved = ensure_vault_path(vault_path)?;
    let mut files = Vec::new();

    fn walk(root: &Path, current: &Path, files: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(current).map_err(|source| IndexError::Io {
            path: current.to_path_buf(),
            source,
        })? {
            let entry = entry.map_err(|source| IndexError::Io {
                path: current.to_path_buf(),
                source,
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }

            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| IndexError::Io {
                path: path.clone(),
                source,
            })?;
            if file_type.is_dir() {
                if IGNORED_DIRS.iter().any(|ignored| *ignored == name) {
                    continue;
                }
                walk(root, &path, files)?;
                continue;
            }

            if file_type.is_file() && name.to_lowercase().ends_with(".md") {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .iter()
                    .map(|segment| segment.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                files.push(relative);
            }
        }
        Ok(())
    }

    walk(&resolved, &resolved, &mut files)?;
    files.sort();
    Ok(files)
}

fn artifact_mime_and_kind(path: &Path) -> Option<(&'static str, &'static str)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Some(("application/pdf", "pdf")),
        "png" => Some(("image/png", "image")),
        "jpg" | "jpeg" => Some(("image/jpeg", "image")),
        "webp" => Some(("image/webp", "image")),
        "gif" => Some(("image/gif", "image")),
        "mp3" => Some(("audio/mpeg", "audio")),
        "wav" => Some(("audio/wav", "audio")),
        "m4a" => Some(("audio/mp4", "audio")),
        "flac" => Some(("audio/flac", "audio")),
        "ogg" => Some(("audio/ogg", "audio")),
        "mp4" => Some(("video/mp4", "video")),
        "mov" => Some(("video/quicktime", "video")),
        "webm" => Some(("video/webm", "video")),
        "mkv" => Some(("video/x-matroska", "video")),
        _ => None,
    }
}

pub fn artifact_mime_type(path: &str) -> Option<&'static str> {
    artifact_mime_and_kind(Path::new(path)).map(|(mime_type, _)| mime_type)
}

pub fn artifact_kind(path: &str) -> Option<&'static str> {
    artifact_mime_and_kind(Path::new(path)).map(|(_, kind)| kind)
}

pub fn is_supported_artifact_path(path: &str) -> bool {
    artifact_mime_and_kind(Path::new(path)).is_some()
}

pub fn list_artifact_files(vault_path: &Path) -> Result<Vec<String>> {
    let resolved = ensure_vault_path(vault_path)?;
    let mut files = Vec::new();

    fn walk(root: &Path, current: &Path, files: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(current).map_err(|source| IndexError::Io {
            path: current.to_path_buf(),
            source,
        })? {
            let entry = entry.map_err(|source| IndexError::Io {
                path: current.to_path_buf(),
                source,
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }

            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| IndexError::Io {
                path: path.clone(),
                source,
            })?;
            if file_type.is_dir() {
                if IGNORED_DIRS.iter().any(|ignored| *ignored == name) {
                    continue;
                }
                walk(root, &path, files)?;
                continue;
            }

            if file_type.is_file() && artifact_mime_and_kind(&path).is_some() {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .iter()
                    .map(|segment| segment.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                files.push(relative);
            }
        }
        Ok(())
    }

    walk(&resolved, &resolved, &mut files)?;
    files.sort();
    Ok(files)
}

pub fn list_top_level_folders(vault_path: &Path) -> Result<Vec<String>> {
    let resolved = ensure_vault_path(vault_path)?;
    let mut folders = Vec::new();
    for entry in fs::read_dir(&resolved).map_err(|source| IndexError::Io {
        path: resolved.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| IndexError::Io {
            path: resolved.clone(),
            source,
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        let file_type = entry.file_type().map_err(|source| IndexError::Io {
            path: entry.path(),
            source,
        })?;
        if file_type.is_dir() && !name.starts_with('.') && !IGNORED_DIRS.contains(&name.as_str()) {
            folders.push(name);
        }
    }
    folders.sort();
    Ok(folders)
}

pub fn slice_lines(text: &str, start_line: usize, end_line: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = start_line.max(1);
    let end = end_line.max(start);
    let start_index = start.saturating_sub(1).min(lines.len());
    let end_index = end.min(lines.len());
    if start_index >= end_index {
        return String::new();
    }
    lines[start_index..end_index].join("\n")
}

pub fn chunk_lines(
    text: &str,
    chunk_size_lines: usize,
    overlap_lines: usize,
    max_chars: usize,
) -> Vec<(usize, usize, usize, String)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let safe_chunk_size = chunk_size_lines.max(1);
    let safe_overlap = overlap_lines.min(safe_chunk_size.saturating_sub(1));
    let safe_max_chars = max_chars.max(1);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chunk_index = 0;

    while start < lines.len() {
        // Extend the chunk while within BOTH the line budget and the char budget,
        // always ending on a whole-line boundary so start/end line ranges stay valid
        // for `slice_lines`/`read_chunk`. A single line over the char budget becomes
        // its own chunk and is truncated downstream by the embedding input clamp.
        let mut end = start;
        let mut char_count = 0usize;
        while end < lines.len() && (end - start) < safe_chunk_size {
            let line_chars = lines[end].chars().count();
            // +1 approximates the '\n' separator added by `join` between lines.
            let added = if end == start {
                line_chars
            } else {
                line_chars + 1
            };
            if end > start && char_count + added > safe_max_chars {
                break;
            }
            char_count += added;
            end += 1;
        }
        if end == start {
            end = start + 1;
        }

        chunks.push((chunk_index, start + 1, end, lines[start..end].join("\n")));
        if end >= lines.len() {
            break;
        }
        // `.max(start + 1)` guarantees forward progress when a small (char-bounded)
        // chunk is shorter than the overlap window.
        start = end.saturating_sub(safe_overlap).max(start + 1);
        chunk_index += 1;
    }

    chunks
}

/// A planned chunk produced by the section-aware chunker. `heading_path` is the
/// ordered stack of ancestor headings (root-most first, this section's own heading
/// last); the indexer renders it as the embedding prefix. `start_line`/`end_line`
/// are 1-based inclusive source line numbers that round-trip to `slice_lines`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionChunk {
    start_line: usize,
    end_line: usize,
    text: String,
    heading_path: Vec<String>,
    /// Top-level (H1) group id; tiny siblings only merge within the same group so the
    /// merge step never fuses two unrelated top-level sections. The preamble is group 0.
    group_id: usize,
}

/// Returns the fence delimiter (```` ``` ```` or `~~~`) opened or closed by `line`,
/// if the line is a fence marker. A fence marker is a line whose first non-whitespace
/// run is at least three of the same fence char.
fn fence_marker(line: &str) -> Option<char> {
    let trimmed = line.trim_start();
    for marker in ['`', '~'] {
        let run = trimmed.chars().take_while(|ch| *ch == marker).count();
        if run >= 3 {
            return Some(marker);
        }
    }
    None
}

/// True when `line` (already known to be outside a fenced code block) is an ATX
/// heading line (`#`..`######` followed by whitespace).
fn is_atx_heading(line: &str) -> bool {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    (1..=6).contains(&level) && line.chars().nth(level).is_some_and(|ch| ch.is_whitespace())
}

/// True when `line` looks like a markdown table row (`|`-delimited). Used so the
/// blank-line splitter does not slice a borderless run of table rows; table rows are
/// not blank, so they already stay together, but a table-detecting guard keeps the
/// hard-wrap fallback from cutting between rows.
fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') || (trimmed.contains('|') && trimmed.contains("---"))
}

fn token_len(text: &str) -> usize {
    tokenize(text).len()
}

/// A heading boundary discovered by the fence-aware scan: 1-based line, nesting level
/// (`#` count), and the heading title text.
struct HeadingBoundary {
    line: usize,
    level: usize,
    title: String,
}

/// Single fence-aware pass over the note: returns every ATX heading that is NOT inside
/// a fenced code block. `extract_heading_sections` is fence-blind (a `# foo` line inside
/// ```` ```bash ```` reads as a heading), so the chunker does its own scan to guarantee
/// fences are never split.
fn scan_heading_boundaries(lines: &[&str]) -> Vec<HeadingBoundary> {
    let mut boundaries = Vec::new();
    let mut fence: Option<char> = None;
    for (index, line) in lines.iter().enumerate() {
        match fence {
            Some(open) => {
                if fence_marker(line) == Some(open) {
                    fence = None;
                }
            }
            None => {
                if let Some(open) = fence_marker(line) {
                    fence = Some(open);
                } else if is_atx_heading(line) {
                    let level = line.chars().take_while(|ch| *ch == '#').count();
                    boundaries.push(HeadingBoundary {
                        line: index + 1,
                        level,
                        title: line[level..].trim().to_string(),
                    });
                }
            }
        }
    }
    boundaries
}

/// Build the embedding/index prefix from a heading path: `"A › B › C"`. An empty path
/// (heading-less preamble) yields an empty string.
fn render_heading_path(path: &[String]) -> String {
    path.join(" › ")
}

/// Section-aware chunker. Tiles the note into NON-overlapping segments at heading
/// boundaries (each heading owns `[heading_line, next_heading_line)` regardless of the
/// next heading's level, plus a leading preamble segment), carrying the ancestor
/// heading stack as `heading_path`. Tiny adjacent siblings are merged up to
/// `min_tokens`; oversized segments are split sub-heading -> paragraph -> hard-wrap,
/// never cutting inside a fenced code block or table. Returns `None` when the note has
/// no usable headings so the caller can fall back to `chunk_lines`.
fn section_chunks(
    content: &str,
    note_title: &str,
    target_tokens: usize,
    min_tokens: usize,
    max_chars: usize,
) -> Option<Vec<SectionChunk>> {
    let lines: Vec<&str> = content.split('\n').collect();
    let boundaries = scan_heading_boundaries(&lines);
    if boundaries.is_empty() {
        return None;
    }

    // Tile into non-overlapping [start, end) line spans with an ancestor stack.
    let mut tiles: Vec<SectionChunk> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();

    // Preamble before the first heading (if any non-empty content).
    let first_line = boundaries[0].line; // 1-based
    if first_line > 1 {
        let text = lines[0..first_line - 1].join("\n");
        if !text.trim().is_empty() {
            tiles.push(SectionChunk {
                start_line: 1,
                end_line: first_line - 1,
                text,
                heading_path: Vec::new(),
                group_id: 0,
            });
        }
    }

    // Top-level group id: incremented on every level-1 heading so the merge step keeps
    // unrelated H1 sections apart. The preamble and any pre-H1 content stay in group 0.
    let mut group_id = 0usize;

    for (index, boundary) in boundaries.iter().enumerate() {
        if boundary.level == 1 {
            group_id += 1;
        }
        // Pop ancestors that do not strictly enclose this heading.
        while stack.last().is_some_and(|(level, _)| *level >= boundary.level) {
            stack.pop();
        }
        // Heading path: ancestors + this heading. Drop a leading entry that duplicates
        // the note title (the usual `# Title` H1) so the path is not `"T › T › ..."`.
        let mut path: Vec<String> = stack.iter().map(|(_, title)| title.clone()).collect();
        path.push(boundary.title.clone());
        stack.push((boundary.level, boundary.title.clone()));

        let mut display_path = vec![note_title.to_string()];
        for (offset, title) in path.iter().enumerate() {
            if offset == 0 && title == note_title {
                continue;
            }
            display_path.push(title.clone());
        }

        let end_line = boundaries
            .get(index + 1)
            .map(|next| next.line - 1)
            .unwrap_or(lines.len());
        let start_line = boundary.line;
        let text = lines[start_line - 1..end_line].join("\n");
        tiles.push(SectionChunk {
            start_line,
            end_line,
            text,
            heading_path: display_path,
            group_id,
        });
    }

    // Merge tiny adjacent tiles up to the floor, only within the same H1 group, so
    // unrelated top-level sections stay apart.
    let merged = merge_small_tiles(tiles, min_tokens);

    // Split any tile that still exceeds the target token budget.
    let mut chunks = Vec::new();
    for tile in merged {
        split_oversized_tile(&lines, tile, target_tokens, max_chars, &mut chunks);
    }
    Some(chunks)
}

/// "Small-to-big" expansion helper (issue #6 item #4, query-time only). Given a note's
/// full `content` and a 1-based source `line` (typically a chunk hit's `start_line`),
/// return the enclosing heading SECTION as `(start_line, end_line, text)` with 1-based
/// inclusive line numbers and the raw source slice.
///
/// The section is reconstructed with the SAME fence-aware, FLAT tiling the chunker uses
/// (`scan_heading_boundaries`, not the fence-blind / level-aware `extract_heading_sections`):
/// the section is `[b_i.line, b_{i+1}.line - 1]` where `b_i` is the last heading boundary
/// with `b_i.line <= line` and `b_{i+1}` is the next boundary of ANY level (or end of note).
/// This mirrors the indexer's `section_chunks` tiling exactly, so the returned span is the
/// coherent unit the oversize splitter may have cut into multiple sub-chunks — never the
/// whole note. Returns `None` when `line` precedes the first heading (preamble /
/// heading-less note), so callers fall back to the chunk's own text/range (no expansion).
pub(crate) fn enclosing_heading_section(
    content: &str,
    line: usize,
) -> Option<(usize, usize, String)> {
    let lines: Vec<&str> = content.split('\n').collect();
    let boundaries = scan_heading_boundaries(&lines);
    // Last boundary at or before `line`; `None` when the line sits in the preamble.
    let index = boundaries
        .iter()
        .rposition(|boundary| boundary.line <= line)?;
    let start_line = boundaries[index].line;
    let end_line = boundaries
        .get(index + 1)
        .map(|next| next.line - 1)
        .unwrap_or(lines.len());
    // Guard against a malformed/out-of-range line that lands past the section's tail.
    if start_line > end_line || start_line > lines.len() {
        return None;
    }
    let text = lines[start_line - 1..end_line].join("\n");
    Some((start_line, end_line, text))
}

fn merge_small_tiles(tiles: Vec<SectionChunk>, min_tokens: usize) -> Vec<SectionChunk> {
    let mut merged: Vec<SectionChunk> = Vec::new();
    for tile in tiles {
        if let Some(previous) = merged.last_mut() {
            let prev_small = token_len(&previous.text) < min_tokens;
            let same_group = previous.group_id == tile.group_id;
            let contiguous = tile.start_line == previous.end_line + 1;
            if prev_small && same_group && contiguous {
                previous.text.push('\n');
                previous.text.push_str(&tile.text);
                previous.end_line = tile.end_line;
                // Keep the shallower/earlier heading path (already set on `previous`).
                continue;
            }
        }
        merged.push(tile);
    }
    merged
}

/// Recursively split a tile that exceeds `target_tokens`: first at contained
/// sub-headings, then at blank-line (paragraph) boundaries, then hard-wrap by lines.
/// Fence and table runs are never cut. Pushes finished pieces onto `out`.
fn split_oversized_tile(
    lines: &[&str],
    tile: SectionChunk,
    target_tokens: usize,
    max_chars: usize,
    out: &mut Vec<SectionChunk>,
) {
    if token_len(&tile.text) <= target_tokens {
        out.push(tile);
        return;
    }

    // 1) Split at sub-headings strictly deeper than the tile's own heading. The tile's
    //    own heading is on its first line; find deeper headings inside the body.
    let body_start = tile.start_line; // 1-based
    let body_end = tile.end_line; // 1-based inclusive
    let own_level = lines
        .get(body_start - 1)
        .and_then(|line| {
            if is_atx_heading(line) {
                Some(line.chars().take_while(|ch| *ch == '#').count())
            } else {
                None
            }
        })
        .unwrap_or(0);

    let inner = &lines[body_start - 1..body_end];
    let inner_boundaries = scan_heading_boundaries(inner);
    let subheadings: Vec<&HeadingBoundary> = inner_boundaries
        .iter()
        .filter(|boundary| boundary.line > 1 && boundary.level > own_level)
        .collect();

    if !subheadings.is_empty() {
        // Cut at each sub-heading; the segment before the first sub-heading keeps the
        // tile's heading path, later segments append their sub-heading title.
        let mut cut_points: Vec<usize> = vec![1]; // 1-based offsets within `inner`
        for boundary in &subheadings {
            cut_points.push(boundary.line);
        }
        cut_points.push(inner.len() + 1);
        cut_points.dedup();

        for window in cut_points.windows(2) {
            let seg_start = window[0];
            let seg_end = window[1] - 1;
            if seg_start > seg_end {
                continue;
            }
            let text = inner[seg_start - 1..seg_end].join("\n");
            if text.trim().is_empty() {
                continue;
            }
            let mut path = tile.heading_path.clone();
            if seg_start > 1 {
                // This segment starts at a sub-heading; append its title.
                if let Some(line) = inner.get(seg_start - 1) {
                    let level = line.chars().take_while(|ch| *ch == '#').count();
                    if is_atx_heading(line) {
                        path.push(line[level..].trim().to_string());
                    }
                }
            }
            let child = SectionChunk {
                start_line: body_start + seg_start - 1,
                end_line: body_start + seg_end - 1,
                text,
                heading_path: path,
                group_id: tile.group_id,
            };
            split_oversized_tile(lines, child, target_tokens, max_chars, out);
        }
        return;
    }

    // 2) No sub-headings: greedily pack the body into <=target_tokens chunks. Cuts land
    //    on line boundaries, preferring paragraph (blank-line) boundaries, and NEVER
    //    inside a fenced code block or a contiguous table run. A single atomic unit that
    //    alone exceeds the budget is word-split as a last resort.
    let pieces = pack_body(
        inner,
        body_start,
        &tile.heading_path,
        tile.group_id,
        target_tokens,
    );
    out.extend(pieces);
}

/// An atomic run of lines that must never be cut internally: a fenced code block, a
/// contiguous markdown table, or a single non-fence/non-table line. The packer only
/// ever cuts between units, so fences and tables stay whole. `blank_before` marks a
/// paragraph boundary (a blank line precedes this unit), the preferred cut point.
struct AtomicUnit {
    start: usize, // 0-based index within `inner`, inclusive
    end: usize,   // 0-based index within `inner`, exclusive
    blank_before: bool,
}

/// Partition `inner` into atomic units (fence blocks, table runs, single lines).
fn atomic_units(inner: &[&str]) -> Vec<AtomicUnit> {
    let mut units = Vec::new();
    let mut index = 0usize;
    let mut prev_blank = false;
    while index < inner.len() {
        let line = inner[index];
        if let Some(open) = fence_marker(line) {
            // Consume through the matching closing fence (or EOF).
            let start = index;
            index += 1;
            while index < inner.len() {
                let closes = fence_marker(inner[index]) == Some(open);
                index += 1;
                if closes {
                    break;
                }
            }
            units.push(AtomicUnit {
                start,
                end: index,
                blank_before: prev_blank,
            });
            prev_blank = false;
            continue;
        }
        if is_table_row(line) {
            let start = index;
            while index < inner.len() && is_table_row(inner[index]) {
                index += 1;
            }
            units.push(AtomicUnit {
                start,
                end: index,
                blank_before: prev_blank,
            });
            prev_blank = false;
            continue;
        }
        // A plain line. Blank lines are boundaries, not content units.
        if line.trim().is_empty() {
            prev_blank = true;
            index += 1;
            continue;
        }
        units.push(AtomicUnit {
            start: index,
            end: index + 1,
            blank_before: prev_blank,
        });
        prev_blank = false;
        index += 1;
    }
    units
}

/// Greedy token-budget packer over atomic units. `offset` is the 1-based source line of
/// `inner[0]`. Whole lines are preserved (so `start_line`/`end_line` stay valid source
/// offsets); a single unit larger than the budget is word-split into sub-line pieces
/// (their line numbers still point at the source lines, but `text` is a substring).
fn pack_body(
    inner: &[&str],
    offset: usize,
    heading_path: &[String],
    group_id: usize,
    target_tokens: usize,
) -> Vec<SectionChunk> {
    let units = atomic_units(inner);
    let mut pieces: Vec<SectionChunk> = Vec::new();
    // Accumulator over [cur_start, cur_end) line indices (0-based within `inner`).
    let mut cur_start: Option<usize> = None;
    let mut cur_end = 0usize;

    let flush = |pieces: &mut Vec<SectionChunk>, start: usize, end: usize| {
        if start >= end {
            return;
        }
        let text = inner[start..end].join("\n");
        if text.trim().is_empty() {
            return;
        }
        pieces.push(SectionChunk {
            start_line: offset + start,
            end_line: offset + end - 1,
            text,
            heading_path: heading_path.to_vec(),
            group_id,
        });
    };

    for unit in &units {
        let unit_text = inner[unit.start..unit.end].join("\n");
        let unit_tokens = token_len(&unit_text);

        // A single unit larger than budget: flush what we have, then word-split it.
        if unit_tokens > target_tokens && unit.start + 1 == unit.end {
            if let Some(start) = cur_start.take() {
                flush(&mut pieces, start, cur_end);
            }
            word_split_line(
                inner[unit.start],
                offset + unit.start,
                heading_path,
                group_id,
                target_tokens,
                &mut pieces,
            );
            continue;
        }

        match cur_start {
            None => {
                cur_start = Some(unit.start);
                cur_end = unit.end;
            }
            Some(start) => {
                let accumulated = token_len(&inner[start..cur_end].join("\n"));
                // Cut before this unit when adding it would overflow the budget, or when
                // this unit begins a new paragraph (blank line before it) and we have
                // already filled the budget -- preferring paragraph boundaries keeps
                // related prose together. Either way the cut is on a whole-line boundary
                // between atomic units, so fences and tables are never split.
                let overflow = accumulated + unit_tokens > target_tokens;
                let paragraph_cut = unit.blank_before && accumulated >= target_tokens;
                if overflow || paragraph_cut {
                    flush(&mut pieces, start, cur_end);
                    cur_start = Some(unit.start);
                    cur_end = unit.end;
                } else {
                    cur_end = unit.end;
                }
            }
        }
    }
    if let Some(start) = cur_start {
        flush(&mut pieces, start, cur_end);
    }
    pieces
}

/// Word-split a single over-budget line into <=`target_tokens` pieces. Both pieces map
/// to the same source `line_number` (1-based); `text` is a whitespace-delimited
/// substring, so it does not equal the full source line (documented sub-line caveat).
fn word_split_line(
    line: &str,
    line_number: usize,
    heading_path: &[String],
    group_id: usize,
    target_tokens: usize,
    out: &mut Vec<SectionChunk>,
) {
    let budget = target_tokens.max(1);
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() {
        return;
    }
    let mut current = String::new();
    let mut count = 0usize;
    for word in words {
        let word_tokens = token_len(word).max(1);
        if count > 0 && count + word_tokens > budget {
            out.push(SectionChunk {
                start_line: line_number,
                end_line: line_number,
                text: std::mem::take(&mut current),
                heading_path: heading_path.to_vec(),
                group_id,
            });
            count = 0;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
        count += word_tokens;
    }
    if !current.is_empty() {
        out.push(SectionChunk {
            start_line: line_number,
            end_line: line_number,
            text: current,
            heading_path: heading_path.to_vec(),
            group_id,
        });
    }
}

pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            current.push(ch);
            continue;
        }

        if !current.is_empty() {
            if current.len() > 1 && !STOPWORDS.contains(&current.as_str()) {
                tokens.push(current.clone());
            }
            current.clear();
        }
    }

    if !current.is_empty() && current.len() > 1 && !STOPWORDS.contains(&current.as_str()) {
        tokens.push(current);
    }

    tokens
}

pub fn count_terms(text: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for token in tokenize(text) {
        *counts.entry(token).or_insert(0) += 1;
    }
    counts
}

pub fn token_count(term_counts: &BTreeMap<String, usize>) -> usize {
    term_counts.values().sum()
}

pub fn vector_norm(term_counts: &BTreeMap<String, usize>) -> f64 {
    let sum: f64 = term_counts
        .values()
        .map(|value| (*value as f64) * (*value as f64))
        .sum();
    sum.sqrt()
}

pub fn frontmatter_title(content: &str) -> Option<String> {
    if !content.starts_with("---\n") {
        return None;
    }

    for line in content.split('\n').skip(1) {
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("title:") {
            return Some(
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
    }

    None
}

pub fn heading_title(content: &str) -> Option<String> {
    content.split('\n').find_map(|line| {
        line.strip_prefix("# ")
            .map(|title| title.trim().to_string())
    })
}

pub fn note_title(path_stem: &str, content: &str) -> String {
    frontmatter_title(content)
        .or_else(|| heading_title(content))
        .unwrap_or_else(|| path_stem.to_string())
}

pub fn extract_wiki_links(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut remaining = content;
    while let Some(start) = remaining.find("[[") {
        remaining = &remaining[start + 2..];
        if let Some(end) = remaining.find("]]") {
            let link = remaining[..end].split('|').next().unwrap_or("").trim();
            if !link.is_empty() {
                links.push(link.to_string());
            }
            remaining = &remaining[end + 2..];
        } else {
            break;
        }
    }
    links
}

fn normalize_heading_slug_value(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    for ch in text.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch.is_ascii_whitespace() || ch == '-' {
            cleaned.push(ch);
        } else if "`*_~[](){}<>#!?.,:;'\\/".contains(ch) {
            continue;
        } else {
            cleaned.push(' ');
        }
    }

    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .trim_matches('-')
        .to_string()
}

pub fn extract_heading_sections(content: &str) -> Vec<HeadingSection> {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut headings = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let level = line.chars().take_while(|ch| *ch == '#').count();
        if level == 0 {
            continue;
        }
        let Some(next) = line.chars().nth(level) else {
            continue;
        };
        if !next.is_whitespace() {
            continue;
        }
        let title = line[level..].trim().to_string();
        headings.push((
            level,
            title.clone(),
            normalize_heading_slug_value(&title),
            index + 1,
        ));
    }

    let mut sections = Vec::new();
    for (index, (level, title, slug, start_line)) in headings.iter().enumerate() {
        let mut end_line = lines.len().max(*start_line);
        for next in headings.iter().skip(index + 1) {
            if next.0 <= *level {
                end_line = next.3.saturating_sub(1);
                break;
            }
        }
        sections.push(HeadingSection {
            level: *level,
            title: title.clone(),
            slug: slug.clone(),
            start_line: *start_line,
            end_line,
            text: lines[start_line.saturating_sub(1)..end_line].join("\n"),
        });
    }

    sections
}

pub fn extract_block_sections(content: &str) -> Vec<BlockSection> {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut blocks = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_end();
        let Some(caret) = trimmed.rfind('^') else {
            continue;
        };
        let id = trimmed[caret + 1..].trim();
        if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') {
            continue;
        }

        let inline_text = trimmed[..caret].trim();
        if !inline_text.is_empty() {
            blocks.push(BlockSection {
                id: id.to_string(),
                start_line: index + 1,
                end_line: index + 1,
                text: inline_text.to_string(),
            });
            continue;
        }

        let mut start_line = index;
        while start_line > 0 {
            let previous = lines[start_line - 1];
            if previous.trim().is_empty() || previous.starts_with('#') {
                break;
            }
            start_line -= 1;
        }

        blocks.push(BlockSection {
            id: id.to_string(),
            start_line: start_line + 1,
            end_line: index + 1,
            text: lines[start_line..index].join("\n").trim().to_string(),
        });
    }

    blocks
}

pub fn now_utc_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn collect_snapshots(vault_path: &Path) -> Result<Vec<FileSnapshot>> {
    let files = list_markdown_files(vault_path)?;
    let mut snapshots = Vec::with_capacity(files.len());

    for relative_path in files {
        let absolute = ensure_inside_vault(vault_path, &relative_path)?;
        let metadata = fs::metadata(&absolute).map_err(|source| IndexError::Io {
            path: absolute.clone(),
            source,
        })?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let mtime_ms = modified
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        snapshots.push(FileSnapshot {
            path: relative_path,
            mtime_ms,
            size: metadata.len(),
        });
    }

    Ok(snapshots)
}

pub fn collect_artifact_snapshots(vault_path: &Path) -> Result<Vec<ArtifactSnapshot>> {
    let files = list_artifact_files(vault_path)?;
    let mut snapshots = Vec::with_capacity(files.len());

    for relative_path in files {
        let absolute = ensure_inside_vault(vault_path, &relative_path)?;
        let metadata = fs::metadata(&absolute).map_err(|source| IndexError::Io {
            path: absolute.clone(),
            source,
        })?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let mtime_ms = modified
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let (mime_type, kind) =
            artifact_mime_and_kind(Path::new(&relative_path)).ok_or_else(|| {
                IndexError::Embedding(format!("unsupported artifact path: {relative_path}"))
            })?;
        snapshots.push(ArtifactSnapshot {
            path: relative_path,
            mtime_ms,
            size: metadata.len(),
            mime_type: mime_type.to_string(),
            kind: kind.to_string(),
        });
    }

    Ok(snapshots)
}

pub fn same_snapshots(left: &[FileSnapshot], right: &[FileSnapshot]) -> bool {
    left == right
}

fn diff_snapshots(existing: &[FileSnapshot], current: &[FileSnapshot]) -> IndexDiff {
    let existing_by_path: BTreeMap<&str, &FileSnapshot> = existing
        .iter()
        .map(|snapshot| (snapshot.path.as_str(), snapshot))
        .collect();
    let current_by_path: BTreeMap<&str, &FileSnapshot> = current
        .iter()
        .map(|snapshot| (snapshot.path.as_str(), snapshot))
        .collect();
    let mut diff = IndexDiff::default();

    for snapshot in current {
        match existing_by_path.get(snapshot.path.as_str()) {
            None => diff.added.push(snapshot.clone()),
            Some(existing_snapshot) if *existing_snapshot != snapshot => {
                diff.modified.push(snapshot.clone());
            }
            Some(_) => diff.unchanged.push(snapshot.clone()),
        }
    }

    for snapshot in existing {
        if !current_by_path.contains_key(snapshot.path.as_str()) {
            diff.deleted.push(snapshot.path.clone());
        }
    }

    diff
}

fn diff_artifact_snapshots(
    existing: &[ArtifactSnapshot],
    current: &[ArtifactSnapshot],
) -> ArtifactDiff {
    let existing_by_path: BTreeMap<&str, &ArtifactSnapshot> = existing
        .iter()
        .map(|snapshot| (snapshot.path.as_str(), snapshot))
        .collect();
    let current_by_path: BTreeMap<&str, &ArtifactSnapshot> = current
        .iter()
        .map(|snapshot| (snapshot.path.as_str(), snapshot))
        .collect();
    let mut diff = ArtifactDiff::default();

    for snapshot in current {
        match existing_by_path.get(snapshot.path.as_str()) {
            None => diff.added.push(snapshot.clone()),
            Some(existing_snapshot) if *existing_snapshot != snapshot => {
                diff.modified.push(snapshot.clone());
            }
            Some(_) => diff.unchanged.push(snapshot.clone()),
        }
    }

    for snapshot in existing {
        if !current_by_path.contains_key(snapshot.path.as_str()) {
            diff.deleted.push(snapshot.path.clone());
        }
    }

    diff
}

pub fn same_artifact_snapshots(left: &[ArtifactSnapshot], right: &[ArtifactSnapshot]) -> bool {
    left == right
}

pub fn same_semantic_config(
    index: &SearchIndex,
    embedding_config: Option<&EmbeddingConfig>,
) -> bool {
    match embedding_config {
        None => index.semantic_backend == SemanticBackend::Sparse,
        Some(config) if config.is_sparse() => index.semantic_backend == SemanticBackend::Sparse,
        Some(config) => {
            let Some(provider) = config.provider.as_ref().map(EmbeddingProvider::as_str) else {
                return false;
            };
            index.semantic_backend == SemanticBackend::Embedding
                && index.embedding_provider.as_deref() == Some(provider)
                && index.embedding_model.as_deref() == config.model.as_deref()
        }
    }
}

pub fn same_persisted_semantic_config(
    header: &PersistedIndexHeader,
    embedding_config: Option<&EmbeddingConfig>,
) -> bool {
    match embedding_config {
        None => header.semantic_backend == SemanticBackend::Sparse,
        Some(config) if config.is_sparse() => header.semantic_backend == SemanticBackend::Sparse,
        Some(config) => {
            let Some(provider) = config.provider.as_ref().map(EmbeddingProvider::as_str) else {
                return false;
            };
            header.semantic_backend == SemanticBackend::Embedding
                && header.embedding_provider.as_deref() == Some(provider)
                && header.embedding_model.as_deref() == config.model.as_deref()
        }
    }
}

pub fn same_artifact_embedding_config(
    index: &SearchIndex,
    artifact_embedding_config: Option<&EmbeddingConfig>,
) -> bool {
    match artifact_embedding_config {
        None => index.artifact_embedding_provider.is_none(),
        Some(config) if config.is_sparse() => index.artifact_embedding_provider.is_none(),
        Some(config) => {
            let Some(provider) = config.provider.as_ref().map(EmbeddingProvider::as_str) else {
                return false;
            };
            index.artifact_embedding_provider.as_deref() == Some(provider)
                && index.artifact_embedding_model.as_deref() == config.model.as_deref()
        }
    }
}

pub fn same_persisted_artifact_embedding_config(
    header: &PersistedIndexHeader,
    artifact_embedding_config: Option<&EmbeddingConfig>,
) -> bool {
    match artifact_embedding_config {
        None => header.artifact_embedding_provider.is_none(),
        Some(config) if config.is_sparse() => header.artifact_embedding_provider.is_none(),
        Some(config) => {
            let Some(provider) = config.provider.as_ref().map(EmbeddingProvider::as_str) else {
                return false;
            };
            header.artifact_embedding_provider.as_deref() == Some(provider)
                && header.artifact_embedding_model.as_deref() == config.model.as_deref()
        }
    }
}

fn semantic_backend_from_config(embedding_config: Option<&EmbeddingConfig>) -> SemanticBackend {
    embedding_config
        .map(EmbeddingConfig::semantic_backend)
        .unwrap_or(SemanticBackend::Sparse)
}

fn normalized_embedding_config(embedding_config: Option<&EmbeddingConfig>) -> EmbeddingConfig {
    embedding_config
        .cloned()
        .unwrap_or_else(EmbeddingConfig::sparse)
        .normalize()
}

fn apply_runtime_embedding_config(
    index: &mut SearchIndex,
    embedding_config: Option<&EmbeddingConfig>,
) {
    let config = normalized_embedding_config(embedding_config);
    if index.semantic_backend != SemanticBackend::Embedding || !config.supports_embeddings() {
        return;
    }

    index.embedding_base_url = config
        .base_url
        .clone()
        .filter(|value| !value.trim().is_empty());
    index.runtime_embedding_api_key = config
        .api_key
        .clone()
        .filter(|value| !value.trim().is_empty());
    index.runtime_query_instruction = config
        .query_instruction
        .clone()
        .filter(|value| !value.trim().is_empty());
}

fn apply_runtime_artifact_embedding_config(
    index: &mut SearchIndex,
    artifact_embedding_config: Option<&EmbeddingConfig>,
) {
    let config = normalized_embedding_config(artifact_embedding_config);
    if index.artifact_embedding_provider.is_none() || !config.supports_embeddings() {
        return;
    }

    index.artifact_embedding_base_url = config
        .base_url
        .clone()
        .filter(|value| !value.trim().is_empty());
    index.runtime_artifact_embedding_api_key = config
        .api_key
        .clone()
        .filter(|value| !value.trim().is_empty());
}

/// Auto-default query instruction for instruction-tuned embedding models.
///
/// Returns the qwen3-embedding default search-query instruction when the model name
/// looks like an instruct embedding model (case-insensitive substring
/// `qwen3-embedding`); otherwise `None`. Keyed purely on the model-name string so
/// generic (non-qwen3) configs — including the hermetic eval's test config — stay
/// `None` and send raw queries unchanged.
pub fn default_query_instruction_for_model(model: Option<&str>) -> Option<String> {
    let model = model?;
    if model.to_ascii_lowercase().contains("qwen3-embedding") {
        Some(embeddings::DEFAULT_SEARCH_QUERY_INSTRUCTION.to_string())
    } else {
        None
    }
}

pub fn embedding_runtime_config(index: &SearchIndex) -> Option<EmbeddingConfig> {
    if index.semantic_backend != SemanticBackend::Embedding {
        return None;
    }

    // Explicit user override wins; otherwise auto-default for recognized instruct models.
    let query_instruction = index
        .runtime_query_instruction
        .clone()
        .or_else(|| default_query_instruction_for_model(index.embedding_model.as_deref()));

    Some(
        EmbeddingConfig {
            provider: index
                .embedding_provider
                .as_deref()
                .and_then(|provider| match provider {
                    "openai-compatible" => Some(EmbeddingProvider::OpenAiCompatible),
                    _ => None,
                }),
            model: index.embedding_model.clone(),
            base_url: index.embedding_base_url.clone(),
            api_key: index.runtime_embedding_api_key.clone(),
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction,
        }
        .normalize(),
    )
}

pub fn artifact_embedding_runtime_config(index: &SearchIndex) -> Option<EmbeddingConfig> {
    let provider =
        index
            .artifact_embedding_provider
            .as_deref()
            .and_then(|provider| match provider {
                "openai-compatible" => Some(EmbeddingProvider::OpenAiCompatible),
                _ => None,
            })?;

    Some(
        EmbeddingConfig {
            provider: Some(provider),
            model: index.artifact_embedding_model.clone(),
            base_url: index.artifact_embedding_base_url.clone(),
            api_key: index.runtime_artifact_embedding_api_key.clone(),
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            // Artifacts are intentionally out of scope for query-instruction wrapping.
            query_instruction: None,
        }
        .normalize(),
    )
}

fn open_index_connection(
    index_file: &Path,
    read_only: bool,
) -> std::result::Result<Connection, rusqlite::Error> {
    sqlite::open_index_connection(index_file, read_only)
}

fn index_file_path(vault_path: &Path, index_dir: Option<&Path>) -> PathBuf {
    sqlite::index_file_path(vault_path, index_dir)
}

fn json_to_sqlite_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
}

fn text_to_json<T: Serialize>(value: &T) -> rusqlite::Result<String> {
    serde_json::to_string(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn parse_json<T: for<'de> Deserialize<'de>>(text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(json_to_sqlite_error)
}

fn parse_json_index<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|error| IndexError::Embedding(error.to_string()))
}

fn metadata_from_connection(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let metadata_exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'metadata'",
            [],
            |_row| Ok(()),
        )
        .optional()
        .map_err(|error| IndexError::Embedding(error.to_string()))?
        .is_some();
    if !metadata_exists {
        return Err(IndexError::Embedding("missing metadata table".to_string()));
    }

    let mut statement = conn
        .prepare("SELECT key, value FROM metadata")
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let mut metadata = BTreeMap::new();
    for row in rows {
        let (key, value) = row.map_err(|error| IndexError::Embedding(error.to_string()))?;
        metadata.insert(key, value);
    }
    Ok(metadata)
}

fn snapshots_from_connection(conn: &Connection) -> Result<Vec<FileSnapshot>> {
    let mut statement = conn
        .prepare("SELECT path, mtime_ms, size FROM file_snapshots ORDER BY path")
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            Ok(FileSnapshot {
                path: row.get::<_, String>(0)?,
                mtime_ms: row.get::<_, i64>(1)? as u64,
                size: row.get::<_, i64>(2)? as u64,
            })
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| IndexError::Embedding(error.to_string()))
}

fn artifact_snapshots_from_connection(conn: &Connection) -> Result<Vec<ArtifactSnapshot>> {
    let mut statement = conn
        .prepare(
            "SELECT path, mtime_ms, size, mime_type, kind FROM artifact_snapshots ORDER BY path",
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            Ok(ArtifactSnapshot {
                path: row.get::<_, String>(0)?,
                mtime_ms: row.get::<_, i64>(1)? as u64,
                size: row.get::<_, i64>(2)? as u64,
                mime_type: row.get::<_, String>(3)?,
                kind: row.get::<_, String>(4)?,
            })
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| IndexError::Embedding(error.to_string()))
}

fn header_from_metadata_and_snapshots(
    metadata: &BTreeMap<String, String>,
    file_snapshots: Vec<FileSnapshot>,
    artifact_snapshots: Vec<ArtifactSnapshot>,
) -> Result<PersistedIndexHeader> {
    let version = metadata
        .get("version")
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| IndexError::Embedding("missing version metadata".to_string()))?;
    if version != INDEX_VERSION {
        return Err(IndexError::Embedding(
            "unsupported index version".to_string(),
        ));
    }

    Ok(PersistedIndexHeader {
        version,
        semantic_backend: match metadata.get("semanticBackend").map(|value| value.as_str()) {
            Some("embedding") => SemanticBackend::Embedding,
            _ => SemanticBackend::Sparse,
        },
        embedding_provider: metadata.get("embeddingProvider").cloned(),
        embedding_model: metadata.get("embeddingModel").cloned(),
        embedding_dimensions: metadata
            .get("embeddingDimensions")
            .and_then(|value| value.parse::<usize>().ok()),
        artifact_embedding_provider: metadata.get("artifactEmbeddingProvider").cloned(),
        artifact_embedding_model: metadata.get("artifactEmbeddingModel").cloned(),
        artifact_embedding_dimensions: metadata
            .get("artifactEmbeddingDimensions")
            .and_then(|value| value.parse::<usize>().ok()),
        file_snapshots,
        artifact_snapshots,
    })
}

fn embedding_blob(vector: &[f64]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(vector.len() * std::mem::size_of::<f32>());
    for value in vector {
        blob.extend_from_slice(&(*value as f32).to_le_bytes());
    }
    blob
}

pub fn index_context(index: &SearchIndex) -> Result<&IndexContext> {
    index
        .context
        .as_ref()
        .ok_or(IndexError::MissingIndexContext)
}

pub fn open_index_connection_for_index(index: &SearchIndex, read_only: bool) -> Result<Connection> {
    let context = index_context(index)?;
    let index_file = index_file_path(&context.vault_path, context.index_dir.as_deref());
    open_index_connection(&index_file, read_only).map_err(|source| IndexError::Io {
        path: index_file,
        source: std::io::Error::new(std::io::ErrorKind::Other, source),
    })
}

pub fn query_vector_blob(vector: &[f64]) -> Vec<u8> {
    embedding_blob(vector)
}

fn prepare_note_from_snapshot(
    resolved_vault_path: &Path,
    snapshot: &FileSnapshot,
) -> Result<PreparedNote> {
    let absolute = ensure_inside_vault(resolved_vault_path, &snapshot.path)?;
    let content = fs::read_to_string(&absolute).map_err(|source| IndexError::Io {
        path: absolute.clone(),
        source,
    })?;
    let stem = path_stem(&snapshot.path);
    let title = note_title(stem, &content);
    let term_counts = count_terms(&format!("{title}\n{content}"));
    let links = extract_wiki_links(&content);

    let note = SearchNote {
        path: snapshot.path.clone(),
        title: title.clone(),
        content: content.clone(),
        term_counts: term_counts.clone(),
        norm: vector_norm(&term_counts),
        token_count: token_count(&term_counts),
        links,
        embedding: None,
    };

    // Section-aware chunking: tile the note at heading boundaries (carrying the heading
    // path as the embedding/index prefix), merging tiny siblings and splitting oversized
    // sections without cutting fenced code blocks or tables. Heading-less notes (and any
    // section that resists reduction) fall back to the line-based `chunk_lines` path.
    let planned: Vec<(usize, usize, String, String)> = match section_chunks(
        &content,
        &title,
        SECTION_CHUNK_TARGET_TOKENS,
        SECTION_CHUNK_MIN_TOKENS,
        DEFAULT_CHUNK_MAX_CHARS,
    ) {
        Some(sections) => sections
            .into_iter()
            .map(|section| {
                let prefix = render_heading_path(&section.heading_path);
                (section.start_line, section.end_line, section.text, prefix)
            })
            .collect(),
        None => chunk_lines(
            &content,
            DEFAULT_CHUNK_SIZE_LINES,
            DEFAULT_CHUNK_OVERLAP_LINES,
            DEFAULT_CHUNK_MAX_CHARS,
        )
        .into_iter()
        .map(|(_, start_line, end_line, text)| (start_line, end_line, text, title.clone()))
        .collect(),
    };

    let mut chunks = Vec::new();
    let mut chunk_texts = Vec::new();
    for (chunk_index, (start_line, end_line, text, prefix)) in planned.into_iter().enumerate() {
        // The EMBEDDING text carries the structural prefix (note title + heading path) so
        // dense vectors get structural context (issue #6 item 1). BM25 `term_counts`
        // stays scoped to `{title}\n{text}` (unchanged from before) so the lexical signal
        // is not diluted by ancestor heading words repeated across every child chunk.
        // The stored `text` is the raw source slice so start/end offsets round-trip.
        let prefix = if prefix.trim().is_empty() {
            title.clone()
        } else {
            prefix
        };
        let embedding_text = format!("{prefix}\n{text}");
        let term_counts = count_terms(&format!("{title}\n{text}"));
        chunks.push(SearchChunk {
            path: snapshot.path.clone(),
            title: title.clone(),
            chunk_index,
            start_line,
            end_line,
            text: text.clone(),
            term_counts: term_counts.clone(),
            norm: vector_norm(&term_counts),
            token_count: token_count(&term_counts),
            embedding: None,
        });
        chunk_texts.push(embedding_text);
    }

    Ok(PreparedNote {
        snapshot: snapshot.clone(),
        note,
        chunks,
        chunk_texts,
    })
}

fn path_title(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(path)
        .to_string()
}

fn prepare_artifact_from_snapshot(
    resolved_vault_path: &Path,
    snapshot: &ArtifactSnapshot,
    load_bytes: bool,
) -> Result<PreparedArtifact> {
    let absolute = ensure_inside_vault(resolved_vault_path, &snapshot.path)?;
    let bytes = if load_bytes && snapshot.size <= DEFAULT_MAX_ARTIFACT_BYTES {
        Some(fs::read(&absolute).map_err(|source| IndexError::Io {
            path: absolute.clone(),
            source,
        })?)
    } else {
        None
    };
    let metadata_json = text_to_json(&serde_json::json!({
        "mtimeMs": snapshot.mtime_ms,
        "size": snapshot.size,
        "vectorization": if snapshot.size <= DEFAULT_MAX_ARTIFACT_BYTES { "eligible" } else { "skipped-size" },
    }))
    .map_err(|error| IndexError::Embedding(error.to_string()))?;

    Ok(PreparedArtifact {
        snapshot: snapshot.clone(),
        artifact: SearchArtifact {
            path: snapshot.path.clone(),
            kind: snapshot.kind.clone(),
            mime_type: snapshot.mime_type.clone(),
            size: snapshot.size,
            title: path_title(&snapshot.path),
            metadata_json,
            embedding: None,
        },
        bytes,
    })
}

fn embed_prepared_artifacts(
    prepared_artifacts: &mut [PreparedArtifact],
    artifact_config: &EmbeddingConfig,
    expected_dimensions: Option<usize>,
) -> (Option<usize>, Option<String>) {
    if !artifact_config.supports_embeddings() {
        return (None, None);
    }

    let eligible = prepared_artifacts
        .iter()
        .enumerate()
        .filter_map(|(index, prepared)| {
            prepared.bytes.as_ref().map(|bytes| {
                (
                    index,
                    embeddings::ArtifactEmbeddingInput {
                        path: prepared.artifact.path.clone(),
                        kind: prepared.artifact.kind.clone(),
                        mime_type: prepared.artifact.mime_type.clone(),
                        bytes: bytes.clone(),
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return (expected_dimensions, None);
    }

    let inputs = eligible
        .iter()
        .map(|(_, input)| input.clone())
        .collect::<Vec<_>>();
    let result = match embeddings::embed_artifacts(&inputs, artifact_config) {
        Ok(result) => result,
        Err(error) => return (expected_dimensions, Some(error.to_string())),
    };
    let mut observed_dimensions = None;
    if let Err(error) = ensure_embedding_dimensions(
        result.dimensions,
        expected_dimensions,
        &mut observed_dimensions,
    ) {
        return (expected_dimensions, Some(error.to_string()));
    }
    if result.vectors.len() != eligible.len() {
        return (
            expected_dimensions,
            Some(
                "embedding provider returned an unexpected number of artifact vectors".to_string(),
            ),
        );
    }

    for ((artifact_index, _), vector) in eligible.into_iter().zip(result.vectors.into_iter()) {
        prepared_artifacts[artifact_index].artifact.embedding =
            Some(normalize_dense_vector(&vector));
    }

    (observed_dimensions.or(expected_dimensions), None)
}

fn collect_document_frequencies<'a>(
    notes: impl IntoIterator<Item = &'a SearchNote>,
) -> BTreeMap<String, usize> {
    let mut document_frequencies = BTreeMap::new();
    for note in notes {
        let unique_terms: BTreeSet<_> = note.term_counts.keys().cloned().collect();
        for term in unique_terms {
            *document_frequencies.entry(term).or_insert(0) += 1;
        }
    }
    document_frequencies
}

fn ensure_embedding_dimensions(
    actual: usize,
    expected: Option<usize>,
    observed: &mut Option<usize>,
) -> Result<()> {
    if let Some(expected) = expected.or(*observed) {
        if actual != expected {
            return Err(IndexError::EmbeddingDimensionsMismatch { expected, actual });
        }
    }
    *observed = Some(actual);
    Ok(())
}

/// Result of embedding a batch of prepared notes.
///
/// `dimensions` is the observed (or carried-forward expected) embedding width.
/// `failed_paths` lists notes whose dense embedding could NOT be produced; those
/// notes keep `embedding = None` on themselves and their chunks and fall back to
/// BM25/sparse retrieval. The build stays usable and the caller can surface the
/// partial result. When every note embeds successfully, `failed_paths` is empty and
/// the assigned vectors are identical to the all-or-nothing path.
#[derive(Debug)]
struct NoteEmbeddingOutcome {
    dimensions: Option<usize>,
    failed_paths: Vec<String>,
}

fn embed_prepared_notes(
    prepared_notes: &mut [PreparedNote],
    index_config: &EmbeddingConfig,
    expected_dimensions: Option<usize>,
) -> Result<NoteEmbeddingOutcome> {
    if !index_config.supports_embeddings() {
        return Ok(NoteEmbeddingOutcome {
            dimensions: None,
            failed_paths: Vec::new(),
        });
    }

    if prepared_notes.is_empty() {
        return Ok(NoteEmbeddingOutcome {
            dimensions: expected_dimensions,
            failed_paths: Vec::new(),
        });
    }

    // Fast path: embed every chunk across all notes in one batched call, sharing
    // the request packing and concurrency. If this succeeds the assigned vectors
    // are byte-identical to the previous all-or-nothing behavior. Each chunk is
    // bounded to a safe size by `chunk_lines` (char budget) and `clamp_input`
    // (token/char budget), so no input can exceed the backend's context window.
    let chunk_texts = prepared_notes
        .iter()
        .flat_map(|prepared| prepared.chunk_texts.iter().cloned())
        .collect::<Vec<_>>();
    match embeddings::embed_text_batches(&chunk_texts, index_config, None) {
        Ok(chunk_embedding_batch) => {
            let mut observed_dimensions = None;
            ensure_embedding_dimensions(
                chunk_embedding_batch.dimensions,
                expected_dimensions,
                &mut observed_dimensions,
            )?;
            let chunk_embeddings = chunk_embedding_batch
                .vectors
                .into_iter()
                .map(|vector| normalize_dense_vector(&vector))
                .collect::<Vec<_>>();

            let mut chunk_iter = chunk_embeddings.into_iter();
            for prepared in prepared_notes.iter_mut() {
                assign_note_chunk_embeddings(prepared, &mut chunk_iter)?;
            }
            if chunk_iter.next().is_some() {
                return Err(IndexError::Embedding(
                    "embedding provider returned too many chunk vectors".to_string(),
                ));
            }
            Ok(NoteEmbeddingOutcome {
                dimensions: observed_dimensions.or(expected_dimensions),
                failed_paths: Vec::new(),
            })
        }
        Err(bulk_error) => {
            // Only fall back to the note-by-note path for TRANSIENT failures
            // (timeouts, dropped connections, 5xx) that might succeed on smaller,
            // isolated inputs. A DETERMINISTIC failure — a 4xx (bad model name,
            // oversized input, auth), misconfiguration, a contract violation, or a
            // dimension mismatch — would fail identically on every note, so retrying
            // note-by-note would only spray N wasted requests. Surface it instead.
            if !bulk_error.is_transient() {
                tracing::error!(
                    error = %bulk_error,
                    "bulk note embedding failed deterministically; not retrying note-by-note"
                );
                return Err(IndexError::Embedding(bulk_error.to_string()));
            }
            // Resilience path: a single failed batch must not abort the whole
            // build. Re-embed note-by-note so one bad note only drops its own dense
            // vector (falling back to BM25/sparse) instead of nuking the index.
            tracing::warn!(
                error = %bulk_error,
                "bulk note embedding failed transiently; retrying note-by-note for partial progress"
            );
            embed_prepared_notes_per_note(
                prepared_notes,
                index_config,
                expected_dimensions,
                PER_NOTE_EMBEDDING_TIMEOUT,
                MAX_CONSECUTIVE_PER_NOTE_FAILURES,
            )
        }
    }
}

/// Per-note fallback used when the bulk embedding call fails transiently. Embeds each
/// note's chunks in isolation with a TIGHT per-note `timeout` (much shorter than the
/// bulk timeout) so one slow note can't stall the build: notes that fail to embed are
/// recorded in `failed_paths` and left without a dense vector; notes that succeed are
/// assigned and dimension-checked against the rest.
///
/// Two cross-note safeguards keep this bounded against an unhealthy backend:
/// - A genuine `DimensionsMismatch` (between notes, or reported by the backend for a
///   single note) is FATAL and propagated — it signals a schema inconsistency, not a
///   transient per-note hiccup, consistent with the cross-note
///   `ensure_embedding_dimensions` guard.
/// - After `max_consecutive_failures` notes fail back-to-back, the backend is treated
///   as unhealthy: the loop SHORT-CIRCUITS, recording every remaining (unattempted)
///   note as failed without trying it, so a dead/hung backend can't grind through all
///   N notes at `timeout` apiece.
fn embed_prepared_notes_per_note(
    prepared_notes: &mut [PreparedNote],
    index_config: &EmbeddingConfig,
    expected_dimensions: Option<usize>,
    timeout: Duration,
    max_consecutive_failures: usize,
) -> Result<NoteEmbeddingOutcome> {
    let mut observed_dimensions = None;
    let mut failed_paths = Vec::new();
    let mut consecutive_failures = 0usize;
    // Tighten only the timeout; keep the rest of the batch options from config so the
    // per-note path packs requests identically to the bulk path.
    let per_note_options = EmbeddingBatchOptions {
        timeout,
        ..EmbeddingBatchOptions::from_config(index_config)
    };

    let mut notes = prepared_notes.iter_mut();
    for prepared in notes.by_ref() {
        if prepared.chunk_texts.is_empty() {
            // No chunks to embed; matches the bulk path's "no vectors" outcome.
            // Neither a success nor a failure: leave the consecutive counter alone.
            prepared.note.embedding = None;
            continue;
        }

        match embeddings::embed_text_batches(
            &prepared.chunk_texts,
            index_config,
            Some(per_note_options.clone()),
        ) {
            Ok(batch) => {
                if let Err(error) = ensure_embedding_dimensions(
                    batch.dimensions,
                    expected_dimensions,
                    &mut observed_dimensions,
                ) {
                    // A dimension mismatch among notes that DID embed is a genuine
                    // schema inconsistency, not a transient per-note failure.
                    return Err(error);
                }
                let chunk_embeddings = batch
                    .vectors
                    .into_iter()
                    .map(|vector| normalize_dense_vector(&vector))
                    .collect::<Vec<_>>();
                let mut chunk_iter = chunk_embeddings.into_iter();
                assign_note_chunk_embeddings(prepared, &mut chunk_iter)?;
                if chunk_iter.next().is_some() {
                    return Err(IndexError::Embedding(
                        "embedding provider returned too many chunk vectors".to_string(),
                    ));
                }
                // A success breaks any failure streak.
                consecutive_failures = 0;
            }
            // A genuine dimension mismatch reported by the backend stays FATAL even
            // for a single note, mapped to the same structured variant the cross-note
            // `ensure_embedding_dimensions` guard above returns.
            Err(embeddings::EmbeddingError::DimensionsMismatch { expected, actual }) => {
                return Err(IndexError::EmbeddingDimensionsMismatch { expected, actual });
            }
            Err(error) => {
                tracing::warn!(
                    path = %prepared.note.path,
                    error = %error,
                    "embedding failed for note; leaving it without a dense vector (BM25/sparse only)"
                );
                clear_note_embeddings(prepared);
                failed_paths.push(prepared.note.path.clone());
                consecutive_failures += 1;
                if consecutive_failures >= max_consecutive_failures {
                    // Backend looks unhealthy. Stop attempting the rest and record
                    // every remaining note as failed so we don't grind through N
                    // sequential per-note timeouts.
                    let mut short_circuited = 0usize;
                    for remaining in notes.by_ref() {
                        clear_note_embeddings(remaining);
                        failed_paths.push(remaining.note.path.clone());
                        short_circuited += 1;
                    }
                    tracing::error!(
                        consecutive_failures,
                        remaining = short_circuited,
                        "per-note embedding short-circuited after consecutive failures; \
                         remaining notes recorded as failed (BM25/sparse only)"
                    );
                    break;
                }
            }
        }
    }

    Ok(NoteEmbeddingOutcome {
        dimensions: observed_dimensions.or(expected_dimensions),
        failed_paths,
    })
}

/// Assign chunk vectors pulled from `chunk_iter` to a note's chunks, and derive the
/// note vector as the normalized mean of its own chunk vectors. The whole-note text
/// is never embedded (it can exceed the backend context window); mean-pooling reuses
/// the chunk vectors at no extra HTTP cost. A note with no chunk vectors keeps
/// `embedding = None`.
fn assign_note_chunk_embeddings(
    prepared: &mut PreparedNote,
    chunk_iter: &mut impl Iterator<Item = Vec<f64>>,
) -> Result<()> {
    let mut accumulator: Option<Vec<f64>> = None;
    let mut count = 0usize;
    for chunk in &mut prepared.chunks {
        let embedding = chunk_iter.next().ok_or_else(|| {
            IndexError::Embedding("embedding provider returned too few chunk vectors".to_string())
        })?;
        match accumulator.as_mut() {
            Some(acc) if acc.len() == embedding.len() => {
                for (slot, value) in acc.iter_mut().zip(embedding.iter()) {
                    *slot += *value;
                }
                count += 1;
            }
            Some(_) => {}
            None => {
                accumulator = Some(embedding.clone());
                count = 1;
            }
        }
        chunk.embedding = Some(embedding);
    }
    prepared.note.embedding = match accumulator {
        Some(acc) if count > 0 => {
            let mean = acc
                .iter()
                .map(|value| value / count as f64)
                .collect::<Vec<_>>();
            Some(normalize_dense_vector(&mean))
        }
        _ => None,
    };
    Ok(())
}

/// Drop any dense vectors from a note and its chunks so it falls back to BM25/sparse.
fn clear_note_embeddings(prepared: &mut PreparedNote) {
    prepared.note.embedding = None;
    for chunk in &mut prepared.chunks {
        chunk.embedding = None;
    }
}

fn write_search_index_to_connection(conn: &mut Connection, index: &SearchIndex) -> Result<()> {
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS metadata;
        DROP TABLE IF EXISTS file_snapshots;
        DROP TABLE IF EXISTS document_frequencies;
        DROP TABLE IF EXISTS note_embeddings_vec;
        DROP TABLE IF EXISTS chunk_embeddings_vec;
        DROP TABLE IF EXISTS artifact_embeddings_vec;
        DROP TABLE IF EXISTS notes;
        DROP TABLE IF EXISTS chunks;
        DROP TABLE IF EXISTS artifacts;
        DROP TABLE IF EXISTS artifact_snapshots;
        DROP TABLE IF EXISTS embedding_config;
        DROP TABLE IF EXISTS artifact_embedding_config;
        "#,
    )
    .map_err(|error| IndexError::Embedding(error.to_string()))?;
    conn.execute_batch(sqlite::CURRENT_SCHEMA_DDL)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    sqlite::recreate_vector_tables(conn, index.embedding_dimensions)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    sqlite::recreate_artifact_vector_table(conn, index.artifact_embedding_dimensions)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    {
        let mut insert_metadata = tx
            .prepare("INSERT INTO metadata (key, value) VALUES (?1, ?2)")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let metadata_entries: Vec<(&str, String)> = vec![
            ("version", index.version.to_string()),
            ("generatedAt", index.generated_at.clone()),
            (
                "semanticBackend",
                index.semantic_backend.as_str().to_string(),
            ),
            ("chunkCount", index.chunk_count.to_string()),
            ("noteCount", index.note_count.to_string()),
            ("artifactCount", index.artifact_count.to_string()),
            (
                "vectorizedArtifactCount",
                index.vectorized_artifact_count.to_string(),
            ),
            (
                "skippedArtifactCount",
                index.skipped_artifact_count.to_string(),
            ),
        ];
        for (key, value) in metadata_entries {
            insert_metadata
                .execute(params![key, value])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        if let Some(provider) = &index.embedding_provider {
            insert_metadata
                .execute(params!["embeddingProvider", provider])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        if let Some(model) = &index.embedding_model {
            insert_metadata
                .execute(params!["embeddingModel", model])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        if let Some(dimensions) = index.embedding_dimensions {
            insert_metadata
                .execute(params!["embeddingDimensions", dimensions.to_string()])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        if let Some(provider) = &index.artifact_embedding_provider {
            insert_metadata
                .execute(params!["artifactEmbeddingProvider", provider])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        if let Some(model) = &index.artifact_embedding_model {
            insert_metadata
                .execute(params!["artifactEmbeddingModel", model])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        if let Some(dimensions) = index.artifact_embedding_dimensions {
            insert_metadata
                .execute(params![
                    "artifactEmbeddingDimensions",
                    dimensions.to_string()
                ])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        if let Some(error) = &index.artifact_embedding_error {
            insert_metadata
                .execute(params!["artifactEmbeddingError", error])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    {
        let mut insert_runtime_config = tx
            .prepare("INSERT INTO embedding_config (key, value) VALUES (?1, ?2)")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        if let Some(base_url) = &index.embedding_base_url {
            insert_runtime_config
                .execute(params!["baseUrl", base_url])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    {
        let mut insert_runtime_config = tx
            .prepare("INSERT INTO artifact_embedding_config (key, value) VALUES (?1, ?2)")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        if let Some(base_url) = &index.artifact_embedding_base_url {
            insert_runtime_config
                .execute(params!["baseUrl", base_url])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    {
        let mut insert_snapshot = tx
            .prepare("INSERT INTO file_snapshots (path, mtime_ms, size) VALUES (?1, ?2, ?3)")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        for snapshot in &index.file_snapshots {
            insert_snapshot
                .execute(params![
                    snapshot.path,
                    snapshot.mtime_ms as i64,
                    snapshot.size as i64
                ])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    {
        let mut insert_snapshot = tx
            .prepare("INSERT INTO artifact_snapshots (path, mtime_ms, size, mime_type, kind) VALUES (?1, ?2, ?3, ?4, ?5)")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        for snapshot in &index.artifact_snapshots {
            insert_snapshot
                .execute(params![
                    snapshot.path,
                    snapshot.mtime_ms as i64,
                    snapshot.size as i64,
                    snapshot.mime_type,
                    snapshot.kind
                ])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    {
        let mut insert_df = tx
            .prepare("INSERT INTO document_frequencies (term, df) VALUES (?1, ?2)")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        for (term, df) in &index.document_frequencies {
            insert_df
                .execute(params![term, *df as i64])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    {
        let mut insert_note = tx
            .prepare(
                "INSERT INTO notes (id, path, title, content, term_counts_json, norm, token_count, links_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let mut insert_note_embedding = if index.semantic_backend == SemanticBackend::Embedding
            && index.embedding_dimensions.is_some()
        {
            Some(
                tx.prepare("INSERT INTO note_embeddings_vec (rowid, embedding) VALUES (?1, ?2)")
                    .map_err(|error| IndexError::Embedding(error.to_string()))?,
            )
        } else {
            None
        };
        for (id, note) in index.notes.iter().enumerate() {
            insert_note
                .execute(params![
                    (id + 1) as i64,
                    note.path,
                    note.title,
                    note.content,
                    text_to_json(&note.term_counts)
                        .map_err(|error| IndexError::Embedding(error.to_string()))?,
                    note.norm,
                    note.token_count as i64,
                    text_to_json(&note.links)
                        .map_err(|error| IndexError::Embedding(error.to_string()))?,
                ])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;

            if let (Some(insert_embedding), Some(embedding)) =
                (insert_note_embedding.as_mut(), note.embedding.as_ref())
            {
                insert_embedding
                    .execute(params![(id + 1) as i64, embedding_blob(embedding)])
                    .map_err(|error| IndexError::Embedding(error.to_string()))?;
            }
        }
    }

    {
        let mut insert_chunk = tx
            .prepare(
                "INSERT INTO chunks (id, path, title, chunk_index, start_line, end_line, text, term_counts_json, norm, token_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let mut insert_chunk_embedding = if index.semantic_backend == SemanticBackend::Embedding
            && index.embedding_dimensions.is_some()
        {
            Some(
                tx.prepare("INSERT INTO chunk_embeddings_vec (rowid, embedding) VALUES (?1, ?2)")
                    .map_err(|error| IndexError::Embedding(error.to_string()))?,
            )
        } else {
            None
        };
        for (id, chunk) in index.chunks.iter().enumerate() {
            let chunk_id = (id + 1) as i64;
            insert_chunk
                .execute(params![
                    chunk_id,
                    chunk.path,
                    chunk.title,
                    chunk.chunk_index as i64,
                    chunk.start_line as i64,
                    chunk.end_line as i64,
                    chunk.text,
                    text_to_json(&chunk.term_counts)
                        .map_err(|error| IndexError::Embedding(error.to_string()))?,
                    chunk.norm,
                    chunk.token_count as i64,
                ])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;

            insert_chunk_terms(&tx, chunk_id, &chunk.term_counts)?;

            if let (Some(insert_embedding), Some(embedding)) =
                (insert_chunk_embedding.as_mut(), chunk.embedding.as_ref())
            {
                insert_embedding
                    .execute(params![chunk_id, embedding_blob(embedding)])
                    .map_err(|error| IndexError::Embedding(error.to_string()))?;
            }
        }
    }

    {
        let mut insert_artifact = tx
            .prepare(
                "INSERT INTO artifacts (id, path, kind, mime_type, size, title, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let mut insert_artifact_embedding = if index.artifact_embedding_dimensions.is_some() {
            Some(
                tx.prepare(
                    "INSERT INTO artifact_embeddings_vec (rowid, embedding) VALUES (?1, ?2)",
                )
                .map_err(|error| IndexError::Embedding(error.to_string()))?,
            )
        } else {
            None
        };
        for (id, artifact) in index.artifacts.iter().enumerate() {
            let artifact_id = (id + 1) as i64;
            insert_artifact
                .execute(params![
                    artifact_id,
                    artifact.path,
                    artifact.kind,
                    artifact.mime_type,
                    artifact.size as i64,
                    artifact.title,
                    artifact.metadata_json,
                ])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;

            if let (Some(insert_embedding), Some(embedding)) = (
                insert_artifact_embedding.as_mut(),
                artifact.embedding.as_ref(),
            ) {
                insert_embedding
                    .execute(params![artifact_id, embedding_blob(embedding)])
                    .map_err(|error| IndexError::Embedding(error.to_string()))?;
            }
        }
    }

    tx.commit()
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    Ok(())
}

fn load_search_index_from_connection(conn: &Connection) -> Result<SearchIndex> {
    let metadata = metadata_from_connection(conn)?;

    let version = metadata
        .get("version")
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| IndexError::Embedding("missing version metadata".to_string()))?;
    if version != INDEX_VERSION {
        return Err(IndexError::Embedding(
            "unsupported index version".to_string(),
        ));
    }

    let file_snapshots = snapshots_from_connection(conn)?;

    let document_frequencies = {
        let mut statement = conn
            .prepare("SELECT term, df FROM document_frequencies ORDER BY term")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        rows.collect::<std::result::Result<BTreeMap<_, _>, _>>()
            .map_err(|error| IndexError::Embedding(error.to_string()))?
    };

    let runtime_config = {
        let mut statement = conn
            .prepare("SELECT key, value FROM embedding_config")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let mut config = BTreeMap::new();
        for row in rows {
            let (key, value) = row.map_err(|error| IndexError::Embedding(error.to_string()))?;
            config.insert(key, value);
        }
        config
    };

    let artifact_runtime_config = {
        let mut statement = conn
            .prepare("SELECT key, value FROM artifact_embedding_config")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let mut config = BTreeMap::new();
        for row in rows {
            let (key, value) = row.map_err(|error| IndexError::Embedding(error.to_string()))?;
            config.insert(key, value);
        }
        config
    };

    let semantic_backend = match metadata.get("semanticBackend").map(|value| value.as_str()) {
        Some("embedding") => SemanticBackend::Embedding,
        _ => SemanticBackend::Sparse,
    };
    let use_vec_tables = sqlite::has_vector_tables(conn)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    if semantic_backend == SemanticBackend::Embedding && !use_vec_tables {
        return Err(IndexError::Embedding(
            "embedding index is missing sqlite-vec tables".to_string(),
        ));
    }

    let notes = {
        let mut statement = conn
            .prepare("SELECT path, title, content, term_counts_json, norm, token_count, links_json FROM notes ORDER BY path")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok(SearchNote {
                    path: row.get::<_, String>(0)?,
                    title: row.get::<_, String>(1)?,
                    content: row.get::<_, String>(2)?,
                    term_counts: parse_json(&row.get::<_, String>(3)?)?,
                    norm: row.get::<_, f64>(4)?,
                    token_count: row.get::<_, i64>(5)? as usize,
                    links: parse_json(&row.get::<_, String>(6)?)?,
                    embedding: None,
                })
            })
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| IndexError::Embedding(error.to_string()))?
    };

    let chunks = {
        let mut statement = conn
            .prepare("SELECT path, title, chunk_index, start_line, end_line, text, term_counts_json, norm, token_count FROM chunks ORDER BY path, chunk_index")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok(SearchChunk {
                    path: row.get::<_, String>(0)?,
                    title: row.get::<_, String>(1)?,
                    chunk_index: row.get::<_, i64>(2)? as usize,
                    start_line: row.get::<_, i64>(3)? as usize,
                    end_line: row.get::<_, i64>(4)? as usize,
                    text: row.get::<_, String>(5)?,
                    term_counts: parse_json(&row.get::<_, String>(6)?)?,
                    norm: row.get::<_, f64>(7)?,
                    token_count: row.get::<_, i64>(8)? as usize,
                    embedding: None,
                })
            })
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| IndexError::Embedding(error.to_string()))?
    };

    let artifact_snapshots = artifact_snapshots_from_connection(conn)?;
    let artifacts = {
        let mut statement = conn
            .prepare("SELECT path, kind, mime_type, size, title, metadata_json FROM artifacts ORDER BY path")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok(SearchArtifact {
                    path: row.get::<_, String>(0)?,
                    kind: row.get::<_, String>(1)?,
                    mime_type: row.get::<_, String>(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    title: row.get::<_, String>(4)?,
                    metadata_json: row.get::<_, String>(5)?,
                    embedding: None,
                })
            })
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| IndexError::Embedding(error.to_string()))?
    };

    if semantic_backend == SemanticBackend::Embedding {
        let note_vec_count: usize = conn
            .query_row("SELECT COUNT(*) FROM note_embeddings_vec", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as usize)
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let chunk_vec_count: usize = conn
            .query_row("SELECT COUNT(*) FROM chunk_embeddings_vec", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as usize)
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        // A partial reindex (one or more notes failed to embed) persists fewer
        // vectors than notes/chunks on purpose: those notes fall back to BM25/sparse
        // and keep `embedding = None`. Embedding is dropped per-note as a whole, so
        // counts can only be <= the row counts. MORE vectors than rows is genuine
        // corruption and stays fatal.
        if note_vec_count > notes.len() || chunk_vec_count > chunks.len() {
            return Err(IndexError::Embedding(
                "embedding index has more persisted vectors than notes/chunks (index corruption)"
                    .to_string(),
            ));
        }
    }

    let artifact_embedding_dimensions = metadata
        .get("artifactEmbeddingDimensions")
        .and_then(|value| value.parse::<usize>().ok());
    let vectorized_artifact_count = if artifact_embedding_dimensions.is_some() {
        if !sqlite::has_artifact_vector_table(conn)
            .map_err(|error| IndexError::Embedding(error.to_string()))?
        {
            return Err(IndexError::Embedding(
                "artifact embedding index is missing sqlite-vec table".to_string(),
            ));
        }
        conn.query_row("SELECT COUNT(*) FROM artifact_embeddings_vec", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count as usize)
        .map_err(|error| IndexError::Embedding(error.to_string()))?
    } else {
        0
    };

    Ok(SearchIndex {
        version,
        generated_at: metadata
            .get("generatedAt")
            .cloned()
            .unwrap_or_else(now_utc_string),
        semantic_backend,
        embedding_provider: metadata.get("embeddingProvider").cloned(),
        embedding_model: metadata.get("embeddingModel").cloned(),
        embedding_dimensions: metadata
            .get("embeddingDimensions")
            .and_then(|value| value.parse::<usize>().ok()),
        embedding_base_url: runtime_config.get("baseUrl").cloned(),
        runtime_embedding_api_key: None,
        runtime_query_instruction: None,
        artifact_embedding_provider: metadata.get("artifactEmbeddingProvider").cloned(),
        artifact_embedding_model: metadata.get("artifactEmbeddingModel").cloned(),
        artifact_embedding_dimensions,
        artifact_embedding_base_url: artifact_runtime_config.get("baseUrl").cloned(),
        runtime_artifact_embedding_api_key: None,
        artifact_embedding_error: metadata.get("artifactEmbeddingError").cloned(),
        file_snapshots,
        artifact_snapshots,
        document_frequencies,
        chunk_count: metadata
            .get("chunkCount")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(chunks.len()),
        note_count: metadata
            .get("noteCount")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(notes.len()),
        artifact_count: metadata
            .get("artifactCount")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(artifacts.len()),
        vectorized_artifact_count: metadata
            .get("vectorizedArtifactCount")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(vectorized_artifact_count),
        skipped_artifact_count: metadata
            .get("skippedArtifactCount")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0),
        notes,
        chunks,
        artifacts,
        context: None,
    })
}

pub fn build_index(
    vault_path: &Path,
    index_dir: Option<&Path>,
    embedding_config: Option<&EmbeddingConfig>,
) -> Result<SearchIndex> {
    build_index_with_artifacts(vault_path, index_dir, embedding_config, None)
}

pub fn build_index_with_artifacts(
    vault_path: &Path,
    index_dir: Option<&Path>,
    embedding_config: Option<&EmbeddingConfig>,
    artifact_embedding_config: Option<&EmbeddingConfig>,
) -> Result<SearchIndex> {
    let snapshots = collect_snapshots(vault_path)?;
    let artifact_snapshots = collect_artifact_snapshots(vault_path)?;
    build_index_from_snapshots_with_artifacts(
        vault_path,
        index_dir,
        snapshots,
        artifact_snapshots,
        embedding_config,
        artifact_embedding_config,
    )
}

pub fn load_index(vault_path: &Path, index_dir: Option<&Path>) -> Result<Option<SearchIndex>> {
    let index_file = index_file_path(vault_path, index_dir);
    if !index_file.exists() {
        return Ok(None);
    }

    let connection = open_index_connection(&index_file, true).map_err(|source| IndexError::Io {
        path: index_file.clone(),
        source: std::io::Error::new(std::io::ErrorKind::Other, source),
    })?;
    match load_search_index_from_connection(&connection) {
        Ok(mut index) => {
            index.context = Some(IndexContext {
                vault_path: vault_path.to_path_buf(),
                index_dir: index_dir.map(PathBuf::from),
            });
            Ok(Some(index))
        }
        Err(_) => Ok(None),
    }
}

pub fn load_persisted_index_header(
    vault_path: &Path,
    index_dir: Option<&Path>,
) -> Result<Option<PersistedIndexHeader>> {
    let index_file = index_file_path(vault_path, index_dir);
    if !index_file.exists() {
        return Ok(None);
    }

    let connection = open_index_connection(&index_file, true).map_err(|source| IndexError::Io {
        path: index_file.clone(),
        source: std::io::Error::new(std::io::ErrorKind::Other, source),
    })?;
    let metadata = match metadata_from_connection(&connection) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    let snapshots = match snapshots_from_connection(&connection) {
        Ok(snapshots) => snapshots,
        Err(_) => return Ok(None),
    };
    let artifact_snapshots = match artifact_snapshots_from_connection(&connection) {
        Ok(snapshots) => snapshots,
        Err(_) => return Ok(None),
    };
    match header_from_metadata_and_snapshots(&metadata, snapshots, artifact_snapshots) {
        Ok(header) => Ok(Some(header)),
        Err(_) => Ok(None),
    }
}

pub fn persisted_index_matches(
    vault_path: &Path,
    index_dir: Option<&Path>,
    snapshots: &[FileSnapshot],
    embedding_config: Option<&EmbeddingConfig>,
) -> Result<bool> {
    let Some(header) = load_persisted_index_header(vault_path, index_dir)? else {
        return Ok(false);
    };
    Ok(same_snapshots(&header.file_snapshots, snapshots)
        && same_persisted_semantic_config(&header, embedding_config))
}

pub fn persisted_index_matches_with_artifacts(
    vault_path: &Path,
    index_dir: Option<&Path>,
    snapshots: &[FileSnapshot],
    artifact_snapshots: &[ArtifactSnapshot],
    embedding_config: Option<&EmbeddingConfig>,
    artifact_embedding_config: Option<&EmbeddingConfig>,
) -> Result<bool> {
    let Some(header) = load_persisted_index_header(vault_path, index_dir)? else {
        return Ok(false);
    };
    Ok(same_snapshots(&header.file_snapshots, snapshots)
        && same_artifact_snapshots(&header.artifact_snapshots, artifact_snapshots)
        && same_persisted_semantic_config(&header, embedding_config)
        && same_persisted_artifact_embedding_config(&header, artifact_embedding_config))
}

fn query_optional_note_id(tx: &rusqlite::Transaction<'_>, path: &str) -> Result<Option<i64>> {
    tx.query_row(
        "SELECT id FROM notes WHERE path = ?1",
        params![path],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(|error| IndexError::Embedding(error.to_string()))
}

fn query_optional_artifact_id(tx: &rusqlite::Transaction<'_>, path: &str) -> Result<Option<i64>> {
    tx.query_row(
        "SELECT id FROM artifacts WHERE path = ?1",
        params![path],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(|error| IndexError::Embedding(error.to_string()))
}

fn query_ids_for_path(tx: &rusqlite::Transaction<'_>, table: &str, path: &str) -> Result<Vec<i64>> {
    let sql = match table {
        "chunks" => "SELECT id FROM chunks WHERE path = ?1",
        "notes" => "SELECT id FROM notes WHERE path = ?1",
        _ => {
            return Err(IndexError::Embedding(format!(
                "unsupported id lookup table: {table}"
            )))
        }
    };
    let mut statement = tx
        .prepare(sql)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map(params![path], |row| row.get::<_, i64>(0))
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| IndexError::Embedding(error.to_string()))
}

fn delete_existing_path_rows(
    tx: &rusqlite::Transaction<'_>,
    path: &str,
    semantic_backend: &SemanticBackend,
) -> Result<()> {
    if semantic_backend == &SemanticBackend::Embedding {
        for chunk_id in query_ids_for_path(tx, "chunks", path)? {
            tx.execute(
                "DELETE FROM chunk_terms WHERE chunk_id = ?1",
                params![chunk_id],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
            tx.execute(
                "DELETE FROM chunk_embeddings_vec WHERE rowid = ?1",
                params![chunk_id],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        for note_id in query_ids_for_path(tx, "notes", path)? {
            tx.execute(
                "DELETE FROM note_embeddings_vec WHERE rowid = ?1",
                params![note_id],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    } else {
        for chunk_id in query_ids_for_path(tx, "chunks", path)? {
            tx.execute(
                "DELETE FROM chunk_terms WHERE chunk_id = ?1",
                params![chunk_id],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    tx.execute("DELETE FROM chunks WHERE path = ?1", params![path])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    tx.execute("DELETE FROM notes WHERE path = ?1", params![path])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    Ok(())
}

fn delete_existing_artifact_rows(
    tx: &rusqlite::Transaction<'_>,
    path: &str,
    has_artifact_vectors: bool,
) -> Result<()> {
    if has_artifact_vectors {
        if let Some(artifact_id) = query_optional_artifact_id(tx, path)? {
            tx.execute(
                "DELETE FROM artifact_embeddings_vec WHERE rowid = ?1",
                params![artifact_id],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }
    tx.execute("DELETE FROM artifacts WHERE path = ?1", params![path])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    Ok(())
}

fn insert_prepared_note(
    tx: &rusqlite::Transaction<'_>,
    note_id: i64,
    next_chunk_id: &mut i64,
    prepared: &PreparedNote,
    semantic_backend: &SemanticBackend,
) -> Result<()> {
    tx.execute(
        "INSERT INTO notes (id, path, title, content, term_counts_json, norm, token_count, links_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            note_id,
            prepared.note.path,
            prepared.note.title,
            prepared.note.content,
            text_to_json(&prepared.note.term_counts).map_err(|error| IndexError::Embedding(error.to_string()))?,
            prepared.note.norm,
            prepared.note.token_count as i64,
            text_to_json(&prepared.note.links).map_err(|error| IndexError::Embedding(error.to_string()))?,
        ],
    )
    .map_err(|error| IndexError::Embedding(error.to_string()))?;

    // A note may legitimately lack a dense vector when its embedding failed during a
    // partial reindex; it falls back to BM25/sparse, so skip the vec row rather than
    // erroring (mirrors the full-build writer).
    if semantic_backend == &SemanticBackend::Embedding {
        if let Some(embedding) = prepared.note.embedding.as_ref() {
            tx.execute(
                "INSERT INTO note_embeddings_vec (rowid, embedding) VALUES (?1, ?2)",
                params![note_id, embedding_blob(embedding)],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    for chunk in &prepared.chunks {
        let chunk_id = *next_chunk_id;
        *next_chunk_id += 1;
        tx.execute(
            "INSERT INTO chunks (id, path, title, chunk_index, start_line, end_line, text, term_counts_json, norm, token_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                chunk_id,
                chunk.path,
                chunk.title,
                chunk.chunk_index as i64,
                chunk.start_line as i64,
                chunk.end_line as i64,
                chunk.text,
                text_to_json(&chunk.term_counts).map_err(|error| IndexError::Embedding(error.to_string()))?,
                chunk.norm,
                chunk.token_count as i64,
            ],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

        insert_chunk_terms(tx, chunk_id, &chunk.term_counts)?;

        if semantic_backend == &SemanticBackend::Embedding {
            if let Some(embedding) = chunk.embedding.as_ref() {
                tx.execute(
                    "INSERT INTO chunk_embeddings_vec (rowid, embedding) VALUES (?1, ?2)",
                    params![chunk_id, embedding_blob(embedding)],
                )
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
            }
        }
    }

    Ok(())
}

fn insert_prepared_artifact(
    tx: &rusqlite::Transaction<'_>,
    artifact_id: i64,
    prepared: &PreparedArtifact,
    has_artifact_vectors: bool,
) -> Result<()> {
    let artifact = &prepared.artifact;
    tx.execute(
        "INSERT INTO artifacts (id, path, kind, mime_type, size, title, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            artifact_id,
            artifact.path,
            artifact.kind,
            artifact.mime_type,
            artifact.size as i64,
            artifact.title,
            artifact.metadata_json,
        ],
    )
    .map_err(|error| IndexError::Embedding(error.to_string()))?;

    if has_artifact_vectors {
        if let Some(embedding) = artifact.embedding.as_ref() {
            tx.execute(
                "INSERT INTO artifact_embeddings_vec (rowid, embedding) VALUES (?1, ?2)",
                params![artifact_id, embedding_blob(embedding)],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }
    Ok(())
}

fn insert_chunk_terms(
    tx: &rusqlite::Transaction<'_>,
    chunk_id: i64,
    term_counts: &BTreeMap<String, usize>,
) -> Result<()> {
    for term in term_counts.keys() {
        tx.execute(
            "INSERT OR IGNORE INTO chunk_terms (term, chunk_id) VALUES (?1, ?2)",
            params![term, chunk_id],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    Ok(())
}

fn ensure_chunk_terms_backfilled(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let existing_terms: i64 = tx
        .query_row("SELECT COUNT(*) FROM chunk_terms", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let chunk_count: i64 = tx
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    if existing_terms > 0 || chunk_count == 0 {
        return Ok(());
    }

    let mut statement = tx
        .prepare("SELECT id, term_counts_json FROM chunks")
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let mut chunks = Vec::new();
    for row in rows {
        let (chunk_id, term_counts_json) =
            row.map_err(|error| IndexError::Embedding(error.to_string()))?;
        let term_counts = parse_json_index(&term_counts_json)?;
        chunks.push((chunk_id, term_counts));
    }
    drop(statement);

    for (chunk_id, term_counts) in chunks {
        insert_chunk_terms(tx, chunk_id, &term_counts)?;
    }
    Ok(())
}

fn note_term_counts_for_path(
    tx: &rusqlite::Transaction<'_>,
    path: &str,
) -> Result<Option<BTreeMap<String, usize>>> {
    let json = tx
        .query_row(
            "SELECT term_counts_json FROM notes WHERE path = ?1",
            params![path],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    json.map(|value| parse_json_index(&value)).transpose()
}

fn subtract_document_terms(
    document_frequencies: &mut BTreeMap<String, usize>,
    term_counts: &BTreeMap<String, usize>,
) {
    let mut empty_terms = Vec::new();
    for term in term_counts.keys() {
        if let Some(df) = document_frequencies.get_mut(term) {
            *df = df.saturating_sub(1);
            if *df == 0 {
                empty_terms.push(term.clone());
            }
        }
    }
    for term in empty_terms {
        document_frequencies.remove(&term);
    }
}

fn add_document_terms(
    document_frequencies: &mut BTreeMap<String, usize>,
    term_counts: &BTreeMap<String, usize>,
) {
    for term in term_counts.keys() {
        *document_frequencies.entry(term.clone()).or_insert(0) += 1;
    }
}

fn update_document_frequencies_delta(
    starting_document_frequencies: &BTreeMap<String, usize>,
    removed_term_counts: &[BTreeMap<String, usize>],
    prepared_notes: &[PreparedNote],
) -> BTreeMap<String, usize> {
    let mut document_frequencies = starting_document_frequencies.clone();
    for term_counts in removed_term_counts {
        subtract_document_terms(&mut document_frequencies, term_counts);
    }
    for prepared in prepared_notes {
        add_document_terms(&mut document_frequencies, &prepared.note.term_counts);
    }
    document_frequencies
}

fn replace_index_metadata(
    tx: &rusqlite::Transaction<'_>,
    snapshots: &[FileSnapshot],
    artifact_snapshots: &[ArtifactSnapshot],
    document_frequencies: &BTreeMap<String, usize>,
    index: &SearchIndex,
    index_config: &EmbeddingConfig,
    artifact_config: &EmbeddingConfig,
    artifact_embedding_error: Option<&str>,
    generated_at: &str,
) -> Result<()> {
    let note_count = tx
        .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get::<_, i64>(0))
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let chunk_count = tx
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let artifact_count = tx
        .query_row("SELECT COUNT(*) FROM artifacts", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let vectorized_artifact_count = if index.artifact_embedding_dimensions.is_some() {
        tx.query_row("SELECT COUNT(*) FROM artifact_embeddings_vec", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count as i64)
        .unwrap_or(0)
    } else {
        0
    };
    let skipped_artifact_count = artifact_snapshots
        .iter()
        .filter(|snapshot| snapshot.size > DEFAULT_MAX_ARTIFACT_BYTES)
        .count();

    tx.execute("DELETE FROM metadata", [])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let metadata_entries: Vec<(&str, String)> = vec![
        ("version", INDEX_VERSION.to_string()),
        ("generatedAt", generated_at.to_string()),
        (
            "semanticBackend",
            index.semantic_backend.as_str().to_string(),
        ),
        ("chunkCount", chunk_count.to_string()),
        ("noteCount", note_count.to_string()),
        ("artifactCount", artifact_count.to_string()),
        (
            "vectorizedArtifactCount",
            vectorized_artifact_count.to_string(),
        ),
        ("skippedArtifactCount", skipped_artifact_count.to_string()),
    ];
    for (key, value) in metadata_entries {
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    if let Some(provider) = &index.embedding_provider {
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params!["embeddingProvider", provider],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    if let Some(model) = &index.embedding_model {
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params!["embeddingModel", model],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    if let Some(dimensions) = index.embedding_dimensions {
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params!["embeddingDimensions", dimensions.to_string()],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    if let Some(provider) = &index.artifact_embedding_provider {
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params!["artifactEmbeddingProvider", provider],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    if let Some(model) = &index.artifact_embedding_model {
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params!["artifactEmbeddingModel", model],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    if let Some(dimensions) = index.artifact_embedding_dimensions {
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params!["artifactEmbeddingDimensions", dimensions.to_string()],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    if let Some(error) = artifact_embedding_error.or(index.artifact_embedding_error.as_deref()) {
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params!["artifactEmbeddingError", error],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }

    tx.execute("DELETE FROM embedding_config", [])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    if index_config.supports_embeddings() {
        if let Some(base_url) = index_config.base_url() {
            tx.execute(
                "INSERT INTO embedding_config (key, value) VALUES (?1, ?2)",
                params!["baseUrl", base_url],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    tx.execute("DELETE FROM artifact_embedding_config", [])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    if artifact_config.supports_embeddings() {
        if let Some(base_url) = artifact_config.base_url() {
            tx.execute(
                "INSERT INTO artifact_embedding_config (key, value) VALUES (?1, ?2)",
                params!["baseUrl", base_url],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    tx.execute("DELETE FROM file_snapshots", [])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    for snapshot in snapshots {
        tx.execute(
            "INSERT INTO file_snapshots (path, mtime_ms, size) VALUES (?1, ?2, ?3)",
            params![
                snapshot.path,
                snapshot.mtime_ms as i64,
                snapshot.size as i64
            ],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }

    tx.execute("DELETE FROM artifact_snapshots", [])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    for snapshot in artifact_snapshots {
        tx.execute(
            "INSERT INTO artifact_snapshots (path, mtime_ms, size, mime_type, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                snapshot.path,
                snapshot.mtime_ms as i64,
                snapshot.size as i64,
                snapshot.mime_type,
                snapshot.kind
            ],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }

    tx.execute("DELETE FROM document_frequencies", [])
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    for (term, df) in document_frequencies {
        tx.execute(
            "INSERT INTO document_frequencies (term, df) VALUES (?1, ?2)",
            params![term, *df as i64],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }

    Ok(())
}

fn refresh_index_incremental(
    vault_path: &Path,
    index_dir: Option<&Path>,
    mut existing: SearchIndex,
    snapshots: Vec<FileSnapshot>,
    artifact_snapshots: Vec<ArtifactSnapshot>,
    embedding_config: Option<&EmbeddingConfig>,
    artifact_embedding_config: Option<&EmbeddingConfig>,
) -> Result<SearchIndex> {
    let diff = diff_snapshots(&existing.file_snapshots, &snapshots);
    let artifact_diff = diff_artifact_snapshots(&existing.artifact_snapshots, &artifact_snapshots);
    if diff.added.is_empty()
        && diff.modified.is_empty()
        && diff.deleted.is_empty()
        && artifact_diff.added.is_empty()
        && artifact_diff.modified.is_empty()
        && artifact_diff.deleted.is_empty()
    {
        apply_runtime_embedding_config(&mut existing, embedding_config);
        apply_runtime_artifact_embedding_config(&mut existing, artifact_embedding_config);
        return Ok(existing);
    }

    let resolved = ensure_vault_path(vault_path)?;
    let index_config = normalized_embedding_config(embedding_config);
    let artifact_config = normalized_embedding_config(artifact_embedding_config);
    let expected_dimensions = if index_config.supports_embeddings() {
        existing.embedding_dimensions
    } else {
        None
    };
    let changed_paths = diff
        .added
        .iter()
        .chain(diff.modified.iter())
        .map(|snapshot| snapshot.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut prepared_notes = snapshots
        .iter()
        .filter(|snapshot| changed_paths.contains(snapshot.path.as_str()))
        .map(|snapshot| prepare_note_from_snapshot(&resolved, snapshot))
        .collect::<Result<Vec<_>>>()?;
    let note_embedding_outcome =
        embed_prepared_notes(&mut prepared_notes, &index_config, expected_dimensions)?;
    let embedding_dimensions = note_embedding_outcome.dimensions;
    if !note_embedding_outcome.failed_paths.is_empty() {
        tracing::warn!(
            failed_count = note_embedding_outcome.failed_paths.len(),
            failed_paths = ?note_embedding_outcome.failed_paths,
            "incremental index built with partial embeddings; listed notes fall back to BM25/sparse"
        );
    }
    if index_config.supports_embeddings() && embedding_dimensions != existing.embedding_dimensions {
        return Err(IndexError::EmbeddingDimensionsMismatch {
            expected: existing.embedding_dimensions.unwrap_or(0),
            actual: embedding_dimensions.unwrap_or(0),
        });
    }
    let artifact_changed_paths = artifact_diff
        .added
        .iter()
        .chain(artifact_diff.modified.iter())
        .map(|snapshot| snapshot.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut prepared_artifacts = artifact_snapshots
        .iter()
        .filter(|snapshot| artifact_changed_paths.contains(snapshot.path.as_str()))
        .map(|snapshot| {
            prepare_artifact_from_snapshot(
                &resolved,
                snapshot,
                artifact_config.supports_embeddings(),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let (artifact_embedding_dimensions, artifact_embedding_error) = embed_prepared_artifacts(
        &mut prepared_artifacts,
        &artifact_config,
        existing.artifact_embedding_dimensions,
    );
    if artifact_config.supports_embeddings()
        && artifact_embedding_dimensions != existing.artifact_embedding_dimensions
    {
        existing.artifact_embedding_dimensions = artifact_embedding_dimensions;
    }

    let index_file = index_file_path(vault_path, index_dir);
    let mut connection =
        open_index_connection(&index_file, false).map_err(|source| IndexError::Io {
            path: index_file.clone(),
            source: std::io::Error::new(std::io::ErrorKind::Other, source),
        })?;
    connection
        .execute_batch(sqlite::CURRENT_SCHEMA_DDL)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    if artifact_config.supports_embeddings()
        && existing.artifact_embedding_dimensions.is_some()
        && !sqlite::has_artifact_vector_table(&connection)
            .map_err(|error| IndexError::Embedding(error.to_string()))?
    {
        sqlite::recreate_artifact_vector_table(&connection, existing.artifact_embedding_dimensions)
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
    }
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    ensure_chunk_terms_backfilled(&tx)?;

    let mut existing_note_ids = BTreeMap::new();
    let mut removed_term_counts = Vec::new();
    for snapshot in &diff.modified {
        if let Some(note_id) = query_optional_note_id(&tx, &snapshot.path)? {
            existing_note_ids.insert(snapshot.path.clone(), note_id);
        }
        if let Some(term_counts) = note_term_counts_for_path(&tx, &snapshot.path)? {
            removed_term_counts.push(term_counts);
        }
    }
    for path in &diff.deleted {
        if let Some(term_counts) = note_term_counts_for_path(&tx, path)? {
            removed_term_counts.push(term_counts);
        }
    }

    for path in diff
        .deleted
        .iter()
        .chain(diff.modified.iter().map(|snapshot| &snapshot.path))
    {
        delete_existing_path_rows(&tx, path, &existing.semantic_backend)?;
    }

    let mut next_note_id = tx
        .query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM notes", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let mut next_chunk_id = tx
        .query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM chunks", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    for prepared in &prepared_notes {
        let note_id = existing_note_ids
            .get(&prepared.snapshot.path)
            .copied()
            .unwrap_or_else(|| {
                let id = next_note_id;
                next_note_id += 1;
                id
            });
        insert_prepared_note(
            &tx,
            note_id,
            &mut next_chunk_id,
            prepared,
            &existing.semantic_backend,
        )?;
    }

    let has_artifact_vectors = existing.artifact_embedding_dimensions.is_some();
    let mut existing_artifact_ids = BTreeMap::new();
    for snapshot in &artifact_diff.modified {
        if let Some(artifact_id) = query_optional_artifact_id(&tx, &snapshot.path)? {
            existing_artifact_ids.insert(snapshot.path.clone(), artifact_id);
        }
    }
    for path in artifact_diff
        .deleted
        .iter()
        .chain(artifact_diff.modified.iter().map(|snapshot| &snapshot.path))
    {
        delete_existing_artifact_rows(&tx, path, has_artifact_vectors)?;
    }

    let mut next_artifact_id = tx
        .query_row(
            "SELECT COALESCE(MAX(id), 0) + 1 FROM artifacts",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    for prepared in &prepared_artifacts {
        let artifact_id = existing_artifact_ids
            .get(&prepared.snapshot.path)
            .copied()
            .unwrap_or_else(|| {
                let id = next_artifact_id;
                next_artifact_id += 1;
                id
            });
        insert_prepared_artifact(&tx, artifact_id, prepared, has_artifact_vectors)?;
    }

    let document_frequencies = update_document_frequencies_delta(
        &existing.document_frequencies,
        &removed_term_counts,
        &prepared_notes,
    );
    replace_index_metadata(
        &tx,
        &snapshots,
        &artifact_snapshots,
        &document_frequencies,
        &existing,
        &index_config,
        &artifact_config,
        artifact_embedding_error.as_deref(),
        &now_utc_string(),
    )?;
    tx.commit()
        .map_err(|error| IndexError::Embedding(error.to_string()))?;

    let mut loaded = load_search_index_from_connection(&connection)?;
    loaded.context = Some(IndexContext {
        vault_path: vault_path.to_path_buf(),
        index_dir: index_dir.map(PathBuf::from),
    });
    apply_runtime_embedding_config(&mut loaded, embedding_config);
    apply_runtime_artifact_embedding_config(&mut loaded, artifact_embedding_config);
    Ok(loaded)
}

pub fn get_search_index(
    vault_path: &Path,
    index_dir: Option<&Path>,
    embedding_config: Option<&EmbeddingConfig>,
) -> Result<(SearchIndex, bool)> {
    get_search_index_with_artifacts(vault_path, index_dir, embedding_config, None)
}

pub fn get_search_index_with_artifacts(
    vault_path: &Path,
    index_dir: Option<&Path>,
    embedding_config: Option<&EmbeddingConfig>,
    artifact_embedding_config: Option<&EmbeddingConfig>,
) -> Result<(SearchIndex, bool)> {
    if let Some(mut existing) = load_index(vault_path, index_dir)? {
        let snapshots = collect_snapshots(vault_path)?;
        let artifact_snapshots = collect_artifact_snapshots(vault_path)?;
        if same_snapshots(&existing.file_snapshots, &snapshots)
            && same_artifact_snapshots(&existing.artifact_snapshots, &artifact_snapshots)
            && same_semantic_config(&existing, embedding_config)
            && same_artifact_embedding_config(&existing, artifact_embedding_config)
        {
            apply_runtime_embedding_config(&mut existing, embedding_config);
            apply_runtime_artifact_embedding_config(&mut existing, artifact_embedding_config);
            return Ok((existing, false));
        }
        if same_semantic_config(&existing, embedding_config)
            && same_artifact_embedding_config(&existing, artifact_embedding_config)
        {
            if let Ok(updated) = refresh_index_incremental(
                vault_path,
                index_dir,
                existing,
                snapshots,
                artifact_snapshots,
                embedding_config,
                artifact_embedding_config,
            ) {
                return Ok((updated, true));
            }
        }
    }

    let rebuilt = build_index_with_artifacts(
        vault_path,
        index_dir,
        embedding_config,
        artifact_embedding_config,
    )?;
    Ok((rebuilt, true))
}

pub fn build_index_from_snapshots(
    vault_path: &Path,
    index_dir: Option<&Path>,
    snapshots: Vec<FileSnapshot>,
    embedding_config: Option<&EmbeddingConfig>,
) -> Result<SearchIndex> {
    build_index_from_snapshots_with_artifacts(
        vault_path,
        index_dir,
        snapshots,
        Vec::new(),
        embedding_config,
        None,
    )
}

pub fn build_index_from_snapshots_with_artifacts(
    vault_path: &Path,
    index_dir: Option<&Path>,
    snapshots: Vec<FileSnapshot>,
    artifact_snapshots: Vec<ArtifactSnapshot>,
    embedding_config: Option<&EmbeddingConfig>,
    artifact_embedding_config: Option<&EmbeddingConfig>,
) -> Result<SearchIndex> {
    let resolved = ensure_vault_path(vault_path)?;
    let index_config = normalized_embedding_config(embedding_config);
    let artifact_config = normalized_embedding_config(artifact_embedding_config);
    let mut prepared_notes = snapshots
        .iter()
        .map(|snapshot| prepare_note_from_snapshot(&resolved, snapshot))
        .collect::<Result<Vec<_>>>()?;
    let note_embedding_outcome = embed_prepared_notes(&mut prepared_notes, &index_config, None)?;
    let embedding_dimensions = note_embedding_outcome.dimensions;
    if !note_embedding_outcome.failed_paths.is_empty() {
        tracing::warn!(
            failed_count = note_embedding_outcome.failed_paths.len(),
            failed_paths = ?note_embedding_outcome.failed_paths,
            "index built with partial embeddings; listed notes fall back to BM25/sparse"
        );
    }
    let mut prepared_artifacts = artifact_snapshots
        .iter()
        .map(|snapshot| {
            prepare_artifact_from_snapshot(
                &resolved,
                snapshot,
                artifact_config.supports_embeddings(),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let (artifact_embedding_dimensions, artifact_embedding_error) =
        embed_prepared_artifacts(&mut prepared_artifacts, &artifact_config, None);
    let notes = prepared_notes
        .iter()
        .map(|prepared| prepared.note.clone())
        .collect::<Vec<_>>();
    let chunks = prepared_notes
        .iter()
        .flat_map(|prepared| prepared.chunks.iter().cloned())
        .collect::<Vec<_>>();
    let artifacts = prepared_artifacts
        .iter()
        .map(|prepared| prepared.artifact.clone())
        .collect::<Vec<_>>();
    let document_frequencies = collect_document_frequencies(&notes);
    let vectorized_artifact_count = artifacts
        .iter()
        .filter(|artifact| artifact.embedding.is_some())
        .count();
    let skipped_artifact_count = artifact_snapshots
        .iter()
        .filter(|snapshot| snapshot.size > DEFAULT_MAX_ARTIFACT_BYTES)
        .count();

    // If embeddings were requested but NOTHING embedded (e.g. the backend was fully
    // down for the whole build), no vec tables get created. Persisting
    // `semantic_backend = Embedding` in that state makes the load gate demand vec
    // tables that don't exist and silently reject the index (-> perpetual rebuild).
    // Record `Sparse` so the index reloads as a usable BM25 index; a later reindex
    // upgrades it once the backend is reachable.
    let semantic_backend = match semantic_backend_from_config(embedding_config) {
        SemanticBackend::Embedding if embedding_dimensions.is_none() => SemanticBackend::Sparse,
        backend => backend,
    };

    let index = SearchIndex {
        version: INDEX_VERSION,
        generated_at: now_utc_string(),
        semantic_backend,
        embedding_provider: if index_config.supports_embeddings() {
            Some(
                index_config
                    .provider
                    .as_ref()
                    .map(EmbeddingProvider::as_str)
                    .unwrap_or("openai-compatible")
                    .to_string(),
            )
        } else {
            None
        },
        embedding_model: index_config
            .model
            .clone()
            .filter(|value| !value.trim().is_empty()),
        embedding_dimensions,
        embedding_base_url: if index_config.supports_embeddings() {
            index_config.base_url().map(|value| value.to_string())
        } else {
            None
        },
        runtime_embedding_api_key: if index_config.supports_embeddings() {
            index_config.api_key.clone()
        } else {
            None
        },
        runtime_query_instruction: if index_config.supports_embeddings() {
            index_config.query_instruction.clone()
        } else {
            None
        },
        artifact_embedding_provider: if artifact_config.supports_embeddings() {
            Some(
                artifact_config
                    .provider
                    .as_ref()
                    .map(EmbeddingProvider::as_str)
                    .unwrap_or("openai-compatible")
                    .to_string(),
            )
        } else {
            None
        },
        artifact_embedding_model: artifact_config
            .model
            .clone()
            .filter(|value| !value.trim().is_empty()),
        artifact_embedding_dimensions,
        artifact_embedding_base_url: if artifact_config.supports_embeddings() {
            artifact_config.base_url().map(|value| value.to_string())
        } else {
            None
        },
        runtime_artifact_embedding_api_key: if artifact_config.supports_embeddings() {
            artifact_config.api_key.clone()
        } else {
            None
        },
        artifact_embedding_error,
        file_snapshots: snapshots,
        artifact_snapshots,
        document_frequencies,
        chunk_count: chunks.len(),
        note_count: notes.len(),
        artifact_count: artifacts.len(),
        vectorized_artifact_count,
        skipped_artifact_count,
        notes,
        chunks,
        artifacts,
        context: Some(IndexContext {
            vault_path: resolved.clone(),
            index_dir: index_dir.map(PathBuf::from),
        }),
    };

    let index_file = index_file_path(vault_path, index_dir);
    if let Some(parent) = index_file.parent() {
        fs::create_dir_all(parent).map_err(|source| IndexError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut connection =
        open_index_connection(&index_file, false).map_err(|source| IndexError::Io {
            path: index_file.clone(),
            source: std::io::Error::new(std::io::ErrorKind::Other, source),
        })?;
    write_search_index_to_connection(&mut connection, &index)?;

    Ok(index)
}

pub fn count_documents(index: &SearchIndex) -> usize {
    index.notes.len()
}

pub fn average(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

pub fn cosine_similarity(
    query_term_counts: &BTreeMap<String, usize>,
    query_norm: f64,
    term_counts: &BTreeMap<String, usize>,
    term_norm: f64,
) -> f64 {
    if query_norm == 0.0 || term_norm == 0.0 {
        return 0.0;
    }

    let mut dot = 0.0;
    for (term, query_value) in query_term_counts {
        if let Some(candidate_value) = term_counts.get(term) {
            dot += (*query_value as f64) * (*candidate_value as f64);
        }
    }
    dot / (query_norm * term_norm)
}

pub fn bm25_score(
    query_terms: &[String],
    term_counts: &BTreeMap<String, usize>,
    document_frequencies: &BTreeMap<String, usize>,
    document_count: usize,
    document_length: usize,
    average_document_length: f64,
) -> f64 {
    const BM25_K1: f64 = 1.2;
    const BM25_B: f64 = 0.75;

    if document_length == 0 || average_document_length <= 0.0 || document_count == 0 {
        return 0.0;
    }

    let mut score = 0.0;
    let unique_terms: BTreeSet<_> = query_terms.iter().collect();
    for term in unique_terms {
        let tf = *term_counts.get(term.as_str()).unwrap_or(&0) as f64;
        if tf == 0.0 {
            continue;
        }
        let df = *document_frequencies.get(term.as_str()).unwrap_or(&0) as f64;
        let idf = (1.0 + (document_count as f64 - df + 0.5) / (df + 0.5)).ln();
        let denominator = tf
            + BM25_K1
                * (1.0 - BM25_B + BM25_B * (document_length as f64 / average_document_length));
        score += idf * ((tf * (BM25_K1 + 1.0)) / denominator);
    }

    score
}

pub fn normalize_dense_vector(vector: &[f64]) -> Vec<f64> {
    let norm = vector.iter().map(|value| value * value).sum::<f64>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return vector.to_vec();
    }
    vector.iter().map(|value| value / norm).collect()
}

pub fn normalize_scored<T: Clone + Scored>(items: &[T]) -> Vec<T::Normalized> {
    let max_score = items
        .iter()
        .map(|item| item.score())
        .fold(0.0_f64, f64::max);
    items
        .iter()
        .map(|item| {
            item.with_normalized_score(if max_score > 0.0 {
                item.score() / max_score
            } else {
                0.0
            })
        })
        .collect()
}

pub trait Scored {
    type Normalized;
    fn score(&self) -> f64;
    fn with_normalized_score(&self, normalized_score: f64) -> Self::Normalized;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegexAtom {
    Literal(char),
    AnyChar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegexPiece {
    atom: RegexAtom,
    repeat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobAtom {
    Literal(char),
    AnyChar,
    Wildcard,
}

fn normalize_char(ch: char, case_sensitive: bool) -> char {
    if case_sensitive {
        ch
    } else {
        ch.to_ascii_lowercase()
    }
}

fn atom_matches(atom: RegexAtom, ch: char, case_sensitive: bool) -> bool {
    match atom {
        RegexAtom::Literal(expected) => {
            normalize_char(expected, case_sensitive) == normalize_char(ch, case_sensitive)
        }
        RegexAtom::AnyChar => true,
    }
}

fn parse_regex_subset(pattern: &str) -> Result<(bool, bool, Vec<RegexPiece>)> {
    let mut anchored_start = false;
    let mut anchored_end = false;
    let mut pieces: Vec<RegexPiece> = Vec::new();
    let mut chars = pattern.chars().peekable();
    let mut index = 0usize;
    while let Some(ch) = chars.next() {
        let is_first = index == 0;
        let is_last = chars.peek().is_none();
        index += 1;

        if ch == '^' && is_first {
            anchored_start = true;
            continue;
        }
        if ch == '$' && is_last {
            anchored_end = true;
            continue;
        }
        if ch == '*' {
            let Some(last) = pieces.last_mut() else {
                return Err(IndexError::InvalidRegex {
                    pattern: pattern.to_string(),
                    message: "missing atom before *".to_string(),
                });
            };
            if last.repeat {
                return Err(IndexError::InvalidRegex {
                    pattern: pattern.to_string(),
                    message: "duplicate * quantifier".to_string(),
                });
            }
            last.repeat = true;
            continue;
        }
        if ch == '\\' {
            let Some(escaped) = chars.next() else {
                return Err(IndexError::InvalidRegex {
                    pattern: pattern.to_string(),
                    message: "dangling escape".to_string(),
                });
            };
            index += 1;
            pieces.push(RegexPiece {
                atom: RegexAtom::Literal(escaped),
                repeat: false,
            });
            continue;
        }
        pieces.push(RegexPiece {
            atom: if ch == '.' {
                RegexAtom::AnyChar
            } else {
                RegexAtom::Literal(ch)
            },
            repeat: false,
        });
    }

    Ok((anchored_start, anchored_end, pieces))
}

fn regex_match_from(
    pieces: &[RegexPiece],
    chars: &[(usize, char)],
    start_pos: usize,
    case_sensitive: bool,
    anchored_end: bool,
) -> Option<usize> {
    if pieces.is_empty() {
        return if !anchored_end || start_pos == chars.len() {
            Some(start_pos)
        } else {
            None
        };
    }

    let piece = pieces[0];
    if piece.repeat {
        let mut candidates = vec![start_pos];
        let mut cursor = start_pos;
        while cursor < chars.len() && atom_matches(piece.atom, chars[cursor].1, case_sensitive) {
            cursor += 1;
            candidates.push(cursor);
        }
        for candidate in candidates.into_iter().rev() {
            if let Some(end_pos) =
                regex_match_from(&pieces[1..], chars, candidate, case_sensitive, anchored_end)
            {
                return Some(end_pos);
            }
        }
        None
    } else if start_pos < chars.len()
        && atom_matches(piece.atom, chars[start_pos].1, case_sensitive)
    {
        regex_match_from(
            &pieces[1..],
            chars,
            start_pos + 1,
            case_sensitive,
            anchored_end,
        )
    } else {
        None
    }
}

pub fn find_pattern_spans(
    text: &str,
    pattern: &str,
    case_sensitive: bool,
) -> Result<Vec<(usize, usize)>> {
    let (anchored_start, anchored_end, pieces) = parse_regex_subset(pattern)?;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut spans = Vec::new();

    if anchored_start {
        if let Some(end_pos) = regex_match_from(&pieces, &chars, 0, case_sensitive, anchored_end) {
            let start = 0;
            let end = if end_pos < chars.len() {
                chars[end_pos].0
            } else {
                text.len()
            };
            if end > start {
                spans.push((start, end));
            }
        }
        return Ok(spans);
    }

    let mut start_pos = 0usize;
    while start_pos <= chars.len() {
        if let Some(end_pos) =
            regex_match_from(&pieces, &chars, start_pos, case_sensitive, anchored_end)
        {
            let start = if start_pos < chars.len() {
                chars[start_pos].0
            } else {
                text.len()
            };
            let end = if end_pos < chars.len() {
                chars[end_pos].0
            } else {
                text.len()
            };
            if end > start {
                spans.push((start, end));
                start_pos = end_pos.max(start_pos + 1);
                continue;
            }
        }
        start_pos += 1;
    }

    Ok(spans)
}

pub fn matches_pattern(text: &str, pattern: &str, case_sensitive: bool) -> Result<bool> {
    Ok(!find_pattern_spans(text, pattern, case_sensitive)?.is_empty())
}

fn parse_glob(glob: &str) -> Result<Vec<GlobAtom>> {
    let mut atoms: Vec<GlobAtom> = Vec::new();
    let mut chars = glob.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => atoms.push(GlobAtom::Wildcard),
            '?' => atoms.push(GlobAtom::AnyChar),
            '\\' => {
                let Some(escaped) = chars.next() else {
                    return Err(IndexError::InvalidRegex {
                        pattern: glob.to_string(),
                        message: "dangling escape".to_string(),
                    });
                };
                atoms.push(GlobAtom::Literal(escaped));
            }
            other => atoms.push(GlobAtom::Literal(other)),
        }
    }
    Ok(atoms)
}

fn glob_match_from(
    atoms: &[GlobAtom],
    chars: &[(usize, char)],
    atom_pos: usize,
    char_pos: usize,
    case_sensitive: bool,
) -> bool {
    if atom_pos == atoms.len() {
        return char_pos == chars.len();
    }

    match atoms[atom_pos] {
        GlobAtom::Literal(expected) => {
            char_pos < chars.len()
                && normalize_char(expected, case_sensitive)
                    == normalize_char(chars[char_pos].1, case_sensitive)
                && glob_match_from(atoms, chars, atom_pos + 1, char_pos + 1, case_sensitive)
        }
        GlobAtom::AnyChar => {
            char_pos < chars.len()
                && glob_match_from(atoms, chars, atom_pos + 1, char_pos + 1, case_sensitive)
        }
        GlobAtom::Wildcard => {
            for candidate in (char_pos..=chars.len()).rev() {
                if glob_match_from(atoms, chars, atom_pos + 1, candidate, case_sensitive) {
                    return true;
                }
            }
            false
        }
    }
}

pub fn path_matches_glob(path: &str, glob: &str) -> Result<bool> {
    let atoms = parse_glob(glob)?;
    let chars: Vec<(usize, char)> = path.char_indices().collect();
    Ok(glob_match_from(&atoms, &chars, 0, 0, true))
}

fn path_stem(path: &str) -> &str {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("deep-obsidian-index-{label}-{nanos}-{suffix}"))
    }

    fn write_fixture(root: &Path, relative: &str, content: &str) {
        let absolute = root.join(relative);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&absolute, content).expect("write fixture");
    }

    fn note_id(root: &Path, relative: &str) -> Option<i64> {
        let connection =
            open_index_connection(&index_file_path(root, None), true).expect("open index");
        connection
            .query_row(
                "SELECT id FROM notes WHERE path = ?1",
                params![relative],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .expect("query note id")
    }

    fn chunk_ids(root: &Path, relative: &str) -> Vec<i64> {
        let connection =
            open_index_connection(&index_file_path(root, None), true).expect("open index");
        let mut statement = connection
            .prepare("SELECT id FROM chunks WHERE path = ?1 ORDER BY chunk_index")
            .expect("prepare chunks");
        let rows = statement
            .query_map(params![relative], |row| row.get::<_, i64>(0))
            .expect("query chunks");
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect chunks")
    }

    fn artifact_id(root: &Path, relative: &str) -> Option<i64> {
        let connection =
            open_index_connection(&index_file_path(root, None), true).expect("open index");
        connection
            .query_row(
                "SELECT id FROM artifacts WHERE path = ?1",
                params![relative],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .expect("query artifact id")
    }

    fn vector_counts(root: &Path) -> (usize, usize) {
        let connection =
            open_index_connection(&index_file_path(root, None), true).expect("open index");
        let note_count = connection
            .query_row("SELECT COUNT(*) FROM note_embeddings_vec", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("note vector count") as usize;
        let chunk_count = connection
            .query_row("SELECT COUNT(*) FROM chunk_embeddings_vec", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("chunk vector count") as usize;
        (note_count, chunk_count)
    }

    fn start_embedding_server(expected_requests: usize) -> (String, Arc<Mutex<Vec<Vec<String>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        let address = listener.local_addr().expect("server address");
        let seen_inputs = Arc::new(Mutex::new(Vec::new()));
        let thread_inputs = seen_inputs.clone();
        thread::spawn(move || {
            for stream in listener.incoming().take(expected_requests) {
                let mut stream = stream.expect("accept embedding request");
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
                let header_end = header_end.expect("request headers") + 4;
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
                thread_inputs
                    .lock()
                    .expect("inputs lock")
                    .push(inputs.clone());
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
        (format!("http://{}", address), seen_inputs)
    }

    #[test]
    fn list_markdown_files_skips_hidden_and_ignored_dirs() {
        let root = unique_temp_dir("list");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n");
        write_fixture(&root, ".obsidian/Hidden.md", "# hidden\n");
        write_fixture(&root, "Projects/Brew Service.md", "# Brew\n");
        write_fixture(&root, "node_modules/Skip.md", "# skip\n");

        let files = list_markdown_files(&root).expect("list");
        assert_eq!(files, vec!["Home.md", "Projects/Brew Service.md"]);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn build_index_counts_terms_and_chunks() {
        let root = unique_temp_dir("build");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(
            &root,
            "Home.md",
            "# Home\n\nSee [[Projects/Brew Service]] and [[Research/Service Contract]].\n",
        );
        write_fixture(
            &root,
            "Projects/Brew Service.md",
            "# Brew Service\n\nInstall the service and validate the runtime.\n",
        );

        let index = build_index(&root, None, None).expect("build");
        assert_eq!(index.note_count, 2);
        assert!(index.chunk_count >= 2);
        assert_eq!(index.semantic_backend, SemanticBackend::Sparse);
        assert!(index.note("Home.md").is_some());
        assert!(index.document_frequencies.contains_key("service"));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn snapshot_collection_and_compare_work() {
        let root = unique_temp_dir("snapshots");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n");
        let first = collect_snapshots(&root).expect("snapshots");
        let second = collect_snapshots(&root).expect("snapshots");
        assert!(same_snapshots(&first, &second));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn artifact_snapshot_collection_supports_common_media_and_ignores_hidden_dirs() {
        let root = unique_temp_dir("artifact-snapshots");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n");
        write_fixture(&root, "docs/Guide.pdf", "fake pdf");
        write_fixture(&root, "images/Logo.PNG", "fake png");
        write_fixture(&root, "audio/Clip.mp3", "fake audio");
        write_fixture(&root, "video/Demo.webm", "fake video");
        write_fixture(&root, ".hidden/Secret.pdf", "ignored");
        write_fixture(&root, ".obsidian/Theme.pdf", "ignored");

        let snapshots = collect_artifact_snapshots(&root).expect("artifact snapshots");
        let paths = snapshots
            .iter()
            .map(|snapshot| snapshot.path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec![
                "audio/Clip.mp3",
                "docs/Guide.pdf",
                "images/Logo.PNG",
                "video/Demo.webm"
            ]
        );
        assert_eq!(snapshots[1].mime_type, "application/pdf");
        assert_eq!(snapshots[2].kind, "image");

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn enclosing_heading_section_mirrors_flat_chunker_tiling() {
        // Flat tiling: each section runs to the NEXT heading of ANY level. A line under
        // `## Two` maps to the `## Two` section [3..end], NOT the enclosing `# One`.
        let content = "# One\nalpha\n## Two\nbeta\ngamma\n## Three\ndelta\n";
        // Line 4 (`beta`) is inside the `## Two` section: lines 3..5 (heading .. before `## Three`).
        let (start, end, text) =
            enclosing_heading_section(content, 4).expect("line 4 has an enclosing section");
        assert_eq!((start, end), (3, 5));
        assert_eq!(text, "## Two\nbeta\ngamma");
        assert!(!text.contains("# One"), "flat tiling must not balloon to the parent");
        assert!(!text.contains("## Three"), "must stop at the next heading");

        // The H1 heading line itself maps to the `# One` section, which (flat) ends at `## Two`.
        let (start, end, _) = enclosing_heading_section(content, 1).expect("heading line section");
        assert_eq!((start, end), (1, 2));
    }

    #[test]
    fn enclosing_heading_section_falls_back_for_preamble_and_heading_less() {
        // Preamble: a line BEFORE the first heading has no enclosing section.
        let with_preamble = "intro paragraph\nmore intro\n# Body\ntext\n";
        assert!(enclosing_heading_section(with_preamble, 1).is_none());
        assert!(enclosing_heading_section(with_preamble, 2).is_none());
        // The heading and below still resolve.
        assert!(enclosing_heading_section(with_preamble, 3).is_some());

        // Heading-less note: no boundaries at all -> always None (caller keeps chunk text).
        let heading_less = "just prose\nno headings here\n";
        assert!(enclosing_heading_section(heading_less, 1).is_none());
        assert!(enclosing_heading_section(heading_less, 2).is_none());
    }

    #[test]
    fn enclosing_heading_section_is_fence_aware() {
        // A `#`-prefixed line inside a fence must NOT be read as a heading (matches the
        // fence-aware chunker, unlike fence-blind `extract_heading_sections`).
        let content = "# Real\nbefore\n```bash\n# not a heading\n```\nafter\n";
        let (start, end, text) = enclosing_heading_section(content, 4).expect("fenced line section");
        assert_eq!(start, 1, "the only heading is the real H1 at line 1");
        // `split('\n')` on a trailing-newline string yields a final empty element, so the
        // section runs to lines.len() = 7 (matching the chunker's own line accounting).
        assert_eq!(end, 7);
        assert!(text.contains("# not a heading"), "fenced pseudo-heading stays in the section");
    }

    #[test]
    fn heading_and_block_extraction_follow_expected_rules() {
        let content =
            "# Title\n\n## Section One\nBody\n\ninline ^block-id\n\nParagraph\n^block-two\n";
        let headings = extract_heading_sections(content);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].slug, "title");
        assert_eq!(headings[1].slug, "section-one");

        let blocks = extract_block_sections(content);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, "block-id");
        assert_eq!(blocks[1].id, "block-two");
    }

    fn section_chunks_default(content: &str, title: &str) -> Vec<SectionChunk> {
        section_chunks(
            content,
            title,
            SECTION_CHUNK_TARGET_TOKENS,
            SECTION_CHUNK_MIN_TOKENS,
            DEFAULT_CHUNK_MAX_CHARS,
        )
        .expect("note has headings")
    }

    #[test]
    fn section_chunks_returns_none_for_heading_less_note() {
        let content = "Just a paragraph of prose.\nNo headings at all here.\n";
        assert!(section_chunks(
            content,
            "Untitled",
            SECTION_CHUNK_TARGET_TOKENS,
            SECTION_CHUNK_MIN_TOKENS,
            DEFAULT_CHUNK_MAX_CHARS,
        )
        .is_none());
    }

    #[test]
    fn section_chunks_keep_fenced_code_block_intact() {
        // A `#`-prefixed line inside the fence must NOT be read as a heading, and a
        // small note collapses to a single chunk that contains the whole fence.
        let content = "# Guide\n\n## A\nalpha text\n\n### B\nbeta text\n\n```bash\n# not a heading\nrun --flag\n```\n";
        let chunks = section_chunks_default(content, "Guide");
        // The whole note is well under the merge floor, so it is a single chunk.
        assert_eq!(chunks.len(), 1, "tiny multi-heading note merges to one chunk");
        let chunk = &chunks[0];
        assert!(chunk.text.contains("```bash"));
        assert!(chunk.text.contains("# not a heading"));
        assert!(chunk.text.contains("run --flag"));
        // The closing fence is present => the fence was never split across a boundary.
        assert_eq!(chunk.text.matches("```").count(), 2);
    }

    #[test]
    fn section_chunks_split_large_note_on_subheadings_and_keep_fence() {
        // Make each section large enough to clear the merge floor and the target budget
        // so the note splits into per-heading chunks; verify the code fence stays whole.
        let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore ".repeat(8);
        let content = format!(
            "# Big\n\n## A\n{filler}\n\n## B\n{filler}\n\n```python\n# inside fence\nx = 1\ny = 2\n```\n{filler}\n",
        );
        let chunks = section_chunks_default(&content, "Big");
        assert!(chunks.len() >= 2, "oversized note must split: {}", chunks.len());
        // Exactly one chunk contains the opening fence, and it also contains the close.
        let fence_chunks: Vec<&SectionChunk> = chunks
            .iter()
            .filter(|chunk| chunk.text.contains("```python"))
            .collect();
        assert_eq!(fence_chunks.len(), 1, "fence opener in exactly one chunk");
        assert_eq!(
            fence_chunks[0].text.matches("```").count(),
            2,
            "fence kept intact (open + close in same chunk)"
        );
        // The B sub-heading drives a chunk whose path ends in "B".
        assert!(chunks
            .iter()
            .any(|chunk| chunk.heading_path.last().map(String::as_str) == Some("B")));
    }

    #[test]
    fn section_chunks_do_not_split_a_table_mid_table() {
        let filler = "padding words to grow the section beyond the merge floor and target budget ".repeat(12);
        let content = format!(
            "# Codes\n\n## Intro\n{filler}\n\n## Table\n| Code | Meaning |\n| --- | --- |\n| ERR_1 | one |\n| ERR_2 | two |\n| ERR_3 | three |\n{filler}\n",
        );
        let chunks = section_chunks_default(&content, "Codes");
        // Every table row must live in a single chunk (none split across chunks).
        for row in ["| ERR_1 | one |", "| ERR_2 | two |", "| ERR_3 | three |"] {
            let count = chunks.iter().filter(|chunk| chunk.text.contains(row)).count();
            assert_eq!(count, 1, "table row {row:?} appears in exactly one chunk");
        }
        // The header and its rows stay in the same chunk (contiguous table run).
        let table_chunk = chunks
            .iter()
            .find(|chunk| chunk.text.contains("| Code | Meaning |"))
            .expect("table chunk");
        assert!(table_chunk.text.contains("| ERR_3 | three |"));
    }

    #[test]
    fn section_chunks_round_trip_line_ranges_to_source() {
        let content = "# Title\n\n## One\nalpha\n\n## Two\nbeta\n";
        let lines: Vec<&str> = content.split('\n').collect();
        let chunks = section_chunks_default(content, "Title");
        for chunk in &chunks {
            assert!(chunk.start_line >= 1 && chunk.end_line <= lines.len());
            assert!(chunk.start_line <= chunk.end_line);
            let expected = lines[chunk.start_line - 1..chunk.end_line].join("\n");
            assert_eq!(chunk.text, expected, "stored text must equal source slice");
        }
    }

    #[test]
    fn section_chunks_heading_path_drops_duplicate_title_and_nests() {
        // Each heading gets a body that clears the merge floor so per-heading chunks
        // survive (no merge into one). `# Service` has its own body so it stays a
        // distinct chunk rather than absorbing its first child.
        let filler =
            "context sentence with enough distinct words to clear the floor and budget ".repeat(16);
        assert!(
            token_len(&filler) >= SECTION_CHUNK_MIN_TOKENS,
            "filler must clear the merge floor: {}",
            token_len(&filler)
        );
        let content = format!(
            "# Service\n{filler}\n\n## Setup\n{filler}\n\n### Steps\n{filler}\n",
        );
        let chunks = section_chunks_default(&content, "Service");
        // No path should start with the title twice.
        for chunk in &chunks {
            assert_ne!(
                chunk.heading_path.first().map(String::as_str),
                chunk.heading_path.get(1).map(String::as_str),
                "title must not be duplicated at the front of the path"
            );
            assert_eq!(chunk.heading_path.first().map(String::as_str), Some("Service"));
        }
        // A nested chunk carries the full path Service › Setup › Steps.
        assert!(chunks.iter().any(|chunk| chunk.heading_path
            == vec![
                "Service".to_string(),
                "Setup".to_string(),
                "Steps".to_string()
            ]));
    }

    #[test]
    fn section_chunks_oversized_multiline_section_packs_under_budget() {
        // Production path: a heading with many newline-separated short lines, no
        // sub-headings, no blanks. The greedy packer must produce multiple chunks each
        // at or under budget, and every chunk must round-trip to whole source lines.
        let lines: Vec<String> = (0..80)
            .map(|i| format!("line{i} alpha beta gamma delta epsilon"))
            .collect();
        let content = format!("# Solo\n{}\n", lines.join("\n"));
        let src: Vec<&str> = content.split('\n').collect();
        let chunks = section_chunks(&content, "Solo", 64, 16, DEFAULT_CHUNK_MAX_CHARS)
            .expect("has heading");
        assert!(chunks.len() > 1, "oversized multiline section must split");
        for chunk in &chunks {
            assert!(
                token_len(&chunk.text) <= 64,
                "chunk over budget: {} tokens",
                token_len(&chunk.text)
            );
            // Whole-line packing => text equals the source line slice.
            let expected = src[chunk.start_line - 1..chunk.end_line].join("\n");
            assert_eq!(chunk.text, expected, "whole-line chunk must round-trip");
        }
    }

    #[test]
    fn section_chunks_oversized_single_line_word_splits_under_budget() {
        // A single physical line that alone exceeds the budget must be word-split. The
        // pieces share one source line number (sub-line caveat) but stay under budget.
        let body = "alpha beta gamma delta epsilon zeta eta theta iota kappa ".repeat(120);
        let content = format!("# Solo\n{body}\n");
        let chunks = section_chunks(&content, "Solo", 64, 16, DEFAULT_CHUNK_MAX_CHARS)
            .expect("has heading");
        assert!(chunks.len() > 1, "oversized single line must word-split");
        for chunk in &chunks {
            assert!(
                token_len(&chunk.text) <= 64,
                "chunk over budget: {} tokens",
                token_len(&chunk.text)
            );
        }
    }

    #[test]
    fn sqlite_index_round_trip_loads_persisted_sparse_index() {
        let root = unique_temp_dir("sqlite-round-trip");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(
            &root,
            "Home.md",
            "# Home\n\nSee [[Projects/Brew Service]] and [[Research/Service Contract]].\n",
        );
        write_fixture(
            &root,
            "Projects/Brew Service.md",
            "# Brew Service\n\nInstall the service and validate the runtime.\n",
        );

        let built = build_index(&root, None, None).expect("build");
        let loaded = load_index(&root, None)
            .expect("load")
            .expect("persisted index");

        assert_eq!(loaded.note_count, built.note_count);
        assert_eq!(loaded.chunk_count, built.chunk_count);
        assert_eq!(loaded.file_snapshots, built.file_snapshots);
        assert_eq!(loaded.document_frequencies, built.document_frequencies);
        assert_eq!(loaded.semantic_backend, SemanticBackend::Sparse);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_search_index_reuses_persisted_index_when_snapshots_match() {
        let root = unique_temp_dir("sqlite-cached");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nService anchor.\n");

        let (first, rebuilt_first) = get_search_index(&root, None, None).expect("first");
        let (second, rebuilt_second) = get_search_index(&root, None, None).expect("second");

        assert!(rebuilt_first);
        assert!(!rebuilt_second);
        assert_eq!(second.generated_at, first.generated_at);
        assert_eq!(second.file_snapshots, first.file_snapshots);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_search_index_rebuilds_when_persisted_version_is_stale() {
        // The version gate must force a rebuild when the on-disk index predates the
        // current schema version (the migration path for the new chunker): load maps a
        // version mismatch to Ok(None) -> get_search_index rebuilds with the current
        // version stamped back into metadata.
        let root = unique_temp_dir("sqlite-stale-version");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nService anchor.\n");

        let (_first, rebuilt_first) = get_search_index(&root, None, None).expect("first build");
        assert!(rebuilt_first);

        // Stamp an older version into the persisted metadata, simulating a pre-bump index.
        {
            let connection =
                open_index_connection(&index_file_path(&root, None), false).expect("open index");
            connection
                .execute(
                    "UPDATE metadata SET value = ?1 WHERE key = 'version'",
                    params![(INDEX_VERSION - 1).to_string()],
                )
                .expect("downgrade version");
        }

        // load_index must reject the stale index (Ok(None)); get_search_index rebuilds.
        assert!(load_index(&root, None).expect("load").is_none());
        let (rebuilt, rebuilt_again) = get_search_index(&root, None, None).expect("rebuild");
        assert!(rebuilt_again, "stale version must trigger a rebuild");
        assert_eq!(rebuilt.version, INDEX_VERSION);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_search_index_incrementally_adds_note_without_rewriting_existing_ids() {
        let root = unique_temp_dir("sqlite-incremental-add");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nAlpha anchor.\n");
        write_fixture(&root, "Other.md", "# Other\n\nBeta anchor.\n");

        build_index(&root, None, None).expect("build");
        let home_id = note_id(&root, "Home.md").expect("home id");
        let home_chunks = chunk_ids(&root, "Home.md");

        write_fixture(&root, "New.md", "# New\n\nGamma anchor.\n");
        let (updated, rebuilt) = get_search_index(&root, None, None).expect("incremental refresh");

        assert!(rebuilt);
        assert_eq!(updated.note_count, 3);
        assert!(updated.note("New.md").is_some());
        assert_eq!(note_id(&root, "Home.md"), Some(home_id));
        assert_eq!(chunk_ids(&root, "Home.md"), home_chunks);
        assert_eq!(updated.document_frequencies.get("gamma"), Some(&1));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_search_index_incrementally_updates_one_note_and_preserves_others() {
        let root = unique_temp_dir("sqlite-incremental-modify");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nAlpha original anchor.\n");
        write_fixture(&root, "Other.md", "# Other\n\nBeta stable anchor.\n");

        build_index(&root, None, None).expect("build");
        let home_id = note_id(&root, "Home.md").expect("home id");
        let home_chunks = chunk_ids(&root, "Home.md");
        let other_id = note_id(&root, "Other.md").expect("other id");
        let other_chunks = chunk_ids(&root, "Other.md");

        write_fixture(
            &root,
            "Home.md",
            "# Home\n\nGamma changed anchor with extra words.\n",
        );
        let (updated, rebuilt) = get_search_index(&root, None, None).expect("incremental refresh");

        assert!(rebuilt);
        assert_eq!(updated.note_count, 2);
        assert_eq!(note_id(&root, "Home.md"), Some(home_id));
        assert_ne!(chunk_ids(&root, "Home.md"), home_chunks);
        assert_eq!(note_id(&root, "Other.md"), Some(other_id));
        assert_eq!(chunk_ids(&root, "Other.md"), other_chunks);
        assert!(!updated.document_frequencies.contains_key("original"));
        assert_eq!(updated.document_frequencies.get("gamma"), Some(&1));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_search_index_incrementally_deletes_note_and_preserves_remaining_ids() {
        let root = unique_temp_dir("sqlite-incremental-delete");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nAlpha anchor.\n");
        write_fixture(&root, "Other.md", "# Other\n\nBeta stable anchor.\n");

        build_index(&root, None, None).expect("build");
        let other_id = note_id(&root, "Other.md").expect("other id");
        let other_chunks = chunk_ids(&root, "Other.md");

        fs::remove_file(root.join("Home.md")).expect("delete fixture");
        let (updated, rebuilt) = get_search_index(&root, None, None).expect("incremental refresh");

        assert!(rebuilt);
        assert_eq!(updated.note_count, 1);
        assert!(updated.note("Home.md").is_none());
        assert_eq!(note_id(&root, "Home.md"), None);
        assert_eq!(note_id(&root, "Other.md"), Some(other_id));
        assert_eq!(chunk_ids(&root, "Other.md"), other_chunks);
        assert!(!updated.document_frequencies.contains_key("alpha"));
        assert_eq!(updated.document_frequencies.get("beta"), Some(&1));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_search_index_incrementally_updates_artifacts_without_rewriting_unchanged_ids() {
        let root = unique_temp_dir("artifact-incremental");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nAlpha anchor.\n");
        write_fixture(&root, "docs/Guide.pdf", "pdf one");
        write_fixture(&root, "images/Logo.png", "png one");

        let (built, rebuilt_first) =
            get_search_index_with_artifacts(&root, None, None, None).expect("build");
        assert!(rebuilt_first);
        assert_eq!(built.note_count, 1);
        assert_eq!(built.artifact_count, 2);
        assert_eq!(built.vectorized_artifact_count, 0);
        let guide_id = artifact_id(&root, "docs/Guide.pdf").expect("guide id");
        let logo_id = artifact_id(&root, "images/Logo.png").expect("logo id");
        let home_id = note_id(&root, "Home.md").expect("home id");

        write_fixture(&root, "audio/Clip.mp3", "audio one");
        let (updated, rebuilt) =
            get_search_index_with_artifacts(&root, None, None, None).expect("refresh");

        assert!(rebuilt);
        assert_eq!(updated.artifact_count, 3);
        assert_eq!(artifact_id(&root, "docs/Guide.pdf"), Some(guide_id));
        assert_eq!(artifact_id(&root, "images/Logo.png"), Some(logo_id));
        assert_eq!(note_id(&root, "Home.md"), Some(home_id));
        assert!(artifact_id(&root, "audio/Clip.mp3").is_some());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn artifact_embeddings_are_whole_file_vectors_and_searchable_separately() {
        let root = unique_temp_dir("artifact-embedding-search");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nAlpha anchor.\n");
        write_fixture(&root, "docs/Guide.pdf", "pdf bytes");
        let (base_url, seen_inputs) = start_embedding_server(2);
        let artifact_config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("omni-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();

        let index =
            build_index_with_artifacts(&root, None, None, Some(&artifact_config)).expect("build");

        assert_eq!(index.artifact_count, 1);
        assert_eq!(index.vectorized_artifact_count, 1);
        assert_eq!(index.chunk_count, 1);
        assert_eq!(index.artifact_embedding_dimensions, Some(3));
        assert_eq!(seen_inputs.lock().expect("inputs lock").len(), 1);

        let matches = crate::search::artifact_semantic_search_with_options(
            &index,
            "find the pdf",
            crate::search::RankingOptions {
                limit: 4,
                semantic_weight: 1.0,
                bm25_weight: 0.0,
            },
        )
        .expect("artifact search");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "docs/Guide.pdf");
        let seen = seen_inputs.lock().expect("inputs lock");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].len(), 1);
        assert_eq!(seen[1], vec!["find the pdf".to_string()]);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_search_index_reused_embedding_index_uses_live_runtime_transport_config() {
        let root = unique_temp_dir("sqlite-cached-embedding-runtime");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nService anchor.\n");

        let index_file = index_file_path(&root, None);
        let mut persisted = build_index(&root, None, None).expect("build");
        persisted.semantic_backend = SemanticBackend::Embedding;
        persisted.embedding_provider = Some("openai-compatible".to_string());
        persisted.embedding_model = Some("text-embedding-3-small".to_string());
        persisted.embedding_dimensions = Some(3);
        persisted.embedding_base_url = Some("http://persisted.example".to_string());
        for (offset, note) in persisted.notes.iter_mut().enumerate() {
            note.embedding = Some(vec![1.0 + offset as f64, 0.0, 0.0]);
        }
        for (offset, chunk) in persisted.chunks.iter_mut().enumerate() {
            chunk.embedding = Some(vec![1.0 + offset as f64, 0.0, 0.0]);
        }

        let mut connection = open_index_connection(&index_file, false).expect("open index");
        write_search_index_to_connection(&mut connection, &persisted).expect("persist");
        connection
            .execute(
                "INSERT INTO embedding_config (key, value) VALUES ('apiKeyEnv', 'PERSISTED_KEY')",
                [],
            )
            .expect("insert old api key env metadata");
        connection
            .execute(
                "INSERT INTO embedding_config (key, value) VALUES ('apiKey', 'persisted-secret')",
                [],
            )
            .expect("insert old api key metadata");

        let runtime_config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-3-small".to_string()),
            base_url: Some("http://runtime.example".to_string()),
            api_key: Some("runtime-secret".to_string()),
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();

        let (loaded, rebuilt) = get_search_index(&root, None, Some(&runtime_config)).expect("load");

        assert!(!rebuilt);
        assert_eq!(loaded.generated_at, persisted.generated_at);
        assert_eq!(
            loaded.embedding_provider.as_deref(),
            Some("openai-compatible")
        );
        assert_eq!(
            loaded.embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(
            loaded.embedding_base_url.as_deref(),
            Some("http://runtime.example")
        );
        assert_eq!(
            loaded.runtime_embedding_api_key.as_deref(),
            Some("runtime-secret")
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_search_index_incremental_embedding_only_requests_changed_note_vectors() {
        let root = unique_temp_dir("sqlite-incremental-embedding");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nService anchor.\n");

        let index_file = index_file_path(&root, None);
        let mut persisted = build_index(&root, None, None).expect("build");
        persisted.semantic_backend = SemanticBackend::Embedding;
        persisted.embedding_provider = Some("openai-compatible".to_string());
        persisted.embedding_model = Some("text-embedding-test".to_string());
        persisted.embedding_dimensions = Some(3);
        persisted.embedding_base_url = Some("http://persisted.example".to_string());
        for note in &mut persisted.notes {
            note.embedding = Some(vec![1.0, 0.0, 0.0]);
        }
        for chunk in &mut persisted.chunks {
            chunk.embedding = Some(vec![1.0, 0.0, 0.0]);
        }
        let mut connection = open_index_connection(&index_file, false).expect("open index");
        write_search_index_to_connection(&mut connection, &persisted).expect("persist");
        let home_id = note_id(&root, "Home.md").expect("home id");
        let home_chunks = chunk_ids(&root, "Home.md");
        // Note vectors are mean-pooled from chunk vectors, so incremental refresh
        // issues a single embedding request (chunks of the changed note only).
        let (base_url, seen_inputs) = start_embedding_server(1);

        write_fixture(&root, "New.md", "# New\n\nFresh embedding target.\n");
        let runtime_config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();
        let (updated, rebuilt) =
            get_search_index(&root, None, Some(&runtime_config)).expect("incremental refresh");

        assert!(rebuilt);
        assert_eq!(updated.semantic_backend, SemanticBackend::Embedding);
        assert_eq!(updated.note_count, 2);
        assert_eq!(note_id(&root, "Home.md"), Some(home_id));
        assert_eq!(chunk_ids(&root, "Home.md"), home_chunks);
        assert_eq!(vector_counts(&root), (2, 2));
        let related = crate::search::related_notes(&updated, "Home.md").expect("related notes");
        assert!(related.iter().any(|item| item.path == "New.md"));
        let seen = seen_inputs.lock().expect("inputs lock");
        assert_eq!(seen.len(), 1);
        assert!(seen.iter().flatten().all(|input| input.contains("New")));
        assert!(seen.iter().flatten().all(|input| !input.contains("Home")));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn chunk_lines_respects_max_chars() {
        // Three 10-char lines, no overlap, 15-char budget -> one line per chunk.
        let text = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc";
        let chunks = chunk_lines(text, 80, 0, 15);
        assert_eq!(chunks.len(), 3);
        for (_, start, end, body) in &chunks {
            assert!(end >= start);
            assert!(body.chars().count() <= 15);
        }
        // Line ranges stay contiguous and cover the whole note.
        assert_eq!(chunks.first().unwrap().1, 1);
        assert_eq!(chunks.last().unwrap().2, 3);
    }

    #[test]
    fn chunk_lines_emits_oversized_single_line_as_own_chunk() {
        let huge = "z".repeat(50);
        let text = format!("short\n{huge}\nshort2");
        let chunks = chunk_lines(&text, 80, 0, 20);
        // The 50-char line exceeds the 20-char budget but is kept whole, on its
        // own line boundary, to be truncated downstream by the input clamp.
        assert!(chunks.iter().any(|(_, _, _, body)| body == &huge));
        // No chunk splits a line mid-way.
        assert!(chunks
            .iter()
            .all(|(_, _, _, body)| text.contains(body.as_str())));
    }

    #[test]
    fn note_embedding_is_mean_pool_of_chunk_vectors() {
        // Two chunks -> one embedding request; the note vector must equal the
        // normalized mean of its chunk vectors, and the full note is never sent.
        let (base_url, seen_inputs) = start_embedding_server(1);
        let config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();

        let make_chunk = |idx: usize, text: &str| SearchChunk {
            path: "Note.md".to_string(),
            title: "Note".to_string(),
            chunk_index: idx,
            start_line: 1,
            end_line: 1,
            text: text.to_string(),
            term_counts: Default::default(),
            norm: 0.0,
            token_count: 0,
            embedding: None,
        };
        let mut prepared = vec![PreparedNote {
            snapshot: FileSnapshot {
                path: "Note.md".to_string(),
                mtime_ms: 0,
                size: 0,
            },
            note: SearchNote {
                path: "Note.md".to_string(),
                title: "Note".to_string(),
                content: "body".to_string(),
                term_counts: Default::default(),
                norm: 0.0,
                token_count: 0,
                links: Vec::new(),
                embedding: None,
            },
            chunks: vec![make_chunk(0, "chunk one"), make_chunk(1, "chunk two")],
            chunk_texts: vec!["Note\nchunk one".to_string(), "Note\nchunk two".to_string()],
        }];

        embed_prepared_notes(&mut prepared, &config, None).expect("embed prepared notes");

        let chunk_vecs: Vec<Vec<f64>> = prepared[0]
            .chunks
            .iter()
            .map(|chunk| chunk.embedding.clone().expect("chunk embedding"))
            .collect();
        assert_eq!(chunk_vecs.len(), 2);
        let dims = chunk_vecs[0].len();
        let mut mean = vec![0.0_f64; dims];
        for vector in &chunk_vecs {
            for (slot, value) in mean.iter_mut().zip(vector.iter()) {
                *slot += value / chunk_vecs.len() as f64;
            }
        }
        let expected = normalize_dense_vector(&mean);
        let note_embedding = prepared[0]
            .note
            .embedding
            .clone()
            .expect("note embedding present");
        assert_eq!(note_embedding.len(), dims);
        for (actual, want) in note_embedding.iter().zip(expected.iter()) {
            assert!(
                (actual - want).abs() < 1e-9,
                "note vector must be the normalized mean of its chunk vectors"
            );
        }
        // Exactly one request, and the full note body was never embedded.
        let seen = seen_inputs.lock().expect("inputs lock");
        assert_eq!(seen.len(), 1);
        assert!(seen
            .iter()
            .flatten()
            .all(|input| input != "body" && input != "Note\nbody"));
    }

    #[test]
    fn normalize_dense_vector_handles_zero_norm() {
        assert_eq!(normalize_dense_vector(&[0.0, 0.0]), vec![0.0, 0.0]);
        let normalized = normalize_dense_vector(&[3.0, 4.0]);
        assert!((normalized[0] - 0.6).abs() < 1e-9);
        assert!((normalized[1] - 0.8).abs() < 1e-9);
    }

    #[test]
    fn pattern_parser_reports_invalid_patterns() {
        let error = matches_pattern("abc", "\\", true).expect_err("invalid regex");
        match error {
            IndexError::InvalidRegex { .. } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn glob_matching_supports_wildcards() {
        assert!(path_matches_glob("Projects/Brew Service.md", "Projects/*.md").expect("glob"));
        assert!(!path_matches_glob("Projects/Brew Service.md", "Research/*.md").expect("glob"));
    }

    /// Embedding server that fails (HTTP 500) for any request whose `input` array
    /// contains the sentinel substring, and returns a valid 3-dim embedding
    /// otherwise. Served in an unbounded loop so the bulk-then-per-note retry path
    /// (whose request count is not 1:1 with notes) never out-runs `take(N)`.
    fn start_sentinel_failing_server(sentinel: &'static str) -> String {
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
    fn embed_prepared_notes_keeps_good_notes_when_one_note_fails() {
        // One note's chunk triggers a backend 500; the other embeds fine. The build
        // must NOT abort: good note gets a dense vector, failing note falls back to
        // BM25/sparse (no vector) and is reported in `failed_paths`.
        const SENTINEL: &str = "EMBED_BREAK";
        let base_url = start_sentinel_failing_server(SENTINEL);
        let config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();

        let make_chunk = |path: &str, idx: usize, text: &str| SearchChunk {
            path: path.to_string(),
            title: path.to_string(),
            chunk_index: idx,
            start_line: 1,
            end_line: 1,
            text: text.to_string(),
            term_counts: Default::default(),
            norm: 0.0,
            token_count: 0,
            embedding: None,
        };
        let make_note = |path: &str, chunk_text: &str| PreparedNote {
            snapshot: FileSnapshot {
                path: path.to_string(),
                mtime_ms: 0,
                size: 0,
            },
            note: SearchNote {
                path: path.to_string(),
                title: path.to_string(),
                content: "body".to_string(),
                term_counts: Default::default(),
                norm: 0.0,
                token_count: 0,
                links: Vec::new(),
                embedding: None,
            },
            chunks: vec![make_chunk(path, 0, chunk_text)],
            chunk_texts: vec![chunk_text.to_string()],
        };

        let mut prepared = vec![
            make_note("Good.md", "harmless content"),
            make_note("Bad.md", SENTINEL),
        ];

        let outcome = embed_prepared_notes(&mut prepared, &config, None)
            .expect("partial failure must not abort the build");

        // Good note: dense vector on note and chunk.
        assert!(
            prepared[0].note.embedding.is_some(),
            "good note must keep its dense vector"
        );
        assert!(prepared[0].chunks[0].embedding.is_some());

        // Failing note: no dense vector anywhere, reported in failed_paths.
        assert!(
            prepared[1].note.embedding.is_none(),
            "failing note must fall back to BM25/sparse (no dense vector)"
        );
        assert!(prepared[1].chunks[0].embedding.is_none());
        assert_eq!(outcome.failed_paths, vec!["Bad.md".to_string()]);

        // Dimensions still observed from the note that did embed.
        assert_eq!(outcome.dimensions, Some(3));
    }

    #[test]
    fn build_index_with_one_failing_note_produces_usable_partial_index() {
        // End-to-end: a reindex where one note's embedding fails must complete
        // (return Ok), persist a loadable index, embed the good note, and leave the
        // failing note BM25-only (no dense vector) rather than aborting the build.
        const SENTINEL: &str = "EMBED_BREAK";
        let root = unique_temp_dir("partial-embed-build");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Good.md", "# Good\n\nHarmless searchable anchor.\n");
        write_fixture(&root, "Bad.md", &format!("# Bad\n\n{SENTINEL} anchor.\n"));

        let base_url = start_sentinel_failing_server(SENTINEL);
        let config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();

        // The whole build must NOT error just because one note failed to embed.
        let index = build_index(&root, None, Some(&config)).expect("partial build must succeed");

        assert_eq!(index.note_count, 2);
        assert_eq!(index.semantic_backend, SemanticBackend::Embedding);
        assert_eq!(index.embedding_dimensions, Some(3));

        // Good note keeps a dense vector; the failing note has none.
        assert!(
            index.note("Good.md").and_then(|n| n.embedding.as_ref()).is_some(),
            "good note must be embedded"
        );
        assert!(
            index.note("Bad.md").and_then(|n| n.embedding.as_ref()).is_none(),
            "failing note must fall back to BM25/sparse (no dense vector)"
        );

        // Persisted index is loadable and holds exactly the embedded vectors.
        let (note_vectors, chunk_vectors) = vector_counts(&root);
        assert_eq!(note_vectors, 1, "only the good note's vector is persisted");
        assert_eq!(chunk_vectors, 1, "only the good note's chunk vector is persisted");
        let loaded = load_index(&root, None)
            .expect("load partial index")
            .expect("persisted index present");
        assert_eq!(loaded.note_count, 2);
        assert!(loaded.note("Bad.md").is_some(), "failing note still indexed for BM25");

        // Usable: BM25 still surfaces the un-embedded note, and semantic search
        // (KNN by rowid) returns the embedded note without choking on the missing
        // vector rows.
        let bm25 = crate::search::bm25_search(&loaded, "anchor").expect("bm25 search");
        assert!(
            bm25.iter().any(|m| m.path == "Bad.md"),
            "un-embedded note must remain findable via BM25"
        );
        let semantic = crate::search::semantic_search(&loaded, "harmless searchable")
            .expect("semantic search on partial index");
        assert!(
            semantic.iter().any(|m| m.path == "Good.md"),
            "embedded note must be findable via semantic search"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Resolve a localhost address that is closed, so every embedding request fails
    /// fast with connection-refused (no waiting on a timeout). We bind then drop the
    /// listener to free the port.
    fn closed_local_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind to reserve port");
        let address = listener.local_addr().expect("server address");
        drop(listener);
        format!("http://{}", address)
    }

    #[test]
    fn build_index_with_total_embedding_failure_stays_loadable_as_bm25() {
        // Server fully down: EVERY note fails to embed. The build must still complete
        // and the persisted index must load (not be silently rejected and rebuilt
        // forever), serving every note via BM25.
        let root = unique_temp_dir("total-embed-failure");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Alpha.md", "# Alpha\n\nAlpha anchor.\n");
        write_fixture(&root, "Beta.md", "# Beta\n\nBeta anchor.\n");

        let config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(closed_local_url()),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();

        let index = build_index(&root, None, Some(&config))
            .expect("total embedding failure must not abort the build");
        assert_eq!(index.note_count, 2);

        // No vectors were produced; the persisted index must reload as a usable
        // (BM25) index rather than being treated as absent.
        let loaded = load_index(&root, None)
            .expect("load index after total embedding failure")
            .expect("persisted index must remain loadable, not silently rejected");
        assert_eq!(loaded.note_count, 2);
        let bm25 = crate::search::bm25_search(&loaded, "anchor").expect("bm25 search");
        assert!(bm25.iter().any(|m| m.path == "Alpha.md"));
        assert!(bm25.iter().any(|m| m.path == "Beta.md"));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn incremental_reindex_tolerates_a_failing_new_note() {
        // Build a healthy semantic index, then incrementally add a note that fails to
        // embed. The incremental rebuild must complete (the per-note insert tolerates
        // a missing vector) with the new note BM25-only and the old one still embedded.
        const SENTINEL: &str = "EMBED_BREAK";
        let root = unique_temp_dir("incremental-partial-embed");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nStable anchor.\n");

        let base_url = start_sentinel_failing_server(SENTINEL);
        let config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize();

        let (first, rebuilt_first) =
            get_search_index(&root, None, Some(&config)).expect("initial semantic build");
        assert!(rebuilt_first);
        assert_eq!(first.semantic_backend, SemanticBackend::Embedding);
        // Persisted state is the source of truth: vectors live in the sqlite-vec
        // tables, not the in-memory note/chunk structs (load leaves those `None`).
        assert_eq!(vector_counts(&root), (1, 1), "initial index embeds the only note");

        // Add a note whose chunk text triggers the backend 500.
        write_fixture(&root, "Bad.md", &format!("# Bad\n\n{SENTINEL} anchor.\n"));
        let (updated, rebuilt_second) = get_search_index(&root, None, Some(&config))
            .expect("incremental reindex must not abort on a failing note");
        assert!(rebuilt_second);
        assert_eq!(updated.note_count, 2);
        // Only the original note's vector persists; the failing note has none.
        assert_eq!(
            vector_counts(&root),
            (1, 1),
            "failing new note adds no vector; original note's vector is preserved"
        );

        // Reloads cleanly with the partial vector set, and the failing note is still
        // searchable via BM25.
        let loaded = load_index(&root, None)
            .expect("load incremental partial index")
            .expect("persisted index present");
        assert_eq!(loaded.semantic_backend, SemanticBackend::Embedding);
        let bm25 = crate::search::bm25_search(&loaded, "anchor").expect("bm25 search");
        assert!(bm25.iter().any(|m| m.path == "Bad.md"));
        assert!(bm25.iter().any(|m| m.path == "Home.md"));

        fs::remove_dir_all(root).ok();
    }

    /// Single-chunk prepared note whose chunk text is `chunk_text`. Single-chunk so
    /// the bisect path inside a batch call never adds extra requests.
    fn single_chunk_note(path: &str, chunk_text: &str) -> PreparedNote {
        PreparedNote {
            snapshot: FileSnapshot {
                path: path.to_string(),
                mtime_ms: 0,
                size: 0,
            },
            note: SearchNote {
                path: path.to_string(),
                title: path.to_string(),
                content: "body".to_string(),
                term_counts: Default::default(),
                norm: 0.0,
                token_count: 0,
                links: Vec::new(),
                embedding: None,
            },
            chunks: vec![SearchChunk {
                path: path.to_string(),
                title: path.to_string(),
                chunk_index: 0,
                start_line: 1,
                end_line: 1,
                text: chunk_text.to_string(),
                term_counts: Default::default(),
                norm: 0.0,
                token_count: 0,
                embedding: None,
            }],
            chunk_texts: vec![chunk_text.to_string()],
        }
    }

    fn embedding_test_config(base_url: String) -> EmbeddingConfig {
        EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: None,
        }
        .normalize()
    }

    /// Server that accepts connections but NEVER responds, counting how many requests
    /// it received. A request against it can only end via the client timeout. Returns
    /// the base URL and a shared request counter; the listener thread holds accepted
    /// connections open for the lifetime of the process.
    fn start_silent_counting_server() -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind silent server");
        let address = listener.local_addr().expect("server address");
        let count = Arc::new(AtomicUsize::new(0));
        let thread_count = Arc::clone(&count);
        thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        thread_count.fetch_add(1, Ordering::Relaxed);
                        // Hold the connection open without ever writing a response.
                        held.push(stream);
                    }
                    Err(_) => break,
                }
            }
        });
        (format!("http://{}", address), count)
    }

    #[test]
    fn per_note_short_circuits_against_a_hung_backend() {
        // FIX 1: against a backend that accepts the connection but hangs, the per-note
        // fallback must give up after K consecutive failures using a SHORT per-note
        // timeout — never grinding through all N notes at the full bulk timeout.
        let (base_url, request_count) = start_silent_counting_server();
        let config = embedding_test_config(base_url);

        // Eight notes, but only the first K should be attempted before short-circuit.
        let mut prepared = (0..8)
            .map(|i| single_chunk_note(&format!("Note{i}.md"), &format!("content {i}")))
            .collect::<Vec<_>>();

        let per_note_timeout = Duration::from_millis(200);
        let max_consecutive_failures = 3;
        let started = std::time::Instant::now();
        let outcome = embed_prepared_notes_per_note(
            &mut prepared,
            &config,
            None,
            per_note_timeout,
            max_consecutive_failures,
        )
        .expect("hung backend must not error; notes fall back to BM25/sparse");
        let elapsed = started.elapsed();

        // Every note is recorded as failed (the first K attempted, the rest
        // short-circuited without an attempt) and has no dense vector.
        assert_eq!(outcome.failed_paths.len(), 8, "all notes recorded as failed");
        assert!(prepared.iter().all(|p| p.note.embedding.is_none()));
        assert!(prepared.iter().all(|p| p.chunks[0].embedding.is_none()));

        // Only K HTTP requests were issued — we did NOT attempt all 8 notes.
        assert_eq!(
            request_count.load(Ordering::Relaxed),
            max_consecutive_failures,
            "must short-circuit after K attempts, not try every note"
        );

        // Bounded time: ~K * per_note_timeout, nowhere near N * 60s. Generous ceiling
        // to stay non-flaky on slow CI.
        assert!(
            elapsed < Duration::from_secs(5),
            "short-circuit must return promptly, took {elapsed:?}"
        );
    }

    #[test]
    fn bulk_deterministic_error_does_not_spray_per_note_calls() {
        // FIX 2: a DETERMINISTIC bulk failure (HTTP 4xx — e.g. bad model name) must NOT
        // trigger the note-by-note retry. The whole embed must error out after the bulk
        // attempt's requests, issuing no extra per-note calls.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 4xx server");
        let address = listener.local_addr().expect("server address");
        let count = Arc::new(AtomicUsize::new(0));
        let thread_count = Arc::clone(&count);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                thread_count.fetch_add(1, Ordering::Relaxed);
                // Drain the request, then always answer HTTP 400 (deterministic).
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
        });
        // batch_size = 1 so each note is its own bulk batch (one request, size 1) and
        // the in-batch bisect can never amplify. The bulk pass then issues exactly N
        // requests; a per-note retry would add a SECOND wave of N more.
        let mut config = embedding_test_config(format!("http://{}", address));
        config.batch_size = 1;
        let config = config.normalize();

        const NOTES: usize = 6;
        let mut prepared = (0..NOTES)
            .map(|i| single_chunk_note(&format!("Note{i}.md"), &format!("content {i}")))
            .collect::<Vec<_>>();

        let error = embed_prepared_notes(&mut prepared, &config, None)
            .expect_err("a deterministic 4xx must propagate, not retry per-note");
        assert!(matches!(error, IndexError::Embedding(_)));

        // Exactly N requests from the bulk pass and ZERO from a per-note retry. If the
        // deterministic error had wrongly fallen back, we'd see ~2N (a second wave of
        // one request per note).
        let requests = count.load(Ordering::Relaxed);
        assert_eq!(
            requests, NOTES,
            "deterministic error must not spray a per-note retry wave; saw {requests} requests"
        );
    }

    #[test]
    fn auto_default_query_instruction_for_qwen3_models() {
        // qwen3-embedding (case-insensitive, with a tag) auto-enables the default.
        assert_eq!(
            default_query_instruction_for_model(Some("qwen3-embedding:0.6b")),
            Some(embeddings::DEFAULT_SEARCH_QUERY_INSTRUCTION.to_string())
        );
        assert_eq!(
            default_query_instruction_for_model(Some("Qwen3-Embedding-8B")),
            Some(embeddings::DEFAULT_SEARCH_QUERY_INSTRUCTION.to_string())
        );
    }

    #[test]
    fn no_auto_default_query_instruction_for_other_models() {
        assert_eq!(
            default_query_instruction_for_model(Some("text-embedding-3-small")),
            None
        );
        assert_eq!(
            default_query_instruction_for_model(Some("pseudo-eval-model")),
            None
        );
        assert_eq!(default_query_instruction_for_model(None), None);
    }

    #[test]
    fn explicit_query_instruction_overrides_auto_default() {
        // A user-set instruction wins even on a qwen3 model.
        let mut index = SearchIndex {
            version: 1,
            generated_at: String::new(),
            semantic_backend: SemanticBackend::Embedding,
            embedding_provider: Some("openai-compatible".to_string()),
            embedding_model: Some("qwen3-embedding:0.6b".to_string()),
            embedding_dimensions: None,
            embedding_base_url: Some("http://unused".to_string()),
            runtime_embedding_api_key: None,
            runtime_query_instruction: Some("Custom task".to_string()),
            artifact_embedding_provider: None,
            artifact_embedding_model: None,
            artifact_embedding_dimensions: None,
            artifact_embedding_base_url: None,
            runtime_artifact_embedding_api_key: None,
            artifact_embedding_error: None,
            file_snapshots: Vec::new(),
            artifact_snapshots: Vec::new(),
            document_frequencies: BTreeMap::new(),
            chunk_count: 0,
            note_count: 0,
            artifact_count: 0,
            vectorized_artifact_count: 0,
            skipped_artifact_count: 0,
            notes: Vec::new(),
            chunks: Vec::new(),
            artifacts: Vec::new(),
            context: None,
        };

        let config = embedding_runtime_config(&index).expect("embedding runtime config");
        assert_eq!(config.query_instruction.as_deref(), Some("Custom task"));

        // With no explicit override, the qwen3 model auto-defaults.
        index.runtime_query_instruction = None;
        let config = embedding_runtime_config(&index).expect("embedding runtime config");
        assert_eq!(
            config.query_instruction.as_deref(),
            Some(embeddings::DEFAULT_SEARCH_QUERY_INSTRUCTION)
        );

        // A non-qwen3 model stays plain (None).
        index.embedding_model = Some("text-embedding-3-small".to_string());
        let config = embedding_runtime_config(&index).expect("embedding runtime config");
        assert_eq!(config.query_instruction, None);
    }

    #[test]
    fn apply_runtime_embedding_config_injects_query_instruction_at_load() {
        // Mirrors how `runtime_embedding_api_key` is injected at load: a populated
        // user config sets the non-persisted `runtime_query_instruction`, so an
        // explicit override survives a plain index load (not just a rebuild).
        let mut index = SearchIndex {
            version: 1,
            generated_at: String::new(),
            semantic_backend: SemanticBackend::Embedding,
            embedding_provider: Some("openai-compatible".to_string()),
            embedding_model: Some("text-embedding-3-small".to_string()),
            embedding_dimensions: None,
            embedding_base_url: Some("http://unused".to_string()),
            runtime_embedding_api_key: None,
            runtime_query_instruction: None,
            artifact_embedding_provider: None,
            artifact_embedding_model: None,
            artifact_embedding_dimensions: None,
            artifact_embedding_base_url: None,
            runtime_artifact_embedding_api_key: None,
            artifact_embedding_error: None,
            file_snapshots: Vec::new(),
            artifact_snapshots: Vec::new(),
            document_frequencies: BTreeMap::new(),
            chunk_count: 0,
            note_count: 0,
            artifact_count: 0,
            vectorized_artifact_count: 0,
            skipped_artifact_count: 0,
            notes: Vec::new(),
            chunks: Vec::new(),
            artifacts: Vec::new(),
            context: None,
        };

        let user_config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-3-small".to_string()),
            base_url: Some("http://unused".to_string()),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
            max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
            context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
            chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
            query_instruction: Some("Custom task".to_string()),
        };

        apply_runtime_embedding_config(&mut index, Some(&user_config));
        assert_eq!(
            index.runtime_query_instruction.as_deref(),
            Some("Custom task")
        );

        // And it flows through to the runtime config even on a non-qwen3 model.
        let runtime = embedding_runtime_config(&index).expect("runtime config");
        assert_eq!(runtime.query_instruction.as_deref(), Some("Custom task"));
    }
}

