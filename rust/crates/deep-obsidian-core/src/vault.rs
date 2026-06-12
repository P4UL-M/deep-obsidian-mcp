use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

pub const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    ".obsidian",
    ".trash",
    ".deep-obsidian-mcp",
    "node_modules",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadTextFileResult {
    pub absolute_path: PathBuf,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTextFileResult {
    pub absolute_path: PathBuf,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteBinaryFileResult {
    pub absolute_path: PathBuf,
    pub created: bool,
    pub bytes_written: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultChildEntry {
    pub name: String,
    pub path: String,
    pub kind: VaultEntryKind,
    pub is_markdown: bool,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSection {
    pub chunk_index: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("vault path does not exist or is not a directory: {0}")]
    InvalidVaultPath(PathBuf),
    #[error("invalid vault-relative path: {0}")]
    InvalidVaultRelativePath(String),
    #[error("writes to protected template folders are forbidden: {0}")]
    ProtectedWritePath(String),
    #[error("path is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

fn current_dir_fallback() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = if path.is_absolute() {
        PathBuf::from(Path::new("/"))
    } else {
        current_dir_fallback()
    };

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                normalized = PathBuf::from(prefix.as_os_str());
            }
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn normalize_vault_relative_path(relative_path: &str) -> Result<String, VaultError> {
    let normalized = relative_path.trim_start_matches('/');
    if normalized.is_empty() {
        return Err(VaultError::InvalidVaultRelativePath(
            relative_path.to_string(),
        ));
    }
    Ok(normalized.to_string())
}

fn is_protected_template_segment(segment: &str) -> bool {
    segment.eq_ignore_ascii_case("template") || segment.eq_ignore_ascii_case("templates")
}

fn ensure_writable_vault_relative_path(relative_path: &str) -> Result<(), VaultError> {
    let normalized = normalize_vault_relative_path(relative_path)?;
    if Path::new(&normalized)
        .components()
        .any(|component| match component {
            Component::Normal(part) => is_protected_template_segment(&part.to_string_lossy()),
            _ => false,
        })
    {
        return Err(VaultError::ProtectedWritePath(relative_path.to_string()));
    }
    Ok(())
}

fn ensure_markdown_dir_ignored(name: &str) -> bool {
    name.starts_with('.')
        || DEFAULT_IGNORED_DIRS
            .iter()
            .any(|candidate| *candidate == name)
}

fn should_ignore_entry(name: &str, include_hidden: bool, include_ignored: bool) -> bool {
    if !include_hidden && name.starts_with('.') {
        return true;
    }
    if !include_ignored
        && DEFAULT_IGNORED_DIRS
            .iter()
            .any(|candidate| *candidate == name)
    {
        return true;
    }
    false
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn walk_markdown_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<String>,
) -> Result<(), VaultError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ensure_markdown_dir_ignored(&name) {
            continue;
        }

        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            walk_markdown_files(root, &path, files)?;
            continue;
        }

        if file_type.is_file() && is_markdown_file(&path) {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|component| match component {
                    Component::Normal(part) => part.to_string_lossy().into_owned(),
                    _ => component.as_os_str().to_string_lossy().into_owned(),
                })
                .collect::<Vec<_>>()
                .join("/");
            files.push(relative);
        }
    }
    Ok(())
}

