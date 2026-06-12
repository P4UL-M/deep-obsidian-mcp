use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    ".obsidian",
    ".trash",
    ".deep-obsidian-mcp",
    "node_modules",
];

#[derive(Debug, Clone, Serialize)]
pub struct VaultInfo {
    #[serde(rename = "vaultPath")]
    pub vault_path: PathBuf,
    #[serde(rename = "markdownFileCount")]
    pub markdown_file_count: usize,
    pub service: &'static str,
    pub prototype: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadFileResult {
    pub path: String,
    #[serde(rename = "startLine")]
    pub start_line: usize,
    #[serde(rename = "endLine")]
    pub end_line: usize,
    #[serde(rename = "lineCount")]
    pub line_count: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteFileResult {
    #[serde(rename = "absolutePath")]
    pub absolute_path: PathBuf,
    pub created: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault path does not exist or is not a directory: {0}")]
    InvalidVaultPath(PathBuf),
    #[error("invalid vault-relative path: {0}")]
    InvalidVaultRelativePath(String),
    #[error("path escapes the vault: {0}")]
    PathEscapesVault(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn ensure_vault_path(vault_path: &Path) -> Result<(), VaultError> {
    let metadata = fs::metadata(vault_path)
        .map_err(|_| VaultError::InvalidVaultPath(vault_path.to_path_buf()))?;
    if !metadata.is_dir() {
        return Err(VaultError::InvalidVaultPath(vault_path.to_path_buf()));
    }
    Ok(())
}

pub fn ensure_inside_vault(vault_path: &Path, relative_path: &str) -> Result<PathBuf, VaultError> {
    let normalized = relative_path.trim_start_matches('/');
    if normalized.is_empty() {
        return Err(VaultError::InvalidVaultRelativePath(
            relative_path.to_string(),
        ));
    }

    let relative = Path::new(normalized);
    if relative.is_absolute() {
        return Err(VaultError::PathEscapesVault(relative_path.to_string()));
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(VaultError::PathEscapesVault(relative_path.to_string()));
    }

    let candidate = vault_path.join(relative);

    // Canonicalization guard: the lexical checks above cannot catch a path that
    // traverses a pre-existing in-vault symlink pointing outside the vault. We
    // canonicalize the deepest EXISTING ancestor of the candidate (itself when it
    // exists; otherwise the nearest existing parent, so not-yet-created write
    // targets still work) and require it to stay under the canonical vault root.
    // Both sides are canonicalized so a vault root that is itself under a symlink
    // is not a false positive. If the vault root does not yet exist on disk there
    // is no symlink to traverse and the lexical guard suffices.
    if let Ok(canonical_vault) = fs::canonicalize(vault_path) {
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
                return Err(VaultError::PathEscapesVault(relative_path.to_string()));
            }
        }
    }

    Ok(candidate)
}

pub fn read_text(vault_path: &Path, relative_path: &str) -> Result<String, VaultError> {
    ensure_vault_path(vault_path)?;
    let path = ensure_inside_vault(vault_path, relative_path)?;
    Ok(fs::read_to_string(path)?)
}

pub fn write_text(
    vault_path: &Path,
    relative_path: &str,
    text: &str,
) -> Result<WriteFileResult, VaultError> {
    ensure_vault_path(vault_path)?;
    let path = ensure_inside_vault(vault_path, relative_path)?;
    let created = !path.exists();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, text)?;
    Ok(WriteFileResult {
        absolute_path: path,
        created,
    })
}

fn is_ignored_dir(name: &str) -> bool {
    name.starts_with('.') || DEFAULT_IGNORED_DIRS.contains(&name)
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
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if file_type.is_dir() {
            if is_ignored_dir(&name) {
                continue;
            }
            walk_markdown_files(root, &path, files)?;
            continue;
        }

        if file_type.is_file() && is_markdown_file(&path) && !name.starts_with('.') {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push(relative);
        }
    }
    Ok(())
}

pub fn list_markdown_files(vault_path: &Path) -> Result<Vec<String>, VaultError> {
    ensure_vault_path(vault_path)?;
    let mut files = Vec::new();
    walk_markdown_files(vault_path, vault_path, &mut files)?;
    files.sort();
    Ok(files)
}

pub fn list_top_level_folders(vault_path: &Path) -> Result<Vec<String>, VaultError> {
    ensure_vault_path(vault_path)?;
    let mut folders = BTreeSet::new();
    for entry in fs::read_dir(vault_path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() && !is_ignored_dir(&name) {
            folders.insert(name);
        }
    }
    Ok(folders.into_iter().collect())
}

pub fn markdown_file_count(vault_path: &Path) -> Result<usize, VaultError> {
    Ok(list_markdown_files(vault_path)?.len())
}

pub fn vault_info(vault_path: &Path) -> Result<VaultInfo, VaultError> {
    Ok(VaultInfo {
        vault_path: vault_path.to_path_buf(),
        markdown_file_count: markdown_file_count(vault_path)?,
        service: "deep-obsidian-server",
        prototype: false,
    })
}

pub fn read_file(
    vault_path: &Path,
    relative_path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<ReadFileResult, VaultError> {
    let text = read_text(vault_path, relative_path)?;
    let lines: Vec<&str> = text.split('\n').collect();
    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or_else(|| lines.len().max(1)).max(start);
    let start_index = start.saturating_sub(1).min(lines.len());
    let end_index = end.min(lines.len());
    let selected = if start_index >= end_index {
        String::new()
    } else {
        lines[start_index..end_index].join("\n")
    };

    Ok(ReadFileResult {
        path: relative_path.to_string(),
        start_line: start,
        end_line: end,
        line_count: selected.split('\n').count(),
        text: selected,
    })
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
        std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), nanos))
    }

    #[test]
    fn ensure_inside_vault_allows_existing_and_new_in_vault_paths() {
        let vault = temp_dir("svault-inside-ok");
        fs::create_dir_all(vault.join("Notes")).unwrap();
        fs::write(vault.join("Notes/Existing.md"), "hi").unwrap();

        let existing = ensure_inside_vault(&vault, "Notes/Existing.md").unwrap();
        assert_eq!(existing, vault.join("Notes/Existing.md"));
        let new = ensure_inside_vault(&vault, "Notes/New.md").unwrap();
        assert_eq!(new, vault.join("Notes/New.md"));

        let _ = fs::remove_dir_all(&vault);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_inside_vault_rejects_symlink_traversal_for_reads_and_writes() {
        let vault = temp_dir("svault-symlink-escape");
        let outside = temp_dir("soutside-symlink-target");
        fs::create_dir_all(&vault).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, vault.join("escape")).unwrap();
        fs::write(outside.join("secret.md"), "secret").unwrap();

        let read_err = ensure_inside_vault(&vault, "escape/secret.md")
            .expect_err("symlinked read path should be rejected");
        assert!(matches!(read_err, VaultError::PathEscapesVault(_)));
        let write_err = ensure_inside_vault(&vault, "escape/new.md")
            .expect_err("symlinked write destination should be rejected");
        assert!(matches!(write_err, VaultError::PathEscapesVault(_)));

        let _ = fs::remove_dir_all(&vault);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_inside_vault_accepts_paths_when_vault_root_is_under_a_symlink() {
        let real_root = temp_dir("svault-real-root");
        fs::create_dir_all(real_root.join("Notes")).unwrap();
        fs::write(real_root.join("Notes/Existing.md"), "hi").unwrap();
        let link_root = temp_dir("svault-link-root");
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
}
