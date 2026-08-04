//! Ripgrep-backed line search for the filesystem backend.
//!
//! Both halves of `grep_search` live here: resolving the `rg` binary (which
//! determines whether the [`Capability::GrepSearch`](crate::Capability::GrepSearch)
//! capability is advertised at all) and running it. Every string produced below is
//! public MCP behaviour.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use deep_obsidian_core::vault::ensure_inside_vault;
use serde_json::Value;

use crate::{BackendError, GrepContextLine, GrepMatch, GrepSubmatch};

/// Clear, actionable error surfaced when a grep is attempted but ripgrep could not
/// be resolved (or a spawn unexpectedly fails with `NotFound`). Never surface the
/// raw `os error 2` for this case.
pub const RIPGREP_UNAVAILABLE_MESSAGE: &str = "grep_search is unavailable: ripgrep (rg) not found on PATH. Install ripgrep or fix the service PATH, then restart.";

/// Resolve the absolute path to the `rg` (ripgrep) binary.
///
/// The MCP server runs under launchd as a Homebrew service, whose `PATH` is the
/// minimal `/usr/bin:/bin:/usr/sbin:/sbin` — it does NOT include Homebrew's bin
/// dir, so spawning bare `rg` fails with `ENOENT`. We resolve an absolute path
/// instead: an explicit env override, then `PATH`, then known install locations,
/// finally falling back to bare `rg` (preserving old behavior when it is on PATH).
pub fn resolve_ripgrep() -> PathBuf {
    resolve_ripgrep_env(|key| std::env::var(key).ok())
}

