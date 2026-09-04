use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use deep_obsidian_backend::{
    BackendKind, BackendRequest, BackendResponse, BaseVersion, Capability, GrepContextLine,
    GrepMatch, MutationRequest, MutationResponse, RecallRequest, VaultChildEntry, VaultEntryKind,
    RIPGREP_UNAVAILABLE_MESSAGE,
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

use crate::federation;
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

/// Why a `list_children` payload carries `foldersTruncated: true`.
///
/// The flag alone would leave a caller unable to act. This says which HALF of the listing
/// is short (folders, never files) and that the cause is a hard provider ceiling rather
/// than a setting or a failure — so nobody retries, and nobody concludes the missing
/// folders do not exist.
const FOLDERS_TRUNCATED_REASON: &str = "this listing's SUBFOLDERS may be incomplete: they are \
synthesized from the shared index's folder facets, and facet-value enumeration is capped at 100 \
values by the provider (a hard limit, not a setting). The FILES listed here are complete. Narrow \
the listing by passing a deeper 'path', or use find_files, which walks every note.";

/// Why a `grep_search` payload carries `exhaustive: false`.
///
/// `grep_search` has always meant ripgrep, which reads every file, so a caller reasonably
/// treats an empty result as proof of absence. On a mount where that is not true, saying
/// so is the entire point: this names the mechanism, says what the number next to it
/// means, and points at the tool that IS complete for this mount.
const NON_EXHAUSTIVE_GREP_NOTE: &str = "this line search was NOT exhaustive: the mount serving it \
has no local files to scan, so it ran a lexical prefilter over its index and then applied your \
pattern to the candidates that came back ('candidateCount'). A match in a chunk the index ranked \
below the candidate cap is not reported, so an empty or short result is NOT proof of absence. Use \
hybrid_search for ranked recall over this mount, or find_files to enumerate its notes.";

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

    /// No text at all: for a payload whose hits carry no snippet in the first place.
    ///
    /// `search_artifacts` is the case — an artifact hit is a file's metadata, and the
    /// artifact match renderer has never emitted a `text` field — so passing options that
    /// would include one describes nothing.
    fn without_text() -> Self {
        Self {
            include_text: false,
            max_text_chars: 0,
        }
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

/// Final path segment of a vault-relative note path, extension included.
fn note_basename(note_path: &str) -> &str {
    note_path.rsplit('/').next().unwrap_or(note_path)
}

/// Rewrite one note's links to a moved note, returning how many changed.
///
/// Guarded on the revision just read, so a concurrent edit to a linking note makes this one
/// fail rather than clobbering it. The caller retries the rename and only the notes that
/// failed are touched again.
async fn rewrite_links_in_note(
    state: &AppState,
    note_path: &str,
    from: &str,
    to: &str,
    old_basename_was_unique: bool,
) -> Result<usize, String> {
    let (content, base_version) = backend_call(state, BackendRequest::read_text(note_path))
        .await
        .map_err(|error| error.to_string())?
        .into_versioned_text()
        .map(|(text, version)| (text, BaseVersion::from_read(version)))
        .map_err(|error| error.to_string())?;
    let outcome = crate::links::rewrite_wiki_links(&content, from, to, old_basename_was_unique);
    if outcome.rewritten == 0 {
        return Ok(0);
    }
    backend_call(
        state,
        BackendRequest::write_text_full(note_path, &outcome.content, base_version, false),
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(outcome.rewritten)
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
/// exactly keeps the answer exact. See [`resolve_recall_target`].
fn scope_property() -> Value {
    json!({
        "type": "string",
        "description": "Optional. Which SINGLE mount to search, on a multi-mount vault. Omit it to search every mount and receive one fused ranking (the payload then carries federated:true, a per-mount mounts[] summary, and mountId on each hit). Pass it to search one mount natively, with that backend's own ranking and no fusion: it must name a mount root exactly ('/' for the mount at the vault root; see vault_info.mounts[].mountAt for the rest). Either way results are reported as logical vault paths. Naming a mount does NOT include content grafted under it by another mount."
    })
}

/// Add the `scope` argument to the routable recall tools.
///
/// Applied ONLY for a multi-mount config. A single-mount vault has nothing to
/// choose between, so its `tools/list` is byte-identical to the frozen golden —
/// the same reason `grep_search` is registered conditionally and
/// `vault_info.mounts` is emitted conditionally.
///
/// # `scope` is OPTIONAL, and used to be required
///
/// While an unscoped question could not be answered at all, declaring `scope` required was
/// the honest schema: a client discovering the limitation from the schema beats discovering
/// it from an error. Now that an unscoped call federates every mount, required would be a
/// LIE — it would tell a client the whole-vault search does not exist.
///
/// The frozen `tools_list` golden survives this, and the reason is worth stating because
/// the digest genuinely does freeze `required`
/// (see `tool_list_digest` in `tests/mcp_contract.rs`): the golden is captured for a
/// SINGLE-mount vault, where this function never runs and `scope` is not a property at all.
/// Both the property list and the required list in the golden are therefore untouched by
/// anything here.
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
    }
}

/// The `resolveDivergence` argument, added to `upsert_note` only when a mount can record
/// a divergence.
///
/// # Why it is a claim and not a command
///
/// The server never merges — a wrong automatic merge produces plausible text and is
/// nearly undetectable — so clearing a divergence mark can only ever be the CALLER
/// asserting that the content it is writing already reconciles both sides. The
/// description says exactly that, because a client that reads this as "clear the flag"
/// will pass it on every write and the mark will stop meaning anything.
fn resolve_divergence_property() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "Assert that this content RECONCILES a recorded divergence, clearing the note's hasDivergence mark. Only meaningful on a mount that records divergences (see vault_info.mounts[].capabilities for 'version-history'). Call resolve_divergence first to get the head, the overtaken version and their common ancestor, merge them yourself, then write the merged content with this set — the server never merges. Ignored if this write itself forks off a newer head, because that creates a divergence rather than resolving one."
    })
}

/// Add `resolveDivergence` to `upsert_note`, for a vault where some mount records one.
///
/// # Why `upsert_note` only, when PR #40 also read it on `update_note_section`
///
/// A reconciliation is a whole-note decision: you merge two complete versions against
/// their common ancestor and write the result. `update_note_section` replaces one section
/// and leaves the rest of the note untouched, so a caller asserting "this reconciles the
/// divergence" there would be asserting it about content it did not write. #40 accepted
/// the argument at that call site but never declared it in the schema, so no client could
/// discover it; narrowing it to the one tool where the claim is meaningful — and declaring
/// it there — is the honest version of the same feature.
fn insert_resolve_divergence_argument(definitions: &mut [ToolDefinition]) {
    for definition in definitions
        .iter_mut()
        .filter(|definition| definition.name == "upsert_note")
    {
        if let Some(properties) = definition
            .input_schema
            .as_object_mut()
            .and_then(|schema| schema.get_mut("properties"))
            .and_then(Value::as_object_mut)
        {
            properties.insert(
                "resolveDivergence".to_string(),
                resolve_divergence_property(),
            );
        }
    }
}

/// The version-history and soft-delete tools, registered only for a vault that has a
/// mount able to serve them.
///
/// # Why registration is capability-gated rather than always-on-and-refusing
///
/// The same discipline `grep_search` already follows: "rg works or `grep_search` does not
/// exist". A tool that is advertised and can only ever refuse costs an agent a round trip
/// and a wrong conclusion about the vault, and it costs every reader of `tools/list` the
/// assumption that a listed tool works. So `delete_note` appears only when some mount
/// advertises [`Capability::SoftDelete`], and the three history tools only when some mount
/// advertises [`Capability::VersionHistory`].
///
/// The two capabilities are checked SEPARATELY rather than as one "shared mount" flag,
/// because they genuinely come apart IN BOTH DIRECTIONS: a read-only Algolia mount has a
/// version history and no soft delete, so `delete_note` must not appear for it; a writable
/// couchdb mount has a soft delete and no version history, so `delete_note` must appear
/// while the three history tools must not. The second case is why `delete_note`'s
/// description and its `howToRecover` cannot assume a history exists — see
/// [`NO_HISTORY_RECOVERY`].
///
/// # Why the four tools are not `scope`-routed
///
/// Each takes a `path`, so the mount is determined by longest-prefix match exactly as it
/// is for `read_file` — there is nothing to choose. A `scope` would be a second, redundant
/// way to say the same thing, and a way for the two to disagree.
fn insert_capability_tools(definitions: &mut Vec<ToolDefinition>, capabilities: &CapabilitySet) {
    if capabilities.soft_delete {
        definitions.push(ToolDefinition {
            name: "delete_note".to_string(),
            description: "Soft-delete a note on a mount whose removal is observable and recoverable: it stops appearing in listings and search, and the response's 'howToRecover' says how to get it back on that mount. How recovery works depends on the mount: one that also advertises 'version-history' moves the previous version to history and names it as 'recoverableFrom' for read_version; a CouchDB (LiveSync) mount keeps no history, but the note's last content stays readable at the same path and writing it back with upsert_note resurrects it. Only mounts advertising 'soft-delete' in vault_info.mounts[].capabilities — local vault files are NOT deletable through MCP.".to_string(),
            annotations: Some(tool_annotations(false, Some(false), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![("path", json!({"type":"string","description":"Logical vault-relative note path, on a mount that supports soft delete."}))],
                vec!["path"],
            ),
        });
    }
    if capabilities.rename {
        definitions.push(ToolDefinition {
            name: "rename_note".to_string(),
            description: "Move a note to a new vault-relative path and repoint the wikilinks that referenced it. Refuses rather than guessing in the two cases where a move changes meaning it was not asked to change: a destination that already holds a note (that would destroy it, which is not what renaming means), and a destination whose basename already exists elsewhere in the vault (short `[[Name]]` links in unrelated notes would silently resolve somewhere new). Link rewriting is a repair pass over the notes that link here, not part of the move: it is idempotent, so if it is interrupted the response says which notes were left and re-running the same rename finishes them. Only mounts advertising 'rename' in vault_info.mounts[].capabilities; the response's 'atomic' says whether the move itself was one operation.".to_string(),
            annotations: Some(tool_annotations(false, Some(true), Some(true))),
            execution: Some(json!({"taskSupport":"forbidden"})),
            input_schema: object_schema(
                vec![
                    ("from", json!({"type":"string","description":"Current vault-relative markdown path."})),
                    ("to", json!({"type":"string","description":"New vault-relative markdown path. Missing parent folders are created."})),
                    ("rewriteLinks", json!({"type":"boolean","default":true,"description":"Repoint inbound wikilinks after the move. Set false to move the note only and get the list of notes that link to it, to fix yourself."})),
                    ("expectedHash", json!({"type":"string","description":"Optional hash of the note being moved. If it does not match, nothing moves."})),
                    ("dryRun", json!({"type":"boolean","default":false,"description":"Report what would move and which notes link here, without changing the vault."})),
                ],
                vec!["from", "to"],
            ),
        });
    }
    if !capabilities.version_history {
        return;
    }
    definitions.push(ToolDefinition {
        name: "note_history".to_string(),
        description: "List the retained versions of a note on a mount that keeps a version history (newest first, with each version's author and timestamp). Retention keeps the most recent versions plus anything inside the mount's age window, so older versions may be absent.".to_string(),
        annotations: Some(tool_annotations(true, None, None)),
        execution: Some(json!({"taskSupport":"forbidden"})),
        input_schema: object_schema(
            vec![
                ("path", json!({"type":"string","description":"Logical vault-relative note path, on a mount that keeps a version history."})),
                // Schema `exclusiveMinimum`/`maximum` pair exactly with the runtime
                // `clamped_usize_arg(.., 50, 1, 500)` bounds, as everywhere else here.
                ("limit", json!({"type":"integer","exclusiveMinimum":0,"maximum":500,"default":50,"description":"Maximum versions to return, newest first. When the note has more, `truncated: true` and `totalCount` are added."})),
            ],
            vec!["path"],
        ),
    });
    definitions.push(ToolDefinition {
        name: "read_version".to_string(),
        description: "Read one specific, possibly superseded version of a note, reassembled from the mount's history. Use note_history to find a versionId.".to_string(),
        annotations: Some(tool_annotations(true, None, None)),
        execution: Some(json!({"taskSupport":"forbidden"})),
        input_schema: object_schema(
            vec![
                ("path", json!({"type":"string","description":"Logical vault-relative note path."})),
                ("versionId", json!({"type":"string","description":"A versionId from note_history."})),
            ],
            vec!["path", "versionId"],
        ),
    });
    definitions.push(ToolDefinition {
        name: "resolve_divergence".to_string(),
        description: "Return a diverged note's current head, the version it overtook, and their common ancestor, so you can three-way merge them yourself. The server NEVER merges: a wrong automatic merge produces plausible text and is nearly undetectable. Write the merged content with upsert_note and resolveDivergence:true to clear the mark.".to_string(),
        annotations: Some(tool_annotations(true, None, None)),
        execution: Some(json!({"taskSupport":"forbidden"})),
        input_schema: object_schema(
            vec![("path", json!({"type":"string","description":"Logical vault-relative note path whose hasDivergence is set (vault_info.mounts[].conflictedPaths lists them)."}))],
            vec!["path"],
        ),
    });
}

/// Which capability-gated tools this vault's mounts can serve.
///
/// A UNION across mounts, because `tools/list` is computed once per process and cannot say
/// "available for some paths". The per-path answer is the refusal each tool produces when
/// the path routes to a mount without the capability, and `vault_info.mounts[].capabilities`
/// is where a client reads the per-mount truth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    pub version_history: bool,
    pub soft_delete: bool,
    pub rename: bool,
}

impl CapabilitySet {
    /// The union over a router's mounts.
    pub fn of(router: &deep_obsidian_backend::VaultRouter) -> Self {
        let mut set = Self::default();
        for mount in router.mounts() {
            let descriptor = mount.backend.descriptor();
            set.version_history |= descriptor.supports(Capability::VersionHistory);
            set.soft_delete |= descriptor.supports(Capability::SoftDelete);
            set.rename |= descriptor.supports(Capability::Rename);
        }
        set
    }
}

fn tool_definitions(
    rg_available: bool,
    multi_mount: bool,
    capabilities: CapabilitySet,
) -> Vec<ToolDefinition> {
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
    // "rg works or grep_search doesn't exist." When the ROOT mount cannot serve line
    // search we omit the tool entirely so it never appears in `tools/list`. Keyed on the
    // root even though a non-root CouchDB mount can serve grep without any `rg` on the
    // host — see `AppState::rg_available` for why.
    if !rg_available {
        definitions.retain(|definition| definition.name != "grep_search");
    }
    if multi_mount {
        insert_scope_argument(&mut definitions);
    }
    if capabilities.version_history {
        insert_resolve_divergence_argument(&mut definitions);
    }
    // Appended AFTER the argument passes, so the four capability tools never accidentally
    // acquire `scope` or `resolveDivergence` — each takes a `path` that determines its
    // mount, and neither argument would mean anything on them.
    insert_capability_tools(&mut definitions, &capabilities);
    definitions
}

