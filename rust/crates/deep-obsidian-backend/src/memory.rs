//! A provider-free backend, used to prove the boundary is really a boundary.
//!
//! It stores notes in a map and has no filesystem, no subprocess, and no watcher.
//! If a contract test passes against both this and [`FilesystemVaultBackend`](crate::FilesystemVaultBackend),
//! the behaviour under test is genuinely specified by the trait rather than by the
//! filesystem.
//!
//! Deliberately `#[cfg(test)]` rather than behind a `testing` feature: an in-crate
//! test module is compiled and run by a plain `cargo test --workspace`, whereas a
//! non-default feature would let the whole contract suite be silently skipped. Slice
//! 2 should promote this to a `testing` feature the moment a *second* crate needs
//! it — at that point the suite must move to `tests/` and CI must pass
//! `--all-features`.

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::sync::Mutex;

use deep_obsidian_core::vault::{VaultChildEntry, VaultEntryKind, VaultError};

use crate::watch::ChangeStream;
use crate::{
    BackendDescriptor, BackendError, BackendKind, BackendRequest, BackendResponse, Capability,
    ChildListing, ContentRequest, ContentResponse, HealthRequest, HealthResponse, ManifestRequest,
    ManifestResponse, MutationRequest, MutationResponse, OpaqueCursor, RecallRequest, VaultBackend,
};

/// The one refusal this backend shares across every capability it lacks.
///
/// Deliberately generic where the real backends' refusals are specific: this backend
/// exists to prove the boundary is a boundary, and a bespoke message per variant would
/// be wording nobody ever reads. What matters is that it refuses at all — an in-memory
/// map that quietly answered "no versions" or "search found nothing" would let a
/// contract test pass while specifying nothing.
const IN_MEMORY_UNSUPPORTED_MESSAGE: &str =
    "this in-memory backend keeps no version history, performs no ranked search, and has no \
observable deletion";

/// An in-memory vault. Paths are vault-relative strings; directories are implied by
/// the path segments of the notes they contain.
pub struct InMemoryVaultBackend {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryVaultBackend {
    pub fn new() -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
        }
    }

    /// Seed a file, bypassing the write policy. For test setup only.
    pub fn seed(&self, path: &str, bytes: impl Into<Vec<u8>>) {
        self.files
            .lock()
            .expect("in-memory vault lock")
            .insert(path.to_string(), bytes.into());
    }

    /// Apply the same normalization and containment rules core applies, so
    /// traversal is rejected identically with identical wording.
    fn normalize(path: &str) -> Result<String, BackendError> {
        let trimmed = path.trim_start_matches('/');
        if trimmed.is_empty() {
            return Err(BackendError::Vault(VaultError::InvalidVaultRelativePath(
                path.to_string(),
            )));
        }
        // Resolve `.` and `..` lexically, refusing anything that climbs above the
        // root — the same outcome as core's normalize-then-strip_prefix guard.
        let mut segments: Vec<String> = Vec::new();
        for component in Path::new(trimmed).components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if segments.pop().is_none() {
                        return Err(BackendError::Vault(VaultError::InvalidVaultRelativePath(
                            path.to_string(),
                        )));
                    }
                }
                Component::Normal(part) => segments.push(part.to_string_lossy().into_owned()),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(BackendError::Vault(VaultError::InvalidVaultRelativePath(
                        path.to_string(),
                    )));
                }
            }
        }
        if segments.is_empty() {
            return Err(BackendError::Vault(VaultError::InvalidVaultRelativePath(
                path.to_string(),
            )));
        }
        Ok(segments.join("/"))
    }

    /// Mirror core's protected-template-folder write policy.
    fn ensure_writable(path: &str) -> Result<String, BackendError> {
        let normalized = Self::normalize(path)?;
        if normalized.split('/').any(|segment| {
            segment.eq_ignore_ascii_case("template") || segment.eq_ignore_ascii_case("templates")
        }) {
            return Err(BackendError::Vault(VaultError::ProtectedWritePath(
                path.to_string(),
            )));
        }
        Ok(normalized)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, BackendError> {
        let normalized = Self::normalize(path)?;
        self.files
            .lock()
            .expect("in-memory vault lock")
            .get(&normalized)
            .cloned()
            .ok_or_else(|| {
                BackendError::io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No such file or directory (os error 2)",
                ))
            })
    }
}

