use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path};

use deep_obsidian_backend::{BackendError, BackendRequest, RouterError, VaultError, VaultRouter};
use deep_obsidian_core::text::{extract_block_sections, extract_heading_sections};
use serde_json::json;
use urlencoding::decode;

use crate::health::{build_vault_overview_payload, insert_mount_index_detail};
use crate::mcp::AppState;
use crate::protocol::{
    ResourceContents, ResourceDefinition, ResourceListResult, ResourceReadResult,
    ResourceTemplateDefinition, ResourceTemplateListResult,
};

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

/// Every note in the logical vault, as a sorted list of LOGICAL paths.
///
/// # Why enumeration federates while recall does not
///
/// `resources/list` and `obsidian://vault/notes-index` enumerate; they do not rank.
/// Concatenating each mount's index is therefore a COMPLETE answer, not a partial
/// one — which is exactly why the recall tools still refuse: their answer is a
/// top-`limit` ordering, and merging orderings needs comparable scores across
/// independently built indexes.
///
/// # Ordering
///
/// Globally lexicographic over logical paths, NOT grouped by mount. Two reasons: a
/// single-mount client already receives one globally sorted list, so a multi-mount
/// vault hands back the same kind of object rather than a differently-shaped one;
/// and it is stable under reordering the `mounts` table in the config, which is
/// pure presentation. For a single mount this reduces to a plain lexicographic
/// sort of the root index's own paths, which is exactly what this returned before
/// there was more than one mount.
///
/// # A broken mount is an error, not a silent omission
///
/// If any mount's index cannot be read the whole listing fails, naming the mount.
/// An enumeration that quietly drops one mount's notes tells a client those notes
/// do not exist, which is worse than telling it the listing is unavailable.
/// # A mount with no local index still contributes its notes
///
/// `state.runtimes.entries()` covers only mounts the server indexes locally, so iterating
/// it alone would silently omit every note on an index-less mount — and omission from
/// `resources/list` is exactly the "these notes do not exist" failure this function's other
/// invariant exists to prevent. Such a mount is asked directly, through
/// [`ManifestRequest::WalkMarkdown`](deep_obsidian_backend::ManifestRequest::WalkMarkdown),
/// which is an ENUMERATION and therefore something it can answer completely — unlike a
/// ranked recall.
///
/// Its failure is fatal to the listing by name, on the same terms as an indexed mount's.
async fn all_logical_note_paths(state: &AppState, reason: &str) -> Result<Vec<String>, String> {
    let mut paths: Vec<String> = Vec::new();
    for entry in state.runtimes.entries() {
        let snapshot = entry
            .runtime
            .fresh_snapshot(reason)
            .await
            .map_err(|error| {
                if entry.is_root() {
                    error
                } else {
                    format!("mount '{}' cannot be listed: {error}", entry.id)
                }
            })?;
        paths.extend(
            snapshot
                .index
                .file_snapshots
                .iter()
                // Index paths are mount-relative; the identity for the root mount.
                .map(|file| {
                    if entry.mount_at.is_empty() {
                        file.path.clone()
                    } else {
                        format!("{}/{}", entry.mount_at, file.path)
                    }
                }),
        );
    }
    for mount in state.router.mounts() {
        if state.runtimes.for_mount(&mount.id).is_some() {
            continue;
        }
        let notes = mount
            .backend
            .execute(BackendRequest::walk_markdown())
            .await
            .and_then(deep_obsidian_backend::BackendResponse::into_markdown_files)
            .map_err(|error| format!("mount '{}' cannot be listed: {error}", mount.id))?;
        paths.extend(notes.iter().map(|note| mount.to_logical(note)));
    }
    paths.sort_unstable();
    // Two mounts cannot own the same logical path (longest-prefix routing gives it to
    // exactly one), so this only ever collapses a duplicate WITHIN one mount's answer.
    // Cheap, and it keeps the 200-resource cap from being spent twice on one note.
    paths.dedup();
    Ok(paths)
}

pub async fn list_resources(state: &AppState) -> Result<ResourceListResult, String> {
    let note_paths = all_logical_note_paths(state, "resources/list").await?;
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
            .map(|path| note_resource(path.as_str())),
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

/// Reads a vault note for the `obsidian://note|heading|block` resources.
///
/// The error strings surfaced by `resources/read` are public MCP behaviour, and
/// the server-local vault helper this replaces was stricter and worded its
/// rejections differently than `deep_obsidian_core::vault`:
///
/// * it validated the vault root first, so a vanished vault reported
///   `vault path does not exist or is not a directory` rather than a per-file IO
///   error;
/// * it rejected *any* `..` / root / prefix component outright instead of
///   normalizing it away, so `Notes/../Home.md` is an error here even though core
///   would resolve it to `Home.md`;
/// * it reported every escape (lexical or through an in-vault symlink) as
///   `path escapes the vault` rather than `invalid vault-relative path`.
///
/// Those guards and that wording are preserved here so the public contract is
/// unchanged; the read itself goes through the backend.
///
/// `vault_path` is taken alongside the router on purpose. The vault-root check is
/// this module's own stricter pre-guard and reports the *configured* path verbatim,
/// whereas the backend's health probe would report the normalized one — a
/// difference that is invisible for ordinary paths but would change a public error
/// string for a path containing `.`/`..` or a trailing slash. The read itself, and
/// only the read, crosses the boundary.
///
/// `vault_path` is the ROOT mount's path. On a multi-mount config the pre-guard
/// therefore still checks the root vault even for a path that routes elsewhere.
/// That is deliberate rather than overlooked: the check's wording is frozen by the
/// `error_path_traversal` and `error_missing_file` goldens, the startup gate
/// already requires every mount to be reachable, and making the guard per-mount
/// would change a public error string for zero practical gain.
async fn read_note_text(
    vault_path: &Path,
    router: &VaultRouter,
    relative_path: &str,
) -> Result<String, String> {
    if !fs::metadata(vault_path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        return Err(format!(
            "vault path does not exist or is not a directory: {}",
            vault_path.display()
        ));
    }

    let normalized = relative_path.trim_start_matches('/');
    if normalized.is_empty() {
        return Err(format!("invalid vault-relative path: {relative_path}"));
    }
    if Path::new(normalized).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!("path escapes the vault: {relative_path}"));
    }

    // Routed: a single-mount config takes the router's pass-through fast path, so
    // the request reaches the same backend with the same path as before.
    router
        .execute(BackendRequest::read_text(relative_path))
        .await
        .and_then(|response| Ok(response.into_text()?))
        .map_err(|error| match error {
            // The lexical guard above already rejected everything core reports
            // lexically, so this can only be core's canonicalization (symlink)
            // guard — an escape in the legacy wording.
            RouterError::Backend(BackendError::Vault(VaultError::InvalidVaultRelativePath(
                path,
            ))) => {
                format!("path escapes the vault: {path}")
            }
            other => other.to_string(),
        })
}