pub fn list_tools(
    rg_available: bool,
    multi_mount: bool,
    capabilities: CapabilitySet,
) -> Vec<ToolDefinition> {
    tool_definitions(rg_available, multi_mount, capabilities)
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

/// One hit from a mount's OWN index, rendered in the same shape as a local hit.
///
/// # What is here and what is deliberately not
///
/// Present, and identical in meaning to the local path: `path` (logical), `title`,
/// `resourceUri`, `chunkIndex`, `startLine`, `endLine`, `score`, `text`. A client that
/// walks `matches[]` needs no branch.
///
/// ABSENT: `semanticScore` and `bm25Score`. Those are the local hybrid ranker's two input
/// signals, and there is nothing to put in them — a remote index reports one ranking, not
/// a decomposition. Emitting `0.0` for both would be a fabricated measurement, and
/// emitting `null` would invite a caller to average it. The mount-level `recallMode` says
/// what produced the ranking instead.
///
/// Takes the LOGICAL path rather than deriving it from a mount, because the federated
/// caller has already translated it (fusion keys on logical paths) and translating twice
/// would double the mount prefix.
fn native_recall_match_json(
    hit: &deep_obsidian_backend::RecallHit,
    logical: &str,
    options: TextPayloadOptions,
) -> Value {
    let mut object = Map::from_iter([
        ("path".to_string(), json!(logical)),
        ("title".to_string(), json!(hit.title.clone())),
        ("resourceUri".to_string(), json!(note_uri(logical))),
        ("chunkIndex".to_string(), json!(hit.chunk_index)),
        ("startLine".to_string(), json!(hit.start_line)),
        ("endLine".to_string(), json!(hit.end_line)),
        ("score".to_string(), json!(hit.score)),
    ]);
    insert_optional_text(&mut object, "text", &hit.snippet, options);
    Value::Object(object)
}

/// The fields every natively-served recall payload carries in addition to a local one.
///
/// # Why these are additive rather than replacing `semanticBackend`/`degraded`
///
/// A local payload reports `semanticBackend` and `degraded` because they describe the
/// LOCAL embedding backend, which is not involved here. Rather than reuse those keys with
/// a different meaning — the most confusing option available — a natively-served payload
/// omits them and states its own provenance:
///
/// * `recallMode` — `"lexical"` or `"neural"`, from the index's own settings. This is the
///   field that makes the `score` interpretable; see [`deep_obsidian_backend::RecallHit`].
/// * `nativeRecall: true` — a single boolean a client can branch on without parsing
///   `recallMode`, and the flag that says "these scores are ordinal and not comparable
///   with a local hybrid score".
/// * `exhausted` — `false` when the mount's index had more hits than this answer carries.
///   Raise `limit` to see them.
///
/// # Why there is no `nextCursor`
///
/// The backend DOES paginate ([`deep_obsidian_backend::RecallSearchResponse::next_cursor`]),
/// and this deliberately does not surface it. Neither tool declares a `cursor` argument, so
/// a cursor in the payload would be a continuation no client could discover or take — which
/// is precisely the defect that kept PR #40's `resolveDivergence` invisible, and it is not
/// worth repeating for a field. `exhausted: false` carries the honest half of the fact
/// ("there are more hits, this tool does not page"), and `limit` is the lever a caller
/// already has. The cursor stays on the boundary, tested there, for the federation slice
/// that will consume it.
///
/// # Why `exhausted` is not `grep_search`'s `exhaustive`
///
/// Different facts, deliberately different words. `exhaustive: false` on a grep means
/// "I did not look everywhere, so an empty result is not proof of absence". `exhausted:
/// false` here means "I looked everywhere and there are simply more results than you asked
/// for". Conflating them would turn a complete-but-truncated ranking into a warning about
/// coverage.
///
/// None of these can appear on a single-mount vault's payload: an index-less mount cannot
/// be the vault root, so `hybrid_search` there is always locally served and byte-identical
/// to what it was.
fn insert_native_recall_provenance(
    result: &mut Map<String, Value>,
    response: &deep_obsidian_backend::RecallSearchResponse,
    mount: &NativeRecallMount,
) {
    result.insert("nativeRecall".to_string(), json!(true));
    result.insert("mountId".to_string(), json!(mount.id.clone()));
    result.insert(
        "recallMode".to_string(),
        json!(response.recall_mode.as_str()),
    );
    result.insert("exhausted".to_string(), json!(response.exhausted));
}

/// Run a ranked search against one mount's own backend.
///
/// Deliberately NOT through the router: the router refuses a ranked search because it
/// cannot merge two mounts' orderings, which is the right answer for a caller that named
/// no mount. This caller HAS named one, exactly as it names one when it picks a local
/// index, so it addresses that backend directly.
///
/// That is also what enforces "no cross-mount content flows": the request reaches one
/// backend and one only, and the response's paths are re-prefixed with that same mount's
/// prefix.
///
/// Always the FIRST page: no tool here declares a `cursor` argument, so there is no cursor
/// a caller could have supplied. See [`insert_native_recall_provenance`].
async fn native_recall_search(
    mount: &NativeRecallMount,
    query: &str,
    limit: usize,
) -> Result<deep_obsidian_backend::RecallSearchResponse, String> {
    mount
        .backend
        .execute(BackendRequest::recall_search(query, limit))
        .await
        .map_err(|error| error.to_string())?
        .into_recall_search()
        .map_err(|error| error.to_string())
}

/// Reason attached to the empty `graph` of a natively-served `load_knowledge`.
///
/// The empty graph needs a reason for the same reason the whole slice needs honesty
/// carriers: `{"nodes":[],"edges":[]}` is exactly what an INDEXED mount returns for a
/// subject with no links, so without this an agent cannot tell "this corpus has no links
/// around your subject" from "links were never looked for here".
const NATIVE_RECALL_NO_GRAPH_REASON: &str = "this mount has no local link graph: its \
backend serves its own ranked search but exposes no note-to-note edges, so the graph is \
empty because none was traversed rather than because none was found. The notes and chunks \
above are complete for this mount.";

/// `load_knowledge` served by a mount that ranks for itself.
///
/// # What is served, and what is honestly empty
///
/// The tool has three parts, and this mount can supply exactly one of them:
///
/// * `chunks` — the ranked passages. Served natively.
/// * `notes` — derived from the chunks, by the SAME rank-derived scoring the local path
///   uses (`1/rank`), so the two agree about what a top chunk is worth. What the local
///   path also does and this cannot is expand each seed through `related_notes`; there is
///   no similarity neighbourhood here, so the note list is exactly the notes the chunks
///   came from.
/// * `graph` — empty, with [`NATIVE_RECALL_NO_GRAPH_REASON`] saying why.
///
/// `includeGraph` is deliberately not consulted: the graph is empty either way, and
/// branching on it would produce two shapes that differ only in whether the reason is
/// stated.
async fn native_load_knowledge_payload(
    mount: &NativeRecallMount,
    subject: &str,
    project: Option<&str>,
    limit_notes: usize,
    limit_chunks: usize,
    text_options: TextPayloadOptions,
) -> Result<Value, String> {
    // The same query composition the local path uses, so a subject-plus-project call
    // means the same thing on either kind of mount.
    let query = [Some(subject.to_string()), project.map(str::to_string)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let response = native_recall_search(mount, &query, limit_chunks).await?;

    let mut chunks = Vec::new();
    let mut note_bucket = HashMap::<String, KnowledgeNote>::new();
    for (position, hit) in response.hits.iter().enumerate() {
        let logical = mount.to_logical(&hit.path);
        let mut chunk_value =
            native_recall_match_json(hit, &mount.to_logical(&hit.path), text_options);
        if let Some(object) = chunk_value.as_object_mut() {
            object.insert("wikiLink".to_string(), json!(note_wiki_link(&logical)));
        }
        chunks.push(chunk_value);
        merge_knowledge_note(
            &mut note_bucket,
            KnowledgeNote {
                title: if hit.title.is_empty() {
                    note_name(&logical)
                } else {
                    hit.title.clone()
                },
                wiki_link: note_wiki_link(&logical),
                path: logical,
                score: 1.0 / (position as f64 + 1.0),
                reasons: vec!["top chunk match".to_string()],
                // No link graph, so no shared links to report. An empty list rather than
                // an omitted key, because the local payload always carries one.
                shared_links: Vec::new(),
            },
        );
    }
    let response_truncated =
        apply_response_text_budget(&mut chunks, "text", RESPONSE_TEXT_BUDGET_CHARS);

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

    let mut result = Map::new();
    result.insert("subject".to_string(), json!(subject));
    if let Some(project) = project {
        result.insert("project".to_string(), json!(project));
    }
    insert_native_recall_provenance(&mut result, &response, mount);
    result.insert("notes".to_string(), json!(notes));
    result.insert("chunks".to_string(), json!(chunks));
    result.insert("graph".to_string(), json!({"nodes":[],"edges":[]}));
    result.insert(
        "graphUnavailableReason".to_string(),
        json!(NATIVE_RECALL_NO_GRAPH_REASON),
    );
    insert_response_truncation_flags(&mut result, response_truncated);
    Ok(Value::Object(result))
}

// ---------------------------------------------------------------------------
// Federated recall: the unscoped multi-mount answer
// ---------------------------------------------------------------------------

/// Note on a federated payload explaining why the answer may not be the best one.
///
/// Emitted only when the deepening loop stopped on the candidate budget with the frontier
/// still open. Without it, the payload would be indistinguishable from a search that
/// searched everything worth searching.
/// Why a federated `find_files` payload carries `truncated: true`.
const FEDERATED_FIND_FILES_TRUNCATION_NOTE: &str = "this vault has several mounts and their notes were merged in logical-path order before the limit was applied, so these are the alphabetically first matches across the whole vault and a mount whose paths sort later may be absent entirely. Raise 'limit', or narrow 'query'.";

const FEDERATION_BUDGET_NOTE: &str = "the federated candidate budget was reached before \
the ranking stabilized, so a better hit may exist on a mount that was not read further. \
Lower 'limit', or scope the search to one mount to search it exhaustively.";

/// Why a federated `load_knowledge` returned no graph when it also returned no chunks.
///
/// A graph traversal needs a note to start from, and the federated path anchors on the
/// top-ranked chunk. With no chunks there is no anchor, and that is a different fact from
/// "the mount that answered has no edges" — asserting the second here would be a false
/// statement about a mount whose graph was never opened.
const FEDERATION_GRAPH_NO_ANCHOR_REASON: &str = "no chunk matched the subject on any mount, so there was no note to anchor a graph traversal on. This says nothing about whether the vault has links around the subject — none were looked for.";

/// Why `load_knowledge`'s graph covers ONE mount on a federated answer.
const FEDERATION_GRAPH_MOUNT_LOCAL_REASON: &str = "link graphs are mount-local: each \
mount's index is built from its own vault directory, so a wiki link from a note on one \
mount to a note on another is not an edge in either graph. This graph is the graph of the \
mount that produced the top-ranked chunk, named in graphMountId; the notes and chunks \
above span every mount.";

/// How one mount answers a federated recall request.
enum FederatedRecallKind {
    /// The server's own SQLite index for that mount, queried EXACTLY ONCE.
    ///
    /// # Why a local index is not paged
    ///
    /// Its candidate pool is derived from the limit the query was issued with
    /// (`hybrid_candidate_limit`), and the graph-proximity rerank runs over that pool — so
    /// a second query at a larger limit produces a DIFFERENT ranking rather than a
    /// continuation of the first. There is no cursor that could make the two agree, and
    /// stitching two incompatible rankings together would corrupt the ranks fusion is
    /// built on. So the one query asks for the whole per-mount candidate target
    /// ([`federation::candidate_target`]) and the mount is then closed. When it came back
    /// full it is closed WITHOUT being exhausted, which is what tells a caller the mount
    /// had more to give — see [`federation::MountList::closed`].
    Local {
        runtime: Arc<RuntimeState>,
        queried: bool,
    },
    /// The mount's backend, through [`RecallRequest::Search`], page by page. This is the
    /// arm the deepening loop exists for.
    Native {
        backend: Arc<dyn deep_obsidian_backend::VaultBackend>,
        cursor: Option<deep_obsidian_backend::OpaqueCursor>,
        /// True once the backend stopped offering a cursor.
        done: bool,
    },
}

impl FederatedRecallKind {
    /// How the payload names this mount's recall source.
    fn label(&self) -> &'static str {
        match self {
            FederatedRecallKind::Local { .. } => "local-index",
            FederatedRecallKind::Native { .. } => "native-recall",
        }
    }
}

/// One hit, kept so the payload can be rendered in the FUSED order rather than in any one
/// mount's order.
///
/// Both variants hold a path already translated into the logical namespace: fusion keys on
/// logical paths (that is what makes them namespaced and therefore collision-free), so
/// translating later would mean translating twice.
enum FederatedHit {
    Local(index_search::SearchMatch),
    Native(deep_obsidian_backend::RecallHit),
    /// An artifact hit from one mount's artifact embedding table. Artifacts are whole
    /// files rather than chunks, so its candidate key carries chunk index 0.
    Artifact(index_search::ArtifactSearchMatch),
}

/// Which ranked list federation is fusing.
///
/// The two share the fusion, the deepening loop, the tie-break and every honesty carrier,
/// and differ only in which per-mount query produces a page — which is exactly the amount
/// of duplication a second copy of this module would have added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FederatedRetrieval {
    /// `hybrid_search` / `load_knowledge`: ranked note chunks. Served by a local index or
    /// by a native-recall backend.
    Chunks,
    /// `search_artifacts`: ranked non-markdown files. Served ONLY by a local index — an
    /// artifact embedding table is built by the server and has no backend equivalent.
    Artifacts,
}

/// One mount taking part in a federated recall, with the provenance its summary reports.
struct FederatedRecallMount {
    id: String,
    /// The logical folder this mount's paths sit under; `""` for the root mount.
    mount_at: String,
    kind: FederatedRecallKind,
    /// Rendered hits by candidate key, filled as pages arrive.
    hits: HashMap<federation::CandidateKey, FederatedHit>,
    /// `Some` for a native mount: which retrieval stage produced its ordering.
    recall_mode: Option<deep_obsidian_backend::RecallMode>,
    /// `Some` for a local mount: which semantic backend its index used.
    semantic_backend: Option<String>,
    /// A local mount whose embedding backend was down, so its list is BM25-only.
    degraded: bool,
    degradation_reason: Option<String>,
    /// Whether serving this mount rebuilt its index.
    rebuilt: bool,
    /// This mount's index snapshot, for a LOCAL mount that was queried.
    ///
    /// Held so the rerank can read stored chunk vectors and embed the query WITHOUT taking a
    /// second snapshot: a refresh between the fusion query and the rerank would score
    /// candidates against an index that no longer contains all of them.
    index: Option<Arc<deep_obsidian_index::index::SearchIndex>>,
    /// Why this mount is legitimately absent from this answer, when it is.
    ///
    /// # Not the same thing as an error
    ///
    /// A mount whose backend cannot hold a binary file cannot hold an ARTIFACT either, so
    /// leaving it out of `search_artifacts` omits nothing — there is no gap to report and
    /// the answer is complete. That is different from a mount that could hold artifacts and
    /// could not be searched, which is a genuine shortfall and sets `error` instead. Both
    /// are reported; only the second one degrades the answer.
    skipped_reason: Option<String>,
}

