use std::collections::HashMap;
use std::path::Path;

use deep_obsidian_core::text::{extract_block_sections, extract_heading_sections};
use serde_json::json;
use urlencoding::decode;

use crate::health::build_vault_overview_payload;
use crate::mcp::AppState;
use crate::protocol::{
    ResourceContents, ResourceDefinition, ResourceListResult, ResourceReadResult,
    ResourceTemplateDefinition, ResourceTemplateListResult,
};
use crate::vault::read_text;

const VAULT_INFO_URI: &str = "obsidian://vault/info";
const NOTES_INDEX_URI: &str = "obsidian://vault/notes-index";
const NOTE_RESOURCE_LIST_LIMIT: usize = 200;

pub(crate) fn note_uri(note_path: &str) -> String {
    format!("obsidian://note?path={}", urlencoding::encode(note_path))
}

pub(crate) fn artifact_uri(path: &str) -> String {
    format!("obsidian://artifact?path={}", urlencoding::encode(path))
}

pub(crate) fn heading_uri(note_path: &str, slug: &str) -> String {
    format!(
        "obsidian://heading?path={}&slug={}",
        urlencoding::encode(note_path),
        urlencoding::encode(slug)
    )
}

pub(crate) fn block_uri(note_path: &str, id: &str) -> String {
    format!(
        "obsidian://block?path={}&id={}",
        urlencoding::encode(note_path),
        urlencoding::encode(id)
    )
}

fn parse_uri_query(uri: &str) -> HashMap<String, String> {
    let query = uri.split_once('?').map(|(_, query)| query).unwrap_or("");
    let mut values = HashMap::new();
    for pair in query.split('&').filter(|item| !item.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let decoded = decode(value)
            .map(|item| item.into_owned())
            .unwrap_or_else(|_| value.to_string());
        values.insert(key.to_string(), decoded);
    }
    values
}

fn vault_info_resource() -> ResourceDefinition {
    ResourceDefinition {
        uri: VAULT_INFO_URI.to_string(),
        name: "vault-overview".to_string(),
        title: Some("Vault Overview".to_string()),
        description: Some(
            "Basic metadata about the configured vault and local search index.".to_string(),
        ),
        mime_type: "application/json".to_string(),
    }
}

fn notes_index_resource(note_count: usize, listed_count: usize) -> ResourceDefinition {
    let description = if note_count > listed_count {
        format!(
            "Compact path manifest for all notes. resources/list includes {} of {} note resources; use note-resource templates for exact reads.",
            listed_count, note_count
        )
    } else {
        "Compact path manifest for all notes in the configured vault.".to_string()
    };

    ResourceDefinition {
        uri: NOTES_INDEX_URI.to_string(),
        name: "vault-notes-index".to_string(),
        title: Some("Vault Notes Index".to_string()),
        description: Some(description),
        mime_type: "application/json".to_string(),
    }
}

fn note_resource(path: &str) -> ResourceDefinition {
    ResourceDefinition {
        uri: note_uri(path),
        name: path.to_string(),
        title: Some("Obsidian Note".to_string()),
        description: Some("Read a full note from the configured vault.".to_string()),
        mime_type: "text/markdown".to_string(),
    }
}

fn sorted_note_paths<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut paths = paths.collect::<Vec<_>>();
    paths.sort_unstable();
    paths
}

pub async fn list_resources(state: &AppState) -> Result<ResourceListResult, String> {
    let snapshot = state.runtime.fresh_snapshot("resources/list").await?;
    let note_paths = sorted_note_paths(
        snapshot
            .index
            .file_snapshots
            .iter()
            .map(|entry| entry.path.as_str()),
    );
    let note_count = note_paths.len();
    let listed_count = note_count.min(NOTE_RESOURCE_LIST_LIMIT);

    let mut resources = vec![
        vault_info_resource(),
        notes_index_resource(note_count, listed_count),
    ];
    resources.extend(
        note_paths
            .iter()
            .take(NOTE_RESOURCE_LIST_LIMIT)
            .map(|path| note_resource(path)),
    );

    Ok(ResourceListResult {
        resources,
        meta: Some(json!({
            "noteResourceLimit": NOTE_RESOURCE_LIST_LIMIT,
            "noteResourceCount": listed_count,
            "noteResourceTotal": note_count,
            "truncated": note_count > listed_count,
            "notesIndexUri": NOTES_INDEX_URI,
            "noteUriTemplate": "obsidian://note{?path}"
        })),
    })
}

pub fn list_resource_templates() -> ResourceTemplateListResult {
    ResourceTemplateListResult {
        resource_templates: vec![
            ResourceTemplateDefinition {
                uri_template: "obsidian://note{?path}".to_string(),
                name: "note-resource".to_string(),
                title: Some("Obsidian Note".to_string()),
                description: Some("Read a full note from the configured vault.".to_string()),
                mime_type: "text/markdown".to_string(),
            },
            ResourceTemplateDefinition {
                uri_template: "obsidian://heading{?path,slug}".to_string(),
                name: "heading-resource".to_string(),
                title: Some("Obsidian Heading Section".to_string()),
                description: Some(
                    "Read the section corresponding to a heading slug within a note.".to_string(),
                ),
                mime_type: "text/markdown".to_string(),
            },
            ResourceTemplateDefinition {
                uri_template: "obsidian://block{?path,id}".to_string(),
                name: "block-resource".to_string(),
                title: Some("Obsidian Block".to_string()),
                description: Some(
                    "Read a block identified by an Obsidian block id inside a note.".to_string(),
                ),
                mime_type: "text/markdown".to_string(),
            },
        ],
    }
}

