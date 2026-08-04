//! Builds the note + chunk records for one version of one note.
//!
//! Ported from PR #40's `shared/records_build.rs`. The only substantive change is
//! that `content_hash` comes from [`deep_obsidian_core::content_hash`] instead of a
//! private copy in the server's tool layer — which is the point: the hash a note
//! record carries is the SAME string `read_file` reports as `hash` and the same one
//! a client feeds back as `expectedHash`, so a shared note's optimistic-concurrency
//! guard works exactly as it does on a filesystem mount.
//!
//! Chunk tiling, title extraction, link extraction and heading extraction all come
//! from `deep_obsidian_index::index` rather than being reimplemented. A chunk
//! boundary that differed from the local indexer's would make the shared corpus
//! disagree with every local index built over the same notes.

use deep_obsidian_algolia::records::{folder_facets, ChunkRecord, NoteRecord};
use deep_obsidian_algolia::{chunk_object_id, note_object_id, RECORD_TYPE_CHUNK, RECORD_TYPE_NOTE};
use deep_obsidian_index::index as index_core;

pub struct BuiltNote {
    pub note: NoteRecord,
    pub chunks: Vec<ChunkRecord>,
}

/// Frontmatter fields the records carry as facets.
///
/// Parsed from a minimal YAML subset (string scalars, inline `[a, b]` lists, and
/// `- item` block lists): enough for the vault's conventions without a YAML
/// dependency, and a field this parser does not understand simply does not become a
/// facet rather than failing the write.
///
/// `share` is parsed but unused by this backend: it was the seed importer's opt-out
/// flag in PR #40, and seeding is not part of this slice. It is kept because the
/// parser is otherwise identical and dropping the key would silently make
/// `share: false` look like a note with no frontmatter at all to whatever reads
/// these records next.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FrontmatterFields {
    pub note_type: Option<String>,
    pub project: Option<String>,
    pub status: Option<String>,
    pub layer: Option<String>,
    pub tags: Vec<String>,
    pub share: Option<bool>,
}

