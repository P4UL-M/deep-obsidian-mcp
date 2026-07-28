//! Record shapes pushed to the shared indexes.
//!
//! One small `note` record per note (metadata, links, head-version pointer) and
//! one `chunk` record per chunk of the current version. History records reuse
//! the same shapes with `supersededBy` / `forkedFrom` set. Chunk `objectID`s
//! embed the version id so two versions' chunks can coexist during a write
//! cutover; the note record's `objectID` is stable so the head pointer is a
//! plain overwrite.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const RECORD_TYPE_NOTE: &str = "note";
pub const RECORD_TYPE_CHUNK: &str = "chunk";

/// Stable objectID for a note's head record.
pub fn note_object_id(path: &str) -> String {
    format!("note:{path}")
}

/// Version-scoped objectID for a chunk record.
pub fn chunk_object_id(path: &str, version_id: &str, chunk_index: usize) -> String {
    format!("chunk:{path}@{version_id}#{chunk_index}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NoteRecord {
    #[serde(rename = "objectID")]
    pub object_id: String,
    pub record_type: String,
    pub note_id: String,
    pub path: String,
    pub dir: String,
    pub stem: String,
    /// Hierarchical folder facets: `lvl0` = top folder, `lvl1` = "top/sub", ...
    pub folders: BTreeMap<String, String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub headings: Vec<String>,
    /// Resolved vault-relative link targets (fast backlinks via `filters`).
    #[serde(default)]
    pub links: Vec<String>,
    /// Original wiki-link text, kept so links can be re-resolved later.
    #[serde(default)]
    pub links_raw: Vec<String>,
    /// Head version pointer.
    pub version_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_version_id: Option<String>,
    #[serde(default)]
    pub has_divergence: bool,
    /// Hash of the raw file bytes — same function as `read_file`'s `knownHash`.
    pub content_hash: String,
    pub chunk_count: usize,
    pub size_bytes: usize,
    pub updated_at_ms: u64,
    pub participant_id: String,
    #[serde(default)]
    pub deleted: bool,
    /// History-only: the version that replaced this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Set when this version's parent was not the head at push time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChunkRecord {
    #[serde(rename = "objectID")]
    pub object_id: String,
    pub record_type: String,
    pub note_id: String,
    pub version_id: String,
    pub path: String,
    pub dir: String,
    pub folders: BTreeMap<String, String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(default)]
    pub headings: Vec<String>,
    pub chunk_index: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub updated_at_ms: u64,
    pub participant_id: String,
}

/// Builds the hierarchical folder facet map for a vault-relative path's
/// directory, e.g. `_Wiki/Decisions/Foo.md` -> {lvl0: "_Wiki", lvl1:
/// "_Wiki/Decisions"}. Capped at three levels to match the index settings.
pub fn folder_facets(dir: &str) -> BTreeMap<String, String> {
    let mut facets = BTreeMap::new();
    if dir.is_empty() {
        return facets;
    }
    let mut prefix = String::new();
    for (level, segment) in dir.split('/').take(3).enumerate() {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        facets.insert(format!("lvl{level}"), prefix.clone());
    }
    facets
}

/// The main-index settings provisioned on first push.
pub fn main_index_settings() -> serde_json::Value {
    serde_json::json!({
        "searchableAttributes": ["unordered(title)", "headings", "unordered(text)", "path"],
        "attributesForFaceting": [
            "searchable(folders.lvl0)", "searchable(folders.lvl1)", "searchable(folders.lvl2)",
            "filterOnly(links)", "filterOnly(path)", "filterOnly(dir)", "filterOnly(noteId)",
            "filterOnly(versionId)", "filterOnly(deleted)", "filterOnly(recordType)",
            "filterOnly(participantId)",
            "noteType", "project", "status", "layer", "tags"
        ],
        "attributeForDistinct": "path",
        "distinct": 1,
        "customRanking": ["desc(updatedAtMs)"],
        "attributesToSnippet": ["text:40"]
    })
}

/// History-index settings: never searched by users, only filtered/browsed.
pub fn history_index_settings() -> serde_json::Value {
    serde_json::json!({
        "searchableAttributes": ["path"],
        "attributesForFaceting": [
            "filterOnly(noteId)", "filterOnly(versionId)", "filterOnly(recordType)",
            "filterOnly(participantId)"
        ],
        "customRanking": ["desc(updatedAtMs)"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_facets_builds_hierarchy() {
        let facets = folder_facets("_Wiki/Decisions");
        assert_eq!(facets.get("lvl0").map(String::as_str), Some("_Wiki"));
        assert_eq!(
            facets.get("lvl1").map(String::as_str),
            Some("_Wiki/Decisions")
        );
        assert!(facets.get("lvl2").is_none());
        assert!(folder_facets("").is_empty());
    }

    #[test]
    fn object_ids_are_version_scoped_for_chunks_only() {
        assert_eq!(note_object_id("A/B.md"), "note:A/B.md");
        assert_eq!(chunk_object_id("A/B.md", "v2", 3), "chunk:A/B.md@v2#3");
    }
}