fn split_lines(content: &str) -> Vec<String> {
    content
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

pub fn ensure_vault_path(vault_path: &Path) -> Result<PathBuf, VaultError> {
    let resolved = normalize_path(vault_path);
    let metadata =
        fs::metadata(&resolved).map_err(|_| VaultError::InvalidVaultPath(resolved.clone()))?;
    if metadata.is_dir() {
        Ok(resolved)
    } else {
        Err(VaultError::InvalidVaultPath(resolved))
    }
}

pub fn ensure_inside_vault(vault_path: &Path, relative_path: &str) -> Result<PathBuf, VaultError> {
    let vault_path = normalize_path(vault_path);
    let normalized = normalize_vault_relative_path(relative_path)?;
    let candidate = normalize_path(&vault_path.join(normalized));
    // Lexical guard: the normalized candidate must stay under the (lexical) root.
    if candidate.strip_prefix(&vault_path).is_err() {
        return Err(VaultError::InvalidVaultRelativePath(
            relative_path.to_string(),
        ));
    }

    // Canonicalization guard: defeat a lexically-inside path that traverses a
    // pre-existing in-vault symlink to land outside the vault. We canonicalize
    // the deepest EXISTING ancestor of the candidate (the candidate itself when
    // it exists; otherwise the nearest existing parent, since write targets need
    // not exist yet) and require it to stay under the canonical vault root.
    if let Ok(canonical_vault) = fs::canonicalize(&vault_path) {
        // If the vault root itself does not yet exist, there is nothing on disk
        // (and thus no symlink) to traverse; the lexical guard above suffices.
        let mut existing = candidate.as_path();
        let canonical_ancestor = loop {
            match fs::canonicalize(existing) {
                Ok(canonical) => break Some(canonical),
                Err(_) => match existing.parent() {
                    Some(parent) => existing = parent,
                    None => break None,
                },
            }
        };
        if let Some(canonical_ancestor) = canonical_ancestor {
            if !canonical_ancestor.starts_with(&canonical_vault) {
                return Err(VaultError::InvalidVaultRelativePath(
                    relative_path.to_string(),
                ));
            }
        }
    }

    Ok(candidate)
}

pub fn read_text_file(
    vault_path: &Path,
    relative_path: &str,
) -> Result<ReadTextFileResult, VaultError> {
    let absolute_path = ensure_inside_vault(vault_path, relative_path)?;
    let text = fs::read_to_string(&absolute_path)?;
    Ok(ReadTextFileResult {
        absolute_path,
        text,
    })
}

pub fn write_text_file(
    vault_path: &Path,
    relative_path: &str,
    text: &str,
) -> Result<WriteTextFileResult, VaultError> {
    ensure_writable_vault_relative_path(relative_path)?;
    let absolute_path = ensure_inside_vault(vault_path, relative_path)?;
    let created = fs::metadata(&absolute_path).is_err();
    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&absolute_path, text)?;
    Ok(WriteTextFileResult {
        absolute_path,
        created,
    })
}

pub fn write_binary_file(
    vault_path: &Path,
    relative_path: &str,
    bytes: &[u8],
) -> Result<WriteBinaryFileResult, VaultError> {
    ensure_writable_vault_relative_path(relative_path)?;
    let absolute_path = ensure_inside_vault(vault_path, relative_path)?;
    let created = fs::metadata(&absolute_path).is_err();
    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&absolute_path, bytes)?;
    Ok(WriteBinaryFileResult {
        absolute_path,
        created,
        bytes_written: bytes.len(),
    })
}

fn resolve_directory_path(
    vault_path: &Path,
    relative_path: Option<&str>,
) -> Result<(PathBuf, PathBuf), VaultError> {
    let resolved_vault = ensure_vault_path(vault_path)?;
    let absolute_path = match relative_path
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(relative) => ensure_inside_vault(&resolved_vault, relative)?,
        None => resolved_vault.clone(),
    };
    let metadata = fs::metadata(&absolute_path)?;
    if metadata.is_dir() {
        Ok((resolved_vault, absolute_path))
    } else {
        Err(VaultError::NotDirectory(absolute_path))
    }
}

pub fn list_children(
    vault_path: &Path,
    relative_path: Option<&str>,
    include_hidden: bool,
    include_ignored: bool,
) -> Result<Vec<VaultChildEntry>, VaultError> {
    let (resolved_vault, directory_path) = resolve_directory_path(vault_path, relative_path)?;
    let mut entries = Vec::new();

    for entry in fs::read_dir(&directory_path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if should_ignore_entry(&name, include_hidden, include_ignored) {
            continue;
        }

        let absolute_path = entry.path();
        let file_type = entry.file_type()?;
        if !file_type.is_dir() && !file_type.is_file() {
            continue;
        }

        let relative = absolute_path
            .strip_prefix(&resolved_vault)
            .unwrap_or(&absolute_path)
            .components()
            .map(|component| match component {
                Component::Normal(part) => part.to_string_lossy().into_owned(),
                _ => component.as_os_str().to_string_lossy().into_owned(),
            })
            .collect::<Vec<_>>()
            .join("/");

        let metadata = entry.metadata()?;
        entries.push(VaultChildEntry {
            name,
            path: relative,
            kind: if file_type.is_dir() {
                VaultEntryKind::Directory
            } else {
                VaultEntryKind::File
            },
            is_markdown: file_type.is_file() && is_markdown_file(&absolute_path),
            size_bytes: if file_type.is_file() {
                Some(metadata.len())
            } else {
                None
            },
        });
    }

    entries.sort_by(|left, right| match (&left.kind, &right.kind) {
        (VaultEntryKind::Directory, VaultEntryKind::File) => std::cmp::Ordering::Less,
        (VaultEntryKind::File, VaultEntryKind::Directory) => std::cmp::Ordering::Greater,
        _ => left.path.cmp(&right.path),
    });
    Ok(entries)
}