pub fn parse_frontmatter_fields(content: &str) -> FrontmatterFields {
    let mut fields = FrontmatterFields::default();
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return fields;
    }
    let mut pending_list_key: Option<String> = None;
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed.trim() == "---" {
            break;
        }
        if let Some(item) = trimmed.trim_start().strip_prefix("- ") {
            if pending_list_key.as_deref() == Some("tags") {
                fields.tags.push(item.trim().trim_matches('"').to_string());
            }
            continue;
        }
        let Some((key, raw_value)) = trimmed.split_once(':') else {
            pending_list_key = None;
            continue;
        };
        let key = key.trim().to_lowercase();
        let value = raw_value.trim().trim_matches('"').to_string();
        pending_list_key = if value.is_empty() {
            Some(key.clone())
        } else {
            None
        };
        match key.as_str() {
            "type" if !value.is_empty() => fields.note_type = Some(value),
            "project" if !value.is_empty() => fields.project = Some(value),
            "status" if !value.is_empty() => fields.status = Some(value),
            "layer" if !value.is_empty() => fields.layer = Some(value),
            "share" if !value.is_empty() => {
                fields.share = Some(!value.eq_ignore_ascii_case("false"))
            }
            "tags" if value.starts_with('[') => {
                fields.tags = value
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|tag| tag.trim().trim_matches('"').to_string())
                    .filter(|tag| !tag.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    fields
}

/// Resolve a raw wiki-link target against the known file set: exact path, path +
/// `.md`, then a UNIQUE basename-stem match. An unresolvable target keeps its raw
/// form, so it is visibly unresolved rather than silently pointing somewhere.
pub fn resolve_link(raw: &str, known_files: &[String]) -> String {
    // Strip alias and heading/block fragments: `A/B|alias`, `A/B#Heading`.
    let target = raw.split('|').next().unwrap_or(raw);
    let target = target.split('#').next().unwrap_or(target).trim();
    if target.is_empty() {
        return raw.to_string();
    }
    let with_md = if target.to_lowercase().ends_with(".md") {
        target.to_string()
    } else {
        format!("{target}.md")
    };
    if known_files.iter().any(|file| file == &with_md) {
        return with_md;
    }
    let stem = with_md
        .rsplit('/')
        .next()
        .unwrap_or(&with_md)
        .to_lowercase();
    let mut matches = known_files
        .iter()
        .filter(|file| file.rsplit('/').next().unwrap_or(file).to_lowercase() == stem);
    if let (Some(only), None) = (matches.next(), matches.next()) {
        return only.clone();
    }
    raw.to_string()
}

/// Version identity for one push.
pub struct NoteVersionMeta {
    pub version_id: String,
    pub parent_version_id: Option<String>,
    pub forked_from: Option<String>,
    pub has_divergence: bool,
    pub participant_id: String,
    pub updated_at_ms: u64,
}

/// Build the full record set for one version of one note.
///
/// `known_files` is the file list links are resolved against. It is the mount's own
/// note list, not the local vault's: a link inside a shared note means a note in the
/// shared corpus, and resolving it against the reader's private vault would make the
/// same record resolve differently for each participant.
pub fn build_note_records(
    path: &str,
    content: &str,
    known_files: &[String],
    meta: &NoteVersionMeta,
) -> BuiltNote {
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".md")
        .to_string();
    let dir = match path.rfind('/') {
        Some(position) => path[..position].to_string(),
        None => String::new(),
    };
    let title = index_core::note_title(&stem, content);
    let fields = parse_frontmatter_fields(content);
    let raw_links = index_core::extract_wiki_links(content);
    let links: Vec<String> = raw_links
        .iter()
        .map(|raw| resolve_link(raw, known_files))
        .collect();
    let headings: Vec<String> = index_core::extract_heading_sections(content)
        .into_iter()
        .map(|section| section.title)
        .collect();
    let content_hash = deep_obsidian_core::content_hash(content.as_bytes());
    let folders = folder_facets(&dir);
    let planned = index_core::plan_note_chunks(content, &title);

    let chunks: Vec<ChunkRecord> = planned
        .iter()
        .map(|chunk| ChunkRecord {
            object_id: chunk_object_id(path, &meta.version_id, chunk.chunk_index),
            record_type: RECORD_TYPE_CHUNK.to_string(),
            note_id: path.to_string(),
            version_id: meta.version_id.clone(),
            path: path.to_string(),
            dir: dir.clone(),
            folders: folders.clone(),
            title: title.clone(),
            note_type: fields.note_type.clone(),
            project: fields.project.clone(),
            layer: fields.layer.clone(),
            headings: headings.clone(),
            chunk_index: chunk.chunk_index,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            text: chunk.text.clone(),
            updated_at_ms: meta.updated_at_ms,
            participant_id: meta.participant_id.clone(),
        })
        .collect();

    let note = NoteRecord {
        object_id: note_object_id(path),
        record_type: RECORD_TYPE_NOTE.to_string(),
        note_id: path.to_string(),
        path: path.to_string(),
        dir,
        stem,
        folders,
        title,
        note_type: fields.note_type,
        project: fields.project,
        status: fields.status,
        layer: fields.layer,
        tags: fields.tags,
        headings,
        links,
        links_raw: raw_links,
        version_id: meta.version_id.clone(),
        parent_version_id: meta.parent_version_id.clone(),
        has_divergence: meta.has_divergence,
        content_hash,
        chunk_count: chunks.len(),
        size_bytes: content.len(),
        updated_at_ms: meta.updated_at_ms,
        participant_id: meta.participant_id.clone(),
        deleted: false,
        superseded_by: None,
        forked_from: meta.forked_from.clone(),
    };

    BuiltNote { note, chunks }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\ntype: wiki-decision\nproject: Deep Obsidian\nstatus: active\ntags: [memory, algolia]\nshare: true\n---\n\n# Title here\n\n## Decision\n\nBody with a [[_Agent/Contracts/Deep Obsidian|contract]] link.\n\n## Rationale\n\nMore text.\n";

    #[test]
    fn frontmatter_fields_parse_scalars_and_inline_lists() {
        let fields = parse_frontmatter_fields(NOTE);
        assert_eq!(fields.note_type.as_deref(), Some("wiki-decision"));
        assert_eq!(fields.project.as_deref(), Some("Deep Obsidian"));
        assert_eq!(fields.tags, vec!["memory", "algolia"]);
        assert_eq!(fields.share, Some(true));

        let blocked = parse_frontmatter_fields("---\nshare: false\n---\nbody");
        assert_eq!(blocked.share, Some(false));

        let block_list = parse_frontmatter_fields("---\ntags:\n  - alpha\n  - beta\n---\n");
        assert_eq!(block_list.tags, vec!["alpha", "beta"]);
    }

    #[test]
    fn resolve_link_prefers_exact_then_unique_stem() {
        let known = vec![
            "_Wiki/Decisions/Foo.md".to_string(),
            "_Agent/Contracts/Deep Obsidian.md".to_string(),
        ];
        assert_eq!(
            resolve_link("_Agent/Contracts/Deep Obsidian|contract", &known),
            "_Agent/Contracts/Deep Obsidian.md"
        );
        assert_eq!(resolve_link("Foo", &known), "_Wiki/Decisions/Foo.md");
        assert_eq!(resolve_link("Missing Note", &known), "Missing Note");
    }

    /// The chunk records' line ranges must address the note's own lines exactly, or
    /// reassembly cannot be byte-exact. Section chunks tile without overlap.
    #[test]
    fn build_note_records_round_trips_chunk_line_ranges() {
        let meta = NoteVersionMeta {
            version_id: "v1".to_string(),
            parent_version_id: None,
            forked_from: None,
            has_divergence: false,
            participant_id: "tester".to_string(),
            updated_at_ms: 42,
        };
        let built = build_note_records("_Wiki/Decisions/Foo.md", NOTE, &[], &meta);
        assert_eq!(built.note.title, "Title here");
        assert_eq!(built.note.chunk_count, built.chunks.len());
        assert!(!built.chunks.is_empty());
        let lines: Vec<&str> = NOTE.split('\n').collect();
        for chunk in &built.chunks {
            let expected = lines[chunk.start_line - 1..chunk.end_line].join("\n");
            assert_eq!(chunk.text, expected);
        }
        assert_eq!(
            built.note.links,
            vec!["_Agent/Contracts/Deep Obsidian".to_string()]
        );
    }

    /// The record's `contentHash` is CORE's hash of the raw bytes — the same string
    /// `read_file` reports and a client feeds back as `expectedHash`. If these two
    /// ever diverge, every optimistic-concurrency guard over a shared note breaks
    /// silently.
    #[test]
    fn content_hash_is_cores_hash_of_the_raw_bytes() {
        let meta = NoteVersionMeta {
            version_id: "v1".to_string(),
            parent_version_id: None,
            forked_from: None,
            has_divergence: false,
            participant_id: "tester".to_string(),
            updated_at_ms: 42,
        };
        let built = build_note_records("A.md", NOTE, &[], &meta);
        assert_eq!(
            built.note.content_hash,
            deep_obsidian_core::content_hash(NOTE.as_bytes())
        );
        assert!(built.note.content_hash.starts_with("fnv1a64:"));
    }
}
