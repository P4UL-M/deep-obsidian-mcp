use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use deep_obsidian_core::text::{
    extract_block_sections, extract_heading_sections, extract_wiki_links, normalize_heading_slug,
    note_title, tokenize,
};
use deep_obsidian_core::vault::{
    chunk_lines, ensure_inside_vault, list_children as vault_list_children,
    list_folders as vault_list_folders, list_markdown_files, list_top_level_folders,
    read_text_file, write_binary_file, write_text_file, VaultChildEntry, VaultEntryKind,
};
use deep_obsidian_index::graph as index_graph;
use deep_obsidian_index::index::{artifact_kind, artifact_mime_type, SearchIndex};
use deep_obsidian_index::search::{self as index_search, RankingOptions, RelatedNoteOptions};
use regex::RegexBuilder;
use serde_json::{json, Map, Value};

use crate::health::build_vault_overview_payload;
use crate::mcp::AppState;
use crate::protocol::{ToolCallResult, ToolContent, ToolDefinition};
use crate::resources::{artifact_uri, block_uri, heading_uri, note_name, note_uri};
const JSON_SCHEMA_URI: &str = "http://json-schema.org/draft-07/schema#";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const DEFAULT_MAX_TEXT_CHARS: usize = 20_000;

#[derive(Debug, Clone)]
struct KnowledgeNote {
    path: String,
    title: String,
    wiki_link: String,
    score: f64,
    reasons: Vec<String>,
    shared_links: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimilarityMode {
    Structure,
    Tone,
    Format,
    Style,
}

#[derive(Debug, Clone)]
struct NoteStyleProfile {
    structure: Vec<f64>,
    tone: Vec<f64>,
    format: Vec<f64>,
}

fn json_text_result(value: Value) -> ToolCallResult {
    json_text_result_with_format(value, None)
}

fn json_text_result_with_format(value: Value, format: Option<&str>) -> ToolCallResult {
    let text = if format == Some("compact") {
        serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
    } else {
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
    };
    ToolCallResult {
        content: vec![ToolContent { kind: "text", text }],
        structured_content: value,
    }
}

fn json_text_result_from_arguments(arguments: &Value, value: Value) -> ToolCallResult {
    let format = optional_string_arg(arguments, "format");
    json_text_result_with_format(value, format.as_deref())
}

fn string_arg(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing {}", key))
}

fn optional_string_arg(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn optional_enum_string_arg(
    arguments: &Value,
    key: &str,
    allowed: &[&str],
) -> Result<Option<String>, String> {
    let Some(value) = optional_string_arg(arguments, key) else {
        return Ok(None);
    };
    if allowed.iter().any(|allowed| *allowed == value) {
        Ok(Some(value))
    } else {
        Err(format!(
            "unsupported {}: {}. Expected one of: {}",
            key,
            value,
            allowed.join(", ")
        ))
    }
}

fn validate_format_arg(arguments: &Value) -> Result<(), String> {
    optional_enum_string_arg(arguments, "format", &["pretty", "compact"]).map(|_| ())
}

fn usize_arg(arguments: &Value, key: &str, default_value: usize) -> usize {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default_value)
}

fn clamped_usize_arg(
    arguments: &Value,
    key: &str,
    default_value: usize,
    min_value: usize,
    max_value: usize,
) -> usize {
    usize_arg(arguments, key, default_value).clamp(min_value, max_value)
}

fn f64_arg(arguments: &Value, key: &str, default_value: f64) -> f64 {
    arguments
        .get(key)
        .and_then(Value::as_f64)
        .unwrap_or(default_value)
}

fn clamped_f64_arg(
    arguments: &Value,
    key: &str,
    default_value: f64,
    min_value: f64,
    max_value: f64,
) -> f64 {
    f64_arg(arguments, key, default_value).clamp(min_value, max_value)
}

fn bool_arg(arguments: &Value, key: &str, default_value: bool) -> bool {
    arguments
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default_value)
}

#[derive(Debug, Clone, Copy)]
struct TextPayloadOptions {
    include_text: bool,
    max_text_chars: usize,
}

impl TextPayloadOptions {
    fn from_arguments(arguments: &Value, default_include_text: bool) -> Self {
        Self {
            include_text: bool_arg(arguments, "includeText", default_include_text),
            max_text_chars: clamped_usize_arg(
                arguments,
                "maxTextChars",
                DEFAULT_MAX_TEXT_CHARS,
                0,
                DEFAULT_MAX_TEXT_CHARS,
            ),
        }
    }
}

fn truncate_text(text: &str, max_chars: usize) -> (String, bool) {
    if max_chars == 0 {
        return (String::new(), !text.is_empty());
    }
    let mut chars = text.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    let was_truncated = chars.next().is_some();
    (truncated, was_truncated)
}

fn insert_optional_text(
    object: &mut Map<String, Value>,
    key: &str,
    text: &str,
    options: TextPayloadOptions,
) {
    object.insert("includeText".to_string(), json!(options.include_text));
    object.insert("maxTextChars".to_string(), json!(options.max_text_chars));
    if !options.include_text {
        object.insert(format!("{key}Omitted"), json!(true));
        return;
    }
    let (text, truncated) = truncate_text(text, options.max_text_chars);
    object.insert(key.to_string(), json!(text));
    object.insert(format!("{key}Truncated"), json!(truncated));
}

fn content_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn existing_file_bytes(vault_path: &Path, path: &str) -> Result<Option<Vec<u8>>, String> {
    let absolute_path = ensure_inside_vault(vault_path, path).map_err(|error| error.to_string())?;
    match fs::read(&absolute_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn expected_hash_arg(arguments: &Value) -> Option<String> {
    optional_string_arg(arguments, "expectedHash").filter(|value| !value.trim().is_empty())
}

fn validate_expected_hash(
    expected_hash: Option<&str>,
    previous_hash: Option<&str>,
    path: &str,
) -> Result<(), String> {
    if let Some(expected_hash) = expected_hash {
        if previous_hash != Some(expected_hash) {
            return Err(format!(
                "hash conflict for {}: expected {}, found {}",
                path,
                expected_hash,
                previous_hash.unwrap_or("null")
            ));
        }
    }
    Ok(())
}

fn normalize_score_order(left: f64, right: f64, left_path: &str, right_path: &str) -> Ordering {
    right
        .partial_cmp(&left)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left_path.cmp(right_path))
}

fn strip_md_extension(note_path: &str) -> &str {
    note_path.strip_suffix(".md").unwrap_or(note_path)
}

fn note_wiki_link(note_path: &str) -> String {
    format!("[[{}]]", strip_md_extension(note_path))
}

fn note_alias_wiki_link(note_path: &str, title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        return note_wiki_link(note_path);
    }
    format!("[[{}|{}]]", strip_md_extension(note_path), title)
}

fn merge_knowledge_note(bucket: &mut HashMap<String, KnowledgeNote>, candidate: KnowledgeNote) {
    if let Some(existing) = bucket.get_mut(&candidate.path) {
        existing.score = existing.score.max(candidate.score);
        for reason in candidate.reasons {
            if !existing.reasons.contains(&reason) {
                existing.reasons.push(reason);
            }
        }
        for link in candidate.shared_links {
            if !existing.shared_links.contains(&link) {
                existing.shared_links.push(link);
            }
        }
        existing.shared_links.truncate(10);
        return;
    }

    bucket.insert(
        candidate.path.clone(),
        KnowledgeNote {
            shared_links: candidate.shared_links.into_iter().take(10).collect(),
            ..candidate
        },
    );
}

fn knowledge_note_value(note: KnowledgeNote) -> Value {
    json!({
        "path": note.path,
        "title": note.title,
        "resourceUri": note_uri(&note.path),
        "wikiLink": note.wiki_link,
        "score": note.score,
        "reasons": note.reasons,
        "sharedLinks": note.shared_links
    })
}

fn slugify_topic(topic: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in topic.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if ch.is_whitespace() || ch == '-' {
            if !last_dash && !out.is_empty() {
                out.push('-');
                last_dash = true;
            }
        }
    }
    out.trim_matches('-').to_string()
}

fn session_note_path(topic: &str, folder: &str) -> String {
    let safe_folder = folder.trim().trim_matches('/').to_string();
    let folder = if safe_folder.is_empty() {
        "Knowledge Capture".to_string()
    } else {
        safe_folder
    };
    format!("{}/Session - {}.md", folder, slugify_topic(topic))
}

fn extract_manual_notes(content: &str) -> Option<String> {
    let marker = "\n## Manual Notes\n";
    content
        .find(marker)
        .map(|index| content[index + 1..].trim_end().to_string())
}

fn merge_with_manual_notes(
    new_content: &str,
    existing_content: &str,
    preserve_manual_notes: bool,
) -> String {
    let normalized = format!("{}\n", new_content.trim_end());
    if !preserve_manual_notes {
        return normalized;
    }
    match extract_manual_notes(existing_content) {
        Some(manual_notes) if !normalized.contains("\n## Manual Notes\n") => {
            format!("{}\n{}\n", normalized, manual_notes)
        }
        _ => normalized,
    }
}

fn finalize_session_note_content(
    content: &str,
    existing_content: Option<&str>,
    preserve_manual_notes: bool,
) -> String {
    match existing_content {
        Some(existing) => merge_with_manual_notes(content, existing, preserve_manual_notes),
        None => format!("{}\n", content.trim_end()),
    }
}

fn finalize_written_content(content: &str) -> String {
    format!("{}\n", content.trim_end())
}

fn note_title_from_content(note_path: &str, content: &str) -> String {
    note_title(
        Path::new(note_path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(note_path),
        content,
    )
}

fn yaml_scalar(value: &Value) -> Result<String, String> {
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) => serde_json::to_string(value).map_err(|error| error.to_string()),
        _ => Err("frontmatter scalar must be null, boolean, number, or string".to_string()),
    }
}

fn yaml_lines(value: &Value, indent: usize) -> Result<Vec<String>, String> {
    let pad = " ".repeat(indent);
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return Ok(vec!["[]".to_string()]);
            }
            let mut lines = Vec::new();
            for item in items {
                let item_lines = yaml_lines(item, indent + 2)?;
                if item_lines.len() == 1 {
                    lines.push(format!("{pad}- {}", item_lines[0]));
                } else {
                    lines.push(format!("{pad}-"));
                    for line in item_lines {
                        lines.push(format!("{}{}", " ".repeat(indent + 2), line));
                    }
                }
            }
            Ok(lines)
        }
        Value::Object(map) => {
            if map.is_empty() {
                return Ok(vec!["{}".to_string()]);
            }
            let mut lines = Vec::new();
            for (key, item) in map {
                let item_lines = yaml_lines(item, indent + 2)?;
                if item_lines.len() == 1 {
                    lines.push(format!("{pad}{key}: {}", item_lines[0]));
                } else {
                    lines.push(format!("{pad}{key}:"));
                    for line in item_lines {
                        lines.push(format!("{}{}", " ".repeat(indent + 2), line));
                    }
                }
            }
            Ok(lines)
        }
        _ => Ok(vec![yaml_scalar(value)?]),
    }
}

fn render_frontmatter(value: &Value) -> Result<String, String> {
    if !value.is_object() {
        return Err("frontmatter must be a JSON object".to_string());
    }
    let body = yaml_lines(value, 0)?.join("\n");
    Ok(format!("---\n{body}\n---"))
}

fn compose_explicit_note_content(arguments: &Value) -> Result<String, String> {
    let explicit_content = optional_string_arg(arguments, "content");
    let body = optional_string_arg(arguments, "body");
    let title = optional_string_arg(arguments, "title");
    let frontmatter = arguments.get("frontmatter");

    if explicit_content.is_some() && (body.is_some() || title.is_some() || frontmatter.is_some()) {
        return Err("upsert_note accepts either full content or explicit body/title/frontmatter fields, not both.".to_string());
    }

    if let Some(content) = explicit_content {
        return Ok(content);
    }

    let body = body.ok_or_else(|| "upsert_note requires either content or body.".to_string())?;
    let mut parts = Vec::new();
    if let Some(frontmatter) = frontmatter {
        parts.push(render_frontmatter(frontmatter)?);
    }
    if let Some(title) = title {
        parts.push(format!("# {}", title.trim()));
    }
    parts.push(body.trim_end().to_string());
    Ok(parts.join("\n\n"))
}

