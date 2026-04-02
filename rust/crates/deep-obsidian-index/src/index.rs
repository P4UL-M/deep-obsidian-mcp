use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior, types::Type};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::embeddings::{self, EmbeddingConfig, EmbeddingProvider};
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
    InvalidRegex {
        pattern: String,
        message: String,
    },
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
pub struct SearchIndex {
    pub version: u32,
    pub generated_at: String,
    pub semantic_backend: SemanticBackend,
    pub embedding_provider: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: Option<usize>,
    pub embedding_base_url: Option<String>,
    pub embedding_api_key_env: Option<String>,
    #[serde(skip_serializing, default)]
    pub embedding_api_key: Option<String>,
    pub file_snapshots: Vec<FileSnapshot>,
    pub document_frequencies: BTreeMap<String, usize>,
    pub chunk_count: usize,
    pub note_count: usize,
    pub notes: Vec<SearchNote>,
    pub chunks: Vec<SearchChunk>,
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

const INDEX_VERSION: u32 = 2;
const DEFAULT_CHUNK_SIZE_LINES: usize = 80;
const DEFAULT_CHUNK_OVERLAP_LINES: usize = 12;
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "in", "into", "is", "it",
    "of", "on", "or", "that", "the", "this", "to", "with",
];
const IGNORED_DIRS: &[&str] = &[".git", ".obsidian", ".trash", ".deep-obsidian-mcp", "node_modules"];

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
        return Err(IndexError::InvalidVaultRelativePath(relative_path.to_string()));
    }

    if Path::new(normalized).components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(IndexError::InvalidVaultRelativePath(relative_path.to_string()));
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
    if Path::new(normalized).components().any(|component| match component {
        std::path::Component::Normal(part) => is_protected_template_segment(&part.to_string_lossy()),
        _ => false,
    }) {
        return Err(IndexError::InvalidVaultRelativePath(relative_path.to_string()));
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
) -> Vec<(usize, usize, usize, String)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let safe_chunk_size = chunk_size_lines.max(1);
    let safe_overlap = overlap_lines.min(safe_chunk_size.saturating_sub(1));
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chunk_index = 0;

    while start < lines.len() {
        let end = (start + safe_chunk_size).min(lines.len());
        chunks.push((
            chunk_index,
            start + 1,
            end,
            lines[start..end].join("\n"),
        ));
        if end >= lines.len() {
            break;
        }
        start = end.saturating_sub(safe_overlap);
        chunk_index += 1;
    }

    chunks
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
    let sum: f64 = term_counts.values().map(|value| (*value as f64) * (*value as f64)).sum();
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
            return Some(value.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }

    None
}

pub fn heading_title(content: &str) -> Option<String> {
    content
        .split('\n')
        .find_map(|line| line.strip_prefix("# ").map(|title| title.trim().to_string()))
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
        headings.push((level, title.clone(), normalize_heading_slug_value(&title), index + 1));
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

pub fn same_snapshots(left: &[FileSnapshot], right: &[FileSnapshot]) -> bool {
    left == right
}

pub fn same_semantic_config(index: &SearchIndex, embedding_config: Option<&EmbeddingConfig>) -> bool {
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

fn semantic_backend_from_config(embedding_config: Option<&EmbeddingConfig>) -> SemanticBackend {
    embedding_config
        .map(EmbeddingConfig::semantic_backend)
        .unwrap_or(SemanticBackend::Sparse)
}

fn normalized_embedding_config(embedding_config: Option<&EmbeddingConfig>) -> EmbeddingConfig {
    embedding_config.cloned().unwrap_or_else(EmbeddingConfig::sparse).normalize()
}

fn apply_runtime_embedding_config(index: &mut SearchIndex, embedding_config: Option<&EmbeddingConfig>) {
    let config = normalized_embedding_config(embedding_config);
    if index.semantic_backend != SemanticBackend::Embedding || !config.supports_embeddings() {
        return;
    }

    index.embedding_base_url = config.base_url.clone().filter(|value| !value.trim().is_empty());
    index.embedding_api_key_env = config.api_key_env.clone().filter(|value| !value.trim().is_empty());
    index.embedding_api_key = config.api_key.clone().filter(|value| !value.trim().is_empty());
}

pub fn embedding_runtime_config(index: &SearchIndex) -> Option<EmbeddingConfig> {
    if index.semantic_backend != SemanticBackend::Embedding {
        return None;
    }

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
            api_key: index.embedding_api_key.clone(),
            api_key_env: index.embedding_api_key_env.clone(),
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
        }
        .normalize(),
    )
}