pub async fn read_resource(state: &AppState, uri: &str) -> Result<ResourceReadResult, String> {
    if uri == VAULT_INFO_URI {
        let snapshot = state
            .runtime
            .fresh_snapshot("resources/read:vault-info")
            .await?;
        let payload = build_vault_overview_payload(&state.config, &snapshot);
        return Ok(ResourceReadResult {
            contents: vec![ResourceContents {
                uri: uri.to_string(),
                mime_type: "application/json".to_string(),
                text: serde_json::to_string_pretty(&payload)
                    .unwrap_or_else(|_| payload.to_string()),
            }],
        });
    }

    if uri == NOTES_INDEX_URI {
        let snapshot = state
            .runtime
            .fresh_snapshot("resources/read:notes-index")
            .await?;
        let note_paths = sorted_note_paths(
            snapshot
                .index
                .file_snapshots
                .iter()
                .map(|entry| entry.path.as_str()),
        );
        let notes = note_paths
            .iter()
            .map(|path| {
                json!({
                    "path": path,
                    "uri": note_uri(path),
                })
            })
            .collect::<Vec<_>>();
        let payload = json!({
            "noteCount": notes.len(),
            "noteUriTemplate": "obsidian://note{?path}",
            "resourcesListLimit": NOTE_RESOURCE_LIST_LIMIT,
            "notes": notes,
        });
        return Ok(ResourceReadResult {
            contents: vec![ResourceContents {
                uri: uri.to_string(),
                mime_type: "application/json".to_string(),
                text: serde_json::to_string_pretty(&payload)
                    .unwrap_or_else(|_| payload.to_string()),
            }],
        });
    }

    let params = parse_uri_query(uri);
    if uri.starts_with("obsidian://note") {
        let path = params
            .get("path")
            .ok_or_else(|| "missing note path".to_string())?;
        let text = read_text(&state.config.vault_path, path).map_err(|error| error.to_string())?;
        return Ok(ResourceReadResult {
            contents: vec![ResourceContents {
                uri: note_uri(path),
                mime_type: "text/markdown".to_string(),
                text,
            }],
        });
    }

    if uri.starts_with("obsidian://heading") {
        let path = params
            .get("path")
            .ok_or_else(|| "missing note path".to_string())?;
        let slug = params
            .get("slug")
            .ok_or_else(|| "missing heading slug".to_string())?;
        let text = read_text(&state.config.vault_path, path).map_err(|error| error.to_string())?;
        let heading = extract_heading_sections(&text)
            .into_iter()
            .find(|section| section.slug == *slug)
            .ok_or_else(|| format!("heading slug not found in {}: {}", path, slug))?;
        return Ok(ResourceReadResult {
            contents: vec![ResourceContents {
                uri: heading_uri(path, slug),
                mime_type: "text/markdown".to_string(),
                text: heading.text,
            }],
        });
    }

    if uri.starts_with("obsidian://block") {
        let path = params
            .get("path")
            .ok_or_else(|| "missing note path".to_string())?;
        let id = params
            .get("id")
            .ok_or_else(|| "missing block id".to_string())?;
        let text = read_text(&state.config.vault_path, path).map_err(|error| error.to_string())?;
        let block = extract_block_sections(&text)
            .into_iter()
            .find(|section| section.id == *id)
            .ok_or_else(|| format!("block id not found in {}: {}", path, id))?;
        return Ok(ResourceReadResult {
            contents: vec![ResourceContents {
                uri: block_uri(path, id),
                mime_type: "text/markdown".to_string(),
                text: block.text,
            }],
        });
    }

    Err(format!("unknown resource uri: {}", uri))
}

pub fn note_name(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_note_paths_are_stable() {
        let paths = sorted_note_paths(["z.md", "a.md", "folder/b.md"].into_iter());

        assert_eq!(paths, vec!["a.md", "folder/b.md", "z.md"]);
    }

    #[test]
    fn notes_index_resource_describes_truncated_lists() {
        let resource = notes_index_resource(250, 200);

        assert_eq!(resource.uri, NOTES_INDEX_URI);
        let description = resource.description.expect("description");
        assert!(description.contains("200 of 250"));
        assert_eq!(resource.mime_type, "application/json");
    }

    #[test]
    fn note_resource_keeps_existing_shape() {
        let resource = note_resource("Folder/My Note.md");

        assert_eq!(resource.uri, "obsidian://note?path=Folder%2FMy%20Note.md");
        assert_eq!(resource.name, "Folder/My Note.md");
        assert_eq!(resource.title.as_deref(), Some("Obsidian Note"));
        assert_eq!(resource.mime_type, "text/markdown");
    }
}