fn split_note_lines(content: &str) -> Vec<String> {
    content
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

fn is_markdown_heading_line(line: &str) -> bool {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    (1..=6).contains(&level) && line.chars().nth(level).is_some_and(|ch| ch.is_whitespace())
}

fn frontmatter_end_line(lines: &[String]) -> usize {
    if lines.first().map(|line| line.trim()) != Some("---") {
        return 0;
    }
    for (index, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            return index + 1;
        }
    }
    0
}

fn skip_blank_lines(lines: &[String], mut index: usize) -> usize {
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    index
}

fn preamble_range(lines: &[String]) -> (usize, usize) {
    let mut start = frontmatter_end_line(lines);
    start = skip_blank_lines(lines, start);
    if start < lines.len() && lines[start].starts_with("# ") {
        start += 1;
        start = skip_blank_lines(lines, start);
    }

    let mut end = start;
    while end < lines.len() {
        if is_markdown_heading_line(&lines[end]) {
            break;
        }
        end += 1;
    }
    (start, end)
}

fn trim_blank_edges(mut lines: Vec<String>) -> Vec<String> {
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

fn join_note_lines(lines: Vec<String>) -> String {
    finalize_written_content(&lines.join("\n"))
}

fn replace_range_with_block(
    original_lines: &[String],
    start: usize,
    end: usize,
    replacement_lines: Vec<String>,
) -> String {
    let mut before = trim_blank_edges(original_lines[..start].to_vec());
    let replacement_lines = trim_blank_edges(replacement_lines);
    let mut after = trim_blank_edges(original_lines[end..].to_vec());

    let mut merged = Vec::new();
    merged.append(&mut before);
    if !replacement_lines.is_empty() {
        if !merged.is_empty() {
            merged.push(String::new());
        }
        merged.extend(replacement_lines);
    }
    if !after.is_empty() {
        if !merged.is_empty() {
            merged.push(String::new());
        }
        merged.append(&mut after);
    }

    join_note_lines(merged)
}

fn replace_note_preamble(content: &str, replacement: &str) -> String {
    let lines = split_note_lines(content);
    let (start, end) = preamble_range(&lines);
    replace_range_with_block(&lines, start, end, split_note_lines(replacement))
}

fn update_or_create_note_section(
    content: &str,
    heading: &str,
    replacement: &str,
    level: usize,
    create_if_missing: bool,
) -> Result<(String, &'static str, usize), String> {
    let lines = split_note_lines(content);
    let normalized_slug = normalize_heading_slug(heading);
    if let Some(section) = extract_heading_sections(content)
        .into_iter()
        .find(|section| section.title == heading || section.slug == normalized_slug)
    {
        let section_start = section.start_line.saturating_sub(1);
        let section_end = section.end_line;
        let heading_line = lines
            .get(section_start)
            .cloned()
            .unwrap_or_else(|| format!("{} {}", "#".repeat(section.level.max(1)), heading));
        let mut replacement_lines = vec![heading_line];
        let body_lines = trim_blank_edges(split_note_lines(replacement));
        if !body_lines.is_empty() {
            replacement_lines.push(String::new());
            replacement_lines.extend(body_lines);
        }
        let updated =
            replace_range_with_block(&lines, section_start, section_end, replacement_lines);
        return Ok((updated, "updated", section.level));
    }

    if !create_if_missing {
        return Err(format!("heading not found: {}", heading));
    }

    let heading_level = level.clamp(1, 6);
    let mut merged = trim_blank_edges(lines);
    if !merged.is_empty() {
        merged.push(String::new());
    }
    merged.push(format!("{} {}", "#".repeat(heading_level), heading.trim()));
    let body_lines = trim_blank_edges(split_note_lines(replacement));
    if !body_lines.is_empty() {
        merged.push(String::new());
        merged.extend(body_lines);
    }
    Ok((join_note_lines(merged), "created", heading_level))
}

fn decode_file_content(content: &str, encoding: &str) -> Result<Vec<u8>, String> {
    match encoding {
        "utf-8" | "utf8" => Ok(content.as_bytes().to_vec()),
        "base64" => BASE64_STANDARD
            .decode(content)
            .map_err(|error| format!("invalid base64 content: {}", error)),
        other => Err(format!("unsupported encoding: {}", other)),
    }
}

fn vault_child_entry_json(entry: &VaultChildEntry) -> Value {
    json!({
        "name": entry.name,
        "path": entry.path,
        "kind": match entry.kind {
            VaultEntryKind::File => "file",
            VaultEntryKind::Directory => "directory",
        },
        "isMarkdown": entry.is_markdown,
        "sizeBytes": entry.size_bytes
    })
}

fn dense_cosine_similarity(left: &[f64], right: &[f64]) -> f64 {
    if left.is_empty() || right.is_empty() || left.len() != right.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (left_value, right_value) in left.iter().zip(right.iter()) {
        dot += left_value * right_value;
        left_norm += left_value * left_value;
        right_norm += right_value * right_value;
    }
    if left_norm <= f64::EPSILON || right_norm <= f64::EPSILON {
        0.0
    } else {
        dot / (left_norm.sqrt() * right_norm.sqrt())
    }
}

fn average_dense_vectors(vectors: &[Vec<f64>]) -> Vec<f64> {
    let Some(first) = vectors.first() else {
        return Vec::new();
    };
    let mut output = vec![0.0; first.len()];
    for vector in vectors {
        if vector.len() != output.len() {
            continue;
        }
        for (index, value) in vector.iter().enumerate() {
            output[index] += value;
        }
    }
    for value in &mut output {
        *value /= vectors.len().max(1) as f64;
    }
    output
}

fn all_word_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '\'' {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn count_prefix_lines(lines: &[String], predicate: impl Fn(&str) -> bool) -> usize {
    lines
        .iter()
        .filter(|line| predicate(line.trim_start()))
        .count()
}

fn note_style_profile(content: &str) -> NoteStyleProfile {
    let lines = split_note_lines(content);
    let line_count = lines.len().max(1) as f64;
    let non_empty_lines = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .count()
        .max(1) as f64;
    let heading_sections = extract_heading_sections(content);
    let heading_count = heading_sections.len() as f64;
    let avg_heading_level = if heading_sections.is_empty() {
        0.0
    } else {
        heading_sections
            .iter()
            .map(|section| section.level as f64)
            .sum::<f64>()
            / heading_sections.len() as f64
    };
    let bullet_lines = count_prefix_lines(&lines, |line| {
        line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ")
    }) as f64;
    let ordered_lines = count_prefix_lines(&lines, |line| {
        let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
        digits > 0 && line.chars().nth(digits) == Some('.')
    }) as f64;
    let quote_lines = count_prefix_lines(&lines, |line| line.starts_with(">")) as f64;
    let code_fence_lines = count_prefix_lines(&lines, |line| line.starts_with("```")) as f64;
    let code_blocks = (code_fence_lines / 2.0).ceil();
    let table_lines = lines
        .iter()
        .filter(|line| line.contains('|') && line.matches('|').count() >= 2)
        .count() as f64;
    let blank_lines = lines.iter().filter(|line| line.trim().is_empty()).count() as f64;
    let long_lines = lines.iter().filter(|line| line.len() >= 100).count() as f64;
    let wiki_link_count = content.matches("[[").count() as f64;
    let markdown_link_count = content.matches("](").count() as f64;
    let frontmatter = if content.starts_with("---\n") {
        1.0
    } else {
        0.0
    };
    let has_h1 = if lines.iter().any(|line| line.starts_with("# ")) {
        1.0
    } else {
        0.0
    };

    let mut paragraph_count = 0usize;
    let mut paragraph_lengths = Vec::new();
    let mut current_paragraph = 0usize;
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || is_markdown_heading_line(trimmed)
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
            || trimmed.starts_with('>')
            || trimmed.starts_with("```")
        {
            if current_paragraph > 0 {
                paragraph_count += 1;
                paragraph_lengths.push(current_paragraph as f64);
                current_paragraph = 0;
            }
            continue;
        }
        current_paragraph += 1;
    }
    if current_paragraph > 0 {
        paragraph_count += 1;
        paragraph_lengths.push(current_paragraph as f64);
    }
    let avg_paragraph_lines = if paragraph_lengths.is_empty() {
        0.0
    } else {
        paragraph_lengths.iter().sum::<f64>() / paragraph_lengths.len() as f64
    };

    let word_tokens = all_word_tokens(content);
    let word_count = word_tokens.len().max(1) as f64;
    let avg_word_length = word_tokens
        .iter()
        .map(|token| token.len() as f64)
        .sum::<f64>()
        / word_count;
    let stopwords = [
        "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "i", "in", "into",
        "is", "it", "me", "my", "of", "on", "or", "our", "that", "the", "this", "to", "us", "we",
        "with", "you", "your",
    ];
    let stopword_count = word_tokens
        .iter()
        .filter(|token| stopwords.contains(&token.as_str()))
        .count() as f64;
    let first_person_count = word_tokens
        .iter()
        .filter(|token| {
            matches!(
                token.as_str(),
                "i" | "me" | "my" | "mine" | "we" | "us" | "our" | "ours"
            )
        })
        .count() as f64;
    let second_person_count = word_tokens
        .iter()
        .filter(|token| matches!(token.as_str(), "you" | "your" | "yours"))
        .count() as f64;
    let contraction_count = word_tokens
        .iter()
        .filter(|token| token.contains('\''))
        .count() as f64;
    let punctuation_chars = content
        .chars()
        .filter(|ch| matches!(ch, ',' | ';' | ':' | '!' | '?'))
        .count() as f64;
    let sentence_count = content
        .chars()
        .filter(|ch| matches!(ch, '.' | '!' | '?'))
        .count()
        .max(1) as f64;
    let avg_sentence_words = word_count / sentence_count;
    let question_rate = content.matches('?').count() as f64 / sentence_count;
    let exclamation_rate = content.matches('!').count() as f64 / sentence_count;

    NoteStyleProfile {
        structure: vec![
            heading_count / line_count,
            avg_heading_level / 6.0,
            bullet_lines / line_count,
            ordered_lines / line_count,
            paragraph_count as f64 / line_count,
            (avg_paragraph_lines / 8.0).min(1.0),
            wiki_link_count / non_empty_lines,
            markdown_link_count / non_empty_lines,
        ],
        tone: vec![
            (avg_sentence_words / 25.0).min(1.0),
            (avg_word_length / 10.0).min(1.0),
            stopword_count / word_count,
            first_person_count / word_count,
            second_person_count / word_count,
            contraction_count / word_count,
            question_rate.min(1.0),
            exclamation_rate.min(1.0),
            (punctuation_chars / (content.len().max(1) as f64)).min(1.0),
        ],
        format: vec![
            frontmatter,
            has_h1,
            blank_lines / line_count,
            quote_lines / line_count,
            code_blocks / line_count,
            table_lines / line_count,
            long_lines / line_count,
            code_fence_lines / line_count,
            bullet_lines / line_count,
            ordered_lines / line_count,
        ],
    }
}

fn style_centroid(profiles: &[NoteStyleProfile]) -> NoteStyleProfile {
    NoteStyleProfile {
        structure: average_dense_vectors(
            &profiles
                .iter()
                .map(|profile| profile.structure.clone())
                .collect::<Vec<_>>(),
        ),
        tone: average_dense_vectors(
            &profiles
                .iter()
                .map(|profile| profile.tone.clone())
                .collect::<Vec<_>>(),
        ),
        format: average_dense_vectors(
            &profiles
                .iter()
                .map(|profile| profile.format.clone())
                .collect::<Vec<_>>(),
        ),
    }
}

fn similarity_mode(value: &str) -> SimilarityMode {
    match value {
        "structure" => SimilarityMode::Structure,
        "tone" => SimilarityMode::Tone,
        "format" => SimilarityMode::Format,
        _ => SimilarityMode::Style,
    }
}

fn find_similar_notes_payload(
    index: &SearchIndex,
    note_path: Option<&str>,
    subject: Option<&str>,
    mode: SimilarityMode,
    limit: usize,
    reference_limit: usize,
) -> Result<Value, String> {
    if note_path.is_none() && subject.is_none() {
        return Err("find_similar_notes requires either path or subject.".to_string());
    }

    let mut reference_paths = Vec::<String>::new();
    let reference_mode;
    if let Some(path) = note_path {
        if index.note(path).is_none() {
            return Err(format!("note not found in index: {}", path));
        }
        reference_mode = "path";
        reference_paths.push(path.to_string());
    } else {
        reference_mode = "subject";
        let query = subject.unwrap_or_default().trim();
        let matches = index_search::hybrid_search_with_options(
            index,
            query,
            RankingOptions {
                limit: (reference_limit.max(1) * 8).max(12),
                semantic_weight: 0.6,
                bm25_weight: 0.4,
            },
        )
        .map_err(|error| error.to_string())?;
        for item in matches {
            if !reference_paths
                .iter()
                .any(|existing| existing == &item.path)
            {
                reference_paths.push(item.path);
            }
            if reference_paths.len() >= reference_limit.max(1) {
                break;
            }
        }
        if reference_paths.is_empty() {
            return Err(
                "find_similar_notes could not derive reference notes from the subject.".to_string(),
            );
        }
    }

    let reference_profiles = reference_paths
        .iter()
        .filter_map(|path| index.note(path))
        .map(|note| note_style_profile(&note.content))
        .collect::<Vec<_>>();
    let centroid = style_centroid(&reference_profiles);
    let reference_set: HashSet<&str> = reference_paths.iter().map(|path| path.as_str()).collect();

    let mut matches = index
        .notes
        .iter()
        .filter(|note| !reference_set.contains(note.path.as_str()))
        .map(|note| {
            let profile = note_style_profile(&note.content);
            let structure_score = dense_cosine_similarity(&centroid.structure, &profile.structure);
            let tone_score = dense_cosine_similarity(&centroid.tone, &profile.tone);
            let format_score = dense_cosine_similarity(&centroid.format, &profile.format);
            let score = match mode {
                SimilarityMode::Structure => structure_score,
                SimilarityMode::Tone => tone_score,
                SimilarityMode::Format => format_score,
                SimilarityMode::Style => {
                    (0.4 * structure_score) + (0.3 * tone_score) + (0.3 * format_score)
                }
            };
            json!({
                "path": note.path,
                "title": note.title,
                "resourceUri": note_uri(&note.path),
                "wikiLink": note_alias_wiki_link(&note.path, &note.title),
                "score": score,
                "structureScore": structure_score,
                "toneScore": tone_score,
                "formatScore": format_score
            })
        })
        .filter(|item| item.get("score").and_then(Value::as_f64).unwrap_or(0.0) > 0.0)
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        normalize_score_order(
            left.get("score").and_then(Value::as_f64).unwrap_or(0.0),
            right.get("score").and_then(Value::as_f64).unwrap_or(0.0),
            left.get("path").and_then(Value::as_str).unwrap_or(""),
            right.get("path").and_then(Value::as_str).unwrap_or(""),
        )
    });
    matches.truncate(limit.max(1));

    Ok(json!({
        "by": match mode {
            SimilarityMode::Structure => "structure",
            SimilarityMode::Tone => "tone",
            SimilarityMode::Format => "format",
            SimilarityMode::Style => "style",
        },
        "referenceMode": reference_mode,
        "referencePaths": reference_paths,
        "count": matches.len(),
        "matches": matches
    }))
}

