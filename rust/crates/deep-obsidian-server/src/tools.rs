use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use deep_obsidian_backend::{
    BackendRequest, BaseVersion, GrepContextLine, GrepMatch, RecallRequest, VaultChildEntry,
    VaultEntryKind, RIPGREP_UNAVAILABLE_MESSAGE,
};
use deep_obsidian_core::text::{
    extract_block_sections, extract_heading_sections, extract_wiki_links, normalize_heading_slug,
    note_title, tokenize,
};
use deep_obsidian_index::graph as index_graph;
use deep_obsidian_index::index::{artifact_kind, artifact_mime_type, IndexError};
use deep_obsidian_index::search::{self as index_search, RankingOptions, RelatedNoteOptions};
use regex::RegexBuilder;
use serde_json::{json, Map, Value};

use crate::health::{build_vault_overview_payload, insert_mount_index_detail};
use crate::mcp::AppState;
use crate::protocol::{ToolCallResult, ToolContent, ToolDefinition};
use crate::resources::{artifact_uri, block_uri, heading_uri, note_name, note_uri};
use crate::runtime::{RuntimeIndexSnapshot, RuntimeState};
const JSON_SCHEMA_URI: &str = "http://json-schema.org/draft-07/schema#";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const DEFAULT_MAX_TEXT_CHARS: usize = 20_000;
/// Per-result snippet default for multi-result search tools when the caller does
/// not pass `maxTextChars`. A search snippet does not need the 20k full-document
/// budget; this keeps the typical `limit`-sized response small.
const DEFAULT_SEARCH_SNIPPET_CHARS: usize = 2_000;
/// Aggregate cap on total emitted snippet text across ALL matches in a single
/// response. Once exhausted, later matches keep their metadata but drop their
/// `text` field (marked `<key>Omitted`). This is the per-response guard that the
/// per-field `max_text_chars` cap alone cannot provide for multi-result tools.
const RESPONSE_TEXT_BUDGET_CHARS: usize = 24_000;
const TRUNCATION_NOTE: &str =
    "Response text truncated to fit the aggregate budget; later matches' text was omitted. Lower `limit` or call read_file for full text.";

/// Clear, actionable message for `search_artifacts` when the artifact embedding backend is
/// unreachable at query time. Artifacts have no lexical (BM25) fallback, so the tool errors
/// — but with this message instead of the raw upstream 400/connection error.
const ARTIFACT_EMBEDDING_BACKEND_UNAVAILABLE_MESSAGE: &str = "artifact embedding backend unavailable — check the artifact embedding service (it may be down or restarting), then retry.";

#[derive(Debug, Clone)]
struct KnowledgeNote {
    path: String,
    title: String,
    wiki_link: String,
    score: f64,
    reasons: Vec<String>,
    shared_links: Vec<String>,
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
        .ok_or_else(|| missing_required_argument(key))
}

/// Build a self-explanatory error for a missing required argument. Well-known
/// argument names get a short hint describing what they mean (cross-checked
/// against the tool input-schema property descriptions); anything else falls
/// back to a clearer-but-generic message.
fn missing_required_argument(key: &str) -> String {
    let hint = match key {
        "query" => Some("the text or pattern to search for"),
        "topic" => Some("the subject to recommend a folder for"),
        "subject" => Some("the conversation subject or user problem to ground against the vault"),
        "path" => Some("vault-relative file path, e.g. \"Projets/A2A/Current Sprint.md\""),
        "heading" => Some("the exact heading title of the section"),
        "content" => Some("the replacement body content for the targeted section"),
        _ => None,
    };
    match hint {
        Some(hint) => format!("missing required argument '{}' ({})", key, hint),
        None => format!("missing required argument '{}'", key),
    }
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

    /// Like [`from_arguments`], but defaults the per-result snippet cap to
    /// [`DEFAULT_SEARCH_SNIPPET_CHARS`] when the caller did not pass
    /// `maxTextChars`. Used by multi-result search tools so the aggregate
    /// response stays small by default. An explicit `maxTextChars` is still
    /// honored (clamped to [`DEFAULT_MAX_TEXT_CHARS`]).
    fn search_snippet_from_arguments(arguments: &Value, default_include_text: bool) -> Self {
        let mut options = Self::from_arguments(arguments, default_include_text);
        if arguments.get("maxTextChars").is_none() {
            options.max_text_chars = DEFAULT_SEARCH_SNIPPET_CHARS;
        }
        options
    }
}

/// Enforce an aggregate text budget across an ordered list of already-built
/// match objects. Walks the matches in order, summing the char length of each
/// present `key` field. The first match that pushes the cumulative total past
/// `budget` is still included whole; every match after it has its `key` field
/// removed and `<key>Omitted` set to `true`. Returns `true` if any match's text
/// was omitted.
fn apply_response_text_budget(matches: &mut [Value], key: &str, budget: usize) -> bool {
    let omitted_key = format!("{key}Omitted");
    let mut used = 0usize;
    let mut exhausted = false;
    let mut any_omitted = false;
    for item in matches.iter_mut() {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        if exhausted {
            if object.remove(key).is_some() {
                // Drop the now-stale per-field `<key>Truncated` flag that
                // `insert_optional_text` wrote, and mark the field omitted.
                object.remove(&format!("{key}Truncated"));
                object.insert(omitted_key.clone(), json!(true));
                any_omitted = true;
            }
            continue;
        }
        let len = object
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        used = used.saturating_add(len);
        if used > budget {
            exhausted = true;
        }
    }
    any_omitted
}

/// Insert response-level truncation signaling fields when [`apply_response_text_budget`]
/// reported that snippet text was omitted. Additive and backward-compatible:
/// nothing is inserted for responses that stayed within budget.
fn insert_response_truncation_flags(object: &mut Map<String, Value>, response_truncated: bool) {
    if response_truncated {
        object.insert("responseTruncated".to_string(), json!(true));
        object.insert("truncationNote".to_string(), json!(TRUNCATION_NOTE));
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

/// The canonical content hash. Re-exported from core so the tool layer's one-shot
/// hashing and the backend's incremental (streaming upload) hashing are the same
/// function rather than two copies that must be kept in sync.
pub(crate) use deep_obsidian_core::content_hash;

/// True when `path` targets a protected Template(s) folder. Mirrors the policy
/// in core's `ensure_writable_vault_relative_path` (which is private), so the
/// out-of-band upload path enforces the same protection as `write_binary_file`.
fn is_protected_write_path(path: &str) -> bool {
    path.trim_start_matches('/').split('/').any(|segment| {
        segment.eq_ignore_ascii_case("template") || segment.eq_ignore_ascii_case("templates")
    })
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

/// Resolves the note text from either `content` (stored exactly) or the
/// compose fields (`body` + optional `title`/`frontmatter`). Returns the text
/// plus an optional warning to surface in the tool result.
fn compose_explicit_note_content(arguments: &Value) -> Result<(String, Option<String>), String> {
    let explicit_content = optional_string_arg(arguments, "content");
    let mut body = optional_string_arg(arguments, "body");
    let title = optional_string_arg(arguments, "title");
    let frontmatter = arguments.get("frontmatter");
    let mut warning = None;

    // Some clients fill every schema property and send content and body
    // together on each call. Identical text is unambiguous, so accept it with
    // a warning instead of failing; different text stays a hard error.
    if let (Some(content), Some(duplicate)) = (&explicit_content, &body) {
        if content.trim_end() != duplicate.trim_end() {
            return Err("upsert_note received both content and body with different text; provide exactly one of them: content (stored as given) or body (composed with title/frontmatter).".to_string());
        }
        warning = Some("content and body were both provided with identical text; content was used. Provide only one of them.".to_string());
        body = None;
    }

    if explicit_content.is_some() && (body.is_some() || title.is_some() || frontmatter.is_some()) {
        return Err("upsert_note accepts either full content or explicit body/title/frontmatter fields, not both.".to_string());
    }

    if let Some(content) = explicit_content {
        return Ok((content, warning));
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
    Ok((parts.join("\n\n"), warning))
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

/// Like `object_schema`, but merges additional top-level schema keys (e.g.
/// `allOf` for conditional requirements) into the resulting object. Used for
/// tools whose constraints cannot be expressed by a flat `required` array.
fn object_schema_with_extra(
    properties: Vec<(&str, Value)>,
    required: Vec<&str>,
    extra: Vec<(&str, Value)>,
) -> Value {
    let mut schema = object_schema(properties, required);
    if let Value::Object(map) = &mut schema {
        for (key, value) in extra {
            map.insert(key.to_string(), value);
        }
    }
    schema
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

/// The recall tools that a `scope` argument can route to exactly one mount's
/// index.
///
/// Deliberately short. A tool belongs here only if answering it from ONE mount's
/// index is a complete answer to the scoped question. `find_files` and
/// `recommend_folder` do not qualify — see [`require_single_mount`].
const SCOPE_ROUTED_RECALL_TOOLS: [&str; 3] =
    ["hybrid_search", "search_artifacts", "load_knowledge"];

/// The multi-mount-only `scope` property.
///
/// A mount root rather than an arbitrary folder, because these tools RANK: a
/// deeper scope could only be honoured by filtering an already-truncated top-`limit`
/// list, which would silently return fewer results than asked for. Naming a mount
/// exactly keeps the answer exact. See [`resolve_recall_scope`].
fn scope_property() -> Value {
    json!({
        "type": "string",
        "description": "Which mount to search, on a multi-mount vault. Must name a mount root exactly ('/' for the mount at the vault root; see vault_info.mounts[].mountAt for the rest). That mount's index serves the whole request and results are reported as logical vault paths. Selecting a mount does NOT include content grafted under it by another mount: search each mount in turn to cover the whole vault."
    })
}

/// Add the `scope` argument to the routable recall tools.
///
/// Applied ONLY for a multi-mount config. A single-mount vault has nothing to
/// choose between, so its `tools/list` is byte-identical to the frozen golden —
/// the same reason `grep_search` is registered conditionally and
/// `vault_info.mounts` is emitted conditionally.
fn insert_scope_argument(definitions: &mut [ToolDefinition]) {
    for definition in definitions
        .iter_mut()
        .filter(|definition| SCOPE_ROUTED_RECALL_TOOLS.contains(&definition.name.as_str()))
    {
        let Some(schema) = definition.input_schema.as_object_mut() else {
            continue;
        };
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.insert("scope".to_string(), scope_property());
        }
        // Required, not optional: these tools cannot answer an unscoped question on
        // a multi-mount vault at all, so the schema says so rather than letting a
        // client discover it through an error.
        match schema.get_mut("required").and_then(Value::as_array_mut) {
            Some(required) => required.push(json!("scope")),
            None => {
                schema.insert("required".to_string(), json!(["scope"]));
            }
        }
    }
}

fn tool_definitions(rg_available: bool, multi_mount: bool) -> Vec<ToolDefinition> {
    let mut definitions = vec![
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
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_SEARCH_SNIPPET_CHARS})),
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
            description: "Create or update a markdown note. Provide EITHER content (full markdown stored exactly as given) OR the compose fields body + optional title/frontmatter — never both. This tool does not inject implicit headings.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema_with_extra(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative markdown path to create or update."})),
                    ("content", json!({"type":"string","description":"Full markdown content to store exactly as provided. Mutually exclusive with body, title, and frontmatter — do not send both content and body."})),
                    ("body", json!({"type":"string","description":"Markdown body for compose mode, combined with optional title/frontmatter. Mutually exclusive with content — set body or content, never both."})),
                    ("title", json!({"type":"string","description":"Optional explicit H1 title to prepend in compose (body) mode. Do not combine with content."})),
                    ("frontmatter", json!({"type":"object","description":"Optional frontmatter object serialized in compose (body) mode. Do not combine with content."})),
                    ("preserveManualNotes", json!({"type":"boolean","default":false})),
                    ("dryRun", json!({"type":"boolean","default":false,"description":"Preview the write without changing the vault."})),
                    ("expectedHash", json!({"type":"string","description":"Optional hash of the current note content. If it does not match, no write occurs."})),
                ],
                vec!["path"],
                // Encode the content XOR body/title/frontmatter contract for
                // clients that validate: exactly one branch must match, and
                // each branch explicitly excludes the other mode's fields.
                vec![(
                    "oneOf",
                    json!([
                        {
                            "required": ["content"],
                            "not": {"anyOf": [
                                {"required": ["body"]},
                                {"required": ["title"]},
                                {"required": ["frontmatter"]}
                            ]}
                        },
                        {
                            "required": ["body"],
                            "not": {"required": ["content"]}
                        }
                    ]),
                )],
            ),
        },
        ToolDefinition {
            name: "update_note_section".to_string(),
            description: "Replace the note preamble or a named heading section without rewriting the whole note.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema_with_extra(
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
                // `heading` is required unless writing the preamble. The `if`
                // matches only when `target` is *present and* equal to "preamble"
                // (the `required: ["target"]` guard stops `properties` matching
                // vacuously when `target` is absent); the `else` branch then
                // requires `heading` for both an explicit target:"heading" and an
                // absent target (which defaults to heading).
                vec![("allOf", json!([
                    {
                        "if": {
                            "required": ["target"],
                            "properties": {"target": {"const": "preamble"}}
                        },
                        "then": {},
                        "else": {"required": ["heading"]}
                    }
                ]))],
            ),
        },
        ToolDefinition {
            name: "request_vault_upload".to_string(),
            description: "Mint a short-lived, single-use upload URL for a binary file too large to inline as base64. Bytes are uploaded out-of-band (e.g. via curl) to the returned URL, which writes them to the bound vault path. Requires the HTTP service transport.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(false))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Vault-relative destination path the uploaded bytes will be written to."})),
                    ("expectedHash", json!({"type":"string","description":"Optional hash of the current destination content for optimistic concurrency, checked at upload commit."})),
                    ("mimeType", json!({"type":"string","description":"Optional informational MIME type of the file being uploaded."})),
                ],
                vec!["path"],
            ),
        },
        ToolDefinition {
            name: "list_children".to_string(),
            description: "List the direct children of a vault directory, including non-markdown files and subfolders. Set foldersOnly:true to return only the subfolders (direct subdirectories).".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("path", json!({"type":"string","description":"Optional vault-relative directory path. Defaults to the vault root."})),
                    ("foldersOnly", json!({"type":"boolean","default":false,"description":"When true, return only the direct subfolders of the directory."})),
                    ("includeHidden", json!({"type":"boolean","default":false})),
                    ("includeIgnored", json!({"type":"boolean","default":false})),
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
                    ("knownHash", json!({"type":"string","description":"If set and it matches the file's current content hash, the body is omitted and `unchanged: true` is returned."})),
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
            name: "hybrid_search".to_string(),
            description: "Combine BM25 lexical ranking with semantic similarity over note chunks using Reciprocal Rank Fusion (rank-based, scale-free). Set bm25Weight:0 for semantic-only ranking, or semanticWeight:0 for BM25-only ranking.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("query", json!({"type":"string","description":"Natural-language or lexical query."})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":50,"default":8})),
                    ("semanticWeight", json!({"type":"number","minimum":0,"maximum":1,"default":1.0,"description":"RRF weight for the semantic list (multiplies its 1/(k+rank) contribution). Default 1.0 (unweighted)."})),
                    ("bm25Weight", json!({"type":"number","minimum":0,"maximum":1,"default":1.0,"description":"RRF weight for the BM25 list (multiplies its 1/(k+rank) contribution). Default 1.0 (unweighted)."})),
                    ("includeText", json!({"type":"boolean","default":true})),
                    ("maxTextChars", json!({"type":"integer","minimum":0,"maximum":DEFAULT_MAX_TEXT_CHARS,"default":DEFAULT_SEARCH_SNIPPET_CHARS})),
                    ("format", json!({"type":"string","enum":["pretty","compact"],"default":"pretty"})),
                ],
                vec!["query"],
            ),
        },
        ToolDefinition {
            name: "search_artifacts".to_string(),
            description: "Semantically search non-markdown vault artifacts (PDF, image, audio, video) by their multimodal embeddings. Requires a configured artifact embedding backend; returns artifact metadata, not chunk text.".to_string(),
            annotations: Some(tool_annotations(true, None, None)),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("query", json!({"type":"string","description":"Natural-language query, embedded via the artifact (multimodal) model and matched against artifact embeddings."})),
                    ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":50,"default":8})),
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
            name: "graph_traverse".to_string(),
            description: "Traverse the Obsidian wiki-link graph around a note. For backlinks (notes that link to the given note), use direction:\"incoming\" with depth:1.".to_string(),
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
    ];
    // "rg works or grep_search doesn't exist." When ripgrep is not available we
    // omit the tool entirely so it never appears in `tools/list`.
    if !rg_available {
        definitions.retain(|definition| definition.name != "grep_search");
    }
    if multi_mount {
        insert_scope_argument(&mut definitions);
    }
    definitions
}

