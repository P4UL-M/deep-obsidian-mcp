use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

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
    #[error("{}", deep_obsidian_core::describe_io_error(.path, .source))]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl VaultError {
    /// Returns a `map_err` closure that attaches `path` to an IO error.
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> VaultError + '_ {
        move |source| VaultError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

pub fn ensure_vault_path(vault_path: &Path) -> Result<(), VaultError> {
    let metadata = fs::metadata(vault_path)
        .map_err(|_| VaultError::InvalidVaultPath(vault_path.to_path_buf()))?;
    if !metadata.is_dir() {
        return Err(VaultError::InvalidVaultPath(vault_path.to_path_buf()));
    }
    Ok(())
}

/// Delegates to the canonical implementation in `deep-obsidian-core` (lexical
/// normalization + symlink-traversal guard), translating its single escape error
/// into this crate's `PathEscapesVault` while keeping the empty-path case as
/// `InvalidVaultRelativePath` for backward-compatible error wording.
pub fn ensure_inside_vault(vault_path: &Path, relative_path: &str) -> Result<PathBuf, VaultError> {
    if relative_path.trim_start_matches('/').is_empty() {
        return Err(VaultError::InvalidVaultRelativePath(
            relative_path.to_string(),
        ));
    }
    deep_obsidian_core::vault::ensure_inside_vault(vault_path, relative_path).map_err(|error| {
        match error {
            deep_obsidian_core::vault::VaultError::Io { path, source } => {
                VaultError::Io { path, source }
            }
            _ => VaultError::PathEscapesVault(relative_path.to_string()),
        }
    })
}

pub fn read_text(vault_path: &Path, relative_path: &str) -> Result<String, VaultError> {
    ensure_vault_path(vault_path)?;
    let path = ensure_inside_vault(vault_path, relative_path)?;
    fs::read_to_string(&path).map_err(VaultError::io(&path))
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
        fs::create_dir_all(parent).map_err(VaultError::io(parent))?;
    }
    fs::write(&path, text).map_err(VaultError::io(&path))?;
    Ok(WriteFileResult {
        absolute_path: path,
        created,
    })
}

fn map_core_error(error: deep_obsidian_core::vault::VaultError) -> VaultError {
    use deep_obsidian_core::vault::VaultError as CoreError;
    match error {
        CoreError::InvalidVaultPath(path) | CoreError::NotDirectory(path) => {
            VaultError::InvalidVaultPath(path)
        }
        CoreError::InvalidVaultRelativePath(path) | CoreError::ProtectedWritePath(path) => {
            VaultError::PathEscapesVault(path)
        }
        CoreError::Io { path, source } => VaultError::Io { path, source },
    }
}

pub fn list_markdown_files(vault_path: &Path) -> Result<Vec<String>, VaultError> {
    deep_obsidian_core::vault::list_markdown_files(vault_path).map_err(map_core_error)
}

pub fn list_top_level_folders(vault_path: &Path) -> Result<Vec<String>, VaultError> {
    deep_obsidian_core::vault::list_top_level_folders(vault_path).map_err(map_core_error)
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