pub fn list_folders(
    vault_path: &Path,
    relative_path: Option<&str>,
    recursive: bool,
    max_depth: usize,
    include_hidden: bool,
    include_ignored: bool,
) -> Result<Vec<String>, VaultError> {
    let (resolved_vault, directory_path) = resolve_directory_path(vault_path, relative_path)?;
    let mut folders = Vec::new();

    fn walk(
        resolved_vault: &Path,
        current: &Path,
        recursive: bool,
        remaining_depth: usize,
        include_hidden: bool,
        include_ignored: bool,
        folders: &mut Vec<String>,
    ) -> Result<(), VaultError> {
        if remaining_depth == 0 {
            return Ok(());
        }

        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if should_ignore_entry(&name, include_hidden, include_ignored) {
                continue;
            }

            let file_type = entry.file_type()?;
            if !file_type.is_dir() {
                continue;
            }

            let path = entry.path();
            let relative = path
                .strip_prefix(resolved_vault)
                .unwrap_or(&path)
                .components()
                .map(|component| match component {
                    Component::Normal(part) => part.to_string_lossy().into_owned(),
                    _ => component.as_os_str().to_string_lossy().into_owned(),
                })
                .collect::<Vec<_>>()
                .join("/");
            folders.push(relative);

            if recursive {
                walk(
                    resolved_vault,
                    &path,
                    recursive,
                    remaining_depth.saturating_sub(1),
                    include_hidden,
                    include_ignored,
                    folders,
                )?;
            }
        }

        Ok(())
    }

    walk(
        &resolved_vault,
        &directory_path,
        recursive,
        if recursive { max_depth.max(1) } else { 1 },
        include_hidden,
        include_ignored,
        &mut folders,
    )?;
    folders.sort();
    Ok(folders)
}

pub fn list_markdown_files(vault_path: &Path) -> Result<Vec<String>, VaultError> {
    let resolved = ensure_vault_path(vault_path)?;
    let mut files = Vec::new();
    walk_markdown_files(&resolved, &resolved, &mut files)?;
    files.sort();
    Ok(files)
}

pub fn list_top_level_folders(vault_path: &Path) -> Result<Vec<String>, VaultError> {
    list_folders(vault_path, None, false, 1, false, false)
}