fn object_schema(properties: Vec<(&str, Value)>, required: Vec<&str>) -> Value {
    let mut schema = Map::new();
    let mut property_map = Map::new();
    for (name, value) in properties {
        property_map.insert(name.to_string(), value);
    }
    schema.insert("$schema".to_string(), json!(JSON_SCHEMA_URI));
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(property_map));
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    Value::Object(schema)
}

fn tool_annotations(read_only: bool, destructive: Option<bool>, idempotent: Option<bool>) -> Value {
    let mut annotations = Map::new();
    annotations.insert("readOnlyHint".to_string(), json!(read_only));
    if let Some(value) = destructive {
        annotations.insert("destructiveHint".to_string(), json!(value));
    }
    if let Some(value) = idempotent {
        annotations.insert("idempotentHint".to_string(), json!(value));
    }
    annotations.insert("openWorldHint".to_string(), json!(false));
    Value::Object(annotations)
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "load_knowledge".to_string(),
            description: "Load vault knowledge related to a conversation subject using hybrid retrieval, related-note expansion, and optional graph context.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("subject", json!({"type":"string","description":"Conversation subject or user problem to ground against the vault."})),
                    ("project", json!({"type":"string","description":"Optional project, repository, or domain hint."})),
                    ("limitNotes", json!({"type":"integer","exclusiveMinimum":0,"maximum":12,"default":6})),
                    ("limitChunks", json!({"type":"integer","exclusiveMinimum":0,"maximum":16,"default":8})),
                    ("includeGraph", json!({"type":"boolean","default":true})),
                    ("graphDepth", json!({"type":"integer","exclusiveMinimum":0,"maximum":3,"default":1})),
                    ("includeText", json!({"type":"boolean","default":true})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_MAX_TEXT_CHARS})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["subject"],
            ),
        },
        ToolDefinition {
            name: "recommend_folder".to_string(),
            description: "Choose the most coherent top-level vault folder for a session note using indexed related-note evidence.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("topic", json!({"type":"string","description":"Session topic."})),
                    ("project", json!({"type":"string","description":"Optional project or repository label."})),
                ],
                vec!["topic"],
            ),
        },
        ToolDefinition {
            name: "vault_info".to_string(),
            description: "Return basic metadata about the Obsidian vault and current local semantic index state.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(vec![], vec![]),
        },
        ToolDefinition {
            name: "upsert_session_note".to_string(),
            description: "Create or update a session note inside the vault using either an explicit note path or a topic-derived filename, with optional manual-notes preservation.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Optional vault-relative markdown path to update explicitly. When provided, it takes precedence over topic/folder routing."})),
                    ("topic", json!({"type":"string","description":"Session topic used to derive the session note filename when no explicit path is provided."})),
                    ("folder", json!({"type":"string","description":"Target folder inside the vault when no explicit path is provided."})),
                    ("content", json!({"type":"string","description":"Full markdown body to store in the session note."})),
                    ("preserveManualNotes", json!({"type":"boolean","default":true})),
                    ("dryRun", json!({"type":"boolean","default":false,"description":"Preview the write without changing the vault."})),
                    ("expectedHash", json!({"type":"string","description":"Optional hash of the current file content. If it does not match, no write occurs."})),
                ],
                vec!["content"],
            ),
        },
        ToolDefinition {
            name: "upsert_note".to_string(),
            description: "Create or update a markdown note with explicit control over content, title, and frontmatter. This tool does not inject implicit headings.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative markdown path to create or update."})),
                    ("content", json!({"type":"string","description":"Full markdown content to store exactly as provided."})),
                    ("body", json!({"type":"string","description":"Markdown body content used when composing the note explicitly."})),
                    ("title", json!({"type":"string","description":"Optional explicit H1 title to prepend when using body mode."})),
                    ("frontmatter", json!({"type":"object","description":"Optional explicit frontmatter object to serialize when using body mode."})),
                    ("preserveManualNotes", json!({"type":"boolean","default":false})),
                    ("dryRun", json!({"type":"boolean","default":false,"description":"Preview the write without changing the vault."})),
                    ("expectedHash", json!({"type":"string","description":"Optional hash of the current note content. If it does not match, no write occurs."})),
                ],
                vec!["path"],
            ),
        },
        ToolDefinition {
            name: "update_note_section".to_string(),
            description: "Replace the note preamble or a named heading section without rewriting the whole note.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative markdown note path."})),
                    ("target", json!({"type":"string","enum":["preamble","heading"],"default":"heading"})),
                    ("heading", json!({"type":"string","description":"Exact heading title when target is heading."})),
                    ("content", json!({"type":"string","description":"Replacement body content for the targeted section."})),
                    ("level", json!({"type":"integer","minimum":1,"maximum":6,"default":2})),
                    ("createIfMissing", json!({"type":"boolean","default":true})),
                    ("dryRun", json!({"type":"boolean","default":false,"description":"Preview the write without changing the vault."})),
                    ("expectedHash", json!({"type":"string","description":"Optional hash of the current note content. If it does not match, no write occurs."})),
                ],
                vec!["path","content"],
            ),
        },
        ToolDefinition {
            name: "write_file_to_vault".to_string(),
            description: "Create or update a non-note file inside the vault using UTF-8 text or base64-encoded bytes.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative file path to create or update."})),
                    ("content", json!({"type":"string","description":"File content as UTF-8 text or base64."})),
                    ("encoding", json!({"type":"string","enum":["utf-8","base64"],"default":"utf-8"})),
                    ("dryRun", json!({"type":"boolean","default":false,"description":"Preview the write without changing the vault."})),
                    ("expectedHash", json!({"type":"string","description":"Optional hash of the current file content. If it does not match, no write occurs."})),
                ],
                vec!["path","content"],
            ),
        },
        ToolDefinition {
            name: "list_children".to_string(),
            description: "List the direct children of a vault directory, including non-markdown files and subfolders.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Optional vault-relative directory path. Defaults to the vault root."})),
                    ("includeHidden", json!({"type":"boolean","default":false})),
                    ("includeIgnored", json!({"type":"boolean","default":false})),
                ],
                vec![],
            ),
        },
        ToolDefinition {
            name: "list_folders".to_string(),
            description: "List folders in the vault or under a subdirectory, optionally recursively.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Optional vault-relative directory path. Defaults to the vault root."})),
                    ("recursive", json!({"type":"boolean","default":false})),
                    ("depth", json!({"type":"integer","minimum":1,"maximum":12,"default":3})),
                    ("includeHidden", json!({"type":"boolean","default":false})),
                    ("includeIgnored", json!({"type":"boolean","default":false})),
                ],
                vec![],
            ),
        },
        ToolDefinition {
            name: "find_similar_notes".to_string(),
            description: "Find notes with a similar editorial style, structure, tone, or format to an existing note or subject-derived reference set.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Optional vault-relative note path used as the style reference."})),
                    ("subject", json!({"type":"string","description":"Optional subject query used to derive reference notes before ranking style similarity."})),
                    ("by", json!({"type":"string","enum":["style","structure","tone","format"],"default":"style"})),
                    ("limit", json!({"type":"integer","minimum":1,"maximum":50,"default":8})),
                    ("referenceLimit", json!({"type":"integer","minimum":1,"maximum":8,"default":3})),
                ],
                vec![],
            ),
        },
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read an entire note or a specific line range from the vault.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative markdown path."})),
                    ("startLine", json!({"type":"integer","exclusiveMinimum":0,"maximum":MAX_SAFE_INTEGER})),
                    ("endLine", json!({"type":"integer","exclusiveMinimum":0,"maximum":MAX_SAFE_INTEGER})),
                    ("includeText", json!({"type":"boolean","default":true})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_MAX_TEXT_CHARS})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["path"],
            ),
        },
        ToolDefinition {
            name: "read_artifact".to_string(),
            description: "Inspect metadata for a supported non-markdown vault artifact, with optional bounded base64 payload.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative artifact path."})),
                    ("includeBase64", json!({"type":"boolean","default":false})),
                    ("maxBytes", json!({"type":"integer","minimum":0,"maximum":1048576,"default":0})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["path"],
            ),
        },
        ToolDefinition {
            name: "read_chunk".to_string(),
            description: "Read a deterministic line-based chunk from a file.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative markdown path."})),
                    ("chunkIndex", json!({"type":"integer","minimum":0,"maximum":MAX_SAFE_INTEGER,"default":0})),
                    ("chunkSizeLines", json!({"type":"integer","exclusiveMinimum":0,"maximum":MAX_SAFE_INTEGER,"default":120})),
                    ("overlapLines", json!({"type":"integer","minimum":0,"maximum":MAX_SAFE_INTEGER,"default":20})),
                    ("includeText", json!({"type":"boolean","default":true})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_MAX_TEXT_CHARS})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["path"],
            ),
        },
        ToolDefinition {
            name: "find_files".to_string(),
            description: "Find markdown files by classic substring or regex path search.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("query", json!({"type":"string","description":"Substring or regex to match against vault-relative file paths."})),
                    ("mode", json!({"type":"string","enum":["substring","regex"],"default":"substring"})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":200,"default":20})),
                ],
                vec!["query"],
            ),
        },
        ToolDefinition {
            name: "grep_search".to_string(),
            description: "Search note contents using ripgrep. Supports fixed string or regex mode.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("query", json!({"type":"string","description":"Search pattern."})),
                    ("regex", json!({"type":"boolean","default":false})),
                    ("caseSensitive", json!({"type":"boolean","default":false})),
                    ("glob", json!({"type":"string","description":"Optional rg glob, for example 'Agent Studio/*.md'."})),
                    ("contextLines", json!({"type":"integer","minimum":0,"maximum":20,"default":0})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":500,"default":50})),
                    ("includeText", json!({"type":"boolean","default":true})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_MAX_TEXT_CHARS})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["query"],
            ),
        },
        ToolDefinition {
            name: "note_outline".to_string(),
            description: "Return headings, block ids, line ranges, resource URIs, and outgoing wiki links for a markdown note.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative markdown path."})),
                    ("includeText", json!({"type":"boolean","default":false,"description":"Include heading and block text excerpts."})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":4000})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["path"],
            ),
        },
        ToolDefinition {
            name: "build_index".to_string(),
            description: "Force a rebuild of the local chunk index used for semantic and related-note search.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(vec![], vec![]),
        },
        ToolDefinition {
            name: "bm25_search".to_string(),
            description: "Search note chunks with classic BM25 lexical ranking.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("query", json!({"type":"string","description":"Lexical query."})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":50,"default":8})),
                    ("includeText", json!({"type":"boolean","default":true})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_MAX_TEXT_CHARS})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["query"],
            ),
        },
        ToolDefinition {
            name: "semantic_search".to_string(),
            description: "Search semantically similar note chunks using the local vectorized chunk index.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("query", json!({"type":"string","description":"Natural-language search query."})),
                    ("scope", json!({"type":"string","enum":["chunks","artifacts","all"],"default":"chunks"})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":50,"default":8})),
                    ("includeText", json!({"type":"boolean","default":true})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_MAX_TEXT_CHARS})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["query"],
            ),
        },
        ToolDefinition {
            name: "hybrid_search".to_string(),
            description: "Combine BM25 lexical ranking with semantic similarity over note chunks.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("query", json!({"type":"string","description":"Natural-language or lexical query."})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":50,"default":8})),
                    ("semanticWeight", json!({"type":"number","minimum":0,"maximum":1,"default":0.6})),
                    ("bm25Weight", json!({"type":"number","minimum":0,"maximum":1,"default":0.4})),
                    ("includeText", json!({"type":"boolean","default":true})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_MAX_TEXT_CHARS})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["query"],
            ),
        },
        ToolDefinition {
            name: "related_notes".to_string(),
            description: "Return notes with similar subjects to a given note path using the local note index.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative note path."})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":50,"default":8})),
                ],
                vec!["path"],
            ),
        },
        ToolDefinition {
            name: "backlinks".to_string(),
            description: "List notes in the vault that link to the given note.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative note path."})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":200,"default":50})),
                ],
                vec!["path"],
            ),
        },
        ToolDefinition {
            name: "graph_traverse".to_string(),
            description: "Traverse the Obsidian wiki-link graph around a note, including backlinks.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative starting note path."})),
                    ("direction", json!({"type":"string","enum":["incoming","outgoing","both"],"default":"both"})),
                    ("depth", json!({"type":"integer","exclusiveMinimum":0,"maximum":6,"default":1})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":500,"default":100})),
                ],
                vec!["path"],
            ),
        },
    ]
}