impl Default for InMemoryVaultBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl VaultBackend for InMemoryVaultBackend {
    fn descriptor(&self) -> BackendDescriptor {
        // Honest about what it cannot do: no ripgrep, no watcher, no uploads. The
        // contract suite asserts the server would not advertise grep for it.
        BackendDescriptor::new(
            BackendKind::InMemory,
            [Capability::BinaryRead, Capability::BinaryWrite],
        )
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, BackendError> {
        match request {
            BackendRequest::Content(ContentRequest::ReadText {
                version: Some(_), ..
            }) => Err(BackendError::Unsupported(
                IN_MEMORY_UNSUPPORTED_MESSAGE.to_string(),
            )),
            BackendRequest::Content(ContentRequest::ReadText { path, .. }) => {
                let bytes = self.read(&path)?;
                let text = String::from_utf8(bytes).map_err(|error| {
                    BackendError::io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error.to_string(),
                    ))
                })?;
                Ok(BackendResponse::Content(ContentResponse::Text {
                    text,
                    // No versioning here either; see the filesystem backend.
                    version: None,
                }))
            }
            BackendRequest::Content(ContentRequest::ReadBytes { path }) => Ok(
                BackendResponse::Content(ContentResponse::Bytes(self.read(&path)?)),
            ),
            BackendRequest::Content(ContentRequest::Stat { path }) => {
                let bytes = self.read(&path)?;
                Ok(BackendResponse::Content(ContentResponse::Stat {
                    size_bytes: bytes.len() as u64,
                }))
            }
            BackendRequest::Content(ContentRequest::ResolvePath { path }) => {
                Self::normalize(&path)?;
                Ok(BackendResponse::Content(ContentResponse::PathAccepted))
            }
            BackendRequest::Mutation(MutationRequest::WriteText { path, content, .. }) => {
                let normalized = Self::ensure_writable(&path)?;
                let created = self
                    .files
                    .lock()
                    .expect("in-memory vault lock")
                    .insert(normalized, content.into_bytes())
                    .is_none();
                Ok(BackendResponse::Mutation(MutationResponse::Written {
                    created,
                }))
            }
            BackendRequest::Mutation(MutationRequest::SweepOrphanStagingFiles) => {
                Ok(BackendResponse::Mutation(MutationResponse::Swept))
            }
            // Removing a key from a map would be a delete, but not an OBSERVABLE and
            // RECOVERABLE one, which is what the request means.
            BackendRequest::Mutation(MutationRequest::Rename { from, to, .. }) => {
                let mut files = self.files.lock().expect("files lock");
                let Some(content) = files.remove(&from) else {
                    return Err(BackendError::Message(format!(
                        "cannot rename {from}: no such note"
                    )));
                };
                let replaced_destination = files.insert(to, content).is_some();
                Ok(BackendResponse::Mutation(MutationResponse::Renamed {
                    replaced_destination,
                    // One lock held across both map operations.
                    atomic: true,
                }))
            }
            BackendRequest::Mutation(MutationRequest::SoftDelete { .. }) => Err(
                BackendError::Unsupported(IN_MEMORY_UNSUPPORTED_MESSAGE.to_string()),
            ),
            BackendRequest::Mutation(MutationRequest::CommitUploadStream { .. }) => Err(
                BackendError::Unsupported("this backend does not support uploads".to_string()),
            ),
            BackendRequest::Manifest(ManifestRequest::WalkMarkdown) => {
                let files = self.files.lock().expect("in-memory vault lock");
                let mut markdown = files
                    .keys()
                    .filter(|path| path.to_lowercase().ends_with(".md"))
                    .cloned()
                    .collect::<Vec<_>>();
                markdown.sort();
                Ok(BackendResponse::Manifest(ManifestResponse::MarkdownFiles(
                    markdown,
                )))
            }
            BackendRequest::Manifest(ManifestRequest::TopLevelFolders) => {
                let files = self.files.lock().expect("in-memory vault lock");
                let mut folders = files
                    .keys()
                    .filter_map(|path| path.split_once('/').map(|(head, _)| head.to_string()))
                    .filter(|folder| !folder.starts_with('.'))
                    .collect::<Vec<_>>();
                folders.sort();
                folders.dedup();
                Ok(BackendResponse::Manifest(ManifestResponse::Folders(
                    folders,
                )))
            }
            BackendRequest::Manifest(ManifestRequest::ListChildren {
                path,
                include_hidden,
                ..
            }) => {
                let prefix = match path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                    Some(path) => format!("{}/", Self::normalize(path)?),
                    None => String::new(),
                };
                let files = self.files.lock().expect("in-memory vault lock");
                let mut directories: Vec<String> = Vec::new();
                let mut entries: Vec<VaultChildEntry> = Vec::new();
                for (stored, bytes) in files.iter() {
                    let Some(rest) = stored.strip_prefix(&prefix) else {
                        continue;
                    };
                    match rest.split_once('/') {
                        // A nested path contributes its immediate directory.
                        Some((directory, _)) => {
                            let full = format!("{prefix}{directory}");
                            if !directories.contains(&full) {
                                directories.push(full);
                            }
                        }
                        None => {
                            if !include_hidden && rest.starts_with('.') {
                                continue;
                            }
                            entries.push(VaultChildEntry {
                                name: rest.to_string(),
                                path: stored.clone(),
                                kind: VaultEntryKind::File,
                                is_markdown: rest.to_lowercase().ends_with(".md"),
                                size_bytes: Some(bytes.len() as u64),
                            });
                        }
                    }
                }
                let mut children = directories
                    .into_iter()
                    .filter(|full| {
                        include_hidden
                            || !full.rsplit('/').next().unwrap_or_default().starts_with('.')
                    })
                    .map(|full| VaultChildEntry {
                        name: full.rsplit('/').next().unwrap_or_default().to_string(),
                        path: full,
                        kind: VaultEntryKind::Directory,
                        is_markdown: false,
                        size_bytes: None,
                    })
                    .collect::<Vec<_>>();
                // Same ordering contract as the filesystem: directories first, each
                // group by vault-relative path.
                children.sort_by(|left, right| left.path.cmp(&right.path));
                entries.sort_by(|left, right| left.path.cmp(&right.path));
                children.extend(entries);
                Ok(BackendResponse::Manifest(ManifestResponse::Children(
                    ChildListing::exhaustive(children),
                )))
            }
            BackendRequest::Manifest(ManifestRequest::Versions { .. }) => Err(
                BackendError::Unsupported(IN_MEMORY_UNSUPPORTED_MESSAGE.to_string()),
            ),
            BackendRequest::Recall(RecallRequest::Grep { .. }) => Err(BackendError::Message(
                crate::grep::RIPGREP_UNAVAILABLE_MESSAGE.to_string(),
            )),
            BackendRequest::Recall(RecallRequest::Search(_)) => Err(BackendError::Unsupported(
                IN_MEMORY_UNSUPPORTED_MESSAGE.to_string(),
            )),
            BackendRequest::Health(HealthRequest::Overview) => {
                Ok(BackendResponse::Health(HealthResponse::Overview {
                    reachable: true,
                }))
            }
        }
    }

    /// No change feed: an in-memory vault only changes when the test changes it.
    fn changes(&self, _after: Option<OpaqueCursor>) -> ChangeStream {
        ChangeStream::empty()
    }
}