pub(crate) fn resolve_ripgrep_env(get_env: impl Fn(&str) -> Option<String>) -> PathBuf {
    // 1. Explicit override.
    for key in ["DEEP_OBSIDIAN_RIPGREP", "RIPGREP_PATH"] {
        if let Some(value) = get_env(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                let candidate = PathBuf::from(trimmed);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    // 2. Search PATH.
    if let Some(path) = get_env("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("rg");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    // 3. Known install locations (Homebrew prefix first, then common paths).
    let mut known: Vec<PathBuf> = Vec::new();
    if let Some(prefix) = get_env("HOMEBREW_PREFIX") {
        let trimmed = prefix.trim();
        if !trimmed.is_empty() {
            known.push(PathBuf::from(trimmed).join("bin").join("rg"));
        }
    }
    for path in [
        "/opt/homebrew/bin/rg",
        "/usr/local/bin/rg",
        "/usr/bin/rg",
        "/bin/rg",
    ] {
        known.push(PathBuf::from(path));
    }
    for candidate in known {
        if candidate.is_file() {
            return candidate;
        }
    }
    // 4. Fallback: bare name (works when rg is on PATH).
    PathBuf::from("rg")
}

/// Render an absolute path emitted by ripgrep as a vault-relative one.
fn relative_vault_path(vault_path: &Path, absolute_path: &str) -> String {
    let path = Path::new(absolute_path);
    match path.strip_prefix(vault_path) {
        Ok(relative) => relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => absolute_path.to_string(),
    }
}

/// Split note text into lines exactly the way the tool layer does, so context line
/// text is identical to what a `read_file` of the same note would report.
fn split_note_lines(content: &str) -> Vec<String> {
    content
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

/// Arguments for one grep run, mirroring [`RecallRequest::Grep`](crate::RecallRequest::Grep).
pub(crate) struct GrepParams {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub glob: Option<String>,
    pub context_lines: usize,
    pub limit: usize,
}

/// Run ripgrep over `vault_path` and return matches with context attached.
///
/// `index_dir` is the backend's configured index directory, if any: hits under it
/// are dropped when it lives inside the vault. It is deployment state of this
/// backend, not part of the cross-backend request, so it arrives separately from
/// [`GrepParams`].
///
/// Synchronous by design: the caller runs it on a blocking thread.
pub(crate) fn run_grep(
    ripgrep_path: &Path,
    vault_path: &Path,
    index_dir: Option<&Path>,
    params: GrepParams,
) -> Result<Vec<GrepMatch>, BackendError> {
    let GrepParams {
        query,
        regex,
        case_sensitive,
        glob,
        context_lines,
        limit,
    } = params;

    // A custom index dir INSIDE the vault (holding the SQLite index and its
    // sidecar files) must never leak into grep results as phantom vault paths.
    // This is filtered on the emitted path rather than with a `--glob`: ripgrep
    // matches globs containing a separator against paths relative to the process
    // working directory (not to the searched root), so the only glob form that
    // fires here is the unanchored `!**/<name>/**` — which would also hide a
    // real note under any same-named directory elsewhere in the vault. An index
    // dir equal to the vault root strips to an empty prefix; skip that
    // degenerate case rather than hiding the whole vault.
    let index_dir_prefix = index_dir
        .and_then(|dir| dir.strip_prefix(vault_path).ok().map(|p| p.to_path_buf()))
        .filter(|relative| relative.components().next().is_some());

    let mut args = vec![
        "--json".to_string(),
        "--line-number".to_string(),
        "--with-filename".to_string(),
        "--hidden".to_string(),
        "--glob".to_string(),
        "!.obsidian/**".to_string(),
        "--glob".to_string(),
        "!.git/**".to_string(),
        "--glob".to_string(),
        "!.deep-obsidian-mcp/**".to_string(),
    ];
    if !regex {
        args.push("--fixed-strings".to_string());
    }
    if !case_sensitive {
        args.push("--ignore-case".to_string());
    }
    if let Some(glob) = glob.as_ref() {
        args.push("--glob".to_string());
        args.push(glob.clone());
    } else {
        args.push("--glob".to_string());
        args.push("*.md".to_string());
    }
    // End-of-options separator: everything after `--` is treated by ripgrep
    // strictly as positionals, so a user `query` (or path) beginning with `-`
    // cannot be parsed as a flag (e.g. `--pre=<interpreter>` argv injection).
    args.push("--".to_string());
    args.push(query);
    args.push(vault_path.to_string_lossy().into_owned());

    let output = ProcessCommand::new(ripgrep_path)
        .args(&args)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                BackendError::Message(RIPGREP_UNAVAILABLE_MESSAGE.to_string())
            } else {
                BackendError::Message(error.to_string())
            }
        })?;

    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(BackendError::Message(if stderr.is_empty() {
            format!("rg failed with status {}", output.status)
        } else {
            stderr
        }));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| BackendError::Message(error.to_string()))?;
    let mut matches = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Value =
            serde_json::from_str(line).map_err(|error| BackendError::Message(error.to_string()))?;
        if parsed.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let data = parsed
            .get("data")
            .ok_or_else(|| BackendError::Message("rg match payload missing data".to_string()))?;
        let absolute_path = data
            .get("path")
            .and_then(|value| value.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError::Message("rg match payload missing path".to_string()))?;
        // Drop hits under a vault-internal index dir before they count
        // towards `limit`, so a phantom path never displaces a real note.
        if let Some(prefix) = index_dir_prefix.as_ref() {
            if Path::new(absolute_path)
                .strip_prefix(vault_path)
                .is_ok_and(|relative| relative.starts_with(prefix))
            {
                continue;
            }
        }
        let line_number = data
            .get("line_number")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                BackendError::Message("rg match payload missing line number".to_string())
            })? as usize;
        let line_text = data
            .get("lines")
            .and_then(|value| value.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError::Message("rg match payload missing line text".to_string()))?
            .trim_end_matches('\n')
            .to_string();
        let submatches = data
            .get("submatches")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                BackendError::Message("rg match payload missing submatches".to_string())
            })?
            .iter()
            .map(|submatch| {
                Ok(GrepSubmatch {
                    start: submatch
                        .get("start")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            BackendError::Message("rg submatch missing start".to_string())
                        })? as usize,
                    end: submatch.get("end").and_then(Value::as_u64).ok_or_else(|| {
                        BackendError::Message("rg submatch missing end".to_string())
                    })? as usize,
                    text: submatch
                        .get("match")
                        .and_then(|value| value.get("text"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            BackendError::Message("rg submatch missing text".to_string())
                        })?
                        .to_string(),
                })
            })
            .collect::<Result<Vec<_>, BackendError>>()?;

        matches.push(GrepMatch {
            path: relative_vault_path(vault_path, absolute_path),
            line_number,
            submatches,
            line_text,
            context_before: Vec::new(),
            context_after: Vec::new(),
        });
        if matches.len() >= limit.max(1) {
            break;
        }
    }

    if context_lines > 0 {
        populate_grep_context(vault_path, &mut matches, context_lines)?;
    }

    Ok(matches)
}