pub fn list_tools() -> Vec<ToolDefinition> {
    tool_definitions()
}

fn search_match_json(match_item: &index_search::SearchMatch, options: TextPayloadOptions) -> Value {
    let mut object = Map::from_iter([
        ("path".to_string(), json!(match_item.path.clone())),
        ("title".to_string(), json!(match_item.title.clone())),
        ("resourceUri".to_string(), json!(note_uri(&match_item.path))),
        ("chunkIndex".to_string(), json!(match_item.chunk_index)),
        ("startLine".to_string(), json!(match_item.start_line)),
        ("endLine".to_string(), json!(match_item.end_line)),
        ("score".to_string(), json!(match_item.score)),
    ]);
    insert_optional_text(&mut object, "text", &match_item.text, options);
    Value::Object(object)
}

fn hybrid_search_match_json(
    match_item: &index_search::SearchMatch,
    options: TextPayloadOptions,
) -> Value {
    let mut object = Map::from_iter([
        ("path".to_string(), json!(match_item.path.clone())),
        ("title".to_string(), json!(match_item.title.clone())),
        ("resourceUri".to_string(), json!(note_uri(&match_item.path))),
        ("chunkIndex".to_string(), json!(match_item.chunk_index)),
        ("startLine".to_string(), json!(match_item.start_line)),
        ("endLine".to_string(), json!(match_item.end_line)),
        (
            "semanticScore".to_string(),
            json!(match_item.semantic_score),
        ),
        ("bm25Score".to_string(), json!(match_item.bm25_score)),
        ("score".to_string(), json!(match_item.score)),
    ]);
    insert_optional_text(&mut object, "text", &match_item.text, options);
    Value::Object(object)
}

fn artifact_search_match_json(match_item: &index_search::ArtifactSearchMatch) -> Value {
    let metadata =
        serde_json::from_str::<Value>(&match_item.metadata_json).unwrap_or_else(|_| json!({}));
    Value::Object(Map::from_iter([
        ("path".to_string(), json!(match_item.path.clone())),
        ("title".to_string(), json!(match_item.title.clone())),
        (
            "resourceUri".to_string(),
            json!(artifact_uri(&match_item.path)),
        ),
        ("kind".to_string(), json!(match_item.kind.clone())),
        ("mimeType".to_string(), json!(match_item.mime_type.clone())),
        ("size".to_string(), json!(match_item.size)),
        ("score".to_string(), json!(match_item.score)),
        ("metadata".to_string(), metadata),
    ]))
}