fn open_index_connection(index_file: &Path, read_only: bool) -> std::result::Result<Connection, rusqlite::Error> {
    sqlite::open_index_connection(index_file, read_only)
}

fn index_file_path(vault_path: &Path, index_dir: Option<&Path>) -> PathBuf {
    sqlite::index_file_path(vault_path, index_dir)
}

fn json_to_sqlite_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
}

fn text_to_json<T: Serialize>(value: &T) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn parse_json<T: for<'de> Deserialize<'de>>(text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(json_to_sqlite_error)
}

fn embedding_blob(vector: &[f64]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(vector.len() * std::mem::size_of::<f32>());
    for value in vector {
        blob.extend_from_slice(&(*value as f32).to_le_bytes());
    }
    blob
}

pub fn index_context(index: &SearchIndex) -> Result<&IndexContext> {
    index.context.as_ref().ok_or(IndexError::MissingIndexContext)
}

pub fn open_index_connection_for_index(
    index: &SearchIndex,
    read_only: bool,
) -> Result<Connection> {
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

fn write_search_index_to_connection(conn: &mut Connection, index: &SearchIndex) -> Result<()> {
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS metadata;
        DROP TABLE IF EXISTS file_snapshots;
        DROP TABLE IF EXISTS document_frequencies;
        DROP TABLE IF EXISTS note_embeddings_vec;
        DROP TABLE IF EXISTS chunk_embeddings_vec;
        DROP TABLE IF EXISTS notes;
        DROP TABLE IF EXISTS chunks;
        DROP TABLE IF EXISTS embedding_config;
        "#,
    )
    .map_err(|error| IndexError::Embedding(error.to_string()))?;
    conn.execute_batch(sqlite::CURRENT_SCHEMA_DDL)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    sqlite::recreate_vector_tables(conn, index.embedding_dimensions)
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
            ("semanticBackend", index.semantic_backend.as_str().to_string()),
            ("chunkCount", index.chunk_count.to_string()),
            ("noteCount", index.note_count.to_string()),
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
        if let Some(api_key_env) = &index.embedding_api_key_env {
            insert_runtime_config
                .execute(params!["apiKeyEnv", api_key_env])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
        if let Some(api_key) = &index.embedding_api_key {
            insert_runtime_config
                .execute(params!["apiKey", api_key])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
        }
    }

    {
        let mut insert_snapshot = tx
            .prepare("INSERT INTO file_snapshots (path, mtime_ms, size) VALUES (?1, ?2, ?3)")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        for snapshot in &index.file_snapshots {
            insert_snapshot
                .execute(params![snapshot.path, snapshot.mtime_ms as i64, snapshot.size as i64])
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
                    text_to_json(&note.term_counts).map_err(|error| IndexError::Embedding(error.to_string()))?,
                    note.norm,
                    note.token_count as i64,
                    text_to_json(&note.links).map_err(|error| IndexError::Embedding(error.to_string()))?,
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
            insert_chunk
                .execute(params![
                    (id + 1) as i64,
                    chunk.path,
                    chunk.title,
                    chunk.chunk_index as i64,
                    chunk.start_line as i64,
                    chunk.end_line as i64,
                    chunk.text,
                    text_to_json(&chunk.term_counts).map_err(|error| IndexError::Embedding(error.to_string()))?,
                    chunk.norm,
                    chunk.token_count as i64,
                ])
                .map_err(|error| IndexError::Embedding(error.to_string()))?;

            if let (Some(insert_embedding), Some(embedding)) =
                (insert_chunk_embedding.as_mut(), chunk.embedding.as_ref())
            {
                insert_embedding
                    .execute(params![(id + 1) as i64, embedding_blob(embedding)])
                    .map_err(|error| IndexError::Embedding(error.to_string()))?;
            }
        }
    }

    tx.commit().map_err(|error| IndexError::Embedding(error.to_string()))?;
    Ok(())
}

