use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{SecondsFormat, Utc};
use rusqlite::{params, types::Type, Connection, OptionalExtension, TransactionBehavior};
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
    note_text: String,
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
const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 25 * 1024 * 1024;
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
) -> Vec<(usize, usize, usize, String)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let safe_chunk_size = chunk_size_lines.max(1);
    let safe_overlap = overlap_lines.min(safe_chunk_size.saturating_sub(1));
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chunk_index = 0;

    while start < lines.len() {
        let end = (start + safe_chunk_size).min(lines.len());
        chunks.push((chunk_index, start + 1, end, lines[start..end].join("\n")));
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
            api_key: index.runtime_embedding_api_key.clone(),
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
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
    let note_text = format!("{title}\n{content}");

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

    let mut chunks = Vec::new();
    let mut chunk_texts = Vec::new();
    for (chunk_index, start_line, end_line, text) in chunk_lines(
        &content,
        DEFAULT_CHUNK_SIZE_LINES,
        DEFAULT_CHUNK_OVERLAP_LINES,
    ) {
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

    Ok(PreparedNote {
        snapshot: snapshot.clone(),
        note,
        chunks,
        note_text,
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

fn embed_prepared_notes(
    prepared_notes: &mut [PreparedNote],
    index_config: &EmbeddingConfig,
    expected_dimensions: Option<usize>,
) -> Result<Option<usize>> {
    if !index_config.supports_embeddings() {
        return Ok(None);
    }

    if prepared_notes.is_empty() {
        return Ok(expected_dimensions);
    }

    let note_texts = prepared_notes
        .iter()
        .map(|prepared| prepared.note_text.clone())
        .collect::<Vec<_>>();
    let mut observed_dimensions = None;
    let note_embedding_batch = embeddings::embed_text_batches(&note_texts, index_config, None)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
    ensure_embedding_dimensions(
        note_embedding_batch.dimensions,
        expected_dimensions,
        &mut observed_dimensions,
    )?;
    let note_embeddings = note_embedding_batch
        .vectors
        .into_iter()
        .map(|vector| normalize_dense_vector(&vector))
        .collect::<Vec<_>>();

    for (prepared, embedding) in prepared_notes.iter_mut().zip(note_embeddings.into_iter()) {
        prepared.note.embedding = Some(embedding);
    }

    let chunk_texts = prepared_notes
        .iter()
        .flat_map(|prepared| prepared.chunk_texts.iter().cloned())
        .collect::<Vec<_>>();
    let chunk_embedding_batch = embeddings::embed_text_batches(&chunk_texts, index_config, None)
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
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

    let mut embeddings = chunk_embeddings.into_iter();
    for prepared in prepared_notes {
        for chunk in &mut prepared.chunks {
            let embedding = embeddings.next().ok_or_else(|| {
                IndexError::Embedding(
                    "embedding provider returned too few chunk vectors".to_string(),
                )
            })?;
            chunk.embedding = Some(embedding);
        }
    }

    if embeddings.next().is_some() {
        return Err(IndexError::Embedding(
            "embedding provider returned too many chunk vectors".to_string(),
        ));
    }

    Ok(observed_dimensions.or(expected_dimensions))
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
        if note_vec_count != notes.len() || chunk_vec_count != chunks.len() {
            return Err(IndexError::Embedding(
                "embedding index is missing one or more persisted vectors".to_string(),
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

    if semantic_backend == &SemanticBackend::Embedding {
        let embedding = prepared.note.embedding.as_ref().ok_or_else(|| {
            IndexError::Embedding(format!("missing note embedding for {}", prepared.note.path))
        })?;
        tx.execute(
            "INSERT INTO note_embeddings_vec (rowid, embedding) VALUES (?1, ?2)",
            params![note_id, embedding_blob(embedding)],
        )
        .map_err(|error| IndexError::Embedding(error.to_string()))?;
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
            let embedding = chunk.embedding.as_ref().ok_or_else(|| {
                IndexError::Embedding(format!("missing chunk embedding for {}", chunk.path))
            })?;
            tx.execute(
                "INSERT INTO chunk_embeddings_vec (rowid, embedding) VALUES (?1, ?2)",
                params![chunk_id, embedding_blob(embedding)],
            )
            .map_err(|error| IndexError::Embedding(error.to_string()))?;
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
    let embedding_dimensions =
        embed_prepared_notes(&mut prepared_notes, &index_config, expected_dimensions)?;
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
    let embedding_dimensions = embed_prepared_notes(&mut prepared_notes, &index_config, None)?;
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
        let (base_url, seen_inputs) = start_embedding_server(2);

        write_fixture(&root, "New.md", "# New\n\nFresh embedding target.\n");
        let runtime_config = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("text-embedding-test".to_string()),
            base_url: Some(base_url),
            api_key: None,
            max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
            batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
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
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().flatten().all(|input| input.contains("New")));
        assert!(seen.iter().flatten().all(|input| !input.contains("Home")));

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