pub fn list_tools(rg_available: bool, multi_mount: bool) -> Vec<ToolDefinition> {
    tool_definitions(rg_available, multi_mount)
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

fn grep_context_line_json(line: &GrepContextLine) -> Value {
    json!({
        "lineNumber": line.line_number,
        "lineText": line.line_text
    })
}

fn grep_match_json(match_item: &GrepMatch, options: TextPayloadOptions) -> Value {
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

/// Match `query` against a note-path list already fetched from the backend.
fn live_find_file_matches(
    files: Vec<String>,
    query: &str,
    mode: &str,
    limit: usize,
) -> Result<Vec<index_search::FilePathMatch>, String> {
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

/// Issue a backend request, rendering any failure as the tool layer's `String` error.
///
/// The rendering is [`BackendError`](deep_obsidian_backend::BackendError)'s `Display`,
/// which delegates to the underlying source — so a vault error keeps core's enriched
/// wording and a bare IO error keeps its unadorned one. That is what makes these
/// call sites byte-identical to the direct `fs`/`vault` calls they replaced.
async fn backend_call(
    state: &AppState,
    request: BackendRequest,
) -> Result<deep_obsidian_backend::BackendResponse, String> {
    state
        .router
        .execute(request)
        .await
        .map_err(|error| error.to_string())
}

/// Refuse a whole-vault tool that has no way to name a single mount.
///
/// Every mount now has its own index, so the limitation is no longer "only the
/// root mount is indexed" — it is that answering these tools across mounts means
/// MERGING and re-ranking several independent result sets, which this slice
/// deliberately does not do. `find_files` (a limit-truncated path match over the
/// whole vault) and `recommend_folder` (a whole-vault placement ranking) are both
/// exactly that question, and neither has an argument that could narrow it, so the
/// honest answer is still an error rather than one mount's partial view.
/// Single-mount configs never reach it.
fn require_single_mount(state: &AppState, tool: &str) -> Result<(), String> {
    if !state.router.is_multi_mount() {
        return Ok(());
    }
    Err(format!(
        "{tool} does not support a multi-mount vault yet: it takes no argument that could narrow it to one mount, and answering it across mounts would mean merging and re-ranking each mount's own index, which is not implemented. Reduce the vault to a single mount, or use a recall tool that takes 'scope'."
    ))
}

/// The mount roots a `scope` may name, rendered for an error message.
fn mount_scope_hint(router: &deep_obsidian_backend::VaultRouter) -> String {
    router
        .mounts()
        .iter()
        .map(|mount| {
            if mount.mount_at.is_empty() {
                "'/'".to_string()
            } else {
                format!("'{}'", mount.mount_at)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Which mount's index serves a recall request, and how to render its paths.
///
/// A search index is built from ONE vault directory and therefore stores
/// MOUNT-RELATIVE paths. Every path leaving it must be translated back into the
/// logical namespace, which is the only addressing scheme a client knows.
///
/// # The root mount is the identity, and that is load-bearing
///
/// For the root mount `mount_at` is `""`, so [`ScopedIndex::to_logical`] and
/// [`ScopedIndex::relabel_path`] are both the identity function. That is what lets
/// the translation be applied unconditionally in every recall tool while the 37
/// single-mount goldens stay byte-identical: a single-mount config has only a root
/// mount, so every translation below is provably a no-op rather than conditionally
/// skipped.
///
/// The opposite direction (logical -> mount-relative) is NOT duplicated here:
/// `VaultRouter::resolve` already returns it as `backend_relative_path`, and
/// having one implementation is what keeps a read and a recall of the same path
/// agreeing on which mount owns it.
struct ScopedIndex {
    runtime: Arc<RuntimeState>,
    /// The logical folder this mount's index paths sit under; `""` for the root.
    mount_at: String,
}

impl ScopedIndex {
    /// A mount-relative index path as a logical vault path. Identity for the root.
    fn to_logical(&self, mount_relative: &str) -> String {
        if self.mount_at.is_empty() {
            mount_relative.to_string()
        } else if mount_relative.is_empty() {
            self.mount_at.clone()
        } else {
            format!("{}/{}", self.mount_at, mount_relative)
        }
    }

    /// Translate the `path` field of an already-built result object in place.
    fn relabel_path(&self, value: &mut Value, key: &str) {
        if self.mount_at.is_empty() {
            return;
        }
        if let Some(object) = value.as_object_mut() {
            if let Some(path) = object.get(key).and_then(Value::as_str) {
                let logical = self.to_logical(path);
                object.insert(key.to_string(), json!(logical));
                // The resource URI is derived from the path, so it has to move with it.
                if object.contains_key("resourceUri") {
                    object.insert("resourceUri".to_string(), json!(note_uri(&logical)));
                }
                if object.contains_key("wikiLink") {
                    object.insert("wikiLink".to_string(), json!(note_wiki_link(&logical)));
                }
            }
        }
    }
}

/// The index serving a recall tool that takes a `scope`.
///
/// * single mount — the root runtime, unconditionally: `scope` is not even in the
///   tool schema, so this is the pre-slice behaviour verbatim;
/// * multi-mount, no `scope` — refused. Each mount has its own index, so an
///   unscoped answer would silently omit every other mount;
/// * multi-mount, `scope` naming a mount root — that mount's runtime.
///
/// A `scope` must name a mount root EXACTLY. These tools rank and truncate to
/// `limit`, so a narrower scope could only be honoured by filtering an
/// already-truncated list — silently returning fewer results than asked for. A
/// refusal that names the acceptable scopes is the exact answer; an approximate
/// one is not.
///
/// # `scope` selects a MOUNT, not a folder subtree
///
/// `'/'` therefore means "the mount at the vault root", not "the whole logical
/// vault": it is answered from the root mount's own index, and content grafted
/// under it by another mount is not included. That is the only reading that leaves
/// the root mount reachable at all — every non-root mount is nested inside the
/// root's subtree by definition, so treating `'/'` as a subtree would refuse it
/// always. The tool schema says so in as many words, and the refusal above
/// enumerates every mount, so a caller who wants the whole vault knows exactly
/// which calls to make. (Contrast `grep_search`, whose `glob` genuinely IS a
/// subtree filter and so must refuse a scope containing another mount — see
/// [`VaultRouter::scope_contains_other_mount`](deep_obsidian_backend::VaultRouter::scope_contains_other_mount).)
fn resolve_recall_scope(
    state: &AppState,
    tool: &str,
    scope: Option<&str>,
) -> Result<ScopedIndex, String> {
    if !state.router.is_multi_mount() {
        return Ok(ScopedIndex {
            runtime: state.runtime().clone(),
            mount_at: String::new(),
        });
    }
    let Some(scope) = scope else {
        return Err(format!(
            "{tool} requires a 'scope' on a multi-mount vault: every mount has its own search index, so an unscoped answer would silently omit every mount but one. Pass 'scope' naming the mount to search: {}.",
            mount_scope_hint(state.router.as_ref())
        ));
    };
    let resolved = state
        .router
        .resolve(scope)
        .map_err(|error| error.to_string())?;
    if !resolved.backend_relative_path.trim_matches('/').is_empty() {
        return Err(format!(
            "{tool} cannot scope to '{scope}': it is inside mount '{}' rather than naming a mount root, and this tool ranks results, so a narrower scope could only be honoured by filtering an already-truncated list. Pass one of: {}.",
            resolved.mount.id,
            mount_scope_hint(state.router.as_ref())
        ));
    }
    mount_index(state, tool, resolved.mount)
}

/// The index serving a recall tool that takes a note `path`: the mount owning it.
///
/// Returns the index alongside the path as that mount's index stores it. The
/// mount's graph and similarity neighbourhood are SELF-CONTAINED: a link from a
/// note on one mount to a note on another is not an edge in either mount's index,
/// because each index is built from one vault directory. Cross-mount edges are a
/// federation concern, not something this can synthesize.
fn resolve_recall_path(
    state: &AppState,
    tool: &str,
    logical_path: &str,
) -> Result<(ScopedIndex, String), String> {
    if !state.router.is_multi_mount() {
        return Ok((
            ScopedIndex {
                runtime: state.runtime().clone(),
                mount_at: String::new(),
            },
            logical_path.to_string(),
        ));
    }
    let resolved = state
        .router
        .resolve(logical_path)
        .map_err(|error| error.to_string())?;
    let mount_relative = resolved.backend_relative_path.clone();
    let index = mount_index(state, tool, resolved.mount)?;
    Ok((index, mount_relative))
}

/// The runtime backing one mount, or a clear error when that mount has no index.
///
/// Unreachable while every backend is a filesystem vault; it is the seam a backend
/// that brings its own index (or none) will report through.
fn mount_index(
    state: &AppState,
    tool: &str,
    mount: &deep_obsidian_backend::Mount,
) -> Result<ScopedIndex, String> {
    let runtime = state
        .runtimes
        .for_mount(&mount.id)
        .ok_or_else(|| {
            format!(
                "{tool} cannot be served: mount '{}' has no index.",
                mount.id
            )
        })?
        .clone();
    Ok(ScopedIndex {
        runtime,
        mount_at: mount.mount_at.clone(),
    })
}

/// One entry of `build_index`'s additive per-mount report.
fn build_index_mount_json(
    id: &str,
    mount_at: &str,
    outcome: Result<&RuntimeIndexSnapshot, &str>,
) -> Value {
    let mut entry = Map::from_iter([
        ("id".to_string(), json!(id)),
        ("mountAt".to_string(), json!(mount_at)),
    ]);
    match outcome {
        Ok(snapshot) => {
            entry.insert("rebuilt".to_string(), json!(true));
            entry.insert(
                "generatedAt".to_string(),
                json!(snapshot.index.generated_at),
            );
            entry.insert("noteCount".to_string(), json!(snapshot.index.note_count));
            entry.insert("chunkCount".to_string(), json!(snapshot.index.chunk_count));
            entry.insert(
                "semanticBackend".to_string(),
                json!(snapshot.index.semantic_backend.as_str()),
            );
        }
        Err(error) => {
            entry.insert("rebuilt".to_string(), json!(false));
            entry.insert("error".to_string(), json!(error));
        }
    }
    Value::Object(entry)
}

/// Sum an integer field over a per-mount report.
fn sum_mount_field(mounts: &[Value], key: &str) -> u64 {
    mounts
        .iter()
        .filter_map(|mount| mount.get(key).and_then(Value::as_u64))
        .sum()
}

/// Merge each mount's declared capabilities into a `mounts` array that
/// [`insert_mount_index_detail`] has already placed in `payload`.
///
/// The two halves of a mount's description come from two places — the backend
/// declares what it can do, the runtime reports how its index is doing — and
/// `vault_info` is the one payload that wants both, so they are joined here rather
/// than either side reaching into the other.
fn insert_mount_capabilities(payload: &mut Value, router: &deep_obsidian_backend::VaultRouter) {
    let Some(mounts) = payload
        .as_object_mut()
        .and_then(|object| object.get_mut("mounts"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for entry in mounts.iter_mut() {
        let Some(object) = entry.as_object_mut() else {
            continue;
        };
        let Some(id) = object
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        if let Some(mount) = router.mounts().iter().find(|mount| mount.id == id) {
            object.insert(
                "capabilities".to_string(),
                json!(mount.backend.descriptor().capabilities),
            );
        }
    }
}

/// Cap on the conflicted paths named in a `vault_info` payload.
///
/// The COUNT is always exact; only the list is truncated. A vault with hundreds of
/// conflicts has a systemic sync problem, and the right answer to it is "you have 412
/// conflicts, here are the first few", not a payload the size of the manifest.
const MAX_REPORTED_CONFLICTED_PATHS: usize = 20;

/// Join each mount's conflicted-entry report into the `mounts` array.
///
/// # Why here, and why nowhere else
///
/// A LiveSync entry can have sibling revisions that CouchDB has not reconciled — two
/// devices edited the same note offline. Reads serve the winning revision, which is
/// correct but hides the fact that a losing edit exists. Somewhere has to say so.
///
/// It goes in `vault_info.mounts[]` and not in a read payload for two reasons. The read
/// payloads (`read_file`, `list_children`, `resources/read`) are frozen by the
/// single-mount goldens, and widening one would change bytes a client already depends
/// on. And `mounts[]` is additive and multi-mount-only by construction, which is the
/// same pattern the capability and index detail already follow — a couchdb mount cannot
/// exist in a single-mount vault, so nothing a golden describes can gain a field.
///
/// It is best-effort: a mount whose remote is unreachable contributes NO field rather
/// than an error or a zero. Reporting `conflictedCount: 0` for a vault nobody could
/// reach would be a lie, and failing `vault_info` because of it would break the one
/// tool a user runs to find out what is wrong.
///
/// The conflicted flag rides on manifest entries that a listing already collects, so on
/// a warm mount this costs no extra round trip.
async fn insert_mount_conflicts(payload: &mut Value, router: &deep_obsidian_backend::VaultRouter) {
    let mut reports: HashMap<String, (usize, Vec<String>)> = HashMap::new();
    for mount in router.mounts() {
        match mount.backend.conflicted_paths().await {
            // The backend's storage has no sibling-version notion, so there is nothing
            // to report and no field to add. See `VaultBackend::conflicted_paths` for
            // why that is distinct from "zero conflicts".
            Ok(None) => {}
            Ok(Some(paths)) => {
                let total = paths.len();
                let logical = paths
                    .iter()
                    .take(MAX_REPORTED_CONFLICTED_PATHS)
                    // Rendered in the LOGICAL namespace, because that is the only
                    // namespace a client can act on.
                    .map(|path| mount.to_logical(path))
                    .collect();
                if total > 0 {
                    tracing::warn!(
                        "mount '{}' has {total} entr{} with unreconciled conflict revisions; \
                         reads serve the winning revision and the losing edits are not visible \
                         in it",
                        mount.id,
                        if total == 1 { "y" } else { "ies" }
                    );
                }
                reports.insert(mount.id.clone(), (total, logical));
            }
            Err(error) => {
                tracing::debug!(
                    "could not collect conflicted paths for mount '{}': {error}",
                    mount.id
                );
            }
        }
    }
    if reports.is_empty() {
        return;
    }

    let Some(mounts) = payload
        .as_object_mut()
        .and_then(|object| object.get_mut("mounts"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for entry in mounts.iter_mut() {
        let Some(object) = entry.as_object_mut() else {
            continue;
        };
        let Some((total, paths)) = object
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| reports.get(id))
        else {
            continue;
        };
        object.insert("conflictedCount".to_string(), json!(total));
        // Named only when there ARE any, so a healthy mount does not carry an empty
        // array a reader has to interpret.
        if *total > 0 {
            object.insert("conflictedPaths".to_string(), json!(paths));
        }
    }
}

/// Read a note's text through the backend.
async fn backend_read_text(state: &AppState, path: &str) -> Result<String, String> {
    backend_call(state, BackendRequest::read_text(path))
        .await?
        .into_text()
        .map_err(|error| error.to_string())
}

/// What a pre-write read of the destination found.
struct PriorNote {
    /// The existing content, when there is an existing note. `None` keeps the frozen
    /// meaning "treat this as a create".
    existing: Option<String>,
    /// The precondition the write must carry. See [`BaseVersion`].
    base_version: BaseVersion,
}

/// Read a note in order to overwrite it, keeping the version the read observed.
///
/// # Why the error is classified instead of discarded
///
/// The write tools have always treated a failed read as "there is no note here", via
/// `.ok()`. On a filesystem vault that is nearly always right: the read fails because
/// the file is absent. On a versioned remote it is dangerous, because the read can
/// also fail because the remote is unreachable or an entry cannot be decrypted — and
/// turning THAT into "nothing is here" would make the write a create-only one, which
/// the storage would then refuse with a conflict against a note that was there all
/// along. The user would be shown a conflict for what is actually an outage.
///
/// So only a genuine "destination absent" (`io_kind() == NotFound`, the same
/// discriminator the upload commit path uses) becomes [`BaseVersion::Absent`].
/// Any other failure still reports "no existing note" — preserving the frozen
/// create-on-read-failure behaviour — but with [`BaseVersion::Unobserved`], so the
/// backend is told the path was never reliably observed and must not conclude it is
/// free.
async fn backend_read_note_for_write(state: &AppState, path: &str) -> PriorNote {
    match state.router.execute(BackendRequest::read_text(path)).await {
        Ok(response) => match response.into_versioned_text() {
            Ok((text, version)) => PriorNote {
                existing: Some(text),
                base_version: BaseVersion::from_read(version),
            },
            // A response-family mismatch is a backend bug, not an absent note.
            Err(_) => PriorNote {
                existing: None,
                base_version: BaseVersion::Unobserved,
            },
        },
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => PriorNote {
            existing: None,
            base_version: BaseVersion::Absent,
        },
        Err(error) => {
            // Not "there is no note": something went wrong reading one. The frozen
            // behaviour is still to proceed as a create, but the write must NOT claim
            // the path was observed to be free — see the doc comment.
            tracing::debug!(
                "read of {path} before a write failed for a reason other than absence ({error}); \
                 proceeding as a create, with no observed precondition"
            );
            PriorNote {
                existing: None,
                base_version: BaseVersion::Unobserved,
            }
        }
    }
}

/// Run hybrid search off the async runtime, surfacing the degradation signal. When the
/// embedding backend is unavailable the core function degrades to BM25-only internally and
/// returns `degraded = true` (never an Err for that case), so the tool layer can set a
/// non-fatal `degraded` flag rather than string-matching a leaked upstream error.
async fn hybrid_search_matches(
    index: std::sync::Arc<deep_obsidian_index::index::SearchIndex>,
    query: String,
    options: RankingOptions,
) -> Result<index_search::HybridSearchOutcome, String> {
    tokio::task::spawn_blocking(move || {
        index_search::hybrid_search_with_options_degradable(index.as_ref(), &query, options)
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
            let snapshot = state.runtime().fresh_snapshot("vault_info").await?;
            let mut payload = build_vault_overview_payload(config, &snapshot);
            // Non-fatal live health probe for the note embedding backend. vault_info must
            // never error when the backend is down — it reports the status as a field.
            // The probe is a bounded blocking HTTP call, so run it off the async runtime.
            let probe_index = snapshot.index.clone();
            let health = tokio::task::spawn_blocking(move || {
                index_search::probe_embedding_backend(probe_index.as_ref())
            })
            .await
            .map_err(|error| error.to_string())?;
            // Additive and multi-mount ONLY. A single-mount config (legacy
            // `vaultPath` or one explicit root mount) must render byte-identically
            // to the frozen golden, so the fields are absent rather than
            // one-element arrays. `build_vault_overview_payload` is deliberately NOT
            // the place for this: it also feeds the `obsidian://vault/info` resource
            // and both health payloads, which add the same detail themselves.
            //
            // The counts above describe the ROOT mount's index; this makes them
            // cover the whole logical vault and adds each mount's own numbers.
            insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
            insert_mount_capabilities(&mut payload, state.router.as_ref());
            insert_mount_conflicts(&mut payload, state.router.as_ref()).await;
            if let Some(object) = payload.as_object_mut() {
                match health {
                    // Sparse backend: nothing to probe, so omit the status field entirely.
                    index_search::EmbeddingBackendHealth::NotApplicable => {}
                    index_search::EmbeddingBackendHealth::Reachable => {
                        object.insert("embeddingBackendStatus".to_string(), json!("reachable"));
                    }
                    index_search::EmbeddingBackendHealth::Unreachable(reason) => {
                        object.insert("embeddingBackendStatus".to_string(), json!("unreachable"));
                        object.insert("embeddingBackendError".to_string(), json!(reason));
                    }
                }
            }
            Ok(json_text_result(payload))
        }
        "list_children" => {
            let path = optional_string_arg(arguments, "path");
            let folders_only = bool_arg(arguments, "foldersOnly", false);
            let include_hidden = bool_arg(arguments, "includeHidden", false);
            let include_ignored = bool_arg(arguments, "includeIgnored", false);
            let entries = backend_call(
                state,
                BackendRequest::list_children(path.clone(), include_hidden, include_ignored),
            )
            .await?
            .into_children()
            .map_err(|error| error.to_string())?;
            if folders_only {
                let folders = entries
                    .into_iter()
                    .filter(|entry| matches!(entry.kind, VaultEntryKind::Directory))
                    .map(|entry| entry.path)
                    .collect::<Vec<_>>();
                Ok(json_text_result(json!({
                    "path": path,
                    "foldersOnly": true,
                    "count": folders.len(),
                    "folders": folders
                })))
            } else {
                Ok(json_text_result(json!({
                    "path": path,
                    "foldersOnly": false,
                    "count": entries.len(),
                    "children": entries.into_iter().map(|entry| vault_child_entry_json(&entry)).collect::<Vec<_>>()
                })))
            }
        }
        "read_file" => {
            let path = string_arg(arguments, "path")?;
            validate_format_arg(arguments)?;
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let text = backend_read_text(state, &path).await?;
            // Full-file content hash, computed with the same helper the write tools use so a
            // write's `newHash` can be fed straight back into a read's `knownHash`. Always the
            // full-file hash regardless of any startLine/endLine slice.
            let hash = content_hash(text.as_bytes());
            let known_hash = optional_string_arg(arguments, "knownHash");
            if known_hash.as_deref() == Some(hash.as_str()) {
                let result = Map::from_iter([
                    ("path".to_string(), json!(path.clone())),
                    ("hash".to_string(), json!(hash)),
                    ("unchanged".to_string(), json!(true)),
                ]);
                return Ok(json_text_result_from_arguments(
                    arguments,
                    Value::Object(result),
                ));
            }
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
                    &text,
                    start_line.unwrap_or(1),
                    end_line.or(start_line).unwrap_or(1),
                )
            } else {
                text
            };
            let line_count = text.split('\n').count();
            let mut result = Map::from_iter([
                ("path".to_string(), json!(path.clone())),
                ("resourceUri".to_string(), json!(note_uri(&path))),
                ("hash".to_string(), json!(hash)),
                ("unchanged".to_string(), json!(false)),
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
            // Stat and byte reads keep the BARE IO error wording this tool has always
            // reported (no path prefix, no remediation) -- see `BackendError`.
            let size_bytes = backend_call(state, BackendRequest::stat(&path))
                .await?
                .into_size_bytes()
                .map_err(|error| error.to_string())?;
            let include_base64 = bool_arg(arguments, "includeBase64", false);
            let max_bytes = clamped_usize_arg(arguments, "maxBytes", 0, 0, 1_048_576);
            let bytes = if include_base64 || max_bytes > 0 {
                backend_call(state, BackendRequest::read_bytes(&path))
                    .await?
                    .into_bytes()
                    .map_err(|error| error.to_string())?
            } else {
                Vec::new()
            };
            let mut result = Map::from_iter([
                ("path".to_string(), json!(path.clone())),
                ("resourceUri".to_string(), json!(artifact_uri(&path))),
                ("kind".to_string(), json!(kind)),
                ("mimeType".to_string(), json!(mime_type)),
                ("size".to_string(), json!(size_bytes)),
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
        "find_files" => {
            require_single_mount(state, "find_files")?;
            let query = string_arg(arguments, "query")?;
            let mode = optional_enum_string_arg(arguments, "mode", &["substring", "regex"])?
                .unwrap_or_else(|| "substring".to_string());
            let limit = clamped_usize_arg(arguments, "limit", 20, 1, 200);
            let files = backend_call(state, BackendRequest::walk_markdown())
                .await?
                .into_markdown_files()
                .map_err(|error| error.to_string())?;
            let matches = live_find_file_matches(files, &query, &mode, limit)?
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
            if !state.rg_available {
                return Err(RIPGREP_UNAVAILABLE_MESSAGE.to_string());
            }
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let regex_mode = bool_arg(arguments, "regex", false);
            let case_sensitive = bool_arg(arguments, "caseSensitive", false);
            let glob = optional_string_arg(arguments, "glob");
            let context_lines = clamped_usize_arg(arguments, "contextLines", 0, 0, 20);
            let limit = clamped_usize_arg(arguments, "limit", 50, 1, 500);
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let matches = backend_call(
                state,
                BackendRequest::Recall(RecallRequest::Grep {
                    query: query.clone(),
                    regex: regex_mode,
                    case_sensitive,
                    glob: glob.clone(),
                    context_lines,
                    limit,
                }),
            )
            .await?
            .into_grep_matches()
            .map_err(|error| error.to_string())?
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
            let text = backend_read_text(state, &path).await?;
            Ok(json_text_result_from_arguments(
                arguments,
                outline_payload(&path, &text, text_options),
            ))
        }
        "build_index" => {
            // Every mount, sequentially. `build_index` is the one recall-adjacent tool
            // that needs no scope: rebuilding is exhaustive by nature, so doing all
            // mounts is the complete answer rather than a merge of partial ones.
            let root_snapshot = state.runtime().rebuild("manual build_index").await?;
            let mut mount_results = Vec::new();
            let mut failures = Vec::new();
            for entry in state.runtimes.entries() {
                if entry.is_root() {
                    mount_results.push(build_index_mount_json(
                        &entry.id,
                        &entry.mount_at,
                        Ok(&root_snapshot),
                    ));
                    continue;
                }
                match entry.runtime.rebuild("manual build_index").await {
                    Ok(snapshot) => mount_results.push(build_index_mount_json(
                        &entry.id,
                        &entry.mount_at,
                        Ok(&snapshot),
                    )),
                    Err(error) => {
                        failures.push(format!("'{}': {error}", entry.id));
                        mount_results.push(build_index_mount_json(
                            &entry.id,
                            &entry.mount_at,
                            Err(&error),
                        ));
                    }
                }
            }
            // A partial rebuild reported as success would be a wrong answer: the
            // mounts that DID rebuild keep their fresh index, and the failure names
            // every mount that did not.
            if !failures.is_empty() {
                return Err(format!(
                    "build_index rebuilt every other mount but failed for {}",
                    failures.join("; ")
                ));
            }
            let snapshot = &root_snapshot;
            let mut result = Map::new();
            result.insert("rebuilt".to_string(), json!(true));
            result.insert(
                "generatedAt".to_string(),
                json!(snapshot.index.generated_at),
            );
            // Aggregate across mounts. Identical to the root's own numbers for a
            // single-mount config, where the loop above ran exactly once.
            result.insert(
                "noteCount".to_string(),
                json!(sum_mount_field(&mount_results, "noteCount")),
            );
            result.insert(
                "chunkCount".to_string(),
                json!(sum_mount_field(&mount_results, "chunkCount")),
            );
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
            // Additive and multi-mount only, so the single-mount payload is
            // byte-identical to the frozen shape.
            if state.runtimes.is_multi_mount() {
                result.insert("mounts".to_string(), json!(mount_results));
            }
            Ok(json_text_result(Value::Object(result)))
        }
        "hybrid_search" => {
            let scoped = resolve_recall_scope(
                state,
                "hybrid_search",
                optional_string_arg(arguments, "scope").as_deref(),
            )?;
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            // RRF per-list weights; default to 1.0 (unweighted, scale-free fusion).
            let semantic_weight = clamped_f64_arg(arguments, "semanticWeight", 1.0, 0.0, 1.0);
            let bm25_weight = clamped_f64_arg(arguments, "bm25Weight", 1.0, 0.0, 1.0);
            let text_options = TextPayloadOptions::search_snippet_from_arguments(arguments, true);
            let snapshot = scoped.runtime.fresh_snapshot("hybrid_search").await?;
            let index = snapshot.index;
            let outcome = hybrid_search_matches(
                index.clone(),
                query.clone(),
                RankingOptions {
                    limit,
                    semantic_weight,
                    bm25_weight,
                },
            )
            .await?;
            let degraded = outcome.degraded;
            let degradation_reason = outcome.degradation_reason.clone();
            let count = outcome.matches.len();
            let mut match_values = outcome
                .matches
                .into_iter()
                .map(|item| {
                    let mut value = hybrid_search_match_json(&item, text_options);
                    // Index paths are mount-relative; clients only know logical ones.
                    // A no-op for the root mount.
                    scoped.relabel_path(&mut value, "path");
                    value
                })
                .collect::<Vec<_>>();
            let response_truncated =
                apply_response_text_budget(&mut match_values, "text", RESPONSE_TEXT_BUDGET_CHARS);
            let mut result = Map::new();
            result.insert("query".to_string(), json!(query));
            result.insert("rebuilt".to_string(), json!(snapshot.rebuilt));
            result.insert(
                "semanticBackend".to_string(),
                json!(index.semantic_backend.as_str()),
            );
            result.insert("semanticWeight".to_string(), json!(semantic_weight));
            result.insert("bm25Weight".to_string(), json!(bm25_weight));
            // Non-fatal degradation flag: when the embedding backend was unavailable the
            // matches above are BM25-only lexical results. `degraded` is always present
            // (false on the healthy path) so callers can branch without probing.
            result.insert("degraded".to_string(), json!(degraded));
            if let Some(reason) = degradation_reason {
                result.insert("degradationReason".to_string(), json!(reason));
            }
            result.insert("count".to_string(), json!(count));
            result.insert("matches".to_string(), json!(match_values));
            insert_response_truncation_flags(&mut result, response_truncated);
            Ok(json_text_result_from_arguments(
                arguments,
                Value::Object(result),
            ))
        }
        "search_artifacts" => {
            let scoped = resolve_recall_scope(
                state,
                "search_artifacts",
                optional_string_arg(arguments, "scope").as_deref(),
            )?;
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            let snapshot = scoped.runtime.fresh_snapshot("search_artifacts").await?;
            let index = snapshot.index;
            let query_for_search = query.clone();
            // artifact_semantic_search embeds the query via the (multimodal) artifact
            // backend over HTTP, so run it off the async runtime.
            let matches = tokio::task::spawn_blocking(move || {
                index_search::artifact_semantic_search_with_options(
                    index.as_ref(),
                    &query_for_search,
                    RankingOptions {
                        limit,
                        ..RankingOptions::default()
                    },
                )
            })
            .await
            .map_err(|error| error.to_string())?
            // search_artifacts has no lexical fallback (artifacts carry no BM25 terms), so a
            // dead artifact backend can only surface as an error. Map the backend-unavailable
            // case to a clear, actionable message instead of leaking the raw upstream 400.
            .map_err(|error| match error {
                IndexError::EmbeddingBackendUnavailable(_) => {
                    ARTIFACT_EMBEDDING_BACKEND_UNAVAILABLE_MESSAGE.to_string()
                }
                other => other.to_string(),
            })?;
            Ok(json_text_result_from_arguments(
                arguments,
                json!({
                    "query": query,
                    "rebuilt": snapshot.rebuilt,
                    "count": matches.len(),
                    "matches": matches
                        .into_iter()
                        .map(|item| json!({
                            // Logical path: identity on the root mount.
                            "path": scoped.to_logical(&item.path),
                            "title": item.title,
                            "kind": item.kind,
                            "mimeType": item.mime_type,
                            "size": item.size,
                            "score": item.score,
                            "metadata": serde_json::from_str::<Value>(&item.metadata_json)
                                .unwrap_or(Value::Null),
                        }))
                        .collect::<Vec<_>>()
                }),
            ))
        }
        "related_notes" => {
            let path = string_arg(arguments, "path")?;
            // Served by whichever mount owns the path, from that mount's index.
            let (scoped, mount_path) = resolve_recall_path(state, "related_notes", &path)?;
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            let snapshot = scoped.runtime.fresh_snapshot("related_notes").await?;
            let index = snapshot.index;
            let matches = index_search::related_notes_with_options(
                &index,
                &mount_path,
                RelatedNoteOptions { limit },
            )
            .map_err(|error| error.to_string())?;
            Ok(json_text_result(json!({
                // Echoed as the caller spelled it, in the logical namespace.
                "path": path,
                "rebuilt": snapshot.rebuilt,
                "semanticBackend": index.semantic_backend.as_str(),
                "count": matches.len(),
                "matches": matches.into_iter().map(|item| note_result_json(scoped.to_logical(&item.path), item.title, |object| {
                    object.insert("score".to_string(), json!(item.score));
                    object.insert("sharedLinks".to_string(), json!(item.shared_links));
                })).collect::<Vec<_>>()
            })))
        }
        "graph_traverse" => {
            let path = string_arg(arguments, "path")?;
            let (scoped, mount_path) = resolve_recall_path(state, "graph_traverse", &path)?;
            let direction = optional_enum_string_arg(
                arguments,
                "direction",
                &["incoming", "outgoing", "both"],
            )?
            .unwrap_or_else(|| "both".to_string());
            let depth = clamped_usize_arg(arguments, "depth", 1, 1, 6);
            let limit = clamped_usize_arg(arguments, "limit", 100, 1, 500);
            let snapshot = scoped.runtime.fresh_snapshot("graph_traverse").await?;
            let index = snapshot.index;
            let graph_direction = match direction.as_str() {
                "incoming" => index_graph::GraphDirection::Incoming,
                "outgoing" => index_graph::GraphDirection::Outgoing,
                _ => index_graph::GraphDirection::Both,
            };
            let graph =
                index_graph::graph_traverse(&index, &mount_path, graph_direction, depth, limit)
                    .map_err(|error| error.to_string())?;
            Ok(json_text_result(json!({
                "path": path,
                "rebuilt": snapshot.rebuilt,
                "direction": direction,
                "depth": depth,
                "nodeCount": graph.nodes.len(),
                "edgeCount": graph.edges.len(),
                "nodes": graph.nodes.into_iter().map(|node| note_result_json(scoped.to_logical(&node.path), node.title, |object| {
                    object.insert("depth".to_string(), json!(node.depth));
                })).collect::<Vec<_>>(),
                // Node paths are addresses and are translated; `rawLink` is the wiki
                // link's literal source text, not a path, so it is left alone.
                "edges": graph.edges.into_iter().map(|edge| json!({
                    "source": scoped.to_logical(&edge.source),
                    "target": scoped.to_logical(&edge.target),
                    "rawLink": edge.raw_link
                })).collect::<Vec<_>>()
            })))
        }
        "load_knowledge" => {
            let scoped = resolve_recall_scope(
                state,
                "load_knowledge",
                optional_string_arg(arguments, "scope").as_deref(),
            )?;
            let subject = string_arg(arguments, "subject")?;
            validate_format_arg(arguments)?;
            let project = optional_string_arg(arguments, "project");
            let limit_notes = clamped_usize_arg(arguments, "limitNotes", 6, 1, 12);
            let limit_chunks = clamped_usize_arg(arguments, "limitChunks", 8, 1, 16);
            let include_graph = bool_arg(arguments, "includeGraph", true);
            let graph_depth = clamped_usize_arg(arguments, "graphDepth", 1, 1, 3);
            let text_options = TextPayloadOptions::search_snippet_from_arguments(arguments, true);
            let snapshot = scoped.runtime.fresh_snapshot("load_knowledge").await?;
            let index = snapshot.index;
            let query = [Some(subject.clone()), project.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let chunk_outcome = hybrid_search_matches(
                index.clone(),
                query.clone(),
                RankingOptions {
                    limit: limit_chunks,
                    // Unweighted RRF fusion (the scale-free default).
                    ..RankingOptions::default()
                },
            )
            .await?;
            let degraded = chunk_outcome.degraded;
            let degradation_reason = chunk_outcome.degradation_reason.clone();

            // `chunk_paths` stay MOUNT-relative: they are the addresses fed back into
            // this mount's index below (related-note seeds, the graph root). Only the
            // rendered payload is translated to logical paths, by `relabel_path`.
            // Both are the identity on the root mount.
            let mut chunk_paths = Vec::new();
            let mut chunks = Vec::new();
            for chunk in chunk_outcome.matches {
                if !chunk_paths.iter().any(|existing| existing == &chunk.path) {
                    chunk_paths.push(chunk.path.clone());
                }
                let mut chunk_value = hybrid_search_match_json(&chunk, text_options);
                if let Some(chunk_object) = chunk_value.as_object_mut() {
                    chunk_object.insert("wikiLink".to_string(), json!(note_wiki_link(&chunk.path)));
                }
                scoped.relabel_path(&mut chunk_value, "path");
                chunks.push(chunk_value);
            }
            let response_truncated =
                apply_response_text_budget(&mut chunks, "text", RESPONSE_TEXT_BUDGET_CHARS);

            let mut note_bucket = HashMap::<String, KnowledgeNote>::new();
            // `chunks` is in hybrid-sorted (best-first) order. The hybrid score is now a
            // raw Reciprocal Rank Fusion value (~1/k, tiny and scale-free), which is NOT
            // comparable to the cosine similarity returned by `related_notes` below. To
            // keep both signals on one comparable [0,1] scale for the merge/sort, derive
            // each chunk match's knowledge score from its hybrid RANK (`1/rank`): top
            // chunk = 1.0, then 0.5, 0.33, ... This preserves the original intent that a
            // direct chunk match outranks a discounted (`* 0.85`) related-of note.
            for (position, chunk) in chunks.iter().enumerate() {
                if let Some(path) = chunk.get("path").and_then(Value::as_str) {
                    let title = chunk
                        .get("title")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| note_name(path));
                    let rank_score = 1.0 / (position as f64 + 1.0);
                    merge_knowledge_note(
                        &mut note_bucket,
                        KnowledgeNote {
                            path: path.to_string(),
                            title,
                            wiki_link: note_wiki_link(path),
                            score: rank_score,
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
                        let logical = scoped.to_logical(&note.path);
                        merge_knowledge_note(
                            &mut note_bucket,
                            KnowledgeNote {
                                wiki_link: note_wiki_link(&logical),
                                path: logical,
                                title: note.title.clone(),
                                score: note.score * 0.85,
                                // The reason is shown to a caller, so it names the
                                // seed by its logical path too.
                                reasons: vec![format!(
                                    "related to {}",
                                    scoped.to_logical(seed_path)
                                )],
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
                    "nodes": graph_payload.nodes.into_iter().map(|node| note_result_json(scoped.to_logical(&node.path), node.title, |object| {
                        object.insert("depth".to_string(), json!(node.depth));
                    })).collect::<Vec<_>>(),
                    "edges": graph_payload.edges.into_iter().map(|edge| json!({
                        "source": scoped.to_logical(&edge.source),
                        "target": scoped.to_logical(&edge.target),
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
            // Non-fatal degradation flag: when the embedding backend was unavailable the
            // chunk retrieval fell back to BM25-only (related-note/graph context still
            // applies). Always present (false on the healthy path).
            result.insert("degraded".to_string(), json!(degraded));
            if let Some(reason) = degradation_reason {
                result.insert("degradationReason".to_string(), json!(reason));
            }
            result.insert("notes".to_string(), json!(notes));
            result.insert("chunks".to_string(), json!(chunks));
            result.insert("graph".to_string(), graph);
            insert_response_truncation_flags(&mut result, response_truncated);
            Ok(json_text_result_from_arguments(
                arguments,
                Value::Object(result),
            ))
        }
        "recommend_folder" => {
            require_single_mount(state, "recommend_folder")?;
            let topic = string_arg(arguments, "topic")?;
            let project = optional_string_arg(arguments, "project");
            let folders = backend_call(state, BackendRequest::top_level_folders())
                .await?
                .into_folders()
                .map_err(|error| error.to_string())?;
            if folders.is_empty() {
                return Ok(json_text_result(json!({
                    "folder": "Knowledge Capture",
                    "reason": "no visible top-level folders found",
                    "scores": []
                })));
            }
            let snapshot = state.runtime().fresh_snapshot("recommend_folder").await?;
            let index = snapshot.index;
            let query = [Some(topic.clone()), project.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            // recommend_folder does not report degradation; it just needs candidate paths,
            // and a BM25-only fallback is fine here, so discard the degradation signal.
            let matches = hybrid_search_matches(
                index.clone(),
                query.clone(),
                RankingOptions {
                    limit: 24,
                    // Unweighted RRF fusion (the scale-free default).
                    ..RankingOptions::default()
                },
            )
            .await?
            .matches;
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
            let (content, compose_warning) = compose_explicit_note_content(arguments)?;
            let preserve_manual_notes = bool_arg(arguments, "preserveManualNotes", false);
            // A read failure still means "no existing note" here, exactly as before,
            // so a first write creates. What is new is that the read also yields the
            // precondition the write will carry; see `backend_read_note_for_write`.
            let prior = backend_read_note_for_write(state, &path).await;
            let existing = prior.existing;
            let previous_hash = existing
                .as_deref()
                .map(|existing| content_hash(existing.as_bytes()));
            validate_expected_hash(expected_hash.as_deref(), previous_hash.as_deref(), &path)?;
            let final_content = existing
                .as_deref()
                .map(|existing| merge_with_manual_notes(&content, existing, preserve_manual_notes))
                .unwrap_or_else(|| finalize_written_content(&content));
            let new_hash = content_hash(final_content.as_bytes());
            let created = existing.is_none();
            // The dry run returns above without ever reaching the write, so no
            // backend — and therefore no remote — is touched by one.
            if !dry_run {
                backend_call(
                    state,
                    BackendRequest::write_text_guarded(&path, &final_content, prior.base_version),
                )
                .await?;
            }
            let title = note_title_from_content(&path, &final_content);
            let mut payload = json!({
                "action": if existing.is_some() { "updated" } else { "created" },
                "path": path,
                "title": title,
                "resourceUri": note_uri(&path),
                "wikiLink": note_alias_wiki_link(&path, &title),
                "created": created,
                "dryRun": dry_run,
                "previousHash": previous_hash,
                "newHash": new_hash
            });
            if let Some(warning) = compose_warning {
                payload["warning"] = json!(warning);
            }
            Ok(json_text_result(payload))
        }
        "update_note_section" => {
            let path = string_arg(arguments, "path")?;
            let target =
                optional_string_arg(arguments, "target").unwrap_or_else(|| "heading".to_string());
            let replacement = string_arg(arguments, "content")?;
            let dry_run = bool_arg(arguments, "dryRun", false);
            let expected_hash = expected_hash_arg(arguments);
            // `update_note_section` REQUIRES an existing note (there is no section to
            // update otherwise), so unlike the upserts this read propagates failures.
            let (existing, base_version) = backend_call(state, BackendRequest::read_text(&path))
                .await?
                .into_versioned_text()
                .map(|(text, version)| (text, BaseVersion::from_read(version)))
                .map_err(|error| error.to_string())?;
            let previous_hash = content_hash(existing.as_bytes());
            validate_expected_hash(expected_hash.as_deref(), Some(&previous_hash), &path)?;
            let (final_content, action, level, heading) = match target.as_str() {
                "preamble" => (
                    replace_note_preamble(&existing, &replacement),
                    "updated".to_string(),
                    None,
                    None,
                ),
                "heading" => {
                    let heading = optional_string_arg(arguments, "heading").ok_or_else(|| {
                        "update_note_section requires 'heading' (the exact heading title) when target is 'heading' (the default). To edit the note preamble instead, set target to 'preamble'.".to_string()
                    })?;
                    let level = clamped_usize_arg(arguments, "level", 2, 1, 6);
                    let create_if_missing = bool_arg(arguments, "createIfMissing", true);
                    let (updated, action, actual_level) = update_or_create_note_section(
                        &existing,
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
                backend_call(
                    state,
                    BackendRequest::write_text_guarded(&path, &final_content, base_version),
                )
                .await?;
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
        "request_vault_upload" => {
            let path = string_arg(arguments, "path")?;
            let expected_hash = expected_hash_arg(arguments);
            let mime_type = optional_string_arg(arguments, "mimeType");
            // Reject traversal NOW, at mint, before issuing any capability.
            backend_call(state, BackendRequest::resolve_path(&path)).await?;
            // Enforce the vault's protected-path policy: never let an
            // upload land inside Template(s)/ folders. Checked at mint so the
            // capability is never even issued for a protected destination.
            if is_protected_write_path(&path) {
                return Err(format!("protected write path: {}", path));
            }
            let Some(base) = state.upload_base.as_ref() else {
                return Err("request_vault_upload requires the HTTP service transport".to_string());
            };
            // Best-effort cleanup of staging files orphaned by a crashed upload. The
            // backend owns the staging mechanics, so it owns the sweep; failures are
            // ignored exactly as before.
            // Through the router: the sweep fans out to every mount, since a
            // crashed upload could have staged bytes in any of them.
            let _ = state
                .router
                .execute(BackendRequest::sweep_orphan_staging_files())
                .await;
            let expires_at = std::time::SystemTime::now() + crate::uploads::TOKEN_TTL;
            let token = state.uploads.mint(crate::uploads::PendingUpload {
                dest_path: path.clone(),
                expected_hash: expected_hash.clone(),
                max_bytes: crate::uploads::DEFAULT_MAX_UPLOAD_BYTES,
                expires_at,
                in_flight: false,
            })?;
            let upload_url = format!("{}/upload/{}", base.trim_end_matches('/'), token);
            Ok(json_text_result(json!({
                "uploadUrl": upload_url,
                "expiresAt": crate::uploads::expires_at_epoch(expires_at),
                "maxBytes": crate::uploads::DEFAULT_MAX_UPLOAD_BYTES,
                "path": path,
                "mimeType": mime_type,
                "curlExample": format!("curl -X PUT --data-binary @YOUR_FILE \"{}\"", upload_url),
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
            let prior = backend_read_note_for_write(state, &target_path).await;
            let existing = prior.existing;
            let previous_hash = existing
                .as_deref()
                .map(|existing| content_hash(existing.as_bytes()));
            validate_expected_hash(
                expected_hash.as_deref(),
                previous_hash.as_deref(),
                &target_path,
            )?;
            let final_content =
                finalize_session_note_content(&content, existing.as_deref(), preserve_manual_notes);
            let new_hash = content_hash(final_content.as_bytes());
            let created = existing.is_none();
            if !dry_run {
                backend_call(
                    state,
                    BackendRequest::write_text_guarded(
                        &target_path,
                        &final_content,
                        prior.base_version,
                    ),
                )
                .await?;
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
        _ => Err(format!("unknown tool: {}", name)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        call_tool, clamped_usize_arg, compose_explicit_note_content, content_hash,
        finalize_session_note_content, json_text_result_from_arguments, merge_with_manual_notes,
        optional_enum_string_arg, outline_payload, replace_note_preamble, string_arg,
        tool_definitions, update_or_create_note_section, TextPayloadOptions,
    };
    use crate::mcp::AppState;
    use crate::runtime::MountRuntimes;
    use deep_obsidian_types::{
        AutoReindexConfig, EmbeddingConfig, EmbeddingProvider, HttpConfig, ResolvedServiceConfig,
        StdioMode, TransportMode,
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
            mounts: Vec::new(),
            experimental: Default::default(),
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
            auth: deep_obsidian_types::AuthConfig::default(),
            config_file_path: None,
        }
    }

    async fn test_state(vault_path: PathBuf) -> AppState {
        let config = test_config(vault_path);
        let backends = crate::mounts::MountBackends::build(&config);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("bootstrap runtime");
        AppState::with_backends(config, runtimes, &backends)
    }

    /// An unroutable base URL (loopback, reserved port) that refuses connections
    /// immediately, so a live embed against it fails fast with a connection error.
    const DEAD_BACKEND_URL: &str = "http://127.0.0.1:1";

    /// `build_index` now loops over the mount table and SUMS the per-mount counts
    /// instead of reading the root snapshot's fields directly. This pins that a
    /// single-mount payload is unchanged by that: the loop runs exactly once, the
    /// sums equal the root index's own numbers, and no `mounts` array appears.
    ///
    /// The one payload of this slice that no golden covers, hence the explicit test.
    #[tokio::test]
    async fn build_index_on_a_single_mount_reports_exactly_the_root_index() {
        let vault_path = temp_dir("build-index-single-mount");
        fs::write(
            vault_path.join("Note.md"),
            "# Note\n\nInstall the service and validate the runtime.\n",
        )
        .expect("write note");
        let state = test_state(vault_path).await;
        let result = call_tool(&state, "build_index", &json!({}))
            .await
            .expect("build_index should succeed");
        let payload = &result.structured_content;

        let snapshot = state.runtime().snapshot().expect("snapshot after rebuild");
        assert_eq!(payload["rebuilt"], json!(true));
        assert_eq!(payload["noteCount"], json!(snapshot.index.note_count));
        assert_eq!(payload["chunkCount"], json!(snapshot.index.chunk_count));
        assert_eq!(payload["generatedAt"], json!(snapshot.index.generated_at));
        assert_eq!(
            payload["semanticBackend"],
            json!(snapshot.index.semantic_backend.as_str())
        );
        // Additive multi-mount detail must NOT appear for one mount.
        assert!(payload.get("mounts").is_none());
    }

    /// Healthy-path: `hybrid_search` on a sparse index always reports `degraded:false`
    /// and omits `degradationReason`. The degraded:true path (backend down) is proven
    /// deterministically in the index crate (`hybrid_search_degrades_to_bm25_*`).
    #[tokio::test]
    async fn hybrid_search_reports_not_degraded_on_healthy_backend() {
        let vault_path = temp_dir("hybrid-not-degraded");
        fs::write(
            vault_path.join("Note.md"),
            "# Note\n\nInstall the service and validate the runtime.\n",
        )
        .expect("write note");
        let state = test_state(vault_path).await;
        let result = call_tool(
            &state,
            "hybrid_search",
            &json!({"query": "install runtime"}),
        )
        .await
        .expect("hybrid_search should succeed");
        assert_eq!(result.structured_content["degraded"], json!(false));
        assert!(result.structured_content.get("degradationReason").is_none());
    }

    /// Healthy-path: `load_knowledge` on a sparse index reports `degraded:false`.
    #[tokio::test]
    async fn load_knowledge_reports_not_degraded_on_healthy_backend() {
        let vault_path = temp_dir("load-knowledge-not-degraded");
        fs::write(
            vault_path.join("Note.md"),
            "# Note\n\nInstall the service and validate the runtime.\n",
        )
        .expect("write note");
        let state = test_state(vault_path).await;
        let result = call_tool(
            &state,
            "load_knowledge",
            &json!({"subject": "install runtime"}),
        )
        .await
        .expect("load_knowledge should succeed");
        assert_eq!(result.structured_content["degraded"], json!(false));
        assert!(result.structured_content.get("degradationReason").is_none());
    }

    /// vault_info must NEVER error and, on a sparse index, must not claim an embedding
    /// backend status (NotApplicable → field omitted).
    #[tokio::test]
    async fn vault_info_is_non_fatal_and_omits_backend_status_when_sparse() {
        let vault_path = temp_dir("vault-info-sparse");
        fs::write(vault_path.join("Note.md"), "# Note\n\nbody\n").expect("write note");
        let state = test_state(vault_path).await;
        let result = call_tool(&state, "vault_info", &json!({}))
            .await
            .expect("vault_info must not error");
        assert!(result
            .structured_content
            .get("embeddingBackendStatus")
            .is_none());
    }

    /// `search_artifacts` against an unreachable artifact embedding backend must surface
    /// the clear, actionable message — never the raw upstream connection/400 error.
    #[tokio::test]
    async fn search_artifacts_yields_actionable_message_when_backend_unavailable() {
        let vault_path = temp_dir("search-artifacts-down");
        fs::write(vault_path.join("Note.md"), "# Note\n\nbody\n").expect("write note");
        let mut config = test_config(vault_path);
        // Note backend stays sparse; only the artifact backend is configured (and dead).
        config.artifact_embedding = EmbeddingConfig {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: Some("artifact-embed-test".to_string()),
            base_url: Some(DEAD_BACKEND_URL.to_string()),
            ..EmbeddingConfig::default()
        };
        let backends = crate::mounts::MountBackends::build(&config);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("bootstrap runtime");
        let state = AppState::with_backends(config, runtimes, &backends);

        let error = call_tool(&state, "search_artifacts", &json!({"query": "diagram"}))
            .await
            .expect_err("search_artifacts must error when the artifact backend is down");
        assert_eq!(error, super::ARTIFACT_EMBEDDING_BACKEND_UNAVAILABLE_MESSAGE);
        assert!(
            !error.contains("127.0.0.1") && !error.to_lowercase().contains("connection"),
            "must not leak the raw upstream error, got: {error}"
        );
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
        let (content, warning) = compose_explicit_note_content(&json!({
            "path": "Blog/Test.md",
            "frontmatter": {
                "title": "Hello",
                "tags": ["blog", "test"]
            },
            "title": "Hello",
            "body": "Body text"
        }))
        .expect("content should compose");

        assert!(warning.is_none());
        assert!(content.starts_with("---\n"));
        assert!(content.contains("title: \"Hello\""));
        assert!(content.contains("tags:"));
        assert!(content.contains("- \"blog\""));
        assert!(content.contains("- \"test\""));
        assert!(content.contains("# Hello"));
        assert!(content.ends_with("Body text"));
    }

    #[test]
    fn compose_explicit_note_content_accepts_identical_content_and_body_with_warning() {
        let (content, warning) = compose_explicit_note_content(&json!({
            "path": "Note.md",
            "content": "# Note\n\nSame text",
            "body": "# Note\n\nSame text\n"
        }))
        .expect("identical duplicate should be accepted");

        assert_eq!(content, "# Note\n\nSame text");
        assert!(warning
            .expect("warning should be set")
            .contains("identical"));
    }

    #[test]
    fn compose_explicit_note_content_rejects_diverging_content_and_body() {
        let error = compose_explicit_note_content(&json!({
            "path": "Note.md",
            "content": "one",
            "body": "two"
        }))
        .expect_err("diverging duplicate should fail");
        assert!(error.contains("different text"));
        assert!(error.contains("exactly one"));
    }

    #[test]
    fn compose_explicit_note_content_rejects_content_with_compose_fields() {
        let error = compose_explicit_note_content(&json!({
            "path": "Note.md",
            "content": "text",
            "title": "Title"
        }))
        .expect_err("content plus title should fail");
        assert!(error.contains("not both"));
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

    #[test]
    fn string_arg_missing_messages_describe_well_known_arguments() {
        let empty = json!({});
        let query = string_arg(&empty, "query").expect_err("query should be required");
        assert!(query.contains("missing required argument 'query'"));
        assert!(query.contains("text or pattern to search for"));

        let topic = string_arg(&empty, "topic").expect_err("topic should be required");
        assert!(topic.contains("missing required argument 'topic'"));
        assert!(topic.contains("recommend a folder for"));

        let heading = string_arg(&empty, "heading").expect_err("heading should be required");
        assert!(heading.contains("missing required argument 'heading'"));
        assert!(heading.contains("exact heading title"));

        let path = string_arg(&empty, "path").expect_err("path should be required");
        assert!(path.contains("missing required argument 'path'"));
        assert!(path.contains("vault-relative file path"));

        // Unknown keys still get a clearer-but-generic message.
        let other = string_arg(&empty, "widget").expect_err("widget should be required");
        assert_eq!(other, "missing required argument 'widget'");
    }

    #[test]
    fn update_note_section_schema_declares_conditional_heading_requirement() {
        let definitions = tool_definitions(true, false);
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "update_note_section")
            .expect("update_note_section tool definition");
        let all_of = definition.input_schema["allOf"]
            .as_array()
            .expect("allOf array");
        let conditional = &all_of[0];
        // The `if` matches only when `target` is present and equals "preamble";
        // the `required: ["target"]` guard prevents a vacuous match on absent
        // `target`, so the `else` requires `heading` for the default case too.
        assert_eq!(conditional["if"]["required"], json!(["target"]));
        assert_eq!(
            conditional["if"]["properties"]["target"]["const"],
            json!("preamble")
        );
        assert_eq!(conditional["else"]["required"], json!(["heading"]));
    }

    #[tokio::test]
    async fn update_note_section_requires_heading_for_default_target() {
        let vault_path = temp_dir("update-section-heading");
        fs::write(
            vault_path.join("Note.md"),
            "# Note\n\nPreamble body\n\n## Status\n\nold\n",
        )
        .expect("write note");
        let state = test_state(vault_path.clone()).await;

        // Default target (heading) with no heading -> clear conditional error.
        let missing = call_tool(
            &state,
            "update_note_section",
            &json!({"path": "Note.md", "content": "new"}),
        )
        .await
        .expect_err("missing heading should fail");
        assert!(missing.contains("target is 'heading'"));
        assert!(missing.contains("set target to 'preamble'"));

        // Providing heading works normally.
        let updated = call_tool(
            &state,
            "update_note_section",
            &json!({"path": "Note.md", "heading": "Status", "content": "fresh"}),
        )
        .await
        .expect("heading update should succeed");
        assert_eq!(updated.structured_content["heading"], "Status");

        // target:preamble works without a heading.
        let preamble = call_tool(
            &state,
            "update_note_section",
            &json!({"path": "Note.md", "target": "preamble", "content": "intro"}),
        )
        .await
        .expect("preamble update should succeed");
        assert_eq!(preamble.structured_content["target"], "preamble");
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
    async fn upsert_note_accepts_identical_content_and_body_and_reports_warning() {
        let vault_path = temp_dir("upsert-duplicate-fields");
        let state = test_state(vault_path.clone()).await;

        let result = call_tool(
            &state,
            "upsert_note",
            &json!({
                "path": "Notes/Dup.md",
                "content": "# Dup\n\nSame text",
                "body": "# Dup\n\nSame text"
            }),
        )
        .await
        .expect("identical content+body should succeed");
        assert_eq!(result.structured_content["created"], true);
        assert!(result.structured_content["warning"]
            .as_str()
            .expect("warning should be reported")
            .contains("identical"));
        let file_text = fs::read_to_string(vault_path.join("Notes/Dup.md")).expect("read note");
        assert_eq!(file_text, "# Dup\n\nSame text\n");

        let diverging = call_tool(
            &state,
            "upsert_note",
            &json!({
                "path": "Notes/Dup.md",
                "content": "# Dup\n\nOne",
                "body": "# Dup\n\nTwo"
            }),
        )
        .await
        .expect_err("diverging content+body should fail");
        assert!(diverging.contains("different text"));
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
    async fn read_file_returns_full_file_hash_matching_write_side() {
        let vault_path = temp_dir("read-file-hash");
        let body = "# Title\n\nline one\nline two\n";
        fs::write(vault_path.join("Note.md"), body).expect("write note");
        let state = test_state(vault_path.clone()).await;

        let result = call_tool(&state, "read_file", &json!({"path": "Note.md"}))
            .await
            .expect("read_file should succeed");

        let hash = result.structured_content["hash"]
            .as_str()
            .expect("hash field");
        // Cross-consistency: same hash the write tools produce over the file bytes.
        assert_eq!(hash, content_hash(body.as_bytes()));
        assert_eq!(result.structured_content["unchanged"], false);
        assert!(result.structured_content["text"].as_str().is_some());
    }

    #[tokio::test]
    async fn read_file_known_hash_match_omits_text_and_marks_unchanged() {
        let vault_path = temp_dir("read-file-known-match");
        let body = "# Title\n\nbody text\n";
        fs::write(vault_path.join("Note.md"), body).expect("write note");
        let state = test_state(vault_path.clone()).await;

        let known = content_hash(body.as_bytes());
        let result = call_tool(
            &state,
            "read_file",
            &json!({"path": "Note.md", "knownHash": known}),
        )
        .await
        .expect("read_file should succeed");

        assert_eq!(result.structured_content["unchanged"], true);
        assert_eq!(result.structured_content["hash"], json!(known));
        assert_eq!(result.structured_content["path"], "Note.md");
        assert!(result.structured_content.get("text").is_none());
    }

    #[tokio::test]
    async fn read_file_known_hash_mismatch_returns_full_text() {
        let vault_path = temp_dir("read-file-known-mismatch");
        let body = "# Title\n\nbody text\n";
        fs::write(vault_path.join("Note.md"), body).expect("write note");
        let state = test_state(vault_path.clone()).await;

        let result = call_tool(
            &state,
            "read_file",
            &json!({"path": "Note.md", "knownHash": "fnv1a64:0000000000000000"}),
        )
        .await
        .expect("read_file should succeed");

        assert_eq!(result.structured_content["unchanged"], false);
        assert_eq!(
            result.structured_content["hash"],
            json!(content_hash(body.as_bytes()))
        );
        assert!(result.structured_content["text"].as_str().is_some());
    }

    #[tokio::test]
    async fn read_file_sliced_read_reports_full_file_hash() {
        let vault_path = temp_dir("read-file-slice-hash");
        let body = "line1\nline2\nline3\nline4\n";
        fs::write(vault_path.join("Note.md"), body).expect("write note");
        let state = test_state(vault_path.clone()).await;

        let full = call_tool(&state, "read_file", &json!({"path": "Note.md"}))
            .await
            .expect("full read should succeed");
        let sliced = call_tool(
            &state,
            "read_file",
            &json!({"path": "Note.md", "startLine": 2, "endLine": 3}),
        )
        .await
        .expect("sliced read should succeed");

        assert_eq!(
            sliced.structured_content["hash"],
            full.structured_content["hash"]
        );
        assert_eq!(
            sliced.structured_content["hash"],
            json!(content_hash(body.as_bytes()))
        );
    }

    #[tokio::test]
    async fn request_vault_upload_requires_http_transport_under_stdio() {
        let vault_path = temp_dir("upload-stdio");
        // `test_state` builds an AppState with upload_base = None (stdio default).
        let state = test_state(vault_path).await;
        let error = call_tool(
            &state,
            "request_vault_upload",
            &json!({ "path": "Assets/file.bin" }),
        )
        .await
        .expect_err("stdio mode should reject upload minting");
        assert_eq!(
            error,
            "request_vault_upload requires the HTTP service transport"
        );
    }

    #[tokio::test]
    async fn request_vault_upload_rejects_traversal_at_mint() {
        let vault_path = temp_dir("upload-traversal");
        let state = test_state(vault_path)
            .await
            .with_upload_base("http://127.0.0.1:7777".to_string());
        let error = call_tool(
            &state,
            "request_vault_upload",
            &json!({ "path": "../escape.bin" }),
        )
        .await
        .expect_err("traversal must be rejected at mint");
        assert!(!error.contains("requires the HTTP service transport"));
    }

    #[tokio::test]
    async fn request_vault_upload_rejects_protected_template_path() {
        let vault_path = temp_dir("upload-protected");
        let state = test_state(vault_path)
            .await
            .with_upload_base("http://127.0.0.1:7777".to_string());
        let error = call_tool(
            &state,
            "request_vault_upload",
            &json!({ "path": "Templates/daily.bin" }),
        )
        .await
        .expect_err("protected template path must be rejected at mint");
        assert!(error.contains("protected write path"));
    }

    #[tokio::test]
    async fn request_vault_upload_mints_and_upload_lands_file() {
        use axum::routing::put;
        use axum::Router;

        let vault_path = temp_dir("upload-e2e");
        // Share one AppState (and thus one UploadStore) between the mint tool call
        // and the HTTP upload endpoint.
        let state = test_state(vault_path.clone())
            .await
            .with_upload_base("http://placeholder".to_string());

        // Mint a token via the tool. We patch the base URL after binding.
        let minted = call_tool(
            &state,
            "request_vault_upload",
            &json!({ "path": "Uploads/picture.bin" }),
        )
        .await
        .expect("mint should succeed");
        let upload_url = minted.structured_content["uploadUrl"]
            .as_str()
            .expect("uploadUrl present")
            .to_string();
        let token = upload_url.rsplit('/').next().unwrap().to_string();

        // Stand up the upload route on a real listener.
        let router = Router::new()
            .route("/upload/{token}", put(crate::bootstrap::upload_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let real_url = format!("http://{}/upload/{}", addr, token);
        let client = reqwest::Client::new();
        let response = client
            .put(&real_url)
            .body(b"binary-payload-bytes".to_vec())
            .send()
            .await
            .expect("upload request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.expect("json body");
        assert_eq!(body["action"], "created");
        assert_eq!(body["bytesWritten"], 20);
        assert_eq!(body["path"], "Uploads/picture.bin");

        let written = fs::read(vault_path.join("Uploads/picture.bin")).expect("file landed");
        assert_eq!(written, b"binary-payload-bytes");
        assert_eq!(
            body["hash"].as_str().unwrap(),
            content_hash(b"binary-payload-bytes")
        );

        // Reusing the consumed token is rejected (403).
        let reuse = client
            .put(&real_url)
            .body(b"again".to_vec())
            .send()
            .await
            .expect("reuse request");
        assert_eq!(reuse.status(), reqwest::StatusCode::FORBIDDEN);

        // An unknown token is also rejected (403), no info leak.
        let unknown = client
            .put(format!("http://{}/upload/deadbeef", addr))
            .body(b"x".to_vec())
            .send()
            .await
            .expect("unknown request");
        assert_eq!(unknown.status(), reqwest::StatusCode::FORBIDDEN);

        server.abort();
    }

    #[tokio::test]
    async fn upload_endpoint_accepts_body_larger_than_axum_default_limit() {
        use axum::extract::DefaultBodyLimit;
        use axum::routing::put;
        use axum::Router;

        let vault_path = temp_dir("upload-large");
        let state = test_state(vault_path.clone())
            .await
            .with_upload_base("http://placeholder".to_string());
        let minted = call_tool(
            &state,
            "request_vault_upload",
            &json!({ "path": "Uploads/big.bin" }),
        )
        .await
        .expect("mint should succeed");
        let token = minted.structured_content["uploadUrl"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();

        let router = Router::new()
            .route(
                "/upload/{token}",
                put(crate::bootstrap::upload_handler).layer(DefaultBodyLimit::disable()),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        // 3 MB exceeds axum's 2 MB DefaultBodyLimit; must still land.
        let payload = vec![0x5au8; 3 * 1024 * 1024];
        let client = reqwest::Client::new();
        let response = client
            .put(format!("http://{}/upload/{}", addr, token))
            .body(payload.clone())
            .send()
            .await
            .expect("large upload request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["bytesWritten"], payload.len());
        let written = fs::read(vault_path.join("Uploads/big.bin")).unwrap();
        assert_eq!(written.len(), payload.len());

        server.abort();
    }

    #[test]
    fn tool_list_omits_grep_search_when_ripgrep_unavailable() {
        let available = super::tool_definitions(true, false);
        assert!(
            available.iter().any(|tool| tool.name == "grep_search"),
            "grep_search should be present when ripgrep is available"
        );

        let unavailable = super::tool_definitions(false, false);
        assert!(
            !unavailable.iter().any(|tool| tool.name == "grep_search"),
            "grep_search must be omitted when ripgrep is unavailable"
        );
        // Omission must be surgical: every other tool stays registered.
        assert_eq!(unavailable.len(), available.len() - 1);
    }

    #[test]
    fn consolidated_tool_surface() {
        let definitions = super::tool_definitions(true, false);
        let names: Vec<&str> = definitions.iter().map(|tool| tool.name.as_str()).collect();
        // search_artifacts re-exposes artifact semantic search (dropped with semantic_search's scope).
        assert!(names.contains(&"search_artifacts"));
        // The decommissioned/merged tools must be gone.
        for removed in [
            "write_file_to_vault",
            "bm25_search",
            "semantic_search",
            "list_folders",
            "backlinks",
            "read_chunk",
        ] {
            assert!(
                !names.contains(&removed),
                "{removed} should have been decommissioned/merged"
            );
        }
        // Their replacements remain.
        for kept in [
            "hybrid_search",
            "request_vault_upload",
            "list_children",
            "graph_traverse",
            "read_file",
            "upsert_note",
        ] {
            assert!(names.contains(&kept), "{kept} must still be registered");
        }
    }

    #[tokio::test]
    async fn grep_search_returns_clear_error_when_ripgrep_unavailable() {
        let vault_path = temp_dir("grep-disabled");
        let config = test_config(vault_path.clone());
        let backends = crate::mounts::MountBackends::build(&config);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("bootstrap runtime");
        // Force the unavailable state regardless of the host environment: a backend
        // whose `rg` path does not exist reports no grep capability.
        let backend: std::sync::Arc<dyn deep_obsidian_backend::VaultBackend> =
            std::sync::Arc::new(deep_obsidian_backend::FilesystemVaultBackend::with_ripgrep(
                config.vault_path.clone(),
                vault_path.join("definitely-missing-rg"),
            ));
        let state = AppState {
            router: std::sync::Arc::new(deep_obsidian_backend::VaultRouter::single(
                "vault",
                backend.clone(),
            )),
            backend,
            config: std::sync::Arc::new(config),
            runtimes,
            auth: std::sync::Arc::new(crate::auth::AuthState::disabled()),
            rg_available: false,
            uploads: crate::uploads::UploadStore::new(),
            upload_base: None,
        };

        let error = super::call_tool(&state, "grep_search", &json!({"query": "needle"}))
            .await
            .expect_err("grep_search must fail when ripgrep is unavailable");
        assert!(
            error.contains("ripgrep"),
            "error should mention ripgrep, got: {error}"
        );
        assert!(
            !error.contains("os error 2"),
            "error must not surface the raw spawn error, got: {error}"
        );
        assert_eq!(error, super::RIPGREP_UNAVAILABLE_MESSAGE);
    }

    #[test]
    fn apply_response_text_budget_omits_text_after_budget_exhausted() {
        let mut matches = vec![
            json!({"path": "a.md", "text": "x".repeat(10), "textTruncated": false}),
            json!({"path": "b.md", "text": "y".repeat(10), "textTruncated": false}),
            json!({"path": "c.md", "text": "z".repeat(10), "textTruncated": false}),
        ];
        // Budget of 15: first match (10) fits, second (cumulative 20 > 15) is the
        // crossing match and is kept whole, third is omitted.
        let truncated = super::apply_response_text_budget(&mut matches, "text", 15);
        assert!(truncated);
        assert_eq!(matches[0]["text"], "x".repeat(10));
        assert!(matches[0].get("textOmitted").is_none());
        assert_eq!(matches[1]["text"], "y".repeat(10));
        assert!(matches[1].get("textOmitted").is_none());
        assert!(matches[2].get("text").is_none());
        assert_eq!(matches[2]["textOmitted"], true);
    }

    #[test]
    fn apply_response_text_budget_leaves_small_responses_untouched() {
        let mut matches = vec![
            json!({"path": "a.md", "text": "small"}),
            json!({"path": "b.md", "text": "also small"}),
        ];
        let truncated = super::apply_response_text_budget(
            &mut matches,
            "text",
            super::RESPONSE_TEXT_BUDGET_CHARS,
        );
        assert!(!truncated);
        assert_eq!(matches[0]["text"], "small");
        assert_eq!(matches[1]["text"], "also small");
        assert!(matches[0].get("textOmitted").is_none());
        assert!(matches[1].get("textOmitted").is_none());
    }

    #[test]
    fn search_snippet_options_default_to_snippet_cap_but_respect_explicit() {
        let defaulted = TextPayloadOptions::search_snippet_from_arguments(&json!({}), true);
        assert_eq!(
            defaulted.max_text_chars,
            super::DEFAULT_SEARCH_SNIPPET_CHARS
        );
        assert!(defaulted.include_text);

        let explicit =
            TextPayloadOptions::search_snippet_from_arguments(&json!({"maxTextChars": 5000}), true);
        assert_eq!(explicit.max_text_chars, 5000);

        // Explicit value above the ceiling is clamped to the per-field max.
        let clamped = TextPayloadOptions::search_snippet_from_arguments(
            &json!({"maxTextChars": 999999}),
            true,
        );
        assert_eq!(clamped.max_text_chars, super::DEFAULT_MAX_TEXT_CHARS);
    }

    #[tokio::test]
    async fn hybrid_search_caps_aggregate_text_and_signals_truncation() {
        let vault_path = temp_dir("bm25-budget");
        // A large body so each chunk snippet is sizable. Many notes sharing the
        // query term so the response carries multiple text-bearing matches.
        let body = (0..400)
            .map(|i| format!("needle paragraph line {i} with some filler content"))
            .collect::<Vec<_>>()
            .join("\n");
        for n in 0..6 {
            fs::write(
                vault_path.join(format!("Note{n}.md")),
                format!("# Note {n}\n\n{body}\n"),
            )
            .expect("write note");
        }
        let state = test_state(vault_path).await;

        // Force large per-result snippets so a few matches blow past the budget.
        let result = call_tool(
            &state,
            "hybrid_search",
            &json!({"query": "needle", "limit": 50, "maxTextChars": 20000}),
        )
        .await
        .expect("hybrid_search should succeed");

        let matches = result.structured_content["matches"]
            .as_array()
            .expect("matches array");
        assert!(
            !matches.is_empty(),
            "index must contain matches for the truncation assertion to be meaningful"
        );
        assert_eq!(result.structured_content["responseTruncated"], true);
        assert!(result.structured_content["truncationNote"]
            .as_str()
            .is_some());

        // Cumulative emitted text stays within budget (allowing the single
        // boundary-crossing match), and at least one later match is omitted.
        let total: usize = matches
            .iter()
            .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
            .map(|text| text.chars().count())
            .sum();
        assert!(
            total <= super::RESPONSE_TEXT_BUDGET_CHARS + 20000,
            "emitted text {total} exceeds budget plus one crossing match"
        );
        let omitted = matches
            .iter()
            .filter(|item| {
                item.get("textOmitted").and_then(serde_json::Value::as_bool) == Some(true)
            })
            .count();
        assert!(omitted > 0, "expected at least one omitted match text");
    }

    #[tokio::test]
    async fn small_to_big_expanded_section_is_subject_to_output_cap() {
        // A chunk hit's returned text is expanded to its enclosing section (issue #6 item #4).
        // That grown text must still flow through the per-result snippet cap (item #6): a
        // section far larger than the snippet cap is truncated, not emitted whole.
        let vault_path = temp_dir("small-to-big-cap");
        // One oversized section so the chunker splits it; the distinctive term lives in a
        // LATER paragraph (its own sub-chunk), so the hit expands to the whole big section.
        let filler = "calibration telemetry harmonic resonance throughput diagnostic \
            subsystem actuator manifold turbine compressor lubrication bearing tolerance "
            .repeat(40);
        let content = format!(
            "# Handbook\n\n## Protocol\nThe opening paragraph.\n{filler}\n\n\
             The later paragraph carries the term gizmotron.\n{filler}\n"
        );
        fs::write(vault_path.join("Handbook.md"), content).expect("write note");
        let state = test_state(vault_path).await;

        // Default snippet cap (no maxTextChars override) = DEFAULT_SEARCH_SNIPPET_CHARS.
        let result = call_tool(
            &state,
            "hybrid_search",
            &json!({"query": "gizmotron", "limit": 5}),
        )
        .await
        .expect("hybrid_search should succeed");
        let matches = result.structured_content["matches"]
            .as_array()
            .expect("matches array");
        // Locate by path (the matched term itself may sit past the truncation point).
        let hit = matches
            .iter()
            .find(|item| {
                item.get("path").and_then(serde_json::Value::as_str) == Some("Handbook.md")
            })
            .expect("handbook hit");
        let text = hit["text"].as_str().expect("text field");
        // The expanded Protocol section is ~12KB; the emitted snippet is capped well below it.
        assert!(
            text.chars().count() <= super::DEFAULT_SEARCH_SNIPPET_CHARS,
            "expanded section snippet must be capped, got {} chars",
            text.chars().count()
        );
        // Truncation is signaled on the match (the cap actually fired on the grown text).
        assert_eq!(
            hit.get("textTruncated")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn hybrid_search_small_response_is_not_truncated() {
        let vault_path = temp_dir("bm25-small");
        fs::write(
            vault_path.join("Only.md"),
            "# Only\n\nA short needle note body.\n",
        )
        .expect("write note");
        let state = test_state(vault_path).await;

        let result = call_tool(&state, "hybrid_search", &json!({"query": "needle"}))
            .await
            .expect("hybrid_search should succeed");

        let matches = result.structured_content["matches"]
            .as_array()
            .expect("matches array");
        assert!(!matches.is_empty(), "expected at least one match");
        assert!(result.structured_content.get("responseTruncated").is_none());
        assert!(result.structured_content.get("truncationNote").is_none());
        // Full text present and not omitted for a small response.
        assert!(matches[0]
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some());
        assert!(matches[0].get("textOmitted").is_none());
    }
}