impl FederatedRecallMount {
    /// A mount-relative path as a logical vault path. Identity for the root mount.
    fn to_logical(&self, mount_relative: &str) -> String {
        if self.mount_at.is_empty() {
            mount_relative.to_string()
        } else {
            format!("{}/{}", self.mount_at, mount_relative)
        }
    }

    /// The inverse: a logical path as this mount's index spells it. Identity for the root
    /// mount, which is what keeps a single-mount vault's addressing untouched.
    ///
    /// Needed because fusion keys on LOGICAL paths (that is what makes them namespaced and
    /// collision-free) while a mount's own index has never heard of the logical namespace.
    fn to_mount_relative(&self, logical: &str) -> String {
        if self.mount_at.is_empty() {
            return logical.to_string();
        }
        logical
            .strip_prefix(&format!("{}/", self.mount_at))
            .unwrap_or(logical)
            .to_string()
    }
}

/// The federated candidate source: one round of `next_page` per mount that needs one.
struct FederatedRecallSource<'a> {
    mounts: &'a mut Vec<FederatedRecallMount>,
    query: String,
    /// The refresh reason recorded against a local mount's index snapshot.
    reason: &'static str,
    retrieval: FederatedRetrieval,
}

/// What a fetch needs, lifted out of the mount so nothing is borrowed across the await.
enum FederatedFetchPlan {
    Local(Arc<RuntimeState>),
    Native(
        Arc<dyn deep_obsidian_backend::VaultBackend>,
        Option<deep_obsidian_backend::OpaqueCursor>,
    ),
    /// The mount has already given everything it can. `exhausted` is what it claimed.
    Spent {
        exhausted: bool,
    },
}

impl federation::CandidateSource for FederatedRecallSource<'_> {
    async fn next_page(
        &mut self,
        list_index: usize,
        page_size: usize,
    ) -> Result<federation::CandidatePage, String> {
        let query = self.query.clone();
        let reason = self.reason;
        let plan = match &self.mounts[list_index].kind {
            FederatedRecallKind::Local { runtime, queried } => {
                if *queried {
                    FederatedFetchPlan::Spent { exhausted: false }
                } else {
                    FederatedFetchPlan::Local(runtime.clone())
                }
            }
            FederatedRecallKind::Native {
                backend,
                cursor,
                done,
            } => {
                if *done {
                    FederatedFetchPlan::Spent { exhausted: true }
                } else {
                    FederatedFetchPlan::Native(backend.clone(), cursor.clone())
                }
            }
        };

        match plan {
            FederatedFetchPlan::Spent { exhausted } => Ok(federation::CandidatePage {
                keys: Vec::new(),
                exhausted,
            }),
            FederatedFetchPlan::Local(runtime) => {
                let snapshot = runtime.fresh_snapshot(reason).await?;
                let index = snapshot.index.clone();
                let mount_index = list_index;
                let count = match self.retrieval {
                    FederatedRetrieval::Chunks => {
                        let outcome = hybrid_search_matches(
                            index.clone(),
                            query,
                            RankingOptions {
                                limit: page_size,
                                ..RankingOptions::default()
                            },
                        )
                        .await?;
                        let mount = &mut self.mounts[mount_index];
                        mount.degraded = outcome.degraded;
                        mount.degradation_reason = outcome.degradation_reason;
                        let mut keys = Vec::with_capacity(outcome.matches.len());
                        for match_item in outcome.matches {
                            let logical = mount.to_logical(&match_item.path);
                            let key = (logical.clone(), match_item.chunk_index);
                            keys.push(key.clone());
                            mount.hits.insert(
                                key,
                                FederatedHit::Local(index_search::SearchMatch {
                                    path: logical,
                                    ..match_item
                                }),
                            );
                        }
                        keys
                    }
                    FederatedRetrieval::Artifacts => {
                        let matches =
                            artifact_search_matches(index.clone(), query, page_size).await?;
                        let mount = &mut self.mounts[mount_index];
                        let mut keys = Vec::with_capacity(matches.len());
                        for match_item in matches {
                            let logical = mount.to_logical(&match_item.path);
                            // Artifacts are whole files, so chunk index 0 is the only key
                            // an artifact can have — and it is unique per path.
                            let key = (logical.clone(), 0_usize);
                            keys.push(key.clone());
                            mount.hits.insert(
                                key,
                                FederatedHit::Artifact(index_search::ArtifactSearchMatch {
                                    path: logical,
                                    ..match_item
                                }),
                            );
                        }
                        keys
                    }
                };
                // The mount returned exactly as many hits as it was allowed: it had more
                // and this is not all of them.
                let filled = count.len() >= page_size;
                let mount = &mut self.mounts[list_index];
                if let FederatedRecallKind::Local { queried, .. } = &mut mount.kind {
                    *queried = true;
                }
                mount.rebuilt |= snapshot.rebuilt;
                mount.semantic_backend = Some(index.semantic_backend.as_str().to_string());
                mount.index = Some(index);
                Ok(federation::CandidatePage {
                    keys: count,
                    exhausted: !filled,
                })
            }
            FederatedFetchPlan::Native(backend, cursor) => {
                let response = backend
                    .execute(BackendRequest::recall_search_page(query, page_size, cursor))
                    .await
                    .map_err(|error| error.to_string())?
                    .into_recall_search()
                    .map_err(|error| error.to_string())?;
                let mount = &mut self.mounts[list_index];
                if let FederatedRecallKind::Native { cursor, done, .. } = &mut mount.kind {
                    *done = response.next_cursor.is_none();
                    *cursor = response.next_cursor.clone();
                }
                mount.recall_mode = Some(response.recall_mode);
                let mut keys = Vec::with_capacity(response.hits.len());
                for hit in response.hits {
                    let logical = mount.to_logical(&hit.path);
                    let key = (logical.clone(), hit.chunk_index);
                    keys.push(key.clone());
                    mount.hits.insert(
                        key,
                        FederatedHit::Native(deep_obsidian_backend::RecallHit {
                            path: logical,
                            ..hit
                        }),
                    );
                }
                Ok(federation::CandidatePage {
                    keys,
                    exhausted: response.exhausted,
                })
            }
        }
    }
}

/// Every mount that takes part in a federated recall, in [`federation::canonical_order`].
///
/// # Nothing is silently dropped
///
/// Every mount in the router's table gets an entry, and a mount that cannot contribute
/// says which of the two reasons applies:
///
/// * it COULD have contributed and could not be asked — `error`, which degrades the answer
///   and puts the mount in `missingBackends`. That covers a mount with neither a local index
///   nor native recall (impossible today, so this is a guard rather than a tested path), and
///   a mount whose backend has binary reads but no artifact index, which is a real gap.
/// * it could never have contributed anything — `skipped_reason`, which does NOT degrade the
///   answer. A backend with no [`Capability::BinaryRead`] cannot hold a binary file and
///   therefore cannot hold an artifact, so its absence from `search_artifacts` omits
///   nothing. Reporting it as missing would train a reader to ignore `missingBackends`.
fn federated_recall_mounts(
    state: &AppState,
    retrieval: FederatedRetrieval,
) -> (Vec<FederatedRecallMount>, Vec<federation::MountList>) {
    let mut mounts: Vec<FederatedRecallMount> = Vec::new();
    let mut lists: Vec<federation::MountList> = Vec::new();
    for mount in state.router.mounts() {
        let weight = state
            .config
            .mounts
            .iter()
            .find(|declared| declared.id == mount.id)
            .and_then(|declared| declared.recall_weight)
            .unwrap_or(federation::DEFAULT_RECALL_WEIGHT);
        let mut list = federation::MountList::new(mount.id.clone(), weight);
        let mut skipped_reason = None;
        // A mount that will never be asked. `Native` with `done` set is the "ask me
        // nothing" shape; the backend handle it carries is never used.
        let never_asked = || FederatedRecallKind::Native {
            backend: mount.backend.clone(),
            cursor: None,
            done: true,
        };
        let kind = match state.runtimes.for_mount(&mount.id) {
            Some(runtime) => FederatedRecallKind::Local {
                runtime: runtime.clone(),
                queried: false,
            },
            // Artifact search has no backend equivalent: the artifact embedding table is
            // built by the server from binary files it read itself.
            None if retrieval == FederatedRetrieval::Artifacts => {
                if mount.backend.descriptor().supports(Capability::BinaryRead) {
                    list.error = Some(format!(
                        "mount '{}' can hold binary files but has no local artifact index, \
                         so its artifacts were not searched",
                        mount.id
                    ));
                } else {
                    skipped_reason = Some(format!(
                        "mount '{}' has a {} backend, which cannot store a binary file and \
                         therefore holds no artifacts; nothing was omitted by not searching it",
                        mount.id,
                        mount.backend.descriptor().kind.as_str()
                    ));
                    list.exhausted = true;
                }
                list.closed = true;
                never_asked()
            }
            None if mount_serves_native_recall(mount) => FederatedRecallKind::Native {
                backend: mount.backend.clone(),
                cursor: None,
                done: false,
            },
            None => {
                list.error = Some(format!(
                    "mount '{}' has no local search index and its backend ({}) does not \
                     answer ranked search, so it contributed nothing to this answer",
                    mount.id,
                    mount.backend.descriptor().kind.as_str()
                ));
                list.closed = true;
                never_asked()
            }
        };
        mounts.push(FederatedRecallMount {
            id: mount.id.clone(),
            mount_at: mount.mount_at.clone(),
            kind,
            hits: HashMap::new(),
            recall_mode: None,
            semantic_backend: None,
            degraded: false,
            degradation_reason: None,
            rebuilt: false,
            index: None,
            skipped_reason,
        });
        lists.push(list);
    }
    // Canonical (mount-id) order for BOTH tables, with the same permutation, so
    // `list_index` addresses the same mount in each. See `federation::canonical_order`.
    let permutation = federation::canonical_order(&lists);
    let mut slots: Vec<Option<FederatedRecallMount>> = mounts.into_iter().map(Some).collect();
    let mut ordered_mounts = Vec::with_capacity(slots.len());
    let mut ordered_lists = Vec::with_capacity(lists.len());
    for index in permutation {
        ordered_mounts.push(
            slots[index]
                .take()
                .expect("a permutation visits each mount once"),
        );
        ordered_lists.push(lists[index].clone());
    }
    (ordered_mounts, ordered_lists)
}

/// Run a federated recall and return the fused hits alongside the mounts that produced
/// them.
///
/// Split from the payload builders because `hybrid_search`, `load_knowledge` and
/// `search_artifacts` render the same fused list three different ways.
async fn federated_recall(
    state: &AppState,
    reason: &'static str,
    retrieval: FederatedRetrieval,
    query: &str,
    limit: usize,
) -> (federation::FederationOutcome, Vec<FederatedRecallMount>) {
    let (mut mounts, lists) = federated_recall_mounts(state, retrieval);
    let mut source = FederatedRecallSource {
        mounts: &mut mounts,
        query: query.to_string(),
        reason,
        retrieval,
    };
    // Fused to the RERANK WINDOW rather than to `limit`: the rerank can only reorder what it
    // is given, and a candidate fusion placed just outside the answer is exactly what rank
    // interleaving produces. This costs no extra fetching -- see `federate_with_window`. The
    // truncation to `limit` happens in `finish_federated_recall`.
    let outcome = federation::federate_with_window(
        lists,
        limit,
        federation::rerank_window(limit),
        &mut source,
    )
    .await;
    (outcome, mounts)
}

/// Apply the final rerank when it is enabled, or truncate the fused window to `limit` when it
/// is not.
///
/// One function so "who truncates the window" has exactly one answer. Skipping it would leave a
/// payload carrying the whole rerank window -- up to 50 hits for a caller that asked for 8.
async fn finish_federated_recall(
    state: &AppState,
    outcome: &mut federation::FederationOutcome,
    mounts: &[FederatedRecallMount],
    query: &str,
    limit: usize,
) -> federation::RerankOutcome {
    if !state.config.federated_rerank {
        outcome.hits.truncate(limit.max(1));
        return federation::RerankOutcome::not_applicable();
    }
    apply_federated_rerank(outcome, mounts, query, limit).await
}