fn load_search_index_from_connection(conn: &Connection) -> Result<SearchIndex> {
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

    let mut metadata_statement = conn
        .prepare("SELECT key, value FROM metadata")
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let metadata_rows = metadata_statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    let mut metadata = BTreeMap::new();
    for row in metadata_rows {
        let (key, value) = row.map_err(|error| IndexError::Embedding(error.to_string()))?;
        metadata.insert(key, value);
    }

    let version = metadata
        .get("version")
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| IndexError::Embedding("missing version metadata".to_string()))?;
    if version != INDEX_VERSION {
        return Err(IndexError::Embedding("unsupported index version".to_string()));
    }

    let file_snapshots = {
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
            .map_err(|error| IndexError::Embedding(error.to_string()))?
    };

    let document_frequencies = {
        let mut statement = conn
            .prepare("SELECT term, df FROM document_frequencies ORDER BY term")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize)))
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        rows.collect::<std::result::Result<BTreeMap<_, _>, _>>()
            .map_err(|error| IndexError::Embedding(error.to_string()))?
    };

    let runtime_config = {
        let mut statement = conn
            .prepare("SELECT key, value FROM embedding_config")
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
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

    if semantic_backend == SemanticBackend::Embedding {
        let note_vec_count: usize = conn
            .query_row("SELECT COUNT(*) FROM note_embeddings_vec", [], |row| row.get::<_, i64>(0))
            .map(|count| count as usize)
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        let chunk_vec_count: usize = conn
            .query_row("SELECT COUNT(*) FROM chunk_embeddings_vec", [], |row| row.get::<_, i64>(0))
            .map(|count| count as usize)
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
        if note_vec_count != notes.len() || chunk_vec_count != chunks.len() {
            return Err(IndexError::Embedding(
                "embedding index is missing one or more persisted vectors".to_string(),
            ));
        }
    }

    Ok(SearchIndex {
        version,
        generated_at: metadata
            .get("generatedAt")
            .cloned()
            .unwrap_or_else(now_utc_string),
        semantic_backend,
        embedding_provider: metadata.get("embeddingProvider").cloned(),
        embedding_model: metadata.get("embeddingModel").cloned(),
        embedding_dimensions: metadata.get("embeddingDimensions").and_then(|value| value.parse::<usize>().ok()),
        embedding_base_url: runtime_config.get("baseUrl").cloned(),
        embedding_api_key_env: runtime_config.get("apiKeyEnv").cloned(),
        embedding_api_key: runtime_config.get("apiKey").cloned(),
        file_snapshots,
        document_frequencies,
        chunk_count: metadata
            .get("chunkCount")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(chunks.len()),
        note_count: metadata
            .get("noteCount")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(notes.len()),
        notes,
        chunks,
        context: None,
    })
}