pub fn slice_lines(text: &str, start_line: usize, end_line: usize) -> String {
    let lines = split_lines(text);
    let start = start_line.max(1).saturating_sub(1).min(lines.len());
    let end = end_line.max(start_line).min(lines.len());
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

pub fn chunk_lines(text: &str, chunk_size_lines: usize, overlap_lines: usize) -> Vec<ChunkSection> {
    let lines = split_lines(text);
    let safe_chunk_size = chunk_size_lines.max(1);
    let safe_overlap = overlap_lines.min(safe_chunk_size.saturating_sub(1));
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut chunk_index = 0usize;

    while start < lines.len() {
        let end = (start + safe_chunk_size).min(lines.len());
        chunks.push(ChunkSection {
            chunk_index,
            start_line: start + 1,
            end_line: end,
            text: lines[start..end].join("\n"),
        });
        if end >= lines.len() {
            break;
        }
        start = end.saturating_sub(safe_overlap);
        chunk_index += 1;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), nanos))
    }

    #[test]
    fn ensure_inside_vault_resolves_relative_paths() {
        let vault = temp_dir("vault");
        let resolved =
            ensure_inside_vault(&vault, "notes/../Home.md").expect("path should resolve");
        assert_eq!(resolved, vault.join("Home.md"));
    }

    #[test]
    fn ensure_inside_vault_rejects_escape() {
        let vault = temp_dir("vault");
        let error = ensure_inside_vault(&vault, "../escape.md").expect_err("escape should fail");
        assert!(matches!(error, VaultError::InvalidVaultRelativePath(_)));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_inside_vault_rejects_symlink_traversal_for_reads_and_writes() {
        let vault = temp_dir("vault-symlink-escape");
        let outside = temp_dir("outside-symlink-target");
        fs::create_dir_all(&vault).unwrap();
        fs::create_dir_all(&outside).unwrap();
        // A pre-existing in-vault symlink pointing outside the vault.
        std::os::unix::fs::symlink(&outside, vault.join("escape")).unwrap();
        // An existing file beyond the symlink (read target).
        fs::write(outside.join("secret.md"), "secret").unwrap();

        // Read path that resolves through the symlink must be rejected.
        let read_err = ensure_inside_vault(&vault, "escape/secret.md")
            .expect_err("symlinked read path should be rejected");
        assert!(matches!(read_err, VaultError::InvalidVaultRelativePath(_)));

        // Not-yet-existing write destination beyond the symlink must be rejected.
        let write_err = ensure_inside_vault(&vault, "escape/new.md")
            .expect_err("symlinked write destination should be rejected");
        assert!(matches!(write_err, VaultError::InvalidVaultRelativePath(_)));

        let _ = fs::remove_dir_all(&vault);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn ensure_inside_vault_allows_existing_and_new_in_vault_paths() {
        let vault = temp_dir("vault-inside-ok");
        fs::create_dir_all(vault.join("Notes")).unwrap();
        fs::write(vault.join("Notes/Existing.md"), "hi").unwrap();

        // Existing in-vault file resolves.
        let existing = ensure_inside_vault(&vault, "Notes/Existing.md").unwrap();
        assert_eq!(existing, vault.join("Notes/Existing.md"));
        // Not-yet-existing in-vault write target (existing parent dir) resolves.
        let new = ensure_inside_vault(&vault, "Notes/New.md").unwrap();
        assert_eq!(new, vault.join("Notes/New.md"));
        // Top-level new file (deepest existing ancestor == vault root) resolves.
        let top = ensure_inside_vault(&vault, "Top.md").unwrap();
        assert_eq!(top, vault.join("Top.md"));

        let _ = fs::remove_dir_all(&vault);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_inside_vault_accepts_paths_when_vault_root_is_under_a_symlink() {
        // A vault whose root is reached through a symlink must not be a false
        // positive: both sides canonicalize through the same link.
        let real_root = temp_dir("vault-real-root");
        fs::create_dir_all(real_root.join("Notes")).unwrap();
        fs::write(real_root.join("Notes/Existing.md"), "hi").unwrap();
        let link_root = temp_dir("vault-link-root");
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

        let existing = ensure_inside_vault(&link_root, "Notes/Existing.md")
            .expect("legitimate path under symlinked vault root should resolve");
        assert_eq!(existing, link_root.join("Notes/Existing.md"));
        let new = ensure_inside_vault(&link_root, "Notes/New.md")
            .expect("new path under symlinked vault root should resolve");
        assert_eq!(new, link_root.join("Notes/New.md"));

        let _ = fs::remove_file(&link_root);
        let _ = fs::remove_dir_all(&real_root);
    }

    #[test]
    fn slice_lines_matches_expected_range() {
        let text = "a\nb\nc\nd";
        assert_eq!(slice_lines(text, 2, 3), "b\nc");
        assert_eq!(slice_lines(text, 5, 7), "");
    }

    #[test]
    fn chunk_lines_uses_overlap_and_trailing_empty_line() {
        let chunks = chunk_lines("a\nb\nc\n", 2, 1);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "a\nb");
        assert_eq!(chunks[1].start_line, 2);
        assert_eq!(chunks[2].text, "c\n");
    }

    #[test]
    fn list_markdown_files_ignores_hidden_and_default_dirs() {
        let vault = temp_dir("vault-files");
        fs::create_dir_all(vault.join("Projects")).unwrap();
        fs::create_dir_all(vault.join(".obsidian")).unwrap();
        fs::create_dir_all(vault.join("node_modules")).unwrap();
        fs::create_dir_all(vault.join("Projects/Sub")).unwrap();
        fs::write(vault.join("Home.md"), "home").unwrap();
        fs::write(vault.join("Projects/Note.md"), "note").unwrap();
        fs::write(vault.join("Projects/Sub/Deep.md"), "deep").unwrap();
        fs::write(vault.join(".obsidian/Hidden.md"), "hidden").unwrap();
        fs::write(vault.join("node_modules/Ignored.md"), "ignored").unwrap();

        let files = list_markdown_files(&vault).unwrap();
        assert_eq!(
            files,
            vec![
                "Home.md".to_string(),
                "Projects/Note.md".to_string(),
                "Projects/Sub/Deep.md".to_string(),
            ]
        );
    }

    #[test]
    fn list_top_level_folders_ignores_hidden_and_default_dirs() {
        let vault = temp_dir("vault-folders");
        fs::create_dir_all(vault.join("A")).unwrap();
        fs::create_dir_all(vault.join("B")).unwrap();
        fs::create_dir_all(vault.join(".git")).unwrap();
        fs::create_dir_all(vault.join("node_modules")).unwrap();

        let folders = list_top_level_folders(&vault).unwrap();
        assert_eq!(folders, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn read_and_write_text_file_follow_vault_rules() {
        let vault = temp_dir("vault-read-write");
        fs::create_dir_all(&vault).unwrap();
        let result = write_text_file(&vault, "Notes/Session.md", "content").unwrap();
        assert!(result.created);
        assert!(result.absolute_path.ends_with("Notes/Session.md"));

        let read = read_text_file(&vault, "Notes/Session.md").unwrap();
        assert_eq!(read.text, "content");
        assert!(read.absolute_path.ends_with("Notes/Session.md"));
    }

    #[test]
    fn write_binary_file_creates_non_markdown_files() {
        let vault = temp_dir("vault-binary-write");
        fs::create_dir_all(&vault).unwrap();
        let result = write_binary_file(&vault, "Assets/data.bin", &[0, 1, 2, 3]).unwrap();
        assert!(result.created);
        assert_eq!(result.bytes_written, 4);
        assert_eq!(
            fs::read(vault.join("Assets/data.bin")).unwrap(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn write_text_file_rejects_template_folder_writes() {
        let vault = temp_dir("vault-template-write-blocked");
        fs::create_dir_all(vault.join("Templates")).unwrap();
        let error = write_text_file(&vault, "Templates/Note.md", "content")
            .expect_err("template write should fail");
        assert!(
            matches!(error, VaultError::ProtectedWritePath(path) if path == "Templates/Note.md")
        );
    }

    #[test]
    fn write_binary_file_rejects_nested_template_folder_writes() {
        let vault = temp_dir("vault-template-binary-write-blocked");
        fs::create_dir_all(vault.join("Notes/Templates")).unwrap();
        let error = write_binary_file(&vault, "Notes/Templates/data.bin", &[1, 2, 3])
            .expect_err("nested template write should fail");
        assert!(
            matches!(error, VaultError::ProtectedWritePath(path) if path == "Notes/Templates/data.bin")
        );
    }

    #[test]
    fn list_children_reports_directories_and_files() {
        let vault = temp_dir("vault-children");
        fs::create_dir_all(vault.join("Notes/Sub")).unwrap();
        fs::write(vault.join("Notes/Home.md"), "home").unwrap();
        fs::write(vault.join("Notes/data.json"), "{}").unwrap();

        let entries = list_children(&vault, Some("Notes"), false, false).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "Notes/Sub");
        assert_eq!(entries[0].kind, VaultEntryKind::Directory);
        assert_eq!(entries[1].path, "Notes/Home.md");
        assert!(entries[1].is_markdown);
        assert_eq!(entries[2].path, "Notes/data.json");
        assert!(!entries[2].is_markdown);
    }

    #[test]
    fn list_folders_supports_recursive_walks() {
        let vault = temp_dir("vault-folders-recursive");
        fs::create_dir_all(vault.join("A/B/C")).unwrap();
        fs::create_dir_all(vault.join("Z")).unwrap();

        let folders = list_folders(&vault, None, true, 3, false, false).unwrap();
        assert_eq!(
            folders,
            vec![
                "A".to_string(),
                "A/B".to_string(),
                "A/B/C".to_string(),
                "Z".to_string()
            ]
        );
    }
}