/// Gather the rerank's per-candidate signals and apply it, in place.
///
/// # What makes the scorer mount-independent
///
/// Both signals are computed by the SERVER over the candidate set, and neither depends on
/// which mount a candidate came from:
///
/// * **semantic** — cosine of ONE query vector against the candidate's own stored chunk
///   vector, read from its own mount's index. Every mount's index is built from the same
///   configured embedding model, so those cosines live in one space; embedding the query once
///   is what guarantees it (see [`index_search::embed_query_vector`]). A candidate with no
///   stored vector — a hit from a backend that ranks for itself — is embedded server-side
///   from the text the payload will show.
/// * **lexical** — BM25 over the candidate set AS ITS OWN CORPUS. Each mount's index has its
///   own document frequencies, so its BM25 numbers are not comparable with another's; deriving
///   the IDFs from the candidate set instead makes them comparable by construction. It also
///   makes the scorer independent of vault size, which is the property that lets a two-mount
///   answer and a one-vault answer be compared at all.
///
/// # Every failure degrades with provenance rather than erroring
///
/// * no embedding backend anywhere — [`federation::RerankStage::None`], NOT degraded. A
///   lexical-only deployment has lost nothing; this is the documented no-op.
/// * the query could not be embedded — `None` and DEGRADED: the ordering signal this
///   deployment normally has is missing, and the answer is subject to rank interleaving.
/// * one mount's vector lookup, or the batch embed for native hits, failed — those candidates
///   have no semantic score and are absent from the semantic list. The query still answers.
async fn apply_federated_rerank(
    outcome: &mut federation::FederationOutcome,
    mounts: &[FederatedRecallMount],
    query: &str,
    limit: usize,
) -> federation::RerankOutcome {
    if outcome.hits.is_empty() {
        outcome.hits.truncate(limit);
        return federation::RerankOutcome::not_applicable();
    }
    // Any embedding-backed mount can embed the query: they share one configured model, so the
    // vector is valid against all of them. `None` means this vault has no dense retrieval at
    // all, which is the lexical-only no-op rather than a fault.
    let Some(scorer) = mounts.iter().find_map(|mount| {
        mount.index.as_ref().filter(|index| {
            index.semantic_backend == deep_obsidian_index::index::SemanticBackend::Embedding
        })
    }) else {
        outcome.hits.truncate(limit);
        return federation::RerankOutcome::not_applicable();
    };

    let scorer = scorer.clone();
    let query_owned = query.to_string();
    let query_vector = match tokio::task::spawn_blocking(move || {
        index_search::embed_query_vector(&scorer, &query_owned)
    })
    .await
    {
        Ok(Ok(vector)) => vector,
        Ok(Err(error)) => {
            outcome.hits.truncate(limit);
            return federation::RerankOutcome::unavailable(format!(
                "the federated results could not be reranked because the query could not be \
                 embedded ({error}), so they are in pure rank-fusion order: with several mounts \
                 that order interleaves each mount's best hits rather than ranking them against \
                 each other"
            ));
        }
        Err(error) => {
            outcome.hits.truncate(limit);
            return federation::RerankOutcome::unavailable(error.to_string());
        }
    };

    // --- semantic: stored vectors per mount, one batch embed for the rest ---------------
    let mut semantic: Vec<Option<f64>> = vec![None; outcome.hits.len()];
    for mount in mounts {
        let Some(index) = mount.index.clone() else {
            continue;
        };
        // This mount's candidates, as ITS index spells them: the fused key is logical.
        let positions: Vec<usize> = outcome
            .hits
            .iter()
            .enumerate()
            .filter(|(_, hit)| hit.mount_id == mount.id)
            .map(|(position, _)| position)
            .collect();
        if positions.is_empty() {
            continue;
        }
        let keys: Vec<(String, usize)> = positions
            .iter()
            .map(|position| {
                let hit = &outcome.hits[*position];
                (mount.to_mount_relative(&hit.key.0), hit.key.1)
            })
            .collect();
        let vector = query_vector.clone();
        // A failure here costs those candidates their semantic score and nothing else.
        let scores = tokio::task::spawn_blocking(move || {
            index_search::semantic_scores_for_chunks(&index, &vector, &keys)
        })
        .await
        .ok()
        .and_then(Result::ok);
        if let Some(scores) = scores {
            for (position, score) in positions.iter().zip(scores) {
                semantic[*position] = score;
            }
        }
    }

    // Candidates still without a vector: a native-recall mount's hits. Embedded server-side
    // from the same text the payload will carry, in ONE batched call.
    let unscored: Vec<usize> = (0..outcome.hits.len())
        .filter(|position| semantic[*position].is_none())
        .collect();
    if !unscored.is_empty() {
        let texts: Vec<String> = unscored
            .iter()
            .map(|position| federated_hit_text(outcome, mounts, *position))
            .collect();
        if texts.iter().any(|text| !text.trim().is_empty()) {
            let scorer = mounts
                .iter()
                .find_map(|mount| mount.index.clone())
                .expect("a scorer index was found above");
            let vector = query_vector.clone();
            let embedded = tokio::task::spawn_blocking(move || {
                index_search::embed_texts_for_index(&scorer, &texts).map(|vectors| {
                    vectors
                        .iter()
                        .map(|document| index_search::semantic_score_for_vectors(&vector, document))
                        .collect::<Vec<f64>>()
                })
            })
            .await
            .ok()
            .and_then(Result::ok);
            if let Some(scores) = embedded {
                for (position, score) in unscored.iter().zip(scores) {
                    semantic[*position] = Some(score);
                }
            }
        }
    }

    // --- lexical: BM25 with the candidate set as the corpus -----------------------------
    //
    // The document model is the CHUNK'S OWN indexed term counts, taken from the mount's index,
    // not a re-tokenization of the text the payload renders. The two are genuinely different:
    // small-to-big expansion means a hit's rendered `text` is the whole enclosing section,
    // several times larger than the chunk that actually matched, so re-tokenizing it would
    // score a document the local ranker never scored. Since the gate compares this ordering
    // against a single-vault `hybrid_search`, using the same document model is the point.
    //
    // A native-recall hit has no indexed chunk, so its snippet is tokenized instead — the only
    // text it has.
    let query_terms = tokenize(query);
    let term_counts: Vec<std::collections::BTreeMap<String, usize>> = (0..outcome.hits.len())
        .map(|position| {
            federated_hit_term_counts(outcome, mounts, position).unwrap_or_else(|| {
                deep_obsidian_index::index::count_terms(&federated_hit_text(
                    outcome, mounts, position,
                ))
            })
        })
        .collect();
    let mut document_frequencies: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for counts in &term_counts {
        for term in counts.keys() {
            *document_frequencies.entry(term.clone()).or_insert(0) += 1;
        }
    }
    let lengths: Vec<f64> = term_counts
        .iter()
        .map(|counts| deep_obsidian_index::index::token_count(counts) as f64)
        .collect();
    let average_length = deep_obsidian_index::index::average(&lengths);
    let lexical: Vec<f64> = term_counts
        .iter()
        .zip(&lengths)
        .map(|(counts, length)| {
            deep_obsidian_index::index::bm25_score(
                &query_terms,
                counts,
                &document_frequencies,
                term_counts.len(),
                *length as usize,
                average_length,
            )
        })
        .collect();

    federation::rerank(
        &mut outcome.hits,
        &federation::RerankSignals { semantic, lexical },
        limit,
    )
}

/// The indexed term counts of one fused candidate's chunk, when it has one.
///
/// `None` for a candidate with no chunk in any local index — a native-recall hit, or a hit
/// whose mount was reindexed between the fusion query and the rerank. The caller falls back
/// to tokenizing the rendered snippet.
fn federated_hit_term_counts(
    outcome: &federation::FederationOutcome,
    mounts: &[FederatedRecallMount],
    position: usize,
) -> Option<std::collections::BTreeMap<String, usize>> {
    let hit = outcome.hits.get(position)?;
    let mount = mounts.iter().find(|mount| mount.id == hit.mount_id)?;
    let index = mount.index.as_ref()?;
    let mount_relative = mount.to_mount_relative(&hit.key.0);
    index
        .chunks
        .iter()
        .find(|chunk| chunk.path == mount_relative && chunk.chunk_index == hit.key.1)
        .map(|chunk| chunk.term_counts.clone())
}

/// The text of one fused candidate, as the payload will render it.
///
/// Scoring the text the CALLER sees rather than some other projection of the note is
/// deliberate: it makes the lexical component explainable from the response alone, and it is
/// the only text a native-recall hit has at all.
fn federated_hit_text(
    outcome: &federation::FederationOutcome,
    mounts: &[FederatedRecallMount],
    position: usize,
) -> String {
    let Some(hit) = outcome.hits.get(position) else {
        return String::new();
    };
    mounts
        .iter()
        .find(|mount| mount.id == hit.mount_id)
        .and_then(|mount| mount.hits.get(&hit.key))
        .map(|found| match found {
            FederatedHit::Local(match_item) => match_item.text.clone(),
            FederatedHit::Native(native) => native.snippet.clone(),
            FederatedHit::Artifact(artifact) => artifact.title.clone(),
        })
        .unwrap_or_default()
}

/// One fused hit, rendered in the scoped payload's shape plus `mountId`.
fn federated_hit_json(hit: &FederatedHit, mount_id: &str, options: TextPayloadOptions) -> Value {
    let mut value = match hit {
        FederatedHit::Local(match_item) => hybrid_search_match_json(match_item, options),
        // The path is already logical (see `FederatedHit`), so it is passed through
        // rather than re-prefixed.
        FederatedHit::Native(native) => native_recall_match_json(native, &native.path, options),
        FederatedHit::Artifact(artifact) => artifact_search_match_json(artifact),
    };
    if let Some(object) = value.as_object_mut() {
        // Which mount answered. The one field a federated hit carries that a scoped hit
        // does not: without it a caller cannot tell where a result came from, and the
        // logical path only says so for a non-root mount.
        object.insert("mountId".to_string(), json!(mount_id));
    }
    value
}

/// The fused hits, rendered best-first, and whether any mount rebuilt its index.
fn federated_matches_json(
    outcome: &federation::FederationOutcome,
    mounts: &[FederatedRecallMount],
    options: TextPayloadOptions,
) -> Vec<Value> {
    outcome
        .hits
        .iter()
        .filter_map(|fused| {
            let mount = mounts.iter().find(|mount| mount.id == fused.mount_id)?;
            let hit = mount.hits.get(&fused.key)?;
            let mut value = federated_hit_json(hit, &mount.id, options);
            // `score` is always THE NUMBER THAT PRODUCED THIS ORDER, so a client that sorts
            // by it gets back the list it was handed. That makes it the rerank score when a
            // rerank ran and the fused score otherwise; `rrfScore` and `mountRank` carry the
            // earlier stages so the ordering stays explainable either way.
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "score".to_string(),
                    json!(fused.rerank_score.unwrap_or(fused.score)),
                );
                object.insert("rrfScore".to_string(), json!(fused.score));
                object.insert("mountRank".to_string(), json!(fused.mount_rank));
            }
            Some(value)
        })
        .collect()
}

/// The `federated`/`mounts[]`/`degraded`/`missingBackends` block every federated payload
/// carries.
///
/// # Why `degraded` is a UNION and `mounts[]` is the detail
///
/// A federated answer can be less than complete for two unrelated reasons: a mount's
/// embedding backend was down so its own list is lexical-only, or a mount could not be
/// reached at all. A caller that has to branch on "is this answer trustworthy" needs ONE
/// boolean, so `degraded` is true for either — and `degradationReason` plus the per-mount
/// entry say which, because the remedies are different (restart the embedding service;
/// fix the unreachable mount).
///
/// `missingBackends` is emitted only when non-empty, and lists every mount whose answer is
/// missing or incomplete. `degraded` is ALWAYS present, matching the scoped payload, so a
/// client can read it without probing.
fn insert_federation_provenance(
    result: &mut Map<String, Value>,
    outcome: &federation::FederationOutcome,
    mounts: &[FederatedRecallMount],
    rerank: &federation::RerankOutcome,
) {
    result.insert("federated".to_string(), json!(true));
    // Which stage produced the ORDER the caller is reading. Always present: "these are in
    // rank-fusion order" and "these were rescored against the query" are different answers to
    // the same question, and nothing in the hits lets a client tell which.
    result.insert("rerank".to_string(), json!(rerank.stage.as_str()));
    if rerank.stage != federation::RerankStage::None {
        result.insert(
            "rerankedCandidates".to_string(),
            json!(rerank.semantic_signals),
        );
    }
    let mut degraded = false;
    let mut reasons: Vec<String> = Vec::new();
    if rerank.degraded {
        degraded = true;
        if let Some(reason) = &rerank.reason {
            reasons.push(reason.clone());
        }
    }
    let mut summaries: Vec<Value> = Vec::new();
    for list in &outcome.mounts {
        let Some(mount) = mounts.iter().find(|mount| mount.id == list.mount_id) else {
            continue;
        };
        let mut entry = Map::from_iter([
            ("id".to_string(), json!(list.mount_id.clone())),
            ("mountAt".to_string(), json!(mount.mount_at.clone())),
            ("source".to_string(), json!(mount.kind.label())),
            ("recallWeight".to_string(), json!(list.weight)),
            ("candidateCount".to_string(), json!(list.keys.len())),
            ("exhausted".to_string(), json!(list.exhausted)),
        ]);
        if let Some(mode) = mount.recall_mode {
            entry.insert("recallMode".to_string(), json!(mode.as_str()));
        }
        if let Some(reason) = &mount.skipped_reason {
            // Reported, and deliberately NOT counted as degradation. See
            // `FederatedRecallMount::skipped_reason`.
            entry.insert("skipped".to_string(), json!(true));
            entry.insert("skippedReason".to_string(), json!(reason));
        }
        if let Some(backend) = &mount.semantic_backend {
            entry.insert("semanticBackend".to_string(), json!(backend));
        }
        if mount.degraded {
            degraded = true;
            entry.insert("degraded".to_string(), json!(true));
            if let Some(reason) = &mount.degradation_reason {
                entry.insert("degradationReason".to_string(), json!(reason));
                reasons.push(format!("mount '{}': {reason}", list.mount_id));
            }
        }
        if let Some(error) = &list.error {
            degraded = true;
            entry.insert("error".to_string(), json!(error));
            reasons.push(format!(
                "mount '{}' could not be searched: {error}",
                list.mount_id
            ));
        }
        summaries.push(Value::Object(entry));
    }
    result.insert("mounts".to_string(), json!(summaries));
    let missing = outcome.missing_mounts();
    if !missing.is_empty() {
        result.insert("missingBackends".to_string(), json!(missing));
    }
    if outcome.budget_reached {
        result.insert("candidateBudgetReached".to_string(), json!(true));
    }
    // Only the UNSTABLE case degrades the answer. Hitting the budget with a stable
    // frontier means the search stopped because there was nothing left worth reading,
    // which is not a shortfall.
    if outcome.frontier_unstable {
        degraded = true;
        reasons.push(FEDERATION_BUDGET_NOTE.to_string());
    }
    result.insert("degraded".to_string(), json!(degraded));
    if !reasons.is_empty() {
        result.insert("degradationReason".to_string(), json!(reasons.join("; ")));
    }
    result.insert(
        "rebuilt".to_string(),
        json!(mounts.iter().any(|mount| mount.rebuilt)),
    );
}

/// `hybrid_search` over every mount, fused.
async fn federated_hybrid_search_payload(
    state: &AppState,
    query: &str,
    limit: usize,
    options: TextPayloadOptions,
) -> Result<Value, String> {
    let (mut outcome, mounts) = federated_recall(
        state,
        "hybrid_search",
        FederatedRetrieval::Chunks,
        query,
        limit,
    )
    .await;
    let rerank = finish_federated_recall(state, &mut outcome, &mounts, query, limit).await;
    let mut match_values = federated_matches_json(&outcome, &mounts, options);
    let response_truncated =
        apply_response_text_budget(&mut match_values, "text", RESPONSE_TEXT_BUDGET_CHARS);
    let mut result = Map::new();
    result.insert("query".to_string(), json!(query));
    insert_federation_provenance(&mut result, &outcome, &mounts, &rerank);
    result.insert("count".to_string(), json!(match_values.len()));
    result.insert("matches".to_string(), json!(match_values));
    insert_response_truncation_flags(&mut result, response_truncated);
    Ok(Value::Object(result))
}

/// One artifact hit, in the shape `search_artifacts` has always rendered.
///
/// Shared by the scoped and the federated paths so the two cannot drift: the federated
/// answer must be the scoped shape plus `mountId`, not a second dialect of it.
fn artifact_search_match_json(item: &index_search::ArtifactSearchMatch) -> Value {
    json!({
        "path": item.path,
        "title": item.title,
        "kind": item.kind,
        "mimeType": item.mime_type,
        "size": item.size,
        "score": item.score,
        "metadata": serde_json::from_str::<Value>(&item.metadata_json).unwrap_or(Value::Null),
    })
}