/// Attach `context_lines` lines either side of each match, reading each matched
/// note at most once.
fn populate_grep_context(
    vault_path: &Path,
    matches: &mut [GrepMatch],
    context_lines: usize,
) -> Result<(), BackendError> {
    let mut cache = HashMap::<String, Vec<String>>::new();
    for match_item in matches {
        let lines = if let Some(lines) = cache.get(&match_item.path) {
            lines
        } else {
            let absolute_path = ensure_inside_vault(vault_path, &match_item.path)
                .map_err(|error| BackendError::Message(error.to_string()))?;
            let text = std::fs::read_to_string(&absolute_path)
                .map_err(|error| BackendError::Message(error.to_string()))?;
            cache.insert(match_item.path.clone(), split_note_lines(&text));
            cache.get(&match_item.path).expect("cached grep context")
        };
        let line_index = match_item.line_number.saturating_sub(1);
        let before_start = line_index.saturating_sub(context_lines);
        match_item.context_before = lines[before_start..line_index.min(lines.len())]
            .iter()
            .enumerate()
            .map(|(offset, line)| GrepContextLine {
                line_number: before_start + offset + 1,
                line_text: line.clone(),
            })
            .collect();
        let after_start = (line_index + 1).min(lines.len());
        let after_end = (after_start + context_lines).min(lines.len());
        match_item.context_after = lines[after_start..after_end]
            .iter()
            .enumerate()
            .map(|(offset, line)| GrepContextLine {
                line_number: after_start + offset + 1,
                line_text: line.clone(),
            })
            .collect();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_ripgrep_honors_existing_override() {
        // `/bin/sh` exists on every supported platform and is a file, so it stands
        // in for a real rg binary.
        let resolved = resolve_ripgrep_env(|key| {
            if key == "DEEP_OBSIDIAN_RIPGREP" {
                Some("/bin/sh".to_string())
            } else {
                None
            }
        });
        assert_eq!(resolved, PathBuf::from("/bin/sh"));
    }

    #[test]
    fn resolve_ripgrep_ignores_missing_override_and_resolves_rg() {
        let resolved = resolve_ripgrep_env(|key| match key {
            "DEEP_OBSIDIAN_RIPGREP" => Some("/definitely/not/here/rg".to_string()),
            "PATH" => Some("/definitely/not/here".to_string()),
            _ => None,
        });
        // Falls through the override and PATH, landing on a known location or the
        // bare name; either way it must not be the bogus override.
        assert_ne!(resolved, PathBuf::from("/definitely/not/here/rg"));
    }

    #[test]
    fn relative_vault_path_strips_the_root() {
        let vault = Path::new("/vault");
        assert_eq!(
            relative_vault_path(vault, "/vault/Notes/Note.md"),
            "Notes/Note.md"
        );
        // A path outside the root is passed through untouched.
        assert_eq!(
            relative_vault_path(vault, "/other/Note.md"),
            "/other/Note.md"
        );
    }

    #[test]
    fn split_note_lines_trims_carriage_returns() {
        assert_eq!(split_note_lines("a\r\nb\n"), vec!["a", "b", ""]);
    }
}