pub async fn read_resource(state: &AppState, uri: &str) -> Result<ResourceReadResult, String> {
    if uri == VAULT_INFO_URI {
        let snapshot = state
            .runtime()
            .fresh_snapshot("resources/read:vault-info")
            .await?;
        let mut payload = build_vault_overview_payload(&state.config, &snapshot);
        // Additive, multi-mount only: the counts above are the ROOT mount's, so make
        // them cover the whole logical vault and report each mount's own state.
        insert_mount_index_detail(&mut payload, &state.mount_index_summaries());
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
        // Every mount, in one globally sorted list of logical paths. See
        // `all_logical_note_paths` for why enumeration federates and ranking does not.
        let note_paths = all_logical_note_paths(state, "resources/read:notes-index").await?;
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
        let text = read_note_text(&state.config.vault_path, state.router.as_ref(), path).await?;
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
        let text = read_note_text(&state.config.vault_path, state.router.as_ref(), path).await?;
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
        let text = read_note_text(&state.config.vault_path, state.router.as_ref(), path).await?;
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
    use deep_obsidian_backend::FilesystemVaultBackend;

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

    /// The legacy topology, as a router: one filesystem mount at the vault root.
    fn single_mount_router(vault: &std::path::Path) -> VaultRouter {
        VaultRouter::single(
            "vault",
            std::sync::Arc::new(FilesystemVaultBackend::new(vault)),
        )
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), nanos))
    }

    /// `resources/read` error strings are public MCP behaviour; these freeze the
    /// wording that the (now removed) server-local vault helper emitted.
    #[tokio::test]
    async fn read_note_text_preserves_legacy_error_wording() {
        let vault = temp_dir("resources-read-note");
        fs::create_dir_all(vault.join("Notes")).unwrap();
        fs::write(vault.join("Home.md"), "home").unwrap();
        // A single root mount: the router hands the request straight through, so
        // these frozen strings are asserted against the same path as before.
        let backend = single_mount_router(&vault);

        assert_eq!(
            read_note_text(&vault, &backend, "Home.md").await.unwrap(),
            "home"
        );

        assert_eq!(
            read_note_text(&vault, &backend, "../escape.md")
                .await
                .unwrap_err(),
            "path escapes the vault: ../escape.md"
        );
        // Stricter than core, which would normalize this to `Home.md`.
        assert_eq!(
            read_note_text(&vault, &backend, "Notes/../Home.md")
                .await
                .unwrap_err(),
            "path escapes the vault: Notes/../Home.md"
        );
        assert_eq!(
            read_note_text(&vault, &backend, "/").await.unwrap_err(),
            "invalid vault-relative path: /"
        );
        // A missing note keeps the enriched IO wording from core.
        let missing = read_note_text(&vault, &backend, "Missing.md")
            .await
            .unwrap_err();
        assert!(
            missing.starts_with(&format!(
                "io error for {}:",
                vault.join("Missing.md").display()
            )),
            "missing notes keep the IO wording: {missing}"
        );

        let absent = temp_dir("resources-read-note-absent");
        let absent_backend = single_mount_router(&absent);
        assert_eq!(
            read_note_text(&absent, &absent_backend, "Home.md")
                .await
                .unwrap_err(),
            format!(
                "vault path does not exist or is not a directory: {}",
                absent.display()
            )
        );

        let _ = fs::remove_dir_all(&vault);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_note_text_reports_symlink_escapes_as_vault_escapes() {
        let vault = temp_dir("resources-read-symlink-vault");
        let outside = temp_dir("resources-read-symlink-outside");
        fs::create_dir_all(&vault).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, vault.join("escape")).unwrap();
        fs::write(outside.join("secret.md"), "secret").unwrap();
        let backend = single_mount_router(&vault);

        assert_eq!(
            read_note_text(&vault, &backend, "escape/secret.md")
                .await
                .unwrap_err(),
            "path escapes the vault: escape/secret.md"
        );

        let _ = fs::remove_dir_all(&vault);
        let _ = fs::remove_dir_all(&outside);
    }
}