/// Run one mount's artifact search off the async runtime.
///
/// `artifact_semantic_search` embeds the query through the (multimodal) artifact backend
/// over HTTP, so it must not run on the async runtime. The backend-unavailable case is
/// remapped to [`ARTIFACT_EMBEDDING_BACKEND_UNAVAILABLE_MESSAGE`] here rather than at the
/// call site, so the federated path reports the same actionable message the scoped path
/// does instead of a raw upstream 400.
async fn artifact_search_matches(
    index: Arc<deep_obsidian_index::index::SearchIndex>,
    query: String,
    limit: usize,
) -> Result<Vec<index_search::ArtifactSearchMatch>, String> {
    tokio::task::spawn_blocking(move || {
        index_search::artifact_semantic_search_with_options(
            index.as_ref(),
            &query,
            RankingOptions {
                limit,
                ..RankingOptions::default()
            },
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| match error {
        IndexError::EmbeddingBackendUnavailable(_) => {
            ARTIFACT_EMBEDDING_BACKEND_UNAVAILABLE_MESSAGE.to_string()
        }
        other => other.to_string(),
    })
}

/// `search_artifacts` over every mount that has an artifact index, fused.
///
/// # Why an all-mounts failure is an error rather than an empty answer
///
/// `search_artifacts` has no lexical fallback — artifacts carry no BM25 terms — so a dead
/// artifact embedding backend produces no ranking at all, on any mount. Returning an empty
/// `matches[]` with `degraded: true` would say "there are no matching artifacts", which is
/// the one thing that is not true. When every mount that could have answered failed, the
/// tool errors with the first mount's message, exactly as the scoped path does.
async fn federated_search_artifacts_payload(
    state: &AppState,
    query: &str,
    limit: usize,
) -> Result<Value, String> {
    let (mut outcome, mounts) = federated_recall(
        state,
        "search_artifacts",
        FederatedRetrieval::Artifacts,
        query,
        limit,
    )
    .await;
    // Artifacts go through the same rerank, but on a WEAKER signal than chunks do, and the
    // difference is worth knowing rather than discovering.
    //
    // An artifact's stored vector lives in the ARTIFACT embedding table, not the chunk one, and
    // its path is not a chunk path -- so `semantic_scores_for_chunks` finds nothing for it and
    // `federated_hit_term_counts` finds nothing either. Both signals therefore fall back to the
    // only text an artifact hit carries: its TITLE. The ordering that comes out is title
    // relevance plus candidate-set BM25 over titles, which is a real ordering but a much
    // thinner one than a chunk rerank, and the recall gates do not exercise it (the eval corpus
    // holds one dummy artifact that no gold query matches). Reranking artifacts against their
    // own stored vectors is a follow-up, not a claim this code makes.
    let rerank = finish_federated_recall(state, &mut outcome, &mounts, query, limit).await;
    let askable = outcome
        .mounts
        .iter()
        .filter(|list| {
            mounts
                .iter()
                .any(|mount| mount.id == list.mount_id && mount.skipped_reason.is_none())
        })
        .collect::<Vec<_>>();
    if !askable.is_empty() && askable.iter().all(|list| list.error.is_some()) {
        return Err(askable[0]
            .error
            .clone()
            .expect("every askable mount failed"));
    }
    let mut match_values =
        federated_matches_json(&outcome, &mounts, TextPayloadOptions::without_text());
    let mut result = Map::new();
    result.insert("query".to_string(), json!(query));
    insert_federation_provenance(&mut result, &outcome, &mounts, &rerank);
    result.insert("count".to_string(), json!(match_values.len()));
    result.insert(
        "matches".to_string(),
        json!(std::mem::take(&mut match_values)),
    );
    Ok(Value::Object(result))
}

/// `load_knowledge`'s four sizing arguments, as one value.
///
/// Grouped rather than passed individually because they travel together everywhere and are
/// all small integers and a bool — the shape a caller is most likely to transpose.
#[derive(Debug, Clone, Copy)]
struct FederatedKnowledgeOptions {
    limit_notes: usize,
    limit_chunks: usize,
    include_graph: bool,
    graph_depth: usize,
}

/// `load_knowledge` over every mount, fused.
///
/// # What is federated and what is honestly mount-local
///
/// * `chunks` — the ranked passages. Federated, so a subject whose best evidence lives on a
///   minority mount is found.
/// * `notes` — derived from the FUSED chunk order by the same `1/(position + 1)` scoring the
///   scoped path applies to its own chunk order, so a top chunk is worth the same either
///   way. What the scoped path additionally does and this does not is expand each seed
///   through `related_notes`: that walks one index's similarity neighbourhood, and running
///   it per mount would mix cosine similarities from independently built indexes into one
///   ordering. The notes here are therefore exactly the notes the fused chunks came from.
/// * `graph` — the link graph of the ONE mount that produced the top-ranked chunk, named in
///   `graphMountId`, with [`FEDERATION_GRAPH_MOUNT_LOCAL_REASON`] saying why it is not the
///   whole vault's. A cross-mount graph does not exist to return: a wiki link from a note on
///   one mount to a note on another is not an edge in either index, because each index is
///   built from its own vault directory. Returning one mount's graph and SAYING so beats an
///   empty graph (which would read as "this subject has no links") and beats a synthesized
///   union (which would invent edges).
async fn federated_load_knowledge_payload(
    state: &AppState,
    subject: &str,
    project: Option<&str>,
    knowledge: FederatedKnowledgeOptions,
    options: TextPayloadOptions,
) -> Result<Value, String> {
    let FederatedKnowledgeOptions {
        limit_notes,
        limit_chunks,
        include_graph,
        graph_depth,
    } = knowledge;
    let query = [Some(subject.to_string()), project.map(ToOwned::to_owned)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let (mut outcome, mounts) = federated_recall(
        state,
        "load_knowledge",
        FederatedRetrieval::Chunks,
        &query,
        limit_chunks,
    )
    .await;
    let rerank = finish_federated_recall(state, &mut outcome, &mounts, &query, limit_chunks).await;

    let mut chunks = federated_matches_json(&outcome, &mounts, options);
    for chunk in chunks.iter_mut() {
        let path = chunk
            .get("path")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let (Some(object), Some(path)) = (chunk.as_object_mut(), path) {
            object.insert("wikiLink".to_string(), json!(note_wiki_link(&path)));
        }
    }
    let response_truncated =
        apply_response_text_budget(&mut chunks, "text", RESPONSE_TEXT_BUDGET_CHARS);

    // Same rank-derived scoring as the scoped path: top chunk = 1.0, then 0.5, 0.33, ...
    let mut note_bucket = HashMap::<String, KnowledgeNote>::new();
    for (position, chunk) in chunks.iter().enumerate() {
        let Some(path) = chunk.get("path").and_then(Value::as_str) else {
            continue;
        };
        let title = chunk
            .get("title")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| note_name(path));
        merge_knowledge_note(
            &mut note_bucket,
            KnowledgeNote {
                path: path.to_string(),
                title,
                wiki_link: note_wiki_link(path),
                score: 1.0 / (position as f64 + 1.0),
                reasons: vec!["top chunk match".to_string()],
                shared_links: Vec::new(),
            },
        );
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

    // The graph of whichever mount produced the top-ranked chunk, if it has a local index.
    let mut graph = json!({"nodes": [], "edges": []});
    let mut graph_mount: Option<String> = None;
    if include_graph {
        if let Some(top) = outcome.hits.first() {
            if let Some(runtime) = state.runtimes.for_mount(&top.mount_id) {
                let mount = mounts.iter().find(|mount| mount.id == top.mount_id);
                let snapshot = runtime.fresh_snapshot("load_knowledge").await?;
                // The graph is addressed in the mount's OWN namespace, so the fused
                // logical path has to be stripped back down before the traversal and
                // re-prefixed on the way out.
                let mount_at = mount.map(|mount| mount.mount_at.as_str()).unwrap_or("");
                let mount_relative = if mount_at.is_empty() {
                    top.key.0.clone()
                } else {
                    top.key
                        .0
                        .strip_prefix(&format!("{mount_at}/"))
                        .unwrap_or(&top.key.0)
                        .to_string()
                };
                let scoped = ScopedIndex {
                    runtime: runtime.clone(),
                    mount_at: mount_at.to_string(),
                };
                let traversed = index_graph::graph_traverse(
                    &snapshot.index,
                    &mount_relative,
                    index_graph::GraphDirection::Both,
                    graph_depth,
                    (limit_notes * 4).max(20),
                )
                .map_err(|error| error.to_string())?;
                graph = json!({
                    "nodes": traversed.nodes.into_iter().map(|node| note_result_json(scoped.to_logical(&node.path), node.title, |object| {
                        object.insert("depth".to_string(), json!(node.depth));
                    })).collect::<Vec<_>>(),
                    "edges": traversed.edges.into_iter().map(|edge| json!({
                        "source": scoped.to_logical(&edge.source),
                        "target": scoped.to_logical(&edge.target),
                        "rawLink": edge.raw_link
                    })).collect::<Vec<_>>()
                });
                graph_mount = Some(top.mount_id.clone());
            }
        }
    }

    let mut result = Map::new();
    result.insert("subject".to_string(), json!(subject));
    if let Some(project) = project {
        result.insert("project".to_string(), json!(project));
    }
    insert_federation_provenance(&mut result, &outcome, &mounts, &rerank);
    result.insert("notes".to_string(), json!(notes));
    result.insert("chunks".to_string(), json!(chunks));
    result.insert("graph".to_string(), graph);
    match graph_mount {
        Some(mount_id) => {
            result.insert("graphMountId".to_string(), json!(mount_id));
            result.insert(
                "graphScopeReason".to_string(),
                json!(FEDERATION_GRAPH_MOUNT_LOCAL_REASON),
            );
        }
        // No graph was traversed, and WHICH of the three reasons applies matters: the empty
        // graph needs a reason for the same argument `NATIVE_RECALL_NO_GRAPH_REASON` exists
        // for, and a reason that is not the actual reason is worse than none. There were
        // chunks, so the mount that produced the best one has no local graph.
        None if include_graph && !outcome.hits.is_empty() => {
            result.insert(
                "graphUnavailableReason".to_string(),
                json!(NATIVE_RECALL_NO_GRAPH_REASON),
            );
        }
        // Nothing matched at all, so there was no note to anchor a traversal on. Saying
        // "this mount exposes no edges" here would be a false statement about a mount whose
        // graph was never consulted.
        None if include_graph => {
            result.insert(
                "graphUnavailableReason".to_string(),
                json!(FEDERATION_GRAPH_NO_ANCHOR_REASON),
            );
        }
        // `includeGraph: false`. The caller declined it, so there is nothing to explain.
        None => {}
    }
    insert_response_truncation_flags(&mut result, response_truncated);
    Ok(Value::Object(result))
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

/// Refuse the one whole-vault tool whose answer cannot be federated.
///
/// # Why `recommend_folder` alone, now that recall federates
///
/// Every other whole-vault tool has an answer that survives being assembled from several
/// mounts: `find_files` is an ENUMERATION filtered by a path match, so concatenating each
/// mount's notes is the same answer a single vault would give; `hybrid_search`,
/// `load_knowledge` and `search_artifacts` are RANKINGS, and ranks fuse (see
/// [`crate::federation`]).
///
/// `recommend_folder` is neither. It scores every top-level folder against ONE corpus's
/// semantics — the folder names come from the vault, but the evidence is "how many of this
/// query's best chunks live under that folder", and those counts are only comparable
/// within one index. Fusing them would mean deciding that a folder on a small mount and a
/// folder on a large one are equally well-evidenced by the same number of hits, which is
/// not a ranking, it is a coin toss with a score attached. And the tool's output is a
/// SINGLE folder a session note will be written to, so a plausible-looking wrong answer
/// silently misfiles work.
///
/// So this stays a refusal on purpose, not for lack of a slice. Single-mount configs never
/// reach it.
fn require_single_mount(state: &AppState, tool: &str) -> Result<(), String> {
    if !state.router.is_multi_mount() {
        return Ok(());
    }
    Err(format!(
        "{tool} does not support a multi-mount vault: it ranks candidate folders by how much of the query's evidence lives under each one, and those counts are only comparable within a single index — merging them across mounts would produce a confident-looking arbitrary answer, and this tool's output is the one folder a note gets written to. Choose the folder yourself (list_children on the vault root shows every top-level folder, including each mount), or reduce the vault to a single mount."
    ))
}

/// The mount roots a `scope` may name, rendered for an error message.
///
/// # Why this is parameterized rather than one list
///
/// A mount can serve recall in two different ways, and the two sets of tools that ask
/// are not the same:
///
/// * `related_notes`, `graph_traverse` and `search_artifacts` need the LOCAL index —
///   they walk a link graph, a similarity neighbourhood, or an artifact embedding table,
///   none of which a remote corpus exposes. Only index-backed mounts can serve them.
/// * `hybrid_search` and `load_knowledge` need a ranked list, which a mount advertising
///   [`Capability::NativeRecall`] produces itself.
///
/// A single list would be wrong for one of the two, and wrong in the worse direction for
/// the second: it would tell a caller that the shared mount cannot be searched when it
/// can, which is how a working feature stays undiscovered. So `include_native_recall`
/// selects the set that can actually answer the tool doing the asking.
///
/// Either way a mount that cannot serve the tool is EXCLUDED, because these lists are
/// remedies: naming the mount whose refusal the reader is holding would tell them to
/// retry the exact call that just failed.
fn mount_scope_hint(state: &AppState, include_native_recall: bool) -> String {
    let scopes: Vec<String> = state
        .router
        .mounts()
        .iter()
        .filter(|mount| {
            state.runtimes.for_mount(&mount.id).is_some()
                || (include_native_recall && mount_serves_native_recall(mount))
        })
        .map(|mount| {
            if mount.mount_at.is_empty() {
                "'/'".to_string()
            } else {
                format!("'{}'", mount.mount_at)
            }
        })
        .collect();
    if scopes.is_empty() {
        // Unreachable today: the ROOT mount always has an index (it is the one mount
        // that must be a filesystem vault). Answered rather than left as an empty list,
        // which would render as a dangling "one of: ." .
        return "no mount in this vault has a local search index".to_string();
    }
    scopes.join(", ")
}

/// True when this mount answers a ranked search itself.
fn mount_serves_native_recall(mount: &deep_obsidian_backend::Mount) -> bool {
    mount
        .backend
        .descriptor()
        .supports(Capability::NativeRecall)
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

/// Which mount, and by which mechanism, serves a recall request.
enum RecallTarget {
    /// The server's own SQLite index for that mount.
    Local(ScopedIndex),
    /// The mount's backend, through [`RecallRequest::Search`].
    Native(NativeRecallMount),
    /// EVERY mount, fused. The answer to an unscoped recall on a multi-mount vault; see
    /// [`crate::federation`].
    Federated,
}

/// A mount that answers ranked search itself.
struct NativeRecallMount {
    id: String,
    /// The logical folder this mount's paths sit under. Never empty: an index-less mount
    /// cannot be the vault root (see the config normalizer), so a native-recall mount is
    /// always nested and its paths always need re-prefixing.
    mount_at: String,
    backend: Arc<dyn deep_obsidian_backend::VaultBackend>,
}

impl NativeRecallMount {
    /// A mount-relative hit path as a logical vault path.
    fn to_logical(&self, mount_relative: &str) -> String {
        if self.mount_at.is_empty() {
            mount_relative.to_string()
        } else {
            format!("{}/{}", self.mount_at, mount_relative)
        }
    }
}

/// Which mount (or mounts) serves a recall request, and by which mechanism.
///
/// * single mount — the root runtime, unconditionally. `scope` is not even in the tool
///   schema, so this is the pre-slice behaviour verbatim and every golden is provably
///   unaffected by everything below;
/// * multi-mount, no `scope` — [`RecallTarget::Federated`]: every mount is searched and the
///   rankings are fused. This REPLACED a refusal. The refusal was right while there was no
///   way to merge two mounts' orderings, because answering from one mount would have
///   reported "no matches" for text that exists in the vault — but it was never the answer
///   a caller wanted, and `scope` is now optional rather than required;
/// * multi-mount, `scope` naming a mount root — that mount's index (or its backend, when it
///   ranks for itself and `include_native_recall` is set).
///
/// A `scope` must still name a mount root EXACTLY. A scoped search ranks and truncates to
/// `limit`, so a narrower scope could only be honoured by filtering an already-truncated
/// list — silently returning fewer results than asked for. A caller who wants a subtree
/// wants the federated answer plus their own filter, or `grep_search`, whose `glob`
/// genuinely IS a subtree filter.
///
/// # `scope` selects a MOUNT, not a folder subtree
///
/// `'/'` therefore means "the mount at the vault root", not "the whole logical vault": it
/// is answered from the root mount's own index, and content grafted under it by another
/// mount is not included. That is the only reading that leaves the root mount addressable
/// at all — every non-root mount is nested inside the root's subtree by definition, so
/// treating `'/'` as a subtree would refuse it always. Omitting `scope` is now how a caller
/// asks for the whole vault.
///
/// # `include_native_recall`
///
/// Set by the tools whose answer is a ranked list of note chunks (`hybrid_search`,
/// `load_knowledge`), because that is the one question a [`Capability::NativeRecall`]
/// backend can answer. `search_artifacts` leaves it clear and keeps the honest "no local
/// index" refusal for a SCOPED call, since an artifact embedding table is not something a
/// remote corpus exposes.
fn resolve_recall_target(
    state: &AppState,
    tool: &str,
    scope: Option<&str>,
    include_native_recall: bool,
) -> Result<RecallTarget, String> {
    if !state.router.is_multi_mount() {
        return Ok(RecallTarget::Local(ScopedIndex {
            runtime: root_index(state, tool)?,
            mount_at: String::new(),
        }));
    }
    let Some(scope) = scope else {
        return Ok(RecallTarget::Federated);
    };
    let resolved = state
        .router
        .resolve(scope)
        .map_err(|error| error.to_string())?;
    if !resolved.backend_relative_path.trim_matches('/').is_empty() {
        return Err(format!(
            "{tool} cannot scope to '{scope}': it is inside mount '{}' rather than naming a mount root, and this tool ranks results, so a narrower scope could only be honoured by filtering an already-truncated list. Pass one of: {}.",
            resolved.mount.id,
            mount_scope_hint(state, include_native_recall)
        ));
    }
    // The LOCAL index wins when a mount has one. No mount has both today, but the order
    // matters if one ever does: the local index is the one the server built from this
    // mount's own content, and it is the one every other recall tool already uses, so
    // preferring it keeps a mount's tools answering from a single source.
    if state.runtimes.for_mount(&resolved.mount.id).is_some() {
        return mount_index(state, tool, resolved.mount).map(RecallTarget::Local);
    }
    if include_native_recall && mount_serves_native_recall(resolved.mount) {
        return Ok(RecallTarget::Native(NativeRecallMount {
            id: resolved.mount.id.clone(),
            mount_at: resolved.mount.mount_at.clone(),
            backend: resolved.mount.backend.clone(),
        }));
    }
    mount_index(state, tool, resolved.mount).map(RecallTarget::Local)
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
                runtime: root_index(state, tool)?,
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

/// The runtime backing one mount, or a clear refusal when that mount has no LOCAL
/// index.
///
/// # This is a designed path, not a bug path
///
/// It used to be unreachable — every backend was a filesystem vault, every mount had
/// an index. It is now the answer for a mount whose backend serves its own content and
/// therefore has no local index at all (an Algolia-backed shared corpus; see
/// [`crate::runtime::mount_has_local_index`]). So the wording must not imply a
/// malfunction: it says WHY there is no index, and names what does work on such a
/// mount, because a caller who reads "has no index." with no explanation will go
/// looking for a broken build.
///
/// # What the refusal must now also say
///
/// A mount reaching this may still serve `hybrid_search` and `load_knowledge` — natively,
/// through its own index (see [`resolve_recall_target`]). So the remedy list names those
/// two by name for a `NativeRecall` mount. Without that, the one refusal a user hits on
/// such a mount would point them AWAY from the recall that does work, which is a worse
/// failure than the refusal itself.
///
/// What it must NOT do is suggest them for a mount that cannot serve them either — hence
/// the capability check rather than a blanket sentence.
fn mount_index(
    state: &AppState,
    tool: &str,
    mount: &deep_obsidian_backend::Mount,
) -> Result<ScopedIndex, String> {
    let runtime = state
        .runtimes
        .for_mount(&mount.id)
        .ok_or_else(|| {
            let native_recall = if mount_serves_native_recall(mount) {
                format!(
                    " This mount does answer RANKED SEARCH itself, so hybrid_search and \
                     load_knowledge scoped to '{}' are served by its own index — what cannot be \
                     served here is anything needing a link graph, a similarity neighbourhood or \
                     an artifact embedding table, none of which a remote corpus exposes.",
                    if mount.mount_at.is_empty() {
                        "/"
                    } else {
                        mount.mount_at.as_str()
                    }
                )
            } else {
                String::new()
            };
            format!(
                "{tool} cannot be scoped to mount '{}': that mount has no local search index. Its \
                 backend ({}) serves its own content — the remote index IS the corpus — so there \
                 is nothing local to rank over, and building a copy would serve one participant's \
                 stale snapshot. Read and write notes on this mount normally (read_file, \
                 upsert_note, list_children, note_outline), use grep_search with a 'glob' under \
                 the mount for line search, and scope index-backed recall to a mount that has an \
                 index: {}.{native_recall}",
                mount.id,
                mount.backend.descriptor().kind.as_str(),
                mount_scope_hint(state, false)
            )
        })?
        .clone();
    Ok(ScopedIndex {
        runtime,
        mount_at: mount.mount_at.clone(),
    })
}

/// The ROOT mount's index runtime, or the honest refusal when the root has none.
///
/// # Why this exists rather than an `expect`
///
/// Everything about the vault ROOT itself — the vault overview, `build_index`'s root
/// pass, `recommend_folder`, and the single-mount fast paths in
/// [`resolve_recall_target`] / [`resolve_recall_path`] — used to be able to assume a root
/// index existed, because the root mount was always a filesystem one. It is not anymore:
/// an ALGOLIA root has no local index by design (the remote index IS the corpus; see
/// [`crate::runtime::mount_has_local_index`]), and a single-mount algolia-rooted config
/// therefore has no local index anywhere.
///
/// A couchdb root is unaffected — it has a local index — so a fully-remote LiveSync vault
/// keeps every index-backed tool.
///
/// The refusal is deliberately [`mount_index`]'s, verbatim, rather than a second wording
/// for the same fact: it already explains WHY there is no index, names what does work on
/// such a mount (plain reads and writes, `grep_search` with a glob), and names the
/// natively-served recall a `NativeRecall` mount can still answer. Inventing a
/// root-specific message would have said less and drifted.
pub(crate) fn root_index(state: &AppState, tool: &str) -> Result<Arc<RuntimeState>, String> {
    if let Some(runtime) = state.runtime() {
        return Ok(runtime.clone());
    }
    let root = state
        .router
        .root()
        // `normalize_service_config` rejects a rootless mount table
        // (`ConfigError::MissingRootMount`), so this cannot happen for a resolved config.
        .expect("a resolved config to declare a root mount");
    mount_index(state, tool, root).map(|scoped| scoped.runtime)
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
/// on. And `mounts[]` is purely additive, appearing only where no golden describes the
/// payload — see [`crate::health::insert_mount_index_detail`], which places the array
/// this joins onto and owns that condition. Note that "no golden describes it" is no
/// longer the same as "multi-mount": a couchdb mount CAN now be the only mount in a
/// table, because it can be the root, and such a vault does get this report.
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

// ---------------------------------------------------------------------------
// Version history, soft delete, divergence
// ---------------------------------------------------------------------------

/// Refuse a capability-gated tool for a path whose mount cannot serve it.
///
/// # Why the refusal is composed here rather than left to the backend
///
/// Both would work — every backend refuses these requests with its own message — but the
/// backend's refusal is about a STORAGE MODEL and cannot name the mount, and on a
/// multi-mount vault "a filesystem vault keeps no version history" leaves a caller
/// wondering which of their mounts that was. So this names the mount, its backend kind and
/// the tool, and then lets the caller find the mounts that DO work through the one payload
/// that lists them per mount.
///
/// The backends still refuse independently, which is not redundant: it is what makes the
/// boundary honest for any future caller that skips this check.
fn refuse_incapable_mount(
    state: &AppState,
    tool: &str,
    path: &str,
    capability: Capability,
) -> Result<(), String> {
    let resolved = state
        .router
        .resolve(path)
        .map_err(|error| error.to_string())?;
    if resolved.mount.backend.descriptor().supports(capability) {
        return Ok(());
    }
    let capable: Vec<String> = state
        .router
        .mounts()
        .iter()
        .filter(|mount| mount.backend.descriptor().supports(capability))
        .map(|mount| {
            if mount.mount_at.is_empty() {
                "the vault root".to_string()
            } else {
                format!("'{}/'", mount.mount_at)
            }
        })
        .collect();
    let alternatives = if capable.is_empty() {
        "No mount in this vault supports it.".to_string()
    } else {
        format!("Mounts that do support it: {}.", capable.join(", "))
    };
    let kind = resolved.mount.backend.descriptor().kind;
    Err(format!(
        "{tool} cannot be used on {path}: it is served by mount '{}' (backend: {}), which does not \
         support this operation. {} {alternatives} See vault_info.mounts[].capabilities.",
        resolved.mount.id,
        kind.as_str(),
        match (capability, kind) {
            // A remote mount that lacks `soft-delete` lacks it for ONE reason — it was not
            // opted in to writes — and the reason is a setting the reader can change. The
            // local-deletion sentence below would be a false claim here: the removal WOULD
            // be observable to every other participant and recoverable, which is exactly
            // why these backends implement it. This is the lesson the couchdb read-only
            // refusal already carries: a refusal that misstates its own cause sends the
            // reader looking in the wrong place.
            (Capability::SoftDelete, BackendKind::Couchdb | BackendKind::Algolia) => {
                "This backend CAN soft-delete — the removal is observable to every other \
                 participant and it is recoverable — but this mount is read-only: its mount \
                 configuration does not set \"writable\": true, so it advertises no delete and \
                 accepts none. Set it and restart the service to allow deletes here."
            }
            (Capability::SoftDelete, _) => {
                "Removing a note here would be an ordinary file deletion, which is observable to \
                 nobody and recoverable from nothing, and this MCP surface deliberately exposes \
                 no deletion of local vault files — delete the note yourself instead."
            }
            // Same discipline in the other direction: CouchDB genuinely retains revisions,
            // so "one content per note by construction" would be false. What is missing is a
            // way to reach them, and no setting adds one.
            (Capability::VersionHistory, BackendKind::Couchdb) =>
                "This storage does retain revisions, but nothing can enumerate or fetch one \
                 through this server (the sidecar protocol has no such call, and CouchDB's \
                 compaction removes older revisions anyway), so there is no version to list, \
                 read or reconcile. No configuration turns this on.",
            (Capability::VersionHistory, _) =>
                "This storage keeps one content per note by construction, so there is no \
                 superseded version to list, read or reconcile.",
            _ => "",
        },
    ))
}

/// `delete_note`: soft-delete a note through the router.
///
/// Refuses a path on a mount without [`Capability::SoftDelete`] — which is every
/// filesystem mount, and therefore every path in a single-mount vault. That refusal is the
/// contract PR #40 pinned as `delete_note_refuses_local_paths`, and it is why this tool
/// gained no destructive local capability by existing.
async fn delete_note_payload(state: &AppState, path: &str) -> Result<Value, String> {
    refuse_incapable_mount(state, "delete_note", path, Capability::SoftDelete)?;
    // Read BEFORE the delete, because the answer decides the wording below and a mount's
    // descriptor is a pure function of its configuration — but reading it first also means
    // a failure here cannot leave a caller with a tombstone and no guidance.
    let has_history = mount_supports(state, path, Capability::VersionHistory);
    let outcome = backend_call(state, BackendRequest::soft_delete(path))
        .await?
        .into_soft_delete()
        .map_err(|error| error.to_string())?;
    let mut payload = Map::from_iter([
        ("path".to_string(), json!(path)),
        ("deleted".to_string(), json!(true)),
        ("alreadyDeleted".to_string(), json!(outcome.already_deleted)),
        ("versionId".to_string(), json!(outcome.version_id)),
    ]);
    if let Some(recoverable) = &outcome.recoverable_from {
        payload.insert("recoverableFrom".to_string(), json!(recoverable));
    }
    payload.insert(
        "howToRecover".to_string(),
        json!(match (&outcome.recoverable_from, has_history) {
            // Byte-identical to what this has always emitted for a history-keeping mount.
            (Some(recoverable), true) => format!(
                "read_version with versionId {recoverable} returns the removed content; \
                 upsert_note it back to undelete the note."
            ),
            _ => NO_HISTORY_RECOVERY.to_string(),
        }),
    );
    Ok(Value::Object(payload))
}

/// How to undo a delete on a mount with no version history.
///
/// # Why this is not "the content is gone"
///
/// It would be the safe-sounding thing to say and it is not true. A CouchDB (LiveSync)
/// tombstone is the entry document with `deleted: true` set on it; its `children` list is
/// untouched, so the chunks the note was made from are still stored and still referenced.
/// A read of the path therefore still returns the last content — that is pre-existing,
/// documented behaviour of this mount, not something a delete changed — and writing it
/// back resurrects the note. Telling a caller the content was destroyed would send them
/// off to a CouchDB backup for something a read will hand them.
///
/// Both read tools are named because a delete here can reach an ATTACHMENT: this mount
/// stores binaries, so removing one is legitimate, and a `newnote` entry read as text is
/// refused by design. Naming only `read_file` would point half of the callers at a tool
/// that refuses. See `couchdb_sidecar.rs::an_attachment_can_be_tombstoned_and_its_bytes_survive`.
///
/// # Why it is not `read_version` either
///
/// That is the sentence this exists to avoid. A mount can have `soft-delete` and NOT
/// `version-history`, and a couchdb mount is exactly that: CouchDB keeps older revisions
/// but compaction deletes them and the sidecar protocol cannot fetch one, so there is no
/// versionId to hand back. Pointing at `read_version` would name a tool that is not even
/// registered for such a vault.
///
/// What IS lost is everything older than the last content, which the message says rather
/// than leaving the reader to assume a history exists.
///
/// The couchdb mount is the ONLY backend that reaches this today, which is why the wording
/// is concrete about what survives rather than hedging. A future backend with `soft-delete`
/// and no `version-history` whose tombstone keeps nothing would need its own sentence here,
/// not this one.
const NO_HISTORY_RECOVERY: &str = "this mount keeps no version history, so there is no \
versionId to read back and nothing older than the note's last content survives. That last \
content is still there: reading this path still returns it (the tombstone keeps the stored \
content) — read_file for a note, read_artifact for an attachment — and writing it back with \
upsert_note resurrects it at this path, on every device that syncs this vault.";

/// Whether the mount owning `path` advertises `capability`.
///
/// A resolution failure answers `false` rather than propagating: every caller uses this to
/// choose WORDING, and a path that does not resolve has already failed for a better reason
/// somewhere else.
fn mount_supports(state: &AppState, path: &str, capability: Capability) -> bool {
    state
        .router
        .resolve(path)
        .map(|resolved| resolved.mount.backend.descriptor().supports(capability))
        .unwrap_or(false)
}

/// One version, rendered.
///
/// Every key is present on every entry, `null` where the link does not exist. PR #40 built
/// history entries and the head entry from two separate `json!` literals, so an archived
/// version carried `supersededBy` while the head carried `parentVersionId` and neither
/// carried the other. That asymmetry was an artifact of the two literals, not a contract:
/// a client walking `versions[]` had to know which entry it was looking at before it knew
/// which keys existed. One shape is the additive fix — every key #40 emitted is still
/// emitted, with the same name and meaning.
fn note_version_json(version: &deep_obsidian_backend::NoteVersion) -> Value {
    json!({
        "versionId": version.version_id,
        "participantId": version.participant_id,
        "updatedAtMs": version.updated_at_ms,
        "parentVersionId": version.parent_version_id,
        "forkedFrom": version.forked_from,
        "supersededBy": version.superseded_by,
        "current": version.current,
    })
}

/// The note the truncation flag carries, so a caller knows what it is missing and how
/// to see it. Sibling to [`FEDERATED_FIND_FILES_TRUNCATION_NOTE`]'s discipline.
const NOTE_HISTORY_TRUNCATION_NOTE: &str = "this note has more retained versions than \
'limit'; the newest were returned and the oldest were dropped. 'totalCount' is how many the \
mount retains. Raise 'limit' to see further back.";

/// `note_history`: a note's retained versions, newest first, at most `limit` of them.
///
/// # Why the limit is applied HERE and not pushed into the boundary
///
/// It reads like it belongs on `ManifestRequest::Versions` — the backend would then fetch
/// fewer records. It must not go there, because `resolve_divergence_payload` issues the
/// same request and then does `.find(|version| version.current)`, treating a missing
/// current version as a hard error. A limit inside the request would let a truncated list
/// drop the head and turn a perfectly ordinary divergence into a failure. Slicing above
/// the boundary keeps the two callers independent, and matches how `find_files` already
/// works: the walk is unbounded and the tool layer decides what to show.
///
/// The cost is that a note with a thousand retained versions still assembles a thousand
/// records inside the mount. That is the ACKNOWLEDGED shape of this fix: the measured
/// problem was an O(versions) payload handed to a client with no way to ask for less
/// (0.94 ms and growing at ~70 versions), and this bounds the payload. Bounding the fetch
/// as well needs a limit the divergence path can opt out of, which is a boundary change
/// worth doing on its own evidence rather than smuggling in here.
///
/// # `count` keeps its meaning
///
/// `count` has always been "how many versions are in `versions`", and it still is —
/// nothing that reads it needs rewriting. When the list was cut short, `totalCount` says
/// how many the mount retains, and it appears ONLY then: an untruncated answer has no new
/// field, so nothing about the pre-existing shape moves.
async fn note_history_payload(state: &AppState, path: &str, limit: usize) -> Result<Value, String> {
    refuse_incapable_mount(state, "note_history", path, Capability::VersionHistory)?;
    let history = backend_call(state, BackendRequest::note_versions(path))
        .await?
        .into_note_history()
        .map_err(|error| error.to_string())?;
    let total = history.versions.len();
    // Newest first is the order the mount returns and the order this tool documents, so
    // taking a prefix keeps the most recent versions and drops the oldest — which is the
    // useful direction, and the reason no reordering is needed.
    let versions: Vec<Value> = history
        .versions
        .iter()
        .take(limit)
        .map(note_version_json)
        .collect();
    let mut payload = Map::from_iter([
        ("path".to_string(), json!(path)),
        ("hasDivergence".to_string(), json!(history.has_divergence)),
        ("count".to_string(), json!(versions.len())),
        ("versions".to_string(), json!(versions)),
    ]);
    // Only when true, and never as `false`. A caller that sees no `truncated` key has the
    // whole history, which is what every caller written before this had.
    if total > limit {
        payload.insert("truncated".to_string(), json!(true));
        payload.insert("totalCount".to_string(), json!(total));
        payload.insert(
            "truncationNote".to_string(),
            json!(NOTE_HISTORY_TRUNCATION_NOTE),
        );
    }
    Ok(Value::Object(payload))
}

/// `read_version`: one named version's text, with the hash of THAT text.
///
/// The hash is over the version's own bytes, computed with the same helper `read_file` and
/// the write tools use — so it can be compared against a `read_file` hash to answer "is
/// the note still what this version said". It is emphatically NOT usable as an
/// `expectedHash` for a write: that guard compares against the CURRENT content, and a
/// historical hash would never match unless the note happens to be unchanged.
async fn read_version_payload(
    state: &AppState,
    path: &str,
    version_id: &str,
) -> Result<Value, String> {
    refuse_incapable_mount(state, "read_version", path, Capability::VersionHistory)?;
    let text = backend_call(state, BackendRequest::read_text_version(path, version_id))
        .await?
        .into_text()
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "path": path,
        "versionId": version_id,
        "hash": content_hash(text.as_bytes()),
        "text": text,
    }))
}

/// `resolve_divergence`: everything needed for a three-way merge, and no merge.
///
/// # What clears the divergence, and what does not
///
/// NOT this tool. It is read-only by construction: it returns the current head, the
/// version that head overtook, and their common ancestor. The mark is cleared by writing
/// the merged content with `upsert_note` and `resolveDivergence: true`, which is a claim
/// only the caller can make — see [`resolve_divergence_property`]. Ported from PR #40
/// unchanged, including the reason: a wrong automatic merge produces plausible text and is
/// nearly undetectable, so the server must not produce one.
///
/// # Payload shape, and the asymmetry that is deliberate
///
/// `head` is `{versionId, participantId, text}` — it has no `hash`, because the head's
/// hash is what `read_file` already reports for this path and duplicating it here would
/// invite a client to feed the wrong one back as `expectedHash`. `overtaken` and
/// `commonAncestor` are full `read_version` payloads, `{path, versionId, hash, text}`,
/// because that is what they are: historical reads. #40 had exactly this asymmetry and it
/// is preserved.
///
/// A version that retention has already purged is OMITTED rather than reported as an
/// error: the head and whichever side survives are still useful, and failing the whole
/// call would leave a caller with nothing at all for an old divergence.
async fn resolve_divergence_payload(state: &AppState, path: &str) -> Result<Value, String> {
    refuse_incapable_mount(
        state,
        "resolve_divergence",
        path,
        Capability::VersionHistory,
    )?;
    let history = backend_call(state, BackendRequest::note_versions(path))
        .await?
        .into_note_history()
        .map_err(|error| error.to_string())?;
    if !history.has_divergence {
        return Ok(json!({
            "path": path,
            "hasDivergence": false,
            "note": "no divergence recorded on this note",
        }));
    }
    let head = history
        .versions
        .iter()
        .find(|version| version.current)
        .ok_or_else(|| {
            format!("{path} reports a divergence but its history names no current version")
        })?;
    let head_text = backend_call(
        state,
        BackendRequest::read_text_version(path, &head.version_id),
    )
    .await?
    .into_text()
    .map_err(|error| error.to_string())?;

    let mut payload = Map::from_iter([
        ("path".to_string(), json!(path)),
        ("hasDivergence".to_string(), json!(true)),
        (
            "head".to_string(),
            json!({
                "versionId": head.version_id,
                "participantId": head.participant_id,
                "text": head_text,
            }),
        ),
    ]);
    // `forkedFrom` is the head this version DISPLACED; `parentVersionId` is what its
    // content was based on. Those are the two other corners of the merge, and they are
    // different versions — which is exactly why the fork records both.
    for (key, version_id) in [
        ("overtaken", head.forked_from.as_deref()),
        ("commonAncestor", head.parent_version_id.as_deref()),
    ] {
        let Some(version_id) = version_id else {
            continue;
        };
        if let Ok(value) = read_version_payload(state, path, version_id).await {
            payload.insert(key.to_string(), value);
        }
    }
    payload.insert(
        "howToResolve".to_string(),
        json!(
            "Merge 'head' and 'overtaken' against 'commonAncestor' yourself, then call \
             upsert_note with the merged content and resolveDivergence: true to clear the mark. \
             The server never merges: a wrong automatic merge produces plausible text and is \
             nearly undetectable."
        ),
    );
    Ok(Value::Object(payload))
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
            let snapshot = root_index(state, "vault_info")?
                .fresh_snapshot("vault_info")
                .await?;
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
            let listing = backend_call(
                state,
                BackendRequest::list_children(path.clone(), include_hidden, include_ignored),
            )
            .await?
            .into_child_listing()
            .map_err(|error| error.to_string())?;
            let entries = listing.entries;
            let mut result = if folders_only {
                let folders = entries
                    .into_iter()
                    .filter(|entry| matches!(entry.kind, VaultEntryKind::Directory))
                    .map(|entry| entry.path)
                    .collect::<Vec<_>>();
                Map::from_iter([
                    ("path".to_string(), json!(path)),
                    ("foldersOnly".to_string(), json!(true)),
                    ("count".to_string(), json!(folders.len())),
                    ("folders".to_string(), json!(folders)),
                ])
            } else {
                Map::from_iter([
                    ("path".to_string(), json!(path)),
                    ("foldersOnly".to_string(), json!(false)),
                    ("count".to_string(), json!(entries.len())),
                    (
                        "children".to_string(),
                        json!(entries
                            .into_iter()
                            .map(|entry| vault_child_entry_json(&entry))
                            .collect::<Vec<_>>()),
                    ),
                ])
            };
            // Emitted ONLY when the subfolder half of the listing was cut short, which no
            // backend whose directories are real directories can be. That is what keeps
            // this payload byte-identical for every filesystem and couchdb mount — the
            // key simply never appears — while a facet-enumerated mount can say so.
            //
            // Both `foldersOnly` shapes carry it: a caller that asked ONLY for folders is
            // precisely the caller a truncated folder list misleads most.
            if listing.folders_truncated {
                result.insert("foldersTruncated".to_string(), json!(true));
                result.insert(
                    "foldersTruncatedReason".to_string(),
                    json!(FOLDERS_TRUNCATED_REASON),
                );
            }
            Ok(json_text_result(Value::Object(result)))
        }
        "read_file" => {
            let path = string_arg(arguments, "path")?;
            validate_format_arg(arguments)?;
            let text_options = TextPayloadOptions::from_arguments(arguments, true);
            let known_hash = optional_string_arg(arguments, "knownHash");
            // The hash goes DOWN to the backend rather than only being compared up here.
            //
            // On a filesystem mount that changes nothing: it cannot know what a file
            // hashes to without reading it, says so, and answers with the text exactly as
            // before. On a mount where a read is a network conversation that reassembles
            // the note out of chunks — the CouchDB and Algolia shapes — the provider can
            // often establish "still that hash" from metadata it holds anyway, and then
            // the whole hydration is skipped rather than only the response body. That was
            // the finding: through a couchdb mount, `knownHash` used to save nothing
            // measurable (2.64 ms against 2.60 ms) because the body was the only part it
            // saved and the body was the cheap part.
            //
            // A backend that skips the read answers `Unchanged`. A backend that does not
            // answers with the text, and the comparison below then runs exactly as it
            // always has — so this is one code path with two entry points, not two
            // behaviours.
            let read = match &known_hash {
                Some(known_hash) => {
                    backend_call(
                        state,
                        BackendRequest::read_text_known_hash(&path, known_hash),
                    )
                    .await?
                }
                None => backend_call(state, BackendRequest::read_text(&path)).await?,
            };
            let (text, hash) = match read
                .into_text_unless_unchanged()
                .map_err(|error| error.to_string())?
            {
                // The backend proved the caller's copy is current without materializing
                // the note. `known_hash` is `Some` by construction — no other request
                // shape can be answered this way.
                (None, _) => {
                    let result = Map::from_iter([
                        ("path".to_string(), json!(path.clone())),
                        ("hash".to_string(), json!(known_hash.unwrap_or_default())),
                        ("unchanged".to_string(), json!(true)),
                    ]);
                    return Ok(json_text_result_from_arguments(
                        arguments,
                        Value::Object(result),
                    ));
                }
                // Full-file content hash, computed with the same helper the write tools use
                // so a write's `newHash` can be fed straight back into a read's
                // `knownHash`. Always the full-file hash regardless of any
                // startLine/endLine slice.
                (Some(text), _) => {
                    let hash = content_hash(text.as_bytes());
                    (text, hash)
                }
            };
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
        // Federated on a multi-mount vault, and the router does all of it: `walk_markdown`
        // now concatenates every mount's notes in the logical namespace, so the match and the
        // truncation below run over the whole vault exactly as they run over one.
        //
        // This tool is an ENUMERATION filtered by a path match, not a ranking — the matcher
        // is a substring or regex test and the results are the first `limit` in walk order —
        // which is why it federates while `recommend_folder` still refuses. See
        // `require_single_mount`.
        "find_files" => {
            let query = string_arg(arguments, "query")?;
            let mode = optional_enum_string_arg(arguments, "mode", &["substring", "regex"])?
                .unwrap_or_else(|| "substring".to_string());
            let limit = clamped_usize_arg(arguments, "limit", 20, 1, 200);
            let files = backend_call(state, BackendRequest::walk_markdown())
                .await?
                .into_markdown_files()
                .map_err(|error| error.to_string())?;
            let found = live_find_file_matches(files, &query, &mode, limit)?;
            let truncated = found.len() >= limit;
            let matches = found.iter().map(file_path_match_json).collect::<Vec<_>>();
            let mut result = Map::from_iter([
                ("query".to_string(), json!(query)),
                ("mode".to_string(), json!(mode)),
                ("count".to_string(), json!(matches.len())),
                ("matches".to_string(), json!(matches)),
            ]);
            // Truncation honesty, and MULTI-MOUNT ONLY so the frozen single-mount goldens are
            // untouched. It matters more here than on one vault: the merged walk is ordered by
            // logical path, so a full result set is the alphabetically first `limit` matches
            // and a whole mount's notes can sit entirely past the cut. On a single vault the
            // same truncation has always been silent, and that shape is frozen.
            if truncated && state.router.is_multi_mount() {
                result.insert("truncated".to_string(), json!(true));
                result.insert(
                    "truncationNote".to_string(),
                    json!(FEDERATED_FIND_FILES_TRUNCATION_NOTE),
                );
            }
            Ok(json_text_result(Value::Object(result)))
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
            let outcome = backend_call(
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
            .into_grep_outcome()
            .map_err(|error| error.to_string())?;
            let matches = outcome
                .matches
                .iter()
                .map(|item| grep_match_json(item, text_options))
                .collect::<Vec<_>>();
            let mut result = Map::from_iter([
                ("query".to_string(), json!(query)),
                ("regex".to_string(), json!(regex_mode)),
                ("caseSensitive".to_string(), json!(case_sensitive)),
                ("glob".to_string(), json!(glob)),
                ("contextLines".to_string(), json!(context_lines)),
                ("count".to_string(), json!(matches.len())),
                ("matches".to_string(), json!(matches)),
            ]);
            // Emitted ONLY when the search was NOT exhaustive.
            //
            // The asymmetry is the point. `grep_search` has always meant ripgrep, which
            // reads every file, so an absent `exhaustive` key keeps meaning exactly what
            // it has always meant and every existing assertion and golden is untouched.
            // A backend that cannot read every file says so explicitly, and says how many
            // candidates it did examine — the number that tells a caller whether raising
            // the bound would help.
            if !outcome.exhausted {
                result.insert("exhaustive".to_string(), json!(false));
                if let Some(candidate_count) = outcome.candidate_count {
                    result.insert("candidateCount".to_string(), json!(candidate_count));
                }
                result.insert(
                    "exhaustiveNote".to_string(),
                    json!(NON_EXHAUSTIVE_GREP_NOTE),
                );
            }
            // A federated grep that lost a mount says WHICH one, and marks itself degraded.
            //
            // `exhaustive: false` above already fired, but on its own it cannot distinguish
            // "a backend caps its candidate set" from "part of your vault is offline", and
            // those have different remedies. Emitted only when non-empty, so a single-mount
            // grep and any successfully routed one are byte-identical to before.
            if !outcome.missing_mounts.is_empty() {
                result.insert("degraded".to_string(), json!(true));
                result.insert(
                    "missingBackends".to_string(),
                    json!(outcome.missing_mounts.clone()),
                );
                result.insert(
                    "degradationReason".to_string(),
                    json!(format!(
                        "these vault mounts could not be searched, so matches inside them are                          missing from this answer rather than absent from the vault: {}",
                        outcome.missing_mounts.join(", ")
                    )),
                );
            }
            Ok(json_text_result_from_arguments(
                arguments,
                Value::Object(result),
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
            let root_snapshot = root_index(state, "build_index")?
                .rebuild("manual build_index")
                .await?;
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
            // A mount with no LOCAL index is reported as skipped rather than omitted.
            // Omitting it would make `mounts` a silently incomplete list of the vault's
            // mounts, which is worse than an entry saying there was nothing to rebuild.
            for mount in state.router.mounts() {
                if state.runtimes.for_mount(&mount.id).is_some() {
                    continue;
                }
                mount_results.push(json!({
                    "id": mount.id,
                    "mountAt": mount.mount_at,
                    "rebuilt": false,
                    "skipped": true,
                    "reason": format!(
                        "mount '{}' has no local search index to rebuild: its backend ({}) serves \
                         its own content",
                        mount.id,
                        mount.backend.descriptor().kind.as_str()
                    ),
                }));
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
            // byte-identical to the frozen shape. Gated on the ROUTER rather than on
            // the runtime table: a vault with a filesystem root plus an Algolia mount
            // has one runtime and two mounts, and it is the second one a client needs
            // to be told about.
            if state.router.is_multi_mount() {
                result.insert("mounts".to_string(), json!(mount_results));
            }
            Ok(json_text_result(Value::Object(result)))
        }
        "hybrid_search" => {
            let target = resolve_recall_target(
                state,
                "hybrid_search",
                optional_string_arg(arguments, "scope").as_deref(),
                true,
            )?;
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            // A mount that ranks for itself: served by its backend, in the same payload
            // shape, with its provenance stated. See `insert_native_recall_provenance`.
            let scoped = match target {
                RecallTarget::Local(scoped) => scoped,
                // Unscoped on a multi-mount vault: every mount, fused.
                RecallTarget::Federated => {
                    let text_options =
                        TextPayloadOptions::search_snippet_from_arguments(arguments, true);
                    return Ok(json_text_result_from_arguments(
                        arguments,
                        federated_hybrid_search_payload(state, &query, limit, text_options).await?,
                    ));
                }
                RecallTarget::Native(mount) => {
                    let text_options =
                        TextPayloadOptions::search_snippet_from_arguments(arguments, true);
                    let response = native_recall_search(&mount, &query, limit).await?;
                    let mut match_values = response
                        .hits
                        .iter()
                        .map(|hit| {
                            native_recall_match_json(
                                hit,
                                &mount.to_logical(&hit.path),
                                text_options,
                            )
                        })
                        .collect::<Vec<_>>();
                    let response_truncated = apply_response_text_budget(
                        &mut match_values,
                        "text",
                        RESPONSE_TEXT_BUDGET_CHARS,
                    );
                    let mut result = Map::new();
                    result.insert("query".to_string(), json!(query));
                    insert_native_recall_provenance(&mut result, &response, &mount);
                    result.insert("count".to_string(), json!(match_values.len()));
                    result.insert("matches".to_string(), json!(match_values));
                    insert_response_truncation_flags(&mut result, response_truncated);
                    return Ok(json_text_result_from_arguments(
                        arguments,
                        Value::Object(result),
                    ));
                }
            };
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
            let target = resolve_recall_target(
                state,
                "search_artifacts",
                optional_string_arg(arguments, "scope").as_deref(),
                // An artifact embedding table is built by the server from binary files it
                // read itself; no backend answers artifact search natively.
                false,
            )?;
            let query = string_arg(arguments, "query")?;
            validate_format_arg(arguments)?;
            let limit = clamped_usize_arg(arguments, "limit", 8, 1, 50);
            let scoped = match target {
                RecallTarget::Local(scoped) => scoped,
                RecallTarget::Federated => {
                    return Ok(json_text_result_from_arguments(
                        arguments,
                        federated_search_artifacts_payload(state, &query, limit).await?,
                    ));
                }
                // Unreachable: `include_native_recall: false` never produces this variant.
                // An error rather than an `unreachable!` because a panic in a tool handler
                // takes down the request, and a future caller that flips the flag deserves
                // a message rather than a crash.
                RecallTarget::Native(_) => {
                    return Err(
                        "search_artifacts cannot be served by a mount's own index".to_string()
                    )
                }
            };
            let snapshot = scoped.runtime.fresh_snapshot("search_artifacts").await?;
            let matches = artifact_search_matches(snapshot.index, query.clone(), limit).await?;
            Ok(json_text_result_from_arguments(
                arguments,
                json!({
                    "query": query,
                    "rebuilt": snapshot.rebuilt,
                    "count": matches.len(),
                    "matches": matches
                        .iter()
                        .map(|item| artifact_search_match_json(&index_search::ArtifactSearchMatch {
                            // Logical path: identity on the root mount.
                            path: scoped.to_logical(&item.path),
                            ..item.clone()
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
        "rename_note" => {
            let from = string_arg(arguments, "from")?;
            let to = string_arg(arguments, "to")?;
            for (label, path) in [("from", &from), ("to", &to)] {
                if !path.to_lowercase().ends_with(".md") {
                    return Err(format!(
                        "rename_note requires a vault-relative .md path; `{label}` is not one."
                    ));
                }
            }
            if from == to {
                return Err("rename_note was given the same path twice.".to_string());
            }
            refuse_incapable_mount(state, "rename_note", &from, Capability::Rename)?;
            let dry_run = bool_arg(arguments, "dryRun", false);
            let rewrite_links = bool_arg(arguments, "rewriteLinks", true);
            let expected_hash = expected_hash_arg(arguments);

            let (existing, base_version) = backend_call(state, BackendRequest::read_text(&from))
                .await?
                .into_versioned_text()
                .map(|(text, version)| (text, BaseVersion::from_read(version)))
                .map_err(|error| error.to_string())?;
            validate_expected_hash(
                expected_hash.as_deref(),
                Some(&content_hash(existing.as_bytes())),
                &from,
            )?;

            // Two refusals, both cases where finishing the move would change something the
            // caller never asked to change.
            let notes = backend_call(state, BackendRequest::walk_markdown())
                .await?
                .into_markdown_files()
                .map_err(|error| error.to_string())?;
            if notes.iter().any(|note| note == &to) {
                return Err(format!(
                    "refusing to rename {from} to {to}: a note already exists there, and \
                     renaming onto it would destroy it. Pick another path, or remove that note \
                     first if replacing it is what you meant."
                ));
            }
            let destination_basename = note_basename(&to);
            if let Some(collision) = notes
                .iter()
                .find(|note| *note != &from && note_basename(note) == destination_basename)
            {
                return Err(format!(
                    "refusing to rename {from} to {to}: the basename {destination_basename:?} is \
                     already used by {collision}. Obsidian resolves a short [[{}]] link by \
                     basename, so finishing this move would silently change where such links in \
                     unrelated notes point. Rename to a distinct basename.",
                    strip_md_extension(destination_basename)
                ));
            }
            // Whether the OLD basename was unique decides whether short links may be
            // rewritten: if it was not, `[[Name]]` never unambiguously meant this note.
            let from_basename = note_basename(&from);
            let old_basename_was_unique = !notes
                .iter()
                .any(|note| note != &from && note_basename(note) == from_basename);

            let (scoped, mount_path) = resolve_recall_path(state, "rename_note", &from)?;
            let snapshot = scoped.runtime.fresh_snapshot("rename_note").await?;
            let graph = index_graph::graph_traverse(
                &snapshot.index,
                &mount_path,
                index_graph::GraphDirection::Incoming,
                1,
                500,
            )
            .map_err(|error| error.to_string())?;
            let linking: Vec<String> = graph
                .nodes
                .into_iter()
                .filter(|node| node.depth > 0)
                .map(|node| scoped.to_logical(&node.path))
                .collect();

            if dry_run {
                return Ok(json_text_result(json!({
                    "from": from,
                    "to": to,
                    "dryRun": true,
                    "linkingNotes": linking,
                    "oldBasenameWasUnique": old_basename_was_unique,
                })));
            }

            let renamed = backend_call(
                state,
                BackendRequest::Mutation(MutationRequest::Rename {
                    from: from.clone(),
                    to: to.clone(),
                    base_version,
                }),
            )
            .await?;
            let atomic = matches!(
                renamed,
                BackendResponse::Mutation(MutationResponse::Renamed { atomic: true, .. })
            );

            // The repair pass. Each note is its own write, so a failure leaves the rest done
            // and the response names the ones that were not — re-running the same rename
            // finishes them, because a rewritten note has no old-path link left to match.
            let mut rewritten = Vec::new();
            let mut failed = Vec::new();
            if rewrite_links {
                for note in &linking {
                    match rewrite_links_in_note(state, note, &from, &to, old_basename_was_unique)
                        .await
                    {
                        Ok(0) => {}
                        Ok(count) => rewritten.push(json!({"path": note, "links": count})),
                        Err(reason) => failed.push(json!({"path": note, "error": reason})),
                    }
                }
            }

            let mut payload = json!({
                "from": from,
                "to": to,
                "atomic": atomic,
                "dryRun": false,
                "resourceUri": note_uri(&to),
                "wikiLink": note_wiki_link(&to),
                "linkingNotes": linking,
                "rewroteLinksIn": rewritten,
            });
            if !failed.is_empty() {
                payload["linkRewritesFailed"] = json!(failed);
                payload["howToRecover"] = json!(
                    "the note moved and some inbound links still point at the old path. \
                     Re-run the same rename_note call: the pass is idempotent, so notes already \
                     rewritten are untouched and only these are retried."
                );
            }
            Ok(json_text_result(payload))
        }
        "delete_note" => {
            let path = string_arg(arguments, "path")?;
            Ok(json_text_result(delete_note_payload(state, &path).await?))
        }
        "note_history" => {
            let path = string_arg(arguments, "path")?;
            let limit = clamped_usize_arg(arguments, "limit", 50, 1, 500);
            Ok(json_text_result(
                note_history_payload(state, &path, limit).await?,
            ))
        }
        "read_version" => {
            let path = string_arg(arguments, "path")?;
            let version_id = string_arg(arguments, "versionId")?;
            Ok(json_text_result(
                read_version_payload(state, &path, &version_id).await?,
            ))
        }
        "resolve_divergence" => {
            let path = string_arg(arguments, "path")?;
            Ok(json_text_result(
                resolve_divergence_payload(state, &path).await?,
            ))
        }
        "load_knowledge" => {
            let target = resolve_recall_target(
                state,
                "load_knowledge",
                optional_string_arg(arguments, "scope").as_deref(),
                true,
            )?;
            let subject = string_arg(arguments, "subject")?;
            validate_format_arg(arguments)?;
            let project = optional_string_arg(arguments, "project");
            let limit_notes = clamped_usize_arg(arguments, "limitNotes", 6, 1, 12);
            let limit_chunks = clamped_usize_arg(arguments, "limitChunks", 8, 1, 16);
            let include_graph = bool_arg(arguments, "includeGraph", true);
            // A mount that ranks for itself serves the CHUNKS half of this tool and
            // nothing else. See `native_load_knowledge_payload` for why the graph comes
            // back empty with a reason rather than absent or fabricated.
            let scoped = match target {
                RecallTarget::Local(scoped) => scoped,
                // Unscoped on a multi-mount vault: chunks fused across every mount, the
                // graph from the mount that produced the best chunk. See
                // `federated_load_knowledge_payload`.
                RecallTarget::Federated => {
                    return Ok(json_text_result_from_arguments(
                        arguments,
                        federated_load_knowledge_payload(
                            state,
                            &subject,
                            project.as_deref(),
                            FederatedKnowledgeOptions {
                                limit_notes,
                                limit_chunks,
                                include_graph,
                                graph_depth: clamped_usize_arg(arguments, "graphDepth", 1, 1, 3),
                            },
                            TextPayloadOptions::search_snippet_from_arguments(arguments, true),
                        )
                        .await?,
                    ));
                }
                RecallTarget::Native(mount) => {
                    return Ok(json_text_result_from_arguments(
                        arguments,
                        native_load_knowledge_payload(
                            &mount,
                            &subject,
                            project.as_deref(),
                            limit_notes,
                            limit_chunks,
                            TextPayloadOptions::search_snippet_from_arguments(arguments, true),
                        )
                        .await?,
                    ));
                }
            };
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
            let snapshot = root_index(state, "recommend_folder")?
                .fresh_snapshot("recommend_folder")
                .await?;
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
            // A CLAIM about the content, forwarded verbatim: the caller is stating that
            // `content` reconciles a recorded divergence. Declared in the schema only when
            // some mount can record one, and ignored by every backend that cannot — see
            // `resolve_divergence_property`.
            let resolve_divergence = bool_arg(arguments, "resolveDivergence", false);
            // The dry run returns above without ever reaching the write, so no
            // backend — and therefore no remote — is touched by one.
            if !dry_run {
                backend_call(
                    state,
                    BackendRequest::write_text_full(
                        &path,
                        &final_content,
                        prior.base_version,
                        resolve_divergence,
                    ),
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
            federated_rerank: true,
            index_dir: vault_path.join(".deep-obsidian-mcp-test"),
            vault_path: Some(vault_path),
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

        let snapshot = state
            .runtime()
            .expect("a filesystem root has a runtime")
            .snapshot()
            .expect("snapshot after rebuild");
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
        let definitions = tool_definitions(true, false, super::CapabilitySet::default());
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
        let available = super::tool_definitions(true, false, super::CapabilitySet::default());
        assert!(
            available.iter().any(|tool| tool.name == "grep_search"),
            "grep_search should be present when ripgrep is available"
        );

        let unavailable = super::tool_definitions(false, false, super::CapabilitySet::default());
        assert!(
            !unavailable.iter().any(|tool| tool.name == "grep_search"),
            "grep_search must be omitted when ripgrep is unavailable"
        );
        // Omission must be surgical: every other tool stays registered.
        assert_eq!(unavailable.len(), available.len() - 1);
    }

    #[test]
    fn consolidated_tool_surface() {
        let definitions = super::tool_definitions(true, false, super::CapabilitySet::default());
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
                config.vault_path.clone().expect("a local vault root"),
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
