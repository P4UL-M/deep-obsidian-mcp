use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct VaultInfo {
    pub vault_path: PathBuf,
    pub markdown_file_count: usize,
    pub service: &'static str,
    pub prototype: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadFileResult {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub line_count: usize,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault path does not exist or is not a directory: {0}")]
    InvalidVaultPath(PathBuf),
    #[error("invalid vault-relative path: {0}")]
    InvalidVaultRelativePath(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn ensure_vault_path(vault_path: &Path) -> Result<(), VaultError> {
    let metadata = fs::metadata(vault_path).map_err(|_| VaultError::InvalidVaultPath(vault_path.to_path_buf()))?;
    if !metadata.is_dir() {
        return Err(VaultError::InvalidVaultPath(vault_path.to_path_buf()));
    }
    Ok(())
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn collect_markdown_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), VaultError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_markdown_files(&path, files)?;
            continue;
        }
        if file_type.is_file() && is_markdown_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

pub fn markdown_file_count(vault_path: &Path) -> Result<usize, VaultError> {
    ensure_vault_path(vault_path)?;
    let mut files = Vec::new();
    collect_markdown_files(vault_path, &mut files)?;
    Ok(files.len())
}

pub fn vault_info(vault_path: &Path) -> Result<VaultInfo, VaultError> {
    Ok(VaultInfo {
        vault_path: vault_path.to_path_buf(),
        markdown_file_count: markdown_file_count(vault_path)?,
        service: "deep-obsidian-server",
        prototype: true,
    })
}

fn is_valid_vault_relative_path(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }

    let path = Path::new(value);
    if path.is_absolute() {
        return false;
    }

    !path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    })
}

fn resolve_relative_path(root: &Path, relative: &str) -> Result<PathBuf, VaultError> {
    if !is_valid_vault_relative_path(relative) {
        return Err(VaultError::InvalidVaultRelativePath(relative.to_string()));
    }
    Ok(root.join(relative))
}

pub fn read_file(vault_path: &Path, relative_path: &str, start_line: Option<usize>, end_line: Option<usize>) -> Result<ReadFileResult, VaultError> {
    ensure_vault_path(vault_path)?;
    let path = resolve_relative_path(vault_path, relative_path)?;
    let text = fs::read_to_string(&path)?;
    let lines: Vec<&str> = text.lines().collect();
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
        line_count: selected.lines().count(),
        text: selected,
    })
}