fn file_path_match_json(match_item: &index_search::FilePathMatch) -> Value {
    let mut object = Map::from_iter([
        ("path".to_string(), json!(match_item.path.clone())),
        (
            "matchedOn".to_string(),
            json!(match_item.matched_on.clone()),
        ),
    ]);
    if match_item.path.to_lowercase().ends_with(".md") {
        object.insert("resourceUri".to_string(), json!(note_uri(&match_item.path)));
    }
    Value::Object(object)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepContextLine {
    line_number: usize,
    line_text: String,
}

#[derive(Debug, Clone, PartialEq)]
struct LiveGrepMatch {
    path: String,
    line_number: usize,
    submatches: Vec<index_search::GrepSubmatch>,
    line_text: String,
    context_before: Vec<GrepContextLine>,
    context_after: Vec<GrepContextLine>,
}

fn grep_context_line_json(line: &GrepContextLine) -> Value {
    json!({
        "lineNumber": line.line_number,
        "lineText": line.line_text
    })
}

fn grep_match_json(match_item: &LiveGrepMatch, options: TextPayloadOptions) -> Value {
    let mut object = Map::from_iter([
        ("path".to_string(), json!(match_item.path.clone())),
        ("resourceUri".to_string(), json!(note_uri(&match_item.path))),
        ("lineNumber".to_string(), json!(match_item.line_number)),
        (
            "submatches".to_string(),
            json!(match_item
                .submatches
                .iter()
                .map(|submatch| json!({
                    "start": submatch.start,
                    "end": submatch.end,
                    "text": submatch.text.clone()
                }))
                .collect::<Vec<_>>()),
        ),
        (
            "contextBefore".to_string(),
            json!(match_item
                .context_before
                .iter()
                .map(grep_context_line_json)
                .collect::<Vec<_>>()),
        ),
        (
            "contextAfter".to_string(),
            json!(match_item
                .context_after
                .iter()
                .map(grep_context_line_json)
                .collect::<Vec<_>>()),
        ),
    ]);
    insert_optional_text(&mut object, "lineText", &match_item.line_text, options);
    Value::Object(object)
}

fn note_result_json(
    path: String,
    title: String,
    extra: impl FnOnce(&mut Map<String, Value>),
) -> Value {
    let mut object = Map::from_iter([
        ("path".to_string(), json!(path.clone())),
        ("title".to_string(), json!(title)),
        ("resourceUri".to_string(), json!(note_uri(&path))),
    ]);
    extra(&mut object);
    Value::Object(object)
}

fn outline_payload(path: &str, content: &str, options: TextPayloadOptions) -> Value {
    let headings = extract_heading_sections(content)
        .into_iter()
        .map(|heading| {
            let mut object = Map::from_iter([
                ("level".to_string(), json!(heading.level)),
                ("title".to_string(), json!(heading.title)),
                ("slug".to_string(), json!(heading.slug.clone())),
                ("startLine".to_string(), json!(heading.start_line)),
                ("endLine".to_string(), json!(heading.end_line)),
                (
                    "resourceUri".to_string(),
                    json!(heading_uri(path, &heading.slug)),
                ),
            ]);
            insert_optional_text(&mut object, "text", &heading.text, options);
            Value::Object(object)
        })
        .collect::<Vec<_>>();
    let blocks = extract_block_sections(content)
        .into_iter()
        .map(|block| {
            let mut object = Map::from_iter([
                ("id".to_string(), json!(block.id.clone())),
                ("startLine".to_string(), json!(block.start_line)),
                ("endLine".to_string(), json!(block.end_line)),
                ("resourceUri".to_string(), json!(block_uri(path, &block.id))),
            ]);
            insert_optional_text(&mut object, "text", &block.text, options);
            Value::Object(object)
        })
        .collect::<Vec<_>>();
    let links = extract_wiki_links(content)
        .into_iter()
        .map(|target| json!({"target": target}))
        .collect::<Vec<_>>();
    json!({
        "path": path,
        "title": note_title_from_content(path, content),
        "resourceUri": note_uri(path),
        "lineCount": split_note_lines(content).len(),
        "headingCount": headings.len(),
        "blockCount": blocks.len(),
        "linkCount": links.len(),
        "headings": headings,
        "blocks": blocks,
        "outgoingLinks": links
    })
}

fn relative_vault_path(vault_path: &Path, absolute_path: &str) -> String {
    let path = Path::new(absolute_path);
    match path.strip_prefix(vault_path) {
        Ok(relative) => relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => absolute_path.to_string(),
    }
}

fn live_find_file_matches(
    vault_path: &Path,
    query: &str,
    mode: &str,
    limit: usize,
) -> Result<Vec<index_search::FilePathMatch>, String> {
    let files = list_markdown_files(vault_path).map_err(|error| error.to_string())?;
    let limit = limit.max(1);
    if mode == "regex" {
        let matcher = RegexBuilder::new(query)
            .case_insensitive(true)
            .build()
            .map_err(|error| error.to_string())?;
        return Ok(files
            .into_iter()
            .filter(|file_path| matcher.is_match(file_path))
            .take(limit)
            .map(|file_path| index_search::FilePathMatch {
                path: file_path,
                matched_on: "regex".to_string(),
            })
            .collect());
    }

    let lowered = query.to_lowercase();
    Ok(files
        .into_iter()
        .filter(|file_path| file_path.to_lowercase().contains(&lowered))
        .take(limit)
        .map(|file_path| index_search::FilePathMatch {
            path: file_path,
            matched_on: "substring".to_string(),
        })
        .collect())
}

async fn live_grep_matches(
    vault_path: std::path::PathBuf,
    query: String,
    regex_mode: bool,
    case_sensitive: bool,
    glob: Option<String>,
    context_lines: usize,
    limit: usize,
) -> Result<Vec<LiveGrepMatch>, String> {
    tokio::task::spawn_blocking(move || {
        let mut args = vec![
            "--json".to_string(),
            "--line-number".to_string(),
            "--with-filename".to_string(),
            "--hidden".to_string(),
            "--glob".to_string(),
            "!.obsidian/**".to_string(),
            "--glob".to_string(),
            "!.git/**".to_string(),
            "--glob".to_string(),
            "!.deep-obsidian-mcp/**".to_string(),
        ];
        if !regex_mode {
            args.push("--fixed-strings".to_string());
        }
        if !case_sensitive {
            args.push("--ignore-case".to_string());
        }
        if let Some(glob) = glob.as_ref() {
            args.push("--glob".to_string());
            args.push(glob.clone());
        } else {
            args.push("--glob".to_string());
            args.push("*.md".to_string());
        }
        args.push(query);
        args.push(vault_path.to_string_lossy().into_owned());

        let output = ProcessCommand::new("rg")
            .args(&args)
            .output()
            .map_err(|error| error.to_string())?;

        if !output.status.success() && output.status.code() != Some(1) {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if stderr.is_empty() {
                format!("rg failed with status {}", output.status)
            } else {
                stderr
            });
        }

        let stdout = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
        let mut matches = Vec::new();
        for line in stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
            if parsed.get("type").and_then(Value::as_str) != Some("match") {
                continue;
            }
            let data = parsed
                .get("data")
                .ok_or_else(|| "rg match payload missing data".to_string())?;
            let absolute_path = data
                .get("path")
                .and_then(|value| value.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| "rg match payload missing path".to_string())?;
            let line_number = data
                .get("line_number")
                .and_then(Value::as_u64)
                .ok_or_else(|| "rg match payload missing line number".to_string())?
                as usize;
            let line_text = data
                .get("lines")
                .and_then(|value| value.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| "rg match payload missing line text".to_string())?
                .trim_end_matches('\n')
                .to_string();
            let submatches = data
                .get("submatches")
                .and_then(Value::as_array)
                .ok_or_else(|| "rg match payload missing submatches".to_string())?
                .iter()
                .map(|submatch| {
                    Ok(index_search::GrepSubmatch {
                        start: submatch
                            .get("start")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| "rg submatch missing start".to_string())?
                            as usize,
                        end: submatch
                            .get("end")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| "rg submatch missing end".to_string())?
                            as usize,
                        text: submatch
                            .get("match")
                            .and_then(|value| value.get("text"))
                            .and_then(Value::as_str)
                            .ok_or_else(|| "rg submatch missing text".to_string())?
                            .to_string(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;

            matches.push(LiveGrepMatch {
                path: relative_vault_path(&vault_path, absolute_path),
                line_number,
                submatches,
                line_text,
                context_before: Vec::new(),
                context_after: Vec::new(),
            });
            if matches.len() >= limit.max(1) {
                break;
            }
        }

        if context_lines > 0 {
            populate_grep_context(&vault_path, &mut matches, context_lines)?;
        }

        Ok(matches)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn populate_grep_context(
    vault_path: &Path,
    matches: &mut [LiveGrepMatch],
    context_lines: usize,
) -> Result<(), String> {
    let mut cache = HashMap::<String, Vec<String>>::new();
    for match_item in matches {
        let lines = if let Some(lines) = cache.get(&match_item.path) {
            lines
        } else {
            let absolute_path = ensure_inside_vault(vault_path, &match_item.path)
                .map_err(|error| error.to_string())?;
            let text = fs::read_to_string(&absolute_path).map_err(|error| error.to_string())?;
            cache.insert(match_item.path.clone(), split_note_lines(&text));
            cache.get(&match_item.path).expect("cached grep context")
        };
        let line_index = match_item.line_number.saturating_sub(1);
        let before_start = line_index.saturating_sub(context_lines);
        match_item.context_before = lines[before_start..line_index.min(lines.len())]
            .iter()
            .enumerate()
            .map(|(offset, line)| GrepContextLine {
                line_number: before_start + offset + 1,
                line_text: line.clone(),
            })
            .collect();
        let after_start = (line_index + 1).min(lines.len());
        let after_end = (after_start + context_lines).min(lines.len());
        match_item.context_after = lines[after_start..after_end]
            .iter()
            .enumerate()
            .map(|(offset, line)| GrepContextLine {
                line_number: after_start + offset + 1,
                line_text: line.clone(),
            })
            .collect();
    }
    Ok(())
}

async fn semantic_search_matches(
    index: std::sync::Arc<deep_obsidian_index::index::SearchIndex>,
    query: String,
    options: RankingOptions,
) -> Result<Vec<index_search::SearchMatch>, String> {
    tokio::task::spawn_blocking(move || {
        index_search::semantic_search_with_options(index.as_ref(), &query, options)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn artifact_semantic_search_matches(
    index: std::sync::Arc<deep_obsidian_index::index::SearchIndex>,
    query: String,
    options: RankingOptions,
) -> Result<Vec<index_search::ArtifactSearchMatch>, String> {
    tokio::task::spawn_blocking(move || {
        index_search::artifact_semantic_search_with_options(index.as_ref(), &query, options)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn hybrid_search_matches(
    index: std::sync::Arc<deep_obsidian_index::index::SearchIndex>,
    query: String,
    options: RankingOptions,
) -> Result<Vec<index_search::SearchMatch>, String> {
    tokio::task::spawn_blocking(move || {
        index_search::hybrid_search_with_options(index.as_ref(), &query, options)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

pub async fn call_tool(
    state: &AppState,
    name: &str,
    arguments: &Value,
) -> Result<ToolCallResult, String> {
    let config = state.config.as_ref();
    match name {
        "vault_info" => {
            let snapshot = state.runtime.fresh_snapshot("vault_info").await?;
            Ok(json_text_result(build_vault_overview_payload(
                config, &snapshot,
            )))
        }
        "list_children" => {
            let path = optional_string_arg(arguments, "path");
            let include_hidden = bool_arg(arguments, "includeHidden", false);
            let include_ignored = bool_arg(arguments, "includeIgnored", false);
            let entries = vault_list_children(
                &config.vault_path,
                path.as_deref(),
                include_hidden,
                include_ignored,
            )
            .map_err(|error| error.to_string())?;
            Ok(json_text_result(json!({
                "path": path,
                "count": entries.len(),
                "children": entries.into_iter().map(|entry| vault_child_entry_json(&entry)).collect::<Vec<_>>()
            })))
        }
        "list_folders" => {
            let path = optional_string_arg(arguments, "path");
            let recursive = bool_arg(arguments, "recursive", false);
            let depth = clamped_usize_arg(arguments, "depth", 3, 1, 12);
            let include_hidden = bool_arg(arguments, "includeHidden", false);
            let include_ignored = bool_arg(arguments, "includeIgnored", false);
            let folders = vault_list_folders(
                &config.vault_path,
                path.as_deref(),
                recursive,
                depth,
                include_hidden,
                include_ignored,
            )
            .map_err(|error| error.to_string())?;
            Ok(json_text_result(json!({
                "path": path,
                "recursive": recursive,
                "depth": depth,
                "count": folders.len(),
                "folders": folders
            })))
        }
        "read_file" => {
            let path = string_arg(arguments, "path")?;
            validate_format_arg(arguments)?;
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let file =
                read_text_file(&config.vault_path, &path).map_err(|error| error.to_string())?;
            let start_line = arguments
                .get("startLine")
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            let end_line = arguments
                .get("endLine")
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            let text = if start_line.is_some() || end_line.is_some() {
                deep_obsidian_core::vault::slice_lines(
                    &file.text,
                    start_line.unwrap_or(1),
                    end_line.or(start_line).unwrap_or(1),
                )
            } else {
                file.text
            };
            let line_count = text.split('\n').count();
            let mut result = Map::from_iter([
                ("path".to_string(), json!(path.clone())),
                ("resourceUri".to_string(), json!(note_uri(&path))),
                ("startLine".to_string(), json!(start_line.unwrap_or(1))),
                ("endLine".to_string(), json!(end_line.unwrap_or(line_count))),
                ("lineCount".to_string(), json!(line_count)),
            ]);
            insert_optional_text(&mut result, "text", &text, text_options);
            Ok(json_text_result_from_arguments(
                arguments,
                Value::Object(result),
            ))
        }
        "read_artifact" => {
            let path = string_arg(arguments, "path")?;
            validate_format_arg(arguments)?;
            let mime_type = artifact_mime_type(&path)
                .ok_or_else(|| format!("unsupported artifact type for {}", path))?;
            let kind = artifact_kind(&path).unwrap_or("artifact");
            let absolute_path = ensure_inside_vault(&config.vault_path, &path)
                .map_err(|error| error.to_string())?;
            let metadata = fs::metadata(&absolute_path).map_err(|error| error.to_string())?;
            let include_base64 = bool_arg(arguments, "includeBase64", false);
            let max_bytes = clamped_usize_arg(arguments, "maxBytes", 0, 0, 1_048_576);
            let bytes = if include_base64 || max_bytes > 0 {
                fs::read(&absolute_path).map_err(|error| error.to_string())?
            } else {
                Vec::new()
            };
            let mut result = Map::from_iter([
                ("path".to_string(), json!(path.clone())),
                ("resourceUri".to_string(), json!(artifact_uri(&path))),
                ("kind".to_string(), json!(kind)),
                ("mimeType".to_string(), json!(mime_type)),
                ("size".to_string(), json!(metadata.len())),
                ("includeBase64".to_string(), json!(include_base64)),
                ("maxBytes".to_string(), json!(max_bytes)),
            ]);
            if !bytes.is_empty() {
                result.insert("hash".to_string(), json!(content_hash(&bytes)));
            }
            if include_base64 {
                if bytes.len() > max_bytes {
                    return Err(format!(
                        "artifact payload for {} is {} bytes, above maxBytes {}",
                        path,
                        bytes.len(),
                        max_bytes
                    ));
                }
                result.insert("base64".to_string(), json!(BASE64_STANDARD.encode(&bytes)));
            }
            Ok(json_text_result_from_arguments(
                arguments,
                Value::Object(result),
            ))
        }
        "read_chunk" => {
            let path = string_arg(arguments, "path")?;
            validate_format_arg(arguments)?;
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let chunk_index =
                clamped_usize_arg(arguments, "chunkIndex", 0, 0, MAX_SAFE_INTEGER as usize);
            let chunk_size_lines = clamped_usize_arg(arguments, "chunkSizeLines", 120, 1, 10_000);
            let overlap_lines = clamped_usize_arg(
                arguments,
                "overlapLines",
                20,
                0,
                chunk_size_lines.saturating_sub(1),
            );
            let file =
                read_text_file(&config.vault_path, &path).map_err(|error| error.to_string())?;
            let chunks = chunk_lines(&file.text, chunk_size_lines, overlap_lines);
            let chunk = chunks.get(chunk_index).ok_or_else(|| {
                format!(
                    "Chunk {} does not exist for {}. Available chunks: {}",
                    chunk_index,
                    path,
                    chunks.len()
                )
            })?;
            let mut result = Map::from_iter([
                ("path".to_string(), json!(path.clone())),
                ("resourceUri".to_string(), json!(note_uri(&path))),
                ("chunkIndex".to_string(), json!(chunk_index)),
                ("chunkCount".to_string(), json!(chunks.len())),
                ("chunkSizeLines".to_string(), json!(chunk_size_lines)),
                ("overlapLines".to_string(), json!(overlap_lines)),
                ("startLine".to_string(), json!(chunk.start_line)),
                ("endLine".to_string(), json!(chunk.end_line)),
            ]);
            insert_optional_text(&mut result, "text", &chunk.text, text_options);
            Ok(json_text_result_from_arguments(
                arguments,
                Value::Object(result),
            ))
        }
        "find_files" => {
            let query = string_arg(arguments, "query")?;
            let mode = optional_enum_string_arg(arguments, "mode", &["substring", "regex"])?
                .unwrap_or_else(|| "substring".to_string());
            let limit = clamped_usize_arg(arguments, "limit", 20, 1, 200);
            let matches = live_find_file_matches(&config.vault_path, &query, &mode, limit)?
                .into_iter()
                .map(|item| file_path_match_json(&item))
                .collect::<Vec<_>>();
            Ok(json_text_result(json!({
                "query": query,
                "mode": mode,
                "count": matches.len(),
                "matches": matches
            })))
        }
        "grep_search" => {
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let regex_mode = bool_arg(arguments, "regex", false);
            let case_sensitive = bool_arg(arguments, "caseSensitive", false);
            let glob = optional_string_arg(arguments, "glob");
            let context_lines = clamped_usize_arg(arguments, "contextLines", 0, 0, 20);
            let limit = clamped_usize_arg(arguments, "limit", 50, 1, 500);
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let matches = live_grep_matches(
                config.vault_path.clone(),
                query.clone(),
                regex_mode,
                case_sensitive,
                glob.clone(),
                context_lines,
                limit,
            )
            .await?
            .into_iter()
            .map(|item| grep_match_json(&item, text_options))
            .collect::<Vec<_>>();
            Ok(json_text_result_from_arguments(
                arguments,
                json!({
                    "query": query,
                    "regex": regex_mode,
                    "caseSensitive": case_sensitive,
                    "glob": glob,
                    "contextLines": context_lines,
                    "count": matches.len(),
                    "matches": matches
                }),
            ))
        }
        "note_outline" => {
            let path = string_arg(arguments, "path")?;
            validate_format_arg(arguments)?;
            let mut text_options = TextPayloadOptions::from_arguments(arguments, false);
            if arguments.get("maxTextChars").is_none() {
                text_options.max_text_chars = 4_000;
            }
            let file =
                read_text_file(&config.vault_path, &path).map_err(|error| error.to_string())?;
            Ok(json_text_result_from_arguments(
                arguments,
                outline_payload(&path, &file.text, text_options),
            ))
        }
        "build_index" => {
            let snapshot = state.runtime.rebuild("manual build_index").await?;
            let mut result = Map::new();
            result.insert("rebuilt".to_string(), json!(true));
            result.insert(
                "generatedAt".to_string(),
                json!(snapshot.index.generated_at),
            );
            result.insert("noteCount".to_string(), json!(snapshot.index.note_count));
            result.insert("chunkCount".to_string(), json!(snapshot.index.chunk_count));
            result.insert(
                "semanticBackend".to_string(),
                json!(snapshot.index.semantic_backend.as_str()),
            );
            if let Some(provider) = &snapshot.index.embedding_provider {
                result.insert("embeddingProvider".to_string(), json!(provider));
            }
            if let Some(model) = &snapshot.index.embedding_model {
                result.insert("embeddingModel".to_string(), json!(model));
            }
            if let Some(dimensions) = snapshot.index.embedding_dimensions {
                result.insert("embeddingDimensions".to_string(), json!(dimensions));
            }
            Ok(json_text_result(Value::Object(result)))
        }
        "bm25_search" => {
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let snapshot = state.runtime.fresh_snapshot("bm25_search").await?;
            let index = snapshot.index;
            let matches = index_search::bm25_search_with_options(
                &index,
                &query,
                RankingOptions {
                    limit,
                    semantic_weight: 0.6,
                    bm25_weight: 0.4,
                },
            )
            .map_err(|error| error.to_string())?;
            Ok(json_text_result_from_arguments(
                arguments,
                json!({
                    "query": query,
                    "rebuilt": snapshot.rebuilt,
                    "count": matches.len(),
                    "matches": matches.into_iter().map(|item| search_match_json(&item, text_options)).collect::<Vec<_>>()
                }),
            ))
        }
        "semantic_search" => {
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let scope =
                optional_enum_string_arg(arguments, "scope", &["chunks", "artifacts", "all"])?
                    .unwrap_or_else(|| "chunks".to_string());
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let snapshot = state.runtime.fresh_snapshot("semantic_search").await?;
            let index = snapshot.index;
            let options = RankingOptions {
                limit,
                semantic_weight: 1.0,
                bm25_weight: 0.0,
            };
            match scope.as_str() {
                "chunks" => {
                    let matches =
                        semantic_search_matches(index.clone(), query.clone(), options).await?;
                    Ok(json_text_result_from_arguments(
                        arguments,
                        json!({
                            "query": query,
                            "scope": scope,
                            "rebuilt": snapshot.rebuilt,
                            "semanticBackend": index.semantic_backend.as_str(),
                            "count": matches.len(),
                            "matches": matches.into_iter().map(|item| search_match_json(&item, text_options)).collect::<Vec<_>>()
                        }),
                    ))
                }
                "artifacts" => {
                    let matches =
                        artifact_semantic_search_matches(index.clone(), query.clone(), options)
                            .await?;
                    Ok(json_text_result_from_arguments(
                        arguments,
                        json!({
                            "query": query,
                            "scope": scope,
                            "rebuilt": snapshot.rebuilt,
                            "semanticBackend": index.semantic_backend.as_str(),
                            "artifactEmbeddingProvider": index.artifact_embedding_provider.clone(),
                            "artifactEmbeddingModel": index.artifact_embedding_model.clone(),
                            "count": matches.len(),
                            "matches": matches.into_iter().map(|item| artifact_search_match_json(&item)).collect::<Vec<_>>()
                        }),
                    ))
                }
                "all" => {
                    let chunk_matches =
                        semantic_search_matches(index.clone(), query.clone(), options.clone())
                            .await?;
                    let artifact_result =
                        artifact_semantic_search_matches(index.clone(), query.clone(), options)
                            .await;
                    let (artifact_matches, artifact_error) = match artifact_result {
                        Ok(matches) => (matches, None),
                        Err(error) => (Vec::new(), Some(error)),
                    };
                    Ok(json_text_result_from_arguments(
                        arguments,
                        json!({
                            "query": query,
                            "scope": scope,
                            "rebuilt": snapshot.rebuilt,
                            "semanticBackend": index.semantic_backend.as_str(),
                            "chunks": {
                                "count": chunk_matches.len(),
                                "matches": chunk_matches.into_iter().map(|item| search_match_json(&item, text_options)).collect::<Vec<_>>()
                            },
                            "artifacts": {
                                "count": artifact_matches.len(),
                                "error": artifact_error,
                                "matches": artifact_matches.into_iter().map(|item| artifact_search_match_json(&item)).collect::<Vec<_>>()
                            }
                        }),
                    ))
                }
                _ => unreachable!(),
            }
        }
        "hybrid_search" => {
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            let semantic_weight = clamped_f64_arg(arguments, "semanticWeight", 0.6, 0.0, 1.0);
            let bm25_weight = clamped_f64_arg(arguments, "bm25Weight", 0.4, 0.0, 1.0);
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let snapshot = state.runtime.fresh_snapshot("hybrid_search").await?;
            let index = snapshot.index;
            let matches = hybrid_search_matches(
                index.clone(),
                query.clone(),
                RankingOptions {
                    limit,
                    semantic_weight,
                    bm25_weight,
                },
            )
            .await?;
            Ok(json_text_result_from_arguments(
                arguments,
                json!({
                    "query": query,
                    "rebuilt": snapshot.rebuilt,
                    "semanticBackend": index.semantic_backend.as_str(),
                    "semanticWeight": semantic_weight,
                    "bm25Weight": bm25_weight,
                    "count": matches.len(),
                    "matches": matches.into_iter().map(|item| hybrid_search_match_json(&item, text_options)).collect::<Vec<_>>()
                }),
            ))
        }
        "related_notes" => {
            let path = string_arg(arguments, "path")?;
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            let snapshot = state.runtime.fresh_snapshot("related_notes").await?;
            let index = snapshot.index;
            let matches = index_search::related_notes_with_options(
                &index,
                &path,
                RelatedNoteOptions { limit },
            )
            .map_err(|error| error.to_string())?;
            Ok(json_text_result(json!({
                "path": path,
                "rebuilt": snapshot.rebuilt,
                "semanticBackend": index.semantic_backend.as_str(),
                "count": matches.len(),
                "matches": matches.into_iter().map(|item| note_result_json(item.path, item.title, |object| {
                    object.insert("score".to_string(), json!(item.score));
                    object.insert("sharedLinks".to_string(), json!(item.shared_links));
                })).collect::<Vec<_>>()
            })))
        }
        "backlinks" => {
            let path = string_arg(arguments, "path")?;
            let limit = clamped_usize_arg(arguments, "limit", 50, 1, 200);
            let snapshot = state.runtime.fresh_snapshot("backlinks").await?;
            let index = snapshot.index;
            let backlinks =
                index_graph::backlinks(&index, &path, limit).map_err(|error| error.to_string())?;
            Ok(json_text_result(json!({
                "path": path,
                "rebuilt": snapshot.rebuilt,
                "count": backlinks.len(),
                "backlinks": backlinks.into_iter().map(|item| note_result_json(item.path, item.title, |object| {
                    object.insert("matchedLinks".to_string(), json!(item.matched_links));
                })).collect::<Vec<_>>()
            })))
        }
        "graph_traverse" => {
            let path = string_arg(arguments, "path")?;
            let direction = optional_enum_string_arg(
                arguments,
                "direction",
                &["incoming", "outgoing", "both"],
            )?
            .unwrap_or_else(|| "both".to_string());
            let depth = clamped_usize_arg(arguments, "depth", 1, 1, 6);
            let limit = clamped_usize_arg(arguments, "limit", 100, 1, 500);
            let snapshot = state.runtime.fresh_snapshot("graph_traverse").await?;
            let index = snapshot.index;
            let graph_direction = match direction.as_str() {
                "incoming" => index_graph::GraphDirection::Incoming,
                "outgoing" => index_graph::GraphDirection::Outgoing,
                _ => index_graph::GraphDirection::Both,
            };
            let graph = index_graph::graph_traverse(&index, &path, graph_direction, depth, limit)
                .map_err(|error| error.to_string())?;
            Ok(json_text_result(json!({
                "path": path,
                "rebuilt": snapshot.rebuilt,
                "direction": direction,
                "depth": depth,
                "nodeCount": graph.nodes.len(),
                "edgeCount": graph.edges.len(),
                "nodes": graph.nodes.into_iter().map(|node| note_result_json(node.path, node.title, |object| {
                    object.insert("depth".to_string(), json!(node.depth));
                })).collect::<Vec<_>>(),
                "edges": graph.edges.into_iter().map(|edge| json!({
                    "source": edge.source,
                    "target": edge.target,
                    "rawLink": edge.raw_link
                })).collect::<Vec<_>>()
            })))
        }
        "load_knowledge" => {
            let subject = string_arg(arguments, "subject")?;
            validate_format_arg(arguments)?;
            let project = optional_string_arg(arguments, "project");
            let limit_notes = clamped_usize_arg(arguments, "limitNotes", 6, 1, 12);
            let limit_chunks = clamped_usize_arg(arguments, "limitChunks", 8, 1, 16);
            let include_graph = bool_arg(arguments, "includeGraph", true);
            let graph_depth = clamped_usize_arg(arguments, "graphDepth", 1, 1, 3);
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let snapshot = state.runtime.fresh_snapshot("load_knowledge").await?;
            let index = snapshot.index;
            let query = [Some(subject.clone()), project.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let chunk_matches = hybrid_search_matches(
                index.clone(),
                query.clone(),
                RankingOptions {
                    limit: limit_chunks,
                    semantic_weight: 0.6,
                    bm25_weight: 0.4,
                },
            )
            .await?;

            let mut chunk_paths = Vec::new();
            let mut chunks = Vec::new();
            for chunk in chunk_matches {
                if !chunk_paths.iter().any(|existing| existing == &chunk.path) {
                    chunk_paths.push(chunk.path.clone());
                }
                let mut chunk_value = hybrid_search_match_json(&chunk, text_options);
                if let Some(chunk_object) = chunk_value.as_object_mut() {
                    chunk_object.insert("wikiLink".to_string(), json!(note_wiki_link(&chunk.path)));
                }
                chunks.push(chunk_value);
            }

            let mut note_bucket = HashMap::<String, KnowledgeNote>::new();
            for chunk in &chunks {
                if let Some(path) = chunk.get("path").and_then(Value::as_str) {
                    let title = chunk
                        .get("title")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| note_name(path));
                    let score = chunk.get("score").and_then(Value::as_f64).unwrap_or(0.0);
                    merge_knowledge_note(
                        &mut note_bucket,
                        KnowledgeNote {
                            path: path.to_string(),
                            title,
                            wiki_link: note_wiki_link(path),
                            score,
                            reasons: vec!["top chunk match".to_string()],
                            shared_links: Vec::new(),
                        },
                    );
                }
            }

            for seed_path in chunk_paths.iter().take(limit_notes.min(4)) {
                if let Ok(related) = index_search::related_notes_with_options(
                    &index,
                    seed_path,
                    RelatedNoteOptions {
                        limit: limit_notes.min(4),
                    },
                ) {
                    for note in related {
                        merge_knowledge_note(
                            &mut note_bucket,
                            KnowledgeNote {
                                path: note.path.clone(),
                                title: note.title.clone(),
                                wiki_link: note_wiki_link(&note.path),
                                score: note.score * 0.85,
                                reasons: vec![format!("related to {}", seed_path)],
                                shared_links: note.shared_links,
                            },
                        );
                    }
                }
            }

            let mut notes = note_bucket
                .into_values()
                .map(knowledge_note_value)
                .collect::<Vec<_>>();
            notes.sort_by(|left, right| {
                let left_score = left.get("score").and_then(Value::as_f64).unwrap_or(0.0);
                let right_score = right.get("score").and_then(Value::as_f64).unwrap_or(0.0);
                normalize_score_order(
                    left_score,
                    right_score,
                    left.get("path").and_then(Value::as_str).unwrap_or(""),
                    right.get("path").and_then(Value::as_str).unwrap_or(""),
                )
            });
            notes.truncate(limit_notes);

            let graph = if include_graph && !chunk_paths.is_empty() {
                let graph_payload = index_graph::graph_traverse(
                    &index,
                    &chunk_paths[0],
                    index_graph::GraphDirection::Both,
                    graph_depth,
                    (limit_notes * 4).max(20),
                )
                .map_err(|error| error.to_string())?;
                json!({
                    "nodes": graph_payload.nodes.into_iter().map(|node| note_result_json(node.path, node.title, |object| {
                        object.insert("depth".to_string(), json!(node.depth));
                    })).collect::<Vec<_>>(),
                    "edges": graph_payload.edges.into_iter().map(|edge| json!({
                        "source": edge.source,
                        "target": edge.target,
                        "rawLink": edge.raw_link
                    })).collect::<Vec<_>>()
                })
            } else {
                json!({"nodes":[],"edges":[]})
            };

            let mut result = Map::new();
            result.insert("subject".to_string(), json!(subject));
            if let Some(project) = project {
                result.insert("project".to_string(), json!(project));
            }
            result.insert("rebuilt".to_string(), json!(snapshot.rebuilt));
            result.insert(
                "semanticBackend".to_string(),
                json!(index.semantic_backend.as_str()),
            );
            result.insert("notes".to_string(), json!(notes));
            result.insert("chunks".to_string(), json!(chunks));
            result.insert("graph".to_string(), graph);
            Ok(json_text_result_from_arguments(
                arguments,
                Value::Object(result),
            ))
        }
        "recommend_folder" => {
            let topic = string_arg(arguments, "topic")?;
            let project = optional_string_arg(arguments, "project");
            let folders =
                list_top_level_folders(&config.vault_path).map_err(|error| error.to_string())?;
            if folders.is_empty() {
                return Ok(json_text_result(json!({
                    "folder": "Knowledge Capture",
                    "reason": "no visible top-level folders found",
                    "scores": []
                })));
            }
            let snapshot = state.runtime.fresh_snapshot("recommend_folder").await?;
            let index = snapshot.index;
            let query = [Some(topic.clone()), project.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let matches = hybrid_search_matches(
                index.clone(),
                query.clone(),
                RankingOptions {
                    limit: 24,
                    semantic_weight: 0.6,
                    bm25_weight: 0.4,
                },
            )
            .await?;
            let query_terms: HashSet<String> = tokenize(&query).into_iter().collect();
            let mut scores = folders
                .into_iter()
                .map(|folder| {
                    let folder_terms: HashSet<String> = tokenize(&folder).into_iter().collect();
                    let matched_terms = folder_terms
                        .iter()
                        .filter(|term| query_terms.contains(*term))
                        .cloned()
                        .collect::<Vec<_>>();
                    let matching_paths = matches
                        .iter()
                        .map(|item| item.path.as_str())
                        .filter(|path| {
                            *path == format!("{}.md", folder)
                                || path.starts_with(&format!("{}/", folder))
                        })
                        .take(6)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>();
                    let score = matched_terms.len() * 8 + matching_paths.len() * 5;
                    json!({
                        "folder": folder,
                        "score": score,
                        "matchedTerms": matched_terms,
                        "matchingPaths": matching_paths
                    })
                })
                .collect::<Vec<_>>();
            scores.sort_by(|left, right| {
                let left_score = left.get("score").and_then(Value::as_u64).unwrap_or(0);
                let right_score = right.get("score").and_then(Value::as_u64).unwrap_or(0);
                right_score.cmp(&left_score).then_with(|| {
                    left.get("folder")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .cmp(right.get("folder").and_then(Value::as_str).unwrap_or(""))
                })
            });
            let best = scores.first().cloned().unwrap_or_else(|| json!({}));
            let best_score = best.get("score").and_then(Value::as_u64).unwrap_or(0);
            Ok(json_text_result(json!({
                "folder": if best_score > 0 { best.get("folder").cloned().unwrap_or_else(|| json!("Knowledge Capture")) } else { json!("Knowledge Capture") },
                "reason": if best_score > 0 {
                    if best.get("matchingPaths").and_then(Value::as_array).map(|items| !items.is_empty()).unwrap_or(false) {
                        "matched top folder among related notes"
                    } else {
                        "matched folder name to query terms"
                    }
                } else {
                    "no strong folder cluster found; using default knowledge bucket"
                },
                "scores": scores
            })))
        }
        "upsert_note" => {
            let path = string_arg(arguments, "path")?;
            if !path.to_lowercase().ends_with(".md") {
                return Err("upsert_note requires a vault-relative .md path.".to_string());
            }
            let dry_run = bool_arg(arguments, "dryRun", false);
            let expected_hash = expected_hash_arg(arguments);
            let content = compose_explicit_note_content(arguments)?;
            let preserve_manual_notes = bool_arg(arguments, "preserveManualNotes", false);
            let existing = read_text_file(&config.vault_path, &path).ok();
            let previous_hash = existing
                .as_ref()
                .map(|existing| content_hash(existing.text.as_bytes()));
            validate_expected_hash(expected_hash.as_deref(), previous_hash.as_deref(), &path)?;
            let final_content = existing
                .as_ref()
                .map(|existing| {
                    merge_with_manual_notes(&content, &existing.text, preserve_manual_notes)
                })
                .unwrap_or_else(|| finalize_written_content(&content));
            let new_hash = content_hash(final_content.as_bytes());
            let created = existing.is_none();
            if !dry_run {
                write_text_file(&config.vault_path, &path, &final_content)
                    .map_err(|error| error.to_string())?;
            }
            let title = note_title_from_content(&path, &final_content);
            Ok(json_text_result(json!({
                "action": if existing.is_some() { "updated" } else { "created" },
                "path": path,
                "title": title,
                "resourceUri": note_uri(&path),
                "wikiLink": note_alias_wiki_link(&path, &title),
                "created": created,
                "dryRun": dry_run,
                "previousHash": previous_hash,
                "newHash": new_hash
            })))
        }
        "update_note_section" => {
            let path = string_arg(arguments, "path")?;
            let target =
                optional_string_arg(arguments, "target").unwrap_or_else(|| "heading".to_string());
            let replacement = string_arg(arguments, "content")?;
            let dry_run = bool_arg(arguments, "dryRun", false);
            let expected_hash = expected_hash_arg(arguments);
            let existing =
                read_text_file(&config.vault_path, &path).map_err(|error| error.to_string())?;
            let previous_hash = content_hash(existing.text.as_bytes());
            validate_expected_hash(expected_hash.as_deref(), Some(&previous_hash), &path)?;
            let (final_content, action, level, heading) = match target.as_str() {
                "preamble" => (
                    replace_note_preamble(&existing.text, &replacement),
                    "updated".to_string(),
                    None,
                    None,
                ),
                "heading" => {
                    let heading = string_arg(arguments, "heading")?;
                    let level = clamped_usize_arg(arguments, "level", 2, 1, 6);
                    let create_if_missing = bool_arg(arguments, "createIfMissing", true);
                    let (updated, action, actual_level) = update_or_create_note_section(
                        &existing.text,
                        &heading,
                        &replacement,
                        level,
                        create_if_missing,
                    )?;
                    (
                        updated,
                        action.to_string(),
                        Some(actual_level),
                        Some(heading),
                    )
                }
                other => {
                    return Err(format!("unsupported update_note_section target: {}", other));
                }
            };
            let new_hash = content_hash(final_content.as_bytes());
            if !dry_run {
                write_text_file(&config.vault_path, &path, &final_content)
                    .map_err(|error| error.to_string())?;
            }
            Ok(json_text_result(json!({
                "action": action,
                "path": path,
                "resourceUri": note_uri(&path),
                "target": target,
                "heading": heading,
                "level": level,
                "created": false,
                "dryRun": dry_run,
                "previousHash": previous_hash,
                "newHash": new_hash
            })))
        }
        "write_file_to_vault" => {
            let path = string_arg(arguments, "path")?;
            let content = string_arg(arguments, "content")?;
            let encoding =
                optional_string_arg(arguments, "encoding").unwrap_or_else(|| "utf-8".to_string());
            let dry_run = bool_arg(arguments, "dryRun", false);
            let expected_hash = expected_hash_arg(arguments);
            let bytes = decode_file_content(&content, &encoding)?;
            let existing_bytes = existing_file_bytes(&config.vault_path, &path)?;
            let previous_hash = existing_bytes.as_ref().map(|bytes| content_hash(bytes));
            validate_expected_hash(expected_hash.as_deref(), previous_hash.as_deref(), &path)?;
            let new_hash = content_hash(&bytes);
            let created = existing_bytes.is_none();
            if !dry_run {
                write_binary_file(&config.vault_path, &path, &bytes)
                    .map_err(|error| error.to_string())?;
            }
            Ok(json_text_result(json!({
                "action": if created { "created" } else { "updated" },
                "path": path,
                "resourceUri": if path.to_lowercase().ends_with(".md") { json!(note_uri(&path)) } else { Value::Null },
                "encoding": encoding,
                "created": created,
                "dryRun": dry_run,
                "previousHash": previous_hash,
                "newHash": new_hash,
                "bytesWritten": bytes.len()
            })))
        }
        "upsert_session_note" => {
            let explicit_path = optional_string_arg(arguments, "path");
            let topic = optional_string_arg(arguments, "topic");
            let folder = optional_string_arg(arguments, "folder");
            let content = string_arg(arguments, "content")?;
            let preserve_manual_notes = bool_arg(arguments, "preserveManualNotes", true);
            let dry_run = bool_arg(arguments, "dryRun", false);
            let expected_hash = expected_hash_arg(arguments);
            if explicit_path.is_none() && (topic.is_none() || folder.is_none()) {
                return Err("upsert_session_note requires either an explicit path or both topic and folder.".to_string());
            }
            if let Some(path) = &explicit_path {
                if !path.to_lowercase().ends_with(".md") {
                    return Err(
                        "Explicit session note path must be a vault-relative .md file.".to_string(),
                    );
                }
            }
            let target_path = explicit_path.clone().unwrap_or_else(|| {
                session_note_path(
                    topic.as_deref().unwrap_or("session"),
                    folder.as_deref().unwrap_or("Knowledge Capture"),
                )
            });
            let existing = read_text_file(&config.vault_path, &target_path).ok();
            let previous_hash = existing
                .as_ref()
                .map(|existing| content_hash(existing.text.as_bytes()));
            validate_expected_hash(
                expected_hash.as_deref(),
                previous_hash.as_deref(),
                &target_path,
            )?;
            let final_content = finalize_session_note_content(
                &content,
                existing.as_ref().map(|existing| existing.text.as_str()),
                preserve_manual_notes,
            );
            let new_hash = content_hash(final_content.as_bytes());
            let created = existing.is_none();
            if !dry_run {
                write_text_file(&config.vault_path, &target_path, &final_content)
                    .map_err(|error| error.to_string())?;
            }
            Ok(json_text_result(json!({
                "action": if existing.is_some() { "updated" } else { "created" },
                "path": target_path,
                "resourceUri": note_uri(&target_path),
                "wikiLink": format!("[[{}]]", strip_md_extension(explicit_path.as_deref().unwrap_or(&session_note_path(topic.as_deref().unwrap_or("session"), folder.as_deref().unwrap_or("Knowledge Capture"))))),
                "created": created,
                "dryRun": dry_run,
                "previousHash": previous_hash,
                "newHash": new_hash
            })))
        }
        "find_similar_notes" => {
            let note_path = optional_string_arg(arguments, "path");
            let subject = optional_string_arg(arguments, "subject");
            let mode = similarity_mode(
                &optional_enum_string_arg(
                    arguments,
                    "by",
                    &["style", "structure", "tone", "format"],
                )?
                .unwrap_or_else(|| "style".to_string()),
            );
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            let reference_limit = clamped_usize_arg(arguments, "referenceLimit", 3, 1, 8);
            let snapshot = state.runtime.fresh_snapshot("find_similar_notes").await?;
            let semantic_backend = snapshot.index.semantic_backend.as_str().to_string();
            let index = snapshot.index;
            let payload = tokio::task::spawn_blocking(move || {
                find_similar_notes_payload(
                    index.as_ref(),
                    note_path.as_deref(),
                    subject.as_deref(),
                    mode,
                    limit,
                    reference_limit,
                )
            })
            .await
            .map_err(|error| error.to_string())??;
            let mut object = payload
                .as_object()
                .cloned()
                .ok_or_else(|| "find_similar_notes returned a non-object payload".to_string())?;
            object.insert("rebuilt".to_string(), json!(snapshot.rebuilt));
            object.insert("semanticBackend".to_string(), json!(semantic_backend));
            Ok(json_text_result(Value::Object(object)))
        }
        _ => Err(format!("unknown tool: {}", name)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        call_tool, clamped_usize_arg, compose_explicit_note_content, content_hash,
        finalize_session_note_content, json_text_result_from_arguments, live_grep_matches,
        merge_with_manual_notes, optional_enum_string_arg, outline_payload, replace_note_preamble,
        update_or_create_note_section, TextPayloadOptions,
    };
    use crate::mcp::AppState;
    use crate::runtime::RuntimeState;
    use deep_obsidian_types::{
        AutoReindexConfig, EmbeddingConfig, HttpConfig, ResolvedServiceConfig, StdioMode,
        TransportMode,
    };
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "deep-obsidian-server-{name}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn test_config(vault_path: PathBuf) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            index_dir: vault_path.join(".deep-obsidian-mcp-test"),
            vault_path,
            transport: TransportMode::Http,
            stdio_mode: StdioMode::Auto,
            http: HttpConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                mcp_path: "/mcp".to_string(),
                health_path: "/healthz".to_string(),
            },
            auto_reindex: AutoReindexConfig {
                enabled: false,
                debounce_ms: 0,
                interval_ms: 0,
            },
            embedding: EmbeddingConfig::default(),
            artifact_embedding: EmbeddingConfig::default(),
            config_file_path: None,
        }
    }

    async fn test_state(vault_path: PathBuf) -> AppState {
        let config = test_config(vault_path);
        let (runtime, _auto_reindex) = RuntimeState::bootstrap(config.clone())
            .await
            .expect("bootstrap runtime");
        AppState::new(config, runtime)
    }

    #[test]
    fn finalize_session_note_content_keeps_body_exact_without_inventing_title() {
        let content = "Date: 2026-04-02\n\n## Context\n\nBody";
        let actual = finalize_session_note_content(content, None, true);
        assert_eq!(actual, "Date: 2026-04-02\n\n## Context\n\nBody\n");
        assert!(!actual.starts_with("# "));
    }

    #[test]
    fn finalize_session_note_content_preserves_manual_notes_without_adding_title() {
        let existing = "# Existing Title\n\nOld body\n\n## Manual Notes\n\nKeep this";
        let content = "Date: 2026-04-02\n\n## Context\n\nNew body";
        let actual = finalize_session_note_content(content, Some(existing), true);
        assert_eq!(
            actual,
            "Date: 2026-04-02\n\n## Context\n\nNew body\n\n## Manual Notes\n\nKeep this\n"
        );
        assert!(!actual.starts_with("# "));
    }

    #[test]
    fn merge_with_manual_notes_keeps_existing_manual_section_once() {
        let existing = "Old body\n\n## Manual Notes\n\nKeep this";
        let content = "New body\n\n## Manual Notes\n\nAlready present";
        let actual = merge_with_manual_notes(content, existing, true);
        assert_eq!(actual, "New body\n\n## Manual Notes\n\nAlready present\n");
    }

    #[test]
    fn compose_explicit_note_content_supports_frontmatter_title_and_body() {
        let content = compose_explicit_note_content(&json!({
            "path": "Blog/Test.md",
            "frontmatter": {
                "title": "Hello",
                "tags": ["blog", "test"]
            },
            "title": "Hello",
            "body": "Body text"
        }))
        .expect("content should compose");

        assert!(content.starts_with("---\n"));
        assert!(content.contains("title: \"Hello\""));
        assert!(content.contains("tags:"));
        assert!(content.contains("- \"blog\""));
        assert!(content.contains("- \"test\""));
        assert!(content.contains("# Hello"));
        assert!(content.ends_with("Body text"));
    }

    #[test]
    fn replace_note_preamble_preserves_frontmatter_and_title() {
        let content = "---\ntitle: Test\n---\n\n# Title\n\nOld intro\n\n## Section\n\nBody";
        let updated = replace_note_preamble(content, "New intro");
        assert_eq!(
            updated,
            "---\ntitle: Test\n---\n\n# Title\n\nNew intro\n\n## Section\n\nBody\n"
        );
    }

    #[test]
    fn update_or_create_note_section_replaces_existing_section() {
        let content = "# Title\n\nIntro\n\n## Ngrok\n\nOld section\n\n## End\n\nDone";
        let (updated, action, level) =
            update_or_create_note_section(content, "Ngrok", "New section", 2, true)
                .expect("section should update");
        assert_eq!(action, "updated");
        assert_eq!(level, 2);
        assert_eq!(
            updated,
            "# Title\n\nIntro\n\n## Ngrok\n\nNew section\n\n## End\n\nDone\n"
        );
    }

    #[test]
    fn update_or_create_note_section_creates_missing_section() {
        let content = "# Title\n\nIntro";
        let (updated, action, level) =
            update_or_create_note_section(content, "Appendix", "New body", 3, true)
                .expect("section should be created");
        assert_eq!(action, "created");
        assert_eq!(level, 3);
        assert_eq!(updated, "# Title\n\nIntro\n\n### Appendix\n\nNew body\n");
    }

    #[test]
    fn outline_payload_returns_resource_uris_without_text_by_default() {
        let content =
            "# Title\n\nIntro\n\n## Section One\n\nBody ^block-a\n\n[[Target Note|Target]]";
        let payload = outline_payload(
            "Folder/Test.md",
            content,
            TextPayloadOptions {
                include_text: false,
                max_text_chars: 4000,
            },
        );

        assert_eq!(
            payload["resourceUri"],
            "obsidian://note?path=Folder%2FTest.md"
        );
        assert_eq!(payload["headingCount"], 2);
        assert_eq!(
            payload["headings"][1]["resourceUri"],
            "obsidian://heading?path=Folder%2FTest.md&slug=section-one"
        );
        assert_eq!(
            payload["blocks"][0]["resourceUri"],
            "obsidian://block?path=Folder%2FTest.md&id=block-a"
        );
        assert_eq!(payload["headings"][0]["textOmitted"], true);
        assert_eq!(payload["outgoingLinks"][0]["target"], "Target Note");
    }

    #[test]
    fn text_payload_options_truncate_and_compact_format() {
        let mut object = serde_json::Map::new();
        super::insert_optional_text(
            &mut object,
            "text",
            "abcdef",
            TextPayloadOptions {
                include_text: true,
                max_text_chars: 3,
            },
        );
        assert_eq!(object["text"], "abc");
        assert_eq!(object["textTruncated"], true);

        let result = json_text_result_from_arguments(&json!({"format":"compact"}), json!({"a": 1}));
        assert_eq!(result.content[0].text, "{\"a\":1}");
    }

    #[test]
    fn clamped_usize_arg_enforces_schema_limit_at_runtime() {
        assert_eq!(
            clamped_usize_arg(&json!({"limit": 999}), "limit", 20, 1, 50),
            50
        );
        assert_eq!(
            clamped_usize_arg(&json!({"limit": 0}), "limit", 20, 1, 50),
            1
        );
        assert_eq!(clamped_usize_arg(&json!({}), "limit", 20, 1, 50), 20);
    }

    #[test]
    fn optional_enum_string_arg_rejects_schema_violations() {
        let error =
            optional_enum_string_arg(&json!({"mode":"glob"}), "mode", &["substring", "regex"])
                .expect_err("invalid mode should fail");
        assert!(error.contains("unsupported mode"));
    }

    #[tokio::test]
    async fn upsert_note_dry_run_and_expected_hash_do_not_write_on_conflict() {
        let vault_path = temp_dir("upsert-hash");
        let state = test_state(vault_path.clone()).await;

        let dry_run = call_tool(
            &state,
            "upsert_note",
            &json!({
                "path": "Notes/Dry.md",
                "content": "# Dry\n\nPreview only",
                "dryRun": true
            }),
        )
        .await
        .expect("dry run should succeed");
        assert_eq!(dry_run.structured_content["dryRun"], true);
        assert!(dry_run.structured_content["newHash"].as_str().is_some());
        assert!(!vault_path.join("Notes/Dry.md").exists());

        let created = call_tool(
            &state,
            "upsert_note",
            &json!({
                "path": "Notes/Dry.md",
                "content": "# Dry\n\nOriginal"
            }),
        )
        .await
        .expect("create should succeed");
        let previous_hash = created.structured_content["newHash"]
            .as_str()
            .expect("new hash")
            .to_string();

        let conflict = call_tool(
            &state,
            "upsert_note",
            &json!({
                "path": "Notes/Dry.md",
                "content": "# Dry\n\nChanged",
                "expectedHash": "fnv1a64:0000000000000000"
            }),
        )
        .await
        .expect_err("hash conflict should fail");
        assert!(conflict.contains("hash conflict"));
        let file_text = fs::read_to_string(vault_path.join("Notes/Dry.md")).expect("read note");
        assert_eq!(file_text, "# Dry\n\nOriginal\n");
        assert_eq!(content_hash(file_text.as_bytes()), previous_hash);
    }

    #[tokio::test]
    async fn read_artifact_returns_metadata_and_bounded_base64() {
        let vault_path = temp_dir("read-artifact");
        fs::create_dir_all(vault_path.join("Assets")).expect("mkdir");
        fs::write(vault_path.join("Assets/Logo.png"), b"png-bytes").expect("write artifact");
        let state = test_state(vault_path.clone()).await;

        let result = call_tool(
            &state,
            "read_artifact",
            &json!({
                "path": "Assets/Logo.png",
                "includeBase64": true,
                "maxBytes": 64
            }),
        )
        .await
        .expect("read artifact should succeed");

        assert_eq!(result.structured_content["path"], "Assets/Logo.png");
        assert_eq!(result.structured_content["kind"], "image");
        assert_eq!(result.structured_content["mimeType"], "image/png");
        assert_eq!(result.structured_content["base64"], "cG5nLWJ5dGVz");
        assert_eq!(
            result.structured_content["resourceUri"],
            "obsidian://artifact?path=Assets%2FLogo.png"
        );
    }

    #[tokio::test]
    async fn grep_search_populates_context_lines() {
        let vault_path = temp_dir("grep-context");
        fs::write(
            vault_path.join("Context.md"),
            "alpha\nbefore\nneedle here\nafter\nomega\n",
        )
        .expect("write note");

        let matches = live_grep_matches(vault_path, "needle".to_string(), false, true, None, 1, 10)
            .await
            .expect("grep matches");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 3);
        assert_eq!(matches[0].context_before[0].line_text, "before");
        assert_eq!(matches[0].context_after[0].line_text, "after");
    }
}