pub fn build_index(
    vault_path: &Path,
    index_dir: Option<&Path>,
    embedding_config: Option<&EmbeddingConfig>,
) -> Result<SearchIndex> {
    let snapshots = collect_snapshots(vault_path)?;
    build_index_from_snapshots(vault_path, index_dir, snapshots, embedding_config)
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

pub fn get_search_index(
    vault_path: &Path,
    index_dir: Option<&Path>,
    embedding_config: Option<&EmbeddingConfig>,
) -> Result<(SearchIndex, bool)> {
    if let Some(mut existing) = load_index(vault_path, index_dir)? {
        let snapshots = collect_snapshots(vault_path)?;
        if same_snapshots(&existing.file_snapshots, &snapshots)
            && same_semantic_config(&existing, embedding_config)
        {
            apply_runtime_embedding_config(&mut existing, embedding_config);
            return Ok((existing, false));
        }
    }

    let rebuilt = build_index(vault_path, index_dir, embedding_config)?;
    Ok((rebuilt, true))
}

pub fn build_index_from_snapshots(
    vault_path: &Path,
    index_dir: Option<&Path>,
    snapshots: Vec<FileSnapshot>,
    embedding_config: Option<&EmbeddingConfig>,
) -> Result<SearchIndex> {
    let resolved = ensure_vault_path(vault_path)?;
    let index_config = normalized_embedding_config(embedding_config);
    let mut notes = Vec::with_capacity(snapshots.len());
    let mut chunks = Vec::new();
    let mut document_frequencies: BTreeMap<String, usize> = BTreeMap::new();
    let mut note_texts = Vec::new();
    let mut chunk_texts = Vec::new();

    for snapshot in &snapshots {
        let absolute = ensure_inside_vault(&resolved, &snapshot.path)?;
        let content = fs::read_to_string(&absolute).map_err(|source| IndexError::Io {
            path: absolute.clone(),
            source,
        })?;
        let stem = path_stem(&snapshot.path);
        let title = note_title(stem, &content);
        let term_counts = count_terms(&format!("{title}\n{content}"));
        let links = extract_wiki_links(&content);

        let unique_terms: BTreeSet<_> = term_counts.keys().cloned().collect();
        for term in unique_terms {
            *document_frequencies.entry(term).or_insert(0) += 1;
        }

        notes.push(SearchNote {
            path: snapshot.path.clone(),
            title: title.clone(),
            content: content.clone(),
            term_counts: term_counts.clone(),
            norm: vector_norm(&term_counts),
            token_count: token_count(&term_counts),
            links,
            embedding: None,
        });
        note_texts.push(format!("{title}\n{content}"));

        for (chunk_index, start_line, end_line, text) in chunk_lines(&content, DEFAULT_CHUNK_SIZE_LINES, DEFAULT_CHUNK_OVERLAP_LINES) {
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
            chunk_texts.push(format!("{title}\n{text}"));
        }
    }

    let mut embedding_dimensions = None;
    if index_config.supports_embeddings() {
        let batches = index_config.batch_size.max(1);
        let embedding_result = {
            let mut note_embeddings = Vec::with_capacity(notes.len());
            for chunk in note_texts.chunks(batches) {
                let batch = embeddings::embed_texts(chunk, &index_config)
                    .map_err(|error| IndexError::Embedding(error.to_string()))?;
                embedding_dimensions = Some(batch.dimensions);
                note_embeddings.extend(batch.vectors.into_iter().map(|vector| normalize_dense_vector(&vector)));
            }
            note_embeddings
        };

        for (note, embedding) in notes.iter_mut().zip(embedding_result.into_iter()) {
            note.embedding = Some(embedding);
        }

        let mut chunk_embeddings = Vec::with_capacity(chunks.len());
        for chunk in chunk_texts.chunks(batches) {
            let batch = embeddings::embed_texts(chunk, &index_config)
                .map_err(|error| IndexError::Embedding(error.to_string()))?;
            embedding_dimensions = Some(batch.dimensions);
            chunk_embeddings.extend(batch.vectors.into_iter().map(|vector| normalize_dense_vector(&vector)));
        }
        for (chunk, embedding) in chunks.iter_mut().zip(chunk_embeddings.into_iter()) {
            chunk.embedding = Some(embedding);
        }
    }

    let index = SearchIndex {
        version: INDEX_VERSION,
        generated_at: now_utc_string(),
        semantic_backend: semantic_backend_from_config(embedding_config),
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
        embedding_model: index_config.model.clone().filter(|value| !value.trim().is_empty()),
        embedding_dimensions,
        embedding_base_url: if index_config.supports_embeddings() {
            index_config.base_url().map(|value| value.to_string())
        } else {
            None
        },
        embedding_api_key_env: if index_config.supports_embeddings() {
            index_config.api_key_env.clone()
        } else {
            None
        },
        embedding_api_key: if index_config.supports_embeddings() {
            index_config.api_key.clone()
        } else {
            None
        },
        file_snapshots: snapshots,
        document_frequencies,
        chunk_count: chunks.len(),
        note_count: notes.len(),
        notes,
        chunks,
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
    let mut connection = open_index_connection(&index_file, false).map_err(|source| IndexError::Io {
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
        let denominator = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * (document_length as f64 / average_document_length));
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
        .map(|item| item.with_normalized_score(if max_score > 0.0 { item.score() / max_score } else { 0.0 }))
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
        RegexAtom::Literal(expected) => normalize_char(expected, case_sensitive) == normalize_char(ch, case_sensitive),
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
            if let Some(end_pos) = regex_match_from(&pieces[1..], chars, candidate, case_sensitive, anchored_end) {
                return Some(end_pos);
            }
        }
        None
    } else if start_pos < chars.len() && atom_matches(piece.atom, chars[start_pos].1, case_sensitive) {
        regex_match_from(&pieces[1..], chars, start_pos + 1, case_sensitive, anchored_end)
    } else {
        None
    }
}

pub fn find_pattern_spans(text: &str, pattern: &str, case_sensitive: bool) -> Result<Vec<(usize, usize)>> {
    let (anchored_start, anchored_end, pieces) = parse_regex_subset(pattern)?;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut spans = Vec::new();

    if anchored_start {
        if let Some(end_pos) = regex_match_from(&pieces, &chars, 0, case_sensitive, anchored_end) {
            let start = 0;
            let end = if end_pos < chars.len() { chars[end_pos].0 } else { text.len() };
            if end > start {
                spans.push((start, end));
            }
        }
        return Ok(spans);
    }

    let mut start_pos = 0usize;
    while start_pos <= chars.len() {
        if let Some(end_pos) = regex_match_from(&pieces, &chars, start_pos, case_sensitive, anchored_end) {
            let start = if start_pos < chars.len() { chars[start_pos].0 } else { text.len() };
            let end = if end_pos < chars.len() { chars[end_pos].0 } else { text.len() };
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

fn glob_match_from(atoms: &[GlobAtom], chars: &[(usize, char)], atom_pos: usize, char_pos: usize, case_sensitive: bool) -> bool {
    if atom_pos == atoms.len() {
        return char_pos == chars.len();
    }

    match atoms[atom_pos] {
        GlobAtom::Literal(expected) => {
            char_pos < chars.len()
                && normalize_char(expected, case_sensitive) == normalize_char(chars[char_pos].1, case_sensitive)
                && glob_match_from(atoms, chars, atom_pos + 1, char_pos + 1, case_sensitive)
        }
        GlobAtom::AnyChar => {
            char_pos < chars.len() && glob_match_from(atoms, chars, atom_pos + 1, char_pos + 1, case_sensitive)
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    fn heading_and_block_extraction_follow_expected_rules() {
        let content = "# Title\n\n## Section One\nBody\n\ninline ^block-id\n\nParagraph\n^block-two\n";
        let headings = extract_heading_sections(content);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].slug, "title");
        assert_eq!(headings[1].slug, "section-one");

        let blocks = extract_block_sections(content);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, "block-id");
        assert_eq!(blocks[1].id, "block-two");
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
        let loaded = load_index(&root, None).expect("load").expect("persisted index");

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
        persisted.embedding_api_key_env = Some("PERSISTED_KEY".to_string());
        persisted.embedding_api_key = Some("persisted-secret".to_string());
        for (offset, note) in persisted.notes.iter_mut().enumerate() {
            note.embedding = Some(vec![1.0 + offset as f64, 0.0, 0.0]);
        }
        for (offset, chunk) in persisted.chunks.iter_mut().enumerate() {
            chunk.embedding = Some(vec![1.0 + offset as f64, 0.0, 0.0]);
        }

        let mut connection = open_index_connection(&index_file, false).expect("open index");
        write_search_index_to_connection(&mut connection, &persisted).expect("persist");

        let runtime_config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-3-small".to_string()),
            base_url: Some("http://runtime.example".to_string()),
            api_key: Some("runtime-secret".to_string()),
            api_key_env: Some("RUNTIME_KEY".to_string()),
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
        }
        .normalize();

        let (loaded, rebuilt) = get_search_index(&root, None, Some(&runtime_config)).expect("load");

        assert!(!rebuilt);
        assert_eq!(loaded.generated_at, persisted.generated_at);
        assert_eq!(loaded.embedding_provider.as_deref(), Some("openai-compatible"));
        assert_eq!(loaded.embedding_model.as_deref(), Some("text-embedding-3-small"));
        assert_eq!(loaded.embedding_base_url.as_deref(), Some("http://runtime.example"));
        assert_eq!(loaded.embedding_api_key_env.as_deref(), Some("RUNTIME_KEY"));
        assert_eq!(loaded.embedding_api_key.as_deref(), Some("runtime-secret"));

        fs::remove_dir_all(root).ok();
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
}
