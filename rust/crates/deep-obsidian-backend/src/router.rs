//! The mount router: one logical vault namespace over several backends.
//!
//! A [`VaultRouter`] owns a table of [`Mount`]s, each a backend grafted onto a
//! logical folder prefix. Every path-bearing request is resolved to exactly one
//! mount by longest-prefix match on folder boundaries; requests that would need
//! two backends at once are refused with a normalized error rather than silently
//! served from one of them.
//!
//! ## Single-mount behaviour is identical BY CONSTRUCTION
//!
//! A router holding exactly one mount, at the root, is the legacy topology. In
//! that case [`VaultRouter::execute`] hands the request to that backend
//! **untouched** — no path normalization, no listing synthesis, no re-sorting, no
//! error rewriting. That is the whole reason the fast path exists: it makes
//! "zero behaviour change for single-mount configs" a structural property instead
//! of something that has to be re-verified against every golden whenever the
//! multi-mount code below changes.
//!
//! ## What this router does NOT do
//!
//! Nothing here federates. Every mount now has its own search index (the server
//! holds one index runtime per mount), but an operation whose ANSWER spans mounts
//! still has to merge and re-rank independently built result sets, and that is not
//! implemented. So whole-vault manifest requests
//! ([`ManifestRequest::WalkMarkdown`], [`ManifestRequest::TopLevelFolders`]), unscoped
//! [`RecallRequest::Grep`] and every [`RecallRequest::Search`] are refused on a
//! multi-mount router with [`RouterError::FederationUnsupported`] rather than answered
//! from a single mount. Presenting one mount's results as the whole vault's would be a
//! wrong answer, not a partial one.
//!
//! Requests that CAN name one mount do route: a `glob`-scoped grep here, and every
//! path- or scope-bearing recall tool in the server, which uses
//! [`VaultRouter::resolve`] to pick the mount and [`Mount::to_logical`] to present
//! that mount's paths back in the logical namespace.

use std::sync::Arc;

use thiserror::Error;

use crate::{
    BackendError, BackendRequest, BackendResponse, ChildListing, ContentRequest, GrepMatch,
    GrepOutcome, HealthRequest, HealthResponse, ManifestRequest, ManifestResponse, MutationRequest,
    MutationResponse, RecallRequest, RecallResponse, VaultBackend, VaultChildEntry, VaultEntryKind,
};

// ---------------------------------------------------------------------------
// Mounts
// ---------------------------------------------------------------------------

/// One backend grafted onto a logical folder prefix.
pub struct Mount {
    /// Stable, user-chosen identifier. Appears in error messages and in
    /// `vault_info`, never in a path.
    pub id: String,
    /// The logical folder prefix, normalized: no leading or trailing slash,
    /// forward slashes only, `""` for the vault root.
    pub mount_at: String,
    pub backend: Arc<dyn VaultBackend>,
}

impl Mount {
    pub fn new(
        id: impl Into<String>,
        mount_at: impl AsRef<str>,
        backend: Arc<dyn VaultBackend>,
    ) -> Self {
        Self {
            id: id.into(),
            mount_at: normalize_prefix(mount_at.as_ref()),
            backend,
        }
    }

    /// True when this mount serves the vault root.
    pub fn is_root(&self) -> bool {
        self.mount_at.is_empty()
    }

    /// Render a mount-relative path back into the logical namespace.
    ///
    /// The inverse of [`VaultRouter::resolve`]'s `backend_relative_path`, and the
    /// only correct way to present a path that came OUT of a mount — a search
    /// index, a grep hit, a listing — to a client, which only ever knows logical
    /// paths.
    ///
    /// For the root mount this is the identity function. That is load-bearing:
    /// every single-mount caller that pipes results through it is provably
    /// unchanged.
    pub fn to_logical(&self, relative: &str) -> String {
        if self.mount_at.is_empty() {
            relative.to_string()
        } else if relative.is_empty() {
            self.mount_at.clone()
        } else {
            format!("{}/{}", self.mount_at, relative)
        }
    }
}

impl std::fmt::Debug for Mount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Mount")
            .field("id", &self.id)
            .field("mount_at", &self.mount_at)
            .field("backend", &self.backend.descriptor().kind)
            .finish()
    }
}

/// Which mount serves a logical path, and what the path looks like to it.
#[derive(Debug)]
pub struct Resolved<'router> {
    pub mount: &'router Mount,
    /// The path as the mount's backend must see it.
    ///
    /// For a non-root mount this is the logical path with the mount prefix
    /// removed. For the ROOT mount it is the caller's path **verbatim** — not
    /// normalized — so that malformed spellings (`..` segments, a leading slash,
    /// a trailing slash) still reach the backend exactly as they did before there
    /// was a router, and produce exactly the same error.
    pub backend_relative_path: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A routing failure, or a passed-through backend failure.
///
/// [`RouterError::Backend`] is `#[error(transparent)]` on purpose: the server
/// renders errors with `Display`, and every backend string is frozen public MCP
/// behaviour. Wrapping must add not one character.
#[derive(Debug, Error)]
pub enum RouterError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// The path falls outside every mount's subtree. Unreachable for a resolved
    /// config, which always has a root mount, but the router does not assume that.
    #[error("no vault mount serves the path: {path}")]
    Unrouted { path: String },
    /// A single operation named two paths that live on different mounts.
    ///
    /// No tool exposes such an operation today (there is no rename or move), so
    /// this is a guard placed at the boundary before one exists, not a condition
    /// users currently hit.
    // NOTE: the fields are `source_path`/`destination_path`, not `source`:
    // `thiserror` treats a field literally named `source` as the error's
    // `std::error::Error::source`, which a `String` cannot be.
    #[error("{operation} would span two vault mounts ({source_path} is on mount '{source_mount}', {destination_path} is on mount '{destination_mount}'); operations across backends are not supported")]
    CrossBackendUnsupported {
        operation: &'static str,
        source_path: String,
        source_mount: String,
        destination_path: String,
        destination_mount: String,
    },
    /// The operation would have to read every mount to answer correctly, and this
    /// slice cannot. Deliberately an error rather than a partial answer.
    #[error("{operation} cannot span multiple vault mounts yet: {remediation}")]
    FederationUnsupported {
        operation: &'static str,
        remediation: &'static str,
    },
    /// Two mounts claim the identical prefix, so a path under it has no unique
    /// owner. Config validation reports this with a friendlier message; the check
    /// is repeated here so a router built by any other route is still sound.
    #[error("mounts '{first}' and '{second}' both mount at {mount_at:?}")]
    DuplicateMount {
        mount_at: String,
        first: String,
        second: String,
    },
}

impl RouterError {
    /// The `io::ErrorKind` behind this failure, when a backend produced one.
    ///
    /// Exists so a caller can keep branching on "destination absent" versus every
    /// other failure after the router was inserted underneath it — the same
    /// distinction [`BackendError::io_kind`] exists for, which `Display` erases.
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            RouterError::Backend(error) => error.io_kind(),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// The comparison key for prefix matching: no leading or trailing slashes.
///
/// This is a *matching* normalization only. It never becomes the path handed to a
/// backend for the root mount (see [`Resolved::backend_relative_path`]), so it
/// cannot launder a malformed path into a valid one.
fn normalize_prefix(path: &str) -> String {
    path.trim().trim_matches('/').to_string()
}

/// True when `prefix` contains `path` on a folder boundary.
///
/// The boundary is what makes prefix matching correct: `"Team"` owns
/// `Team/Note.md` and `Team` itself, but NOT `Teamwork/Note.md`. A plain
/// `starts_with` would get that wrong, which is the single most likely way a
/// mount router leaks one vault's content into another's namespace.
fn prefix_contains(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if path == prefix {
        return true;
    }
    path.len() > prefix.len() && path.as_bytes()[prefix.len()] == b'/' && path.starts_with(prefix)
}

/// The longest run of leading literal (metacharacter-free) directory segments of
/// a ripgrep glob, if any.
///
/// Used to decide whether a `grep_search` is scoped tightly enough to belong to
/// one mount. Deliberately conservative: ripgrep's `--glob` is a filter applied
/// over a walk, not a search root, so a glob is only *evidence* of scope. Anything
/// that is not an unambiguous literal directory prefix — a negation, a
/// metacharacter or `.`/`..` in the first segment, an alternation, no directory
/// component at all — yields `None`, and the caller refuses the request rather
/// than guessing.
fn literal_glob_prefix(glob: &str) -> Option<String> {
    let glob = glob.trim();
    if glob.is_empty() || glob.starts_with('!') {
        return None;
    }
    let segments: Vec<&str> = glob.split('/').collect();
    if segments.len() < 2 {
        // No directory component: the glob matches by basename anywhere.
        return None;
    }
    let mut literal = Vec::new();
    // The last segment is the filename pattern and is never part of the prefix.
    for segment in &segments[..segments.len() - 1] {
        let is_literal = !segment.is_empty()
            && *segment != "."
            && *segment != ".."
            && !segment
                .chars()
                .any(|character| matches!(character, '*' | '?' | '[' | ']' | '{' | '}' | '\\'));
        if !is_literal {
            break;
        }
        literal.push(*segment);
    }
    if literal.is_empty() {
        None
    } else {
        Some(literal.join("/"))
    }
}

/// Core's child ordering, replicated exactly: directories before files, then by
/// vault-relative PATH (not name).
///
/// Replicated rather than reused because `deep-obsidian-core::vault` sorts inside
/// its own listing function and exposes no comparator. Applying it to an
/// already-sorted single-mount listing would be a no-op — but the single-mount
/// fast path means it is never applied there at all.
fn sort_children(entries: &mut [VaultChildEntry]) {
    entries.sort_by(|left, right| match (&left.kind, &right.kind) {
        (VaultEntryKind::Directory, VaultEntryKind::File) => std::cmp::Ordering::Less,
        (VaultEntryKind::File, VaultEntryKind::Directory) => std::cmp::Ordering::Greater,
        _ => left.path.cmp(&right.path),
    });
}

// ---------------------------------------------------------------------------
// The router
// ---------------------------------------------------------------------------

/// A logical vault namespace assembled from one or more mounts.
#[derive(Debug)]
pub struct VaultRouter {
    /// Config order, deliberately: it is the order `vault_info` reports and the
    /// order fan-out probes run in. Resolution scans the whole (tiny) table for
    /// the longest match, so no sort order is load-bearing.
    mounts: Vec<Mount>,
}

impl VaultRouter {
    /// Build a router, rejecting a table in which two mounts claim the same
    /// prefix.
    pub fn new(mounts: Vec<Mount>) -> Result<Self, RouterError> {
        for (position, mount) in mounts.iter().enumerate() {
            if let Some(earlier) = mounts[..position]
                .iter()
                .find(|other| other.mount_at == mount.mount_at)
            {
                return Err(RouterError::DuplicateMount {
                    mount_at: mount.mount_at.clone(),
                    first: earlier.id.clone(),
                    second: mount.id.clone(),
                });
            }
        }
        Ok(Self { mounts })
    }

    /// Convenience constructor for the one-backend-at-the-root topology.
    pub fn single(id: impl Into<String>, backend: Arc<dyn VaultBackend>) -> Self {
        Self {
            mounts: vec![Mount::new(id, "", backend)],
        }
    }

    pub fn mounts(&self) -> &[Mount] {
        &self.mounts
    }

    /// The mount serving the vault root, if any.
    pub fn root(&self) -> Option<&Mount> {
        self.mounts.iter().find(|mount| mount.is_root())
    }

    /// `Some` only for the legacy topology: exactly one mount, at the root.
    ///
    /// # Do not remove this without replacing what it provides
    ///
    /// Measured, not assumed: with this returning `None` unconditionally, 13 of the
    /// 14 MCP contract goldens still pass — the path handling, the listing merge and
    /// the re-sort below really are byte-compatible. The fourteenth
    /// (`tool_find_files_substring` / `find_files`) fails, because the multi-mount
    /// branch of [`VaultRouter::execute`] refuses whole-vault manifests outright.
    /// So this fast path is load-bearing for exactly one thing: letting
    /// [`ManifestRequest::WalkMarkdown`] and [`ManifestRequest::TopLevelFolders`]
    /// work at all. Whoever federates those can drop it; until then it is the reason
    /// `find_files` and `recommend_folder` still function on a single-mount vault
    /// (both refuse a multi-mount one, because their answer is a whole-vault
    /// ranking, not an enumeration).
    fn single_root_mount(&self) -> Option<&Mount> {
        match self.mounts.as_slice() {
            [only] if only.is_root() => Some(only),
            _ => None,
        }
    }

    /// True when more than one mount is in play.
    pub fn is_multi_mount(&self) -> bool {
        self.mounts.len() > 1
    }

    /// Which mount owns `logical_path`, by longest-prefix match on folder
    /// boundaries.
    pub fn resolve(&self, logical_path: &str) -> Result<Resolved<'_>, RouterError> {
        let key = normalize_prefix(logical_path);
        let mount = self
            .mounts
            .iter()
            .filter(|mount| prefix_contains(&mount.mount_at, &key))
            .max_by_key(|mount| mount.mount_at.len())
            .ok_or_else(|| RouterError::Unrouted {
                path: logical_path.to_string(),
            })?;

        let backend_relative_path = if mount.mount_at.is_empty() {
            // Verbatim: see `Resolved::backend_relative_path`.
            logical_path.to_string()
        } else if key.len() == mount.mount_at.len() {
            String::new()
        } else {
            key[mount.mount_at.len() + 1..].to_string()
        };

        Ok(Resolved {
            mount,
            backend_relative_path,
        })
    }

    /// True when the subtree `logical_prefix` contains a mount OTHER than
    /// `mount_id`.
    ///
    /// The guard every "scope it to one mount" argument needs. Resolving a prefix
    /// to its owning mount is not enough: with mounts at `""` and `Team/Alpha`, the
    /// prefix `Team` resolves to the ROOT mount, yet part of that subtree lives on
    /// the `alpha` mount. Serving the scope from the resolved mount alone would
    /// silently omit it, so callers refuse instead.
    pub fn scope_contains_other_mount(&self, mount_id: &str, logical_prefix: &str) -> bool {
        let prefix = normalize_prefix(logical_prefix);
        self.mounts
            .iter()
            .any(|mount| mount.id != mount_id && prefix_contains(&prefix, &mount.mount_at))
    }

    /// The mount backing `logical_path`, for callers that need to hold the
    /// backend across an await (the streaming upload commit).
    pub fn backend_for(&self, logical_path: &str) -> Result<Arc<dyn VaultBackend>, RouterError> {
        Ok(self.resolve(logical_path)?.mount.backend.clone())
    }

    /// Resolve two paths that a single operation must touch together, refusing the
    /// cross-backend case.
    ///
    /// Nothing calls this yet — there is no rename or move tool. It exists so that
    /// when one is added, the guard is already at the boundary rather than
    /// something the new tool has to remember.
    pub fn resolve_pair(
        &self,
        operation: &'static str,
        source: &str,
        destination: &str,
    ) -> Result<(Resolved<'_>, Resolved<'_>), RouterError> {
        let from = self.resolve(source)?;
        let to = self.resolve(destination)?;
        if from.mount.id != to.mount.id {
            return Err(RouterError::CrossBackendUnsupported {
                operation,
                source_path: source.to_string(),
                source_mount: from.mount.id.clone(),
                destination_path: destination.to_string(),
                destination_mount: to.mount.id.clone(),
            });
        }
        Ok((from, to))
    }

    /// Perform one request against whichever mount (or mounts) it belongs to.
    pub async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, RouterError> {
        // Legacy topology: hand the request over untouched. See the module docs.
        if let Some(mount) = self.single_root_mount() {
            return Ok(mount.backend.execute(request).await?);
        }

        match request {
            BackendRequest::Manifest(ManifestRequest::ListChildren {
                path,
                include_hidden,
                include_ignored,
            }) => Ok(BackendResponse::Manifest(ManifestResponse::Children(
                self.child_listing(path.as_deref(), include_hidden, include_ignored)
                    .await?,
            ))),
            // Path-bearing, so it routes like a read rather than needing federation:
            // one note lives on exactly one mount, and its history is that mount's.
            BackendRequest::Manifest(ManifestRequest::Versions { path }) => {
                let resolved = self.resolve(&path)?;
                Ok(resolved
                    .mount
                    .backend
                    .execute(BackendRequest::note_versions(
                        resolved.backend_relative_path.clone(),
                    ))
                    .await?)
            }
            // Whole-vault manifests would need every mount to be correct.
            BackendRequest::Manifest(ManifestRequest::WalkMarkdown) => {
                Err(RouterError::FederationUnsupported {
                    operation: "listing every markdown file",
                    remediation: "it would have to read every mount, which is not implemented yet",
                })
            }
            BackendRequest::Manifest(ManifestRequest::TopLevelFolders) => {
                Err(RouterError::FederationUnsupported {
                    operation: "listing top-level folders",
                    remediation: "it would have to read every mount, which is not implemented yet",
                })
            }
            BackendRequest::Content(request) => self.route_content(request).await,
            BackendRequest::Mutation(MutationRequest::SweepOrphanStagingFiles) => {
                // Housekeeping, never a contract: sweep every mount, ignore every
                // failure, exactly as the single-mount path ignores its own.
                for mount in &self.mounts {
                    let _ = mount
                        .backend
                        .execute(BackendRequest::sweep_orphan_staging_files())
                        .await;
                }
                Ok(BackendResponse::Mutation(MutationResponse::Swept))
            }
            BackendRequest::Mutation(request) => self.route_mutation(request).await,
            BackendRequest::Recall(RecallRequest::Grep {
                query,
                regex,
                case_sensitive,
                glob,
                context_lines,
                limit,
            }) => {
                self.grep(query, regex, case_sensitive, glob, context_lines, limit)
                    .await
            }
            // A ranked search across mounts is the federation problem in its purest
            // form: each mount's index scores on its own scale, so merging the
            // orderings needs comparable scores. Refused rather than answered from one
            // mount. The server does NOT reach this — it selects the mount itself and
            // calls that backend directly, exactly as it selects one mount's local
            // index — so this arm is the honest answer for a caller that did not.
            BackendRequest::Recall(RecallRequest::Search(_)) => {
                Err(RouterError::FederationUnsupported {
                    operation: "ranked search across mounts",
                    remediation:
                        "each mount ranks on its own scale, so a merged ordering needs comparable \
                         scores, which is not implemented; scope the search to one mount",
                })
            }
            BackendRequest::Health(HealthRequest::Overview) => {
                // A startup gate: every mount must be reachable, and the first
                // failure is reported with that backend's own wording.
                for mount in &self.mounts {
                    mount
                        .backend
                        .execute(BackendRequest::health_overview())
                        .await?;
                }
                Ok(BackendResponse::Health(HealthResponse::Overview {
                    reachable: true,
                }))
            }
        }
    }

    async fn route_content(&self, request: ContentRequest) -> Result<BackendResponse, RouterError> {
        let path = match &request {
            ContentRequest::ReadText { path, .. }
            | ContentRequest::ReadBytes { path }
            | ContentRequest::Stat { path }
            | ContentRequest::ResolvePath { path } => path.clone(),
        };
        let resolved = self.resolve(&path)?;
        let routed = match request {
            // `version` is forwarded, not dropped: dropping it would turn a request for
            // a superseded version into a request for the current one, and the caller
            // would be handed the wrong content with no error at all.
            ContentRequest::ReadText { version, .. } => ContentRequest::ReadText {
                path: resolved.backend_relative_path,
                version,
            },
            ContentRequest::ReadBytes { .. } => ContentRequest::ReadBytes {
                path: resolved.backend_relative_path,
            },
            ContentRequest::Stat { .. } => ContentRequest::Stat {
                path: resolved.backend_relative_path,
            },
            ContentRequest::ResolvePath { .. } => ContentRequest::ResolvePath {
                path: resolved.backend_relative_path,
            },
        };
        Ok(resolved
            .mount
            .backend
            .execute(BackendRequest::Content(routed))
            .await?)
    }

    async fn route_mutation(
        &self,
        request: MutationRequest,
    ) -> Result<BackendResponse, RouterError> {
        match request {
            MutationRequest::WriteText {
                path,
                content,
                base_version,
                resolve_divergence,
            } => {
                let resolved = self.resolve(&path)?;
                // `base_version` must be forwarded, not rebuilt: it is the caller's
                // observation of THIS destination, and dropping it here would
                // silently downgrade every write on a multi-mount vault to an
                // unguarded one while every single-mount test still passed. The same
                // argument covers `resolve_divergence`: dropping it would make every
                // divergence permanently unresolvable on a multi-mount vault.
                Ok(resolved
                    .mount
                    .backend
                    .execute(BackendRequest::write_text_full(
                        resolved.backend_relative_path.clone(),
                        content,
                        base_version,
                        resolve_divergence,
                    ))
                    .await?)
            }
            MutationRequest::SoftDelete { path } => {
                let resolved = self.resolve(&path)?;
                Ok(resolved
                    .mount
                    .backend
                    .execute(BackendRequest::soft_delete(
                        resolved.backend_relative_path.clone(),
                    ))
                    .await?)
            }
            MutationRequest::CommitUploadStream {
                path,
                expected_hash,
                max_bytes,
                chunks,
            } => {
                let resolved = self.resolve(&path)?;
                let backend = resolved.mount.backend.clone();
                let routed_path = resolved.backend_relative_path.clone();
                Ok(backend
                    .execute(BackendRequest::Mutation(
                        MutationRequest::CommitUploadStream {
                            path: routed_path,
                            expected_hash,
                            max_bytes,
                            chunks,
                        },
                    ))
                    .await?)
            }
            // Handled by the caller so that the fan-out is visible next to the
            // other whole-table operations.
            MutationRequest::SweepOrphanStagingFiles => {
                unreachable!("sweep is handled before route_mutation")
            }
        }
    }

    /// Direct children of `logical_folder`, merging the owning mount's own listing
    /// with a synthesized folder for every mount that lives underneath.
    ///
    /// # Shadowing
    ///
    /// If the owning mount also has a *physical* directory where a nested mount is
    /// grafted, the MOUNT WINS and the physical directory is shadowed — it is not
    /// listed twice, and it is not listed instead. That is the only choice
    /// consistent with `resolve`: reads and writes under that prefix already go to
    /// the nested mount by longest-prefix match, so listing the physical directory
    /// would advertise a folder whose contents are unreachable.
    pub async fn list_children(
        &self,
        logical_folder: Option<&str>,
        include_hidden: bool,
        include_ignored: bool,
    ) -> Result<Vec<VaultChildEntry>, RouterError> {
        self.child_listing(logical_folder, include_hidden, include_ignored)
            .await
            .map(|listing| listing.entries)
    }

    /// [`Self::list_children`] plus whether the owning mount could name every
    /// subfolder.
    ///
    /// # What the flag means after a merge
    ///
    /// It is the OWNING mount's flag, unchanged. The synthesized mount folders added
    /// here are derived from the router's own mount table, which is complete by
    /// construction, so they can never be the missing part — merging them in neither
    /// clears a shortfall nor introduces one. And when the owning mount's listing
    /// FAILED and only synthesized folders remain, the flag is `false`: nothing was
    /// enumerated, so nothing was truncated, and claiming truncation there would
    /// misattribute an outright failure to a cap.
    pub async fn child_listing(
        &self,
        logical_folder: Option<&str>,
        include_hidden: bool,
        include_ignored: bool,
    ) -> Result<ChildListing, RouterError> {
        let raw = logical_folder.unwrap_or("");
        let folder = normalize_prefix(raw);
        let synthesized = self.synthesized_child_folders(&folder);

        let resolved = self.resolve(raw)?;
        let routed_path = if resolved.backend_relative_path.is_empty() {
            None
        } else {
            Some(resolved.backend_relative_path.clone())
        };
        let own = resolved
            .mount
            .backend
            .execute(BackendRequest::list_children(
                routed_path,
                include_hidden,
                include_ignored,
            ))
            .await
            .and_then(BackendResponse::into_child_listing);

        let (mut entries, folders_truncated) = match own {
            Ok(listing) => (
                listing
                    .entries
                    .into_iter()
                    .map(|entry| VaultChildEntry {
                        path: resolved.mount.to_logical(&entry.path),
                        ..entry
                    })
                    // Shadowing: a mount grafted onto this name wins.
                    .filter(|entry| !synthesized.iter().any(|folder| folder.path == entry.path))
                    .collect::<Vec<_>>(),
                listing.folders_truncated,
            ),
            // The folder may exist only as a synthetic ancestor of a nested mount
            // (nothing physical under the owning mount). Reporting the owning
            // mount's "not found" would then hide a folder the user really can
            // descend into. With no nested mounts there is nothing to synthesize
            // and the error is the honest answer, so it propagates.
            Err(error) => {
                if synthesized.is_empty() {
                    return Err(error.into());
                }
                (Vec::new(), false)
            }
        };

        entries.extend(synthesized);
        sort_children(&mut entries);
        Ok(ChildListing {
            entries,
            folders_truncated,
        })
    }

    /// One directory entry per distinct immediate child of `folder` that leads to
    /// a mount.
    ///
    /// Generalized past "a mount mounted exactly one level down": a mount at
    /// `Team/Alpha` also makes `Team/` exist logically, so listing the root
    /// synthesizes `Team`. Without that, a nested mount would be invisible from
    /// above and therefore undiscoverable.
    fn synthesized_child_folders(&self, folder: &str) -> Vec<VaultChildEntry> {
        let mut names: Vec<String> = Vec::new();
        for mount in &self.mounts {
            if mount.mount_at.is_empty() || !prefix_contains(folder, &mount.mount_at) {
                continue;
            }
            // The mount is at or below `folder`; take the one segment immediately
            // below `folder`.
            let below = if folder.is_empty() {
                mount.mount_at.as_str()
            } else if mount.mount_at.len() == folder.len() {
                // The mount IS this folder, so it has no entry of its own here.
                continue;
            } else {
                &mount.mount_at[folder.len() + 1..]
            };
            let segment = below.split('/').next().unwrap_or(below);
            if segment.is_empty() || names.iter().any(|existing| existing == segment) {
                continue;
            }
            names.push(segment.to_string());
        }

        names
            .into_iter()
            .map(|name| VaultChildEntry {
                path: if folder.is_empty() {
                    name.clone()
                } else {
                    format!("{folder}/{name}")
                },
                name,
                kind: VaultEntryKind::Directory,
                // Shaped exactly like a physical directory entry so a client
                // cannot tell a mount point from a folder.
                is_markdown: false,
                size_bytes: None,
            })
            .collect()
    }

    /// Line search, scoped to the single mount the caller's glob narrows to.
    async fn grep(
        &self,
        query: String,
        regex: bool,
        case_sensitive: bool,
        glob: Option<String>,
        context_lines: usize,
        limit: usize,
    ) -> Result<BackendResponse, RouterError> {
        const UNSCOPED: RouterError = RouterError::FederationUnsupported {
            operation: "grep_search",
            remediation:
                "scope it to one mount by passing a 'glob' that starts with that mount's folder (for example \"Team/**/*.md\")",
        };

        let Some(glob) = glob else {
            return Err(UNSCOPED);
        };
        let Some(prefix) = literal_glob_prefix(&glob) else {
            return Err(UNSCOPED);
        };
        let resolved = self.resolve(&prefix)?;
        // The scoped subtree must contain no OTHER mount, or the single-mount run
        // below would silently skip part of it.
        if self.scope_contains_other_mount(&resolved.mount.id, &prefix) {
            return Err(UNSCOPED);
        }

        let mount = resolved.mount;
        let mount_relative_glob = if mount.mount_at.is_empty() {
            glob.clone()
        } else {
            glob.trim_start_matches('/')
                .strip_prefix(&format!("{}/", mount.mount_at))
                .unwrap_or(&glob)
                .to_string()
        };

        let outcome = mount
            .backend
            .execute(BackendRequest::Recall(RecallRequest::Grep {
                query,
                regex,
                case_sensitive,
                glob: Some(mount_relative_glob),
                context_lines,
                limit,
            }))
            .await
            .and_then(BackendResponse::into_grep_outcome)?;
        // Only the PATHS are rewritten. The exhaustiveness report travels through
        // untouched: it is the serving mount's own statement about its own search, and
        // re-deriving or defaulting it here would turn a candidate-bounded backend's
        // "I did not look everywhere" into silence — which is exactly the honesty
        // failure the field exists to prevent.
        Ok(BackendResponse::Recall(RecallResponse::Grep(GrepOutcome {
            matches: outcome
                .matches
                .into_iter()
                .map(|item| GrepMatch {
                    path: mount.to_logical(&item.path),
                    ..item
                })
                .collect(),
            ..outcome
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Capability, FilesystemVaultBackend};
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// A fresh temp directory. Mirrors `filesystem::tests::temp_dir`.
    fn temp_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("router-{prefix}-{}-{}", std::process::id(), nanos))
    }

    /// Real filesystem backends, not the in-memory stand-in: listing semantics
    /// (what a missing directory does, how entries sort) and grep are exactly what
    /// the router has to compose, and only the filesystem backend has them.
    struct Vaults {
        root: PathBuf,
    }

    impl Vaults {
        fn new(prefix: &str) -> Self {
            let root = temp_root(prefix);
            let _ = std::fs::remove_dir_all(&root);
            Self { root }
        }

        /// Create a vault directory seeded with `files`, and return a backend on it.
        fn vault(&self, name: &str, files: &[(&str, &str)]) -> Arc<dyn VaultBackend> {
            let path = self.root.join(name);
            std::fs::create_dir_all(&path).expect("create vault");
            for (relative, content) in files {
                let file = path.join(relative);
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent).expect("create parent");
                }
                std::fs::write(&file, content).expect("write file");
            }
            Arc::new(FilesystemVaultBackend::new(path))
        }

        /// A path inside the temp root that is deliberately never created.
        fn absent(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }

    impl Drop for Vaults {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn grep_available(router: &VaultRouter) -> bool {
        router
            .mounts()
            .iter()
            .all(|mount| mount.backend.descriptor().supports(Capability::GrepSearch))
    }

    /// The canonical two-mount topology: a root vault plus a `Team` mount.
    fn two_mounts(vaults: &Vaults) -> VaultRouter {
        VaultRouter::new(vec![
            Mount::new(
                "vault",
                "",
                vaults.vault("root", &[("Root.md", "root"), ("Notes/Deep.md", "deep")]),
            ),
            Mount::new(
                "team",
                "Team",
                vaults.vault("team", &[("Charter.md", "charter body")]),
            ),
        ])
        .expect("router")
    }

    // -----------------------------------------------------------------------
    // resolve
    // -----------------------------------------------------------------------

    #[test]
    fn longest_prefix_wins() {
        let vaults = Vaults::new("longest-prefix");
        let router = VaultRouter::new(vec![
            Mount::new("vault", "", vaults.vault("root", &[])),
            Mount::new("team", "Team", vaults.vault("team", &[])),
            Mount::new("alpha", "Team/Alpha", vaults.vault("alpha", &[])),
        ])
        .expect("router");

        assert_eq!(router.resolve("Root.md").unwrap().mount.id, "vault");
        assert_eq!(router.resolve("Team/Charter.md").unwrap().mount.id, "team");
        let deep = router.resolve("Team/Alpha/Plan.md").unwrap();
        assert_eq!(deep.mount.id, "alpha");
        assert_eq!(deep.backend_relative_path, "Plan.md");
    }

    #[test]
    fn prefixes_only_match_on_folder_boundaries() {
        let vaults = Vaults::new("boundaries");
        let router = two_mounts(&vaults);
        // "Team" must not capture "Teamwork".
        let sibling = router.resolve("Teamwork/Note.md").unwrap();
        assert_eq!(sibling.mount.id, "vault");
        assert_eq!(sibling.backend_relative_path, "Teamwork/Note.md");
        // The prefix itself resolves to its mount, with an empty relative path.
        let exact = router.resolve("Team").unwrap();
        assert_eq!(exact.mount.id, "team");
        assert_eq!(exact.backend_relative_path, "");
    }

    #[test]
    fn root_paths_reach_the_backend_verbatim() {
        // Byte-identity insurance: a malformed spelling must not be laundered by
        // the router before the backend gets its chance to reject it with the
        // frozen wording.
        let vaults = Vaults::new("verbatim");
        let router = two_mounts(&vaults);
        for spelling in ["Notes/../Root.md", "/Root.md", "Root.md/", ".."] {
            assert_eq!(
                router.resolve(spelling).unwrap().backend_relative_path,
                spelling
            );
        }
    }

    #[test]
    fn a_path_under_no_mount_is_unrouted() {
        let vaults = Vaults::new("unrouted");
        let router = VaultRouter::new(vec![Mount::new("team", "Team", vaults.vault("team", &[]))])
            .expect("router");
        let error = router.resolve("Elsewhere/Note.md").unwrap_err();
        assert!(matches!(error, RouterError::Unrouted { .. }));
        assert!(error.to_string().contains("Elsewhere/Note.md"));
        // ...while a path inside the mount still routes.
        assert!(router.resolve("Team/Note.md").is_ok());
    }

    #[test]
    fn scope_contains_other_mount_only_reports_mounts_inside_the_subtree() {
        let vaults = Vaults::new("scope-guard");
        let router = VaultRouter::new(vec![
            Mount::new("vault", "", vaults.vault("root", &[])),
            Mount::new("alpha", "Team/Alpha", vaults.vault("alpha", &[])),
        ])
        .expect("router");

        // `Team` resolves to the ROOT mount, but part of that subtree is served by
        // `alpha`, so the root mount alone does not cover it.
        assert!(router.scope_contains_other_mount("vault", "Team"));
        assert!(router.scope_contains_other_mount("vault", "Team/Alpha"));
        // A sibling folder on a name boundary is NOT inside the subtree.
        assert!(!router.scope_contains_other_mount("vault", "Teamwork"));
        // A subtree with nothing else grafted in it.
        assert!(!router.scope_contains_other_mount("vault", "Notes"));
        // The mount itself is never "another mount".
        assert!(!router.scope_contains_other_mount("alpha", "Team/Alpha"));
        // Spelling-insensitive, like every other prefix comparison here.
        assert!(router.scope_contains_other_mount("vault", "/Team/"));
    }

    #[test]
    fn duplicate_mount_prefixes_are_rejected() {
        let vaults = Vaults::new("duplicate");
        let error = VaultRouter::new(vec![
            Mount::new("team", "Team", vaults.vault("a", &[])),
            // A different spelling of the same prefix.
            Mount::new("team-two", "/Team/", vaults.vault("b", &[])),
        ])
        .expect_err("duplicate");
        assert!(matches!(error, RouterError::DuplicateMount { .. }));
    }

    #[test]
    fn cross_backend_pairs_are_refused_and_same_mount_pairs_are_not() {
        let vaults = Vaults::new("cross-backend");
        let router = two_mounts(&vaults);
        let error = router
            .resolve_pair("rename", "Root.md", "Team/Root.md")
            .expect_err("cross backend");
        let rendered = error.to_string();
        assert!(matches!(error, RouterError::CrossBackendUnsupported { .. }));
        assert!(rendered.contains("rename"));
        assert!(rendered.contains("'vault'"));
        assert!(rendered.contains("'team'"));

        let (from, to) = router
            .resolve_pair("rename", "Team/A.md", "Team/B.md")
            .expect("same mount");
        assert_eq!(from.mount.id, "team");
        assert_eq!(to.backend_relative_path, "B.md");
    }

    // -----------------------------------------------------------------------
    // Single-mount fast path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_single_root_mount_passes_requests_through_untouched() {
        let vaults = Vaults::new("fast-path");
        let router = VaultRouter::single("vault", vaults.vault("root", &[("Root.md", "root")]));
        assert!(!router.is_multi_mount());
        // Whole-vault requests, which a multi-mount router refuses, still work.
        let files = router
            .execute(BackendRequest::walk_markdown())
            .await
            .unwrap()
            .into_markdown_files()
            .unwrap();
        assert_eq!(files, vec!["Root.md".to_string()]);
        assert_eq!(
            router
                .execute(BackendRequest::read_text("Root.md"))
                .await
                .unwrap()
                .into_text()
                .unwrap(),
            "root"
        );
    }

    // -----------------------------------------------------------------------
    // Routed single-path operations
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reads_and_writes_route_to_the_owning_mount() {
        let vaults = Vaults::new("routed-io");
        let router = two_mounts(&vaults);
        assert_eq!(
            router
                .execute(BackendRequest::read_text("Team/Charter.md"))
                .await
                .unwrap()
                .into_text()
                .unwrap(),
            "charter body"
        );
        router
            .execute(BackendRequest::write_text("Team/New.md", "fresh"))
            .await
            .unwrap();
        // The write landed on the team mount at its MOUNT-relative path...
        assert_eq!(
            std::fs::read_to_string(vaults.root.join("team/New.md")).unwrap(),
            "fresh"
        );
        // ...is readable through the logical path...
        assert_eq!(
            router
                .execute(BackendRequest::read_text("Team/New.md"))
                .await
                .unwrap()
                .into_text()
                .unwrap(),
            "fresh"
        );
        // ...and never touched the root mount.
        assert!(!vaults.root.join("root/New.md").exists());
        assert!(!vaults.root.join("root/Team").exists());
    }

    #[tokio::test]
    async fn whole_vault_manifests_are_refused_on_a_multi_mount_router() {
        let vaults = Vaults::new("manifests");
        let router = two_mounts(&vaults);
        for request in [
            BackendRequest::walk_markdown(),
            BackendRequest::top_level_folders(),
        ] {
            let error = router.execute(request).await.expect_err("federated");
            assert!(matches!(error, RouterError::FederationUnsupported { .. }));
        }
    }

    #[tokio::test]
    async fn an_upload_commit_lands_in_the_mount_owning_the_destination() {
        // The one routed mutation whose path rewrite is NOT covered by the
        // read/write test: the streaming commit carries its own path and is handed
        // an owned backend handle, so it takes a separate arm of `route_mutation`.
        let vaults = Vaults::new("upload");
        let router = two_mounts(&vaults);

        let outcome = router
            .execute(BackendRequest::Mutation(
                MutationRequest::CommitUploadStream {
                    path: "Team/Assets/logo.bin".to_string(),
                    expected_hash: None,
                    max_bytes: 1024,
                    chunks: crate::UploadChunks::new(
                        vec![Ok(b"header".to_vec()), Ok(b"-body".to_vec())].into_iter(),
                    ),
                },
            ))
            .await
            .expect("commit")
            .into_upload_outcome()
            .expect("upload outcome");
        assert!(outcome.created);
        assert_eq!(outcome.bytes_written, 11);

        // Landed on the TEAM vault at the MOUNT-relative path...
        assert_eq!(
            std::fs::read(vaults.root.join("team/Assets/logo.bin")).unwrap(),
            b"header-body"
        );
        // ...and nowhere in the root vault under either spelling.
        assert!(!vaults.root.join("root/Team").exists());
        assert!(!vaults.root.join("root/Assets").exists());

        // The bytes are readable back through the LOGICAL path, which is the only
        // address a client ever has.
        assert_eq!(
            router
                .execute(BackendRequest::read_bytes("Team/Assets/logo.bin"))
                .await
                .unwrap()
                .into_bytes()
                .unwrap(),
            b"header-body"
        );
    }

    #[tokio::test]
    async fn a_commit_to_an_unroutable_destination_fails_rather_than_landing_anywhere() {
        let vaults = Vaults::new("upload-unrouted");
        // No root mount, so a path outside `Team/` belongs to nobody.
        let router = VaultRouter::new(vec![Mount::new("team", "Team", vaults.vault("team", &[]))])
            .expect("router");

        let error = router
            .execute(BackendRequest::Mutation(
                MutationRequest::CommitUploadStream {
                    path: "Elsewhere/logo.bin".to_string(),
                    expected_hash: None,
                    max_bytes: 1024,
                    chunks: crate::UploadChunks::new(vec![Ok(b"bytes".to_vec())].into_iter()),
                },
            ))
            .await
            .expect_err("unroutable destination");
        assert!(matches!(error, RouterError::Unrouted { .. }));
        assert!(!vaults.root.join("team/logo.bin").exists());
        assert!(!vaults.root.join("team/Elsewhere").exists());
    }

    #[tokio::test]
    async fn the_staging_sweep_reaches_every_mount() {
        let vaults = Vaults::new("sweep");
        let router = two_mounts(&vaults);
        // Housekeeping fans out and cannot fail.
        assert!(router
            .execute(BackendRequest::sweep_orphan_staging_files())
            .await
            .is_ok());
    }

    // -----------------------------------------------------------------------
    // Listing synthesis
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn root_listing_merges_own_children_with_mount_folders() {
        let vaults = Vaults::new("listing-root");
        let router = two_mounts(&vaults);
        let entries = router.list_children(None, false, false).await.unwrap();
        let rendered: Vec<(&str, bool)> = entries
            .iter()
            .map(|entry| {
                (
                    entry.path.as_str(),
                    matches!(entry.kind, VaultEntryKind::Directory),
                )
            })
            .collect();
        // Core's ordering, applied across the merged set: directories first (Notes
        // from the root mount, Team synthesized), then files.
        assert_eq!(
            rendered,
            vec![("Notes", true), ("Team", true), ("Root.md", false)]
        );
        // A synthesized mount folder is shaped exactly like a physical directory,
        // so no client can tell a mount point from a folder.
        let team = entries.iter().find(|entry| entry.path == "Team").unwrap();
        let notes = entries.iter().find(|entry| entry.path == "Notes").unwrap();
        assert_eq!(team.name, "Team");
        assert_eq!(team.is_markdown, notes.is_markdown);
        assert_eq!(team.size_bytes, notes.size_bytes);
        assert_eq!(team.kind, notes.kind);
    }

    #[tokio::test]
    async fn a_mount_shadows_a_physical_folder_of_the_same_name() {
        let vaults = Vaults::new("shadow");
        let router = VaultRouter::new(vec![
            Mount::new(
                "vault",
                "",
                // The root vault physically contains Team/, but a mount is grafted
                // there, so every read under Team/ already goes to the mount.
                vaults.vault(
                    "root",
                    &[("Team/Shadowed.md", "hidden"), ("Root.md", "root")],
                ),
            ),
            Mount::new(
                "team",
                "Team",
                vaults.vault("team", &[("Charter.md", "charter")]),
            ),
        ])
        .expect("router");

        let entries = router.list_children(None, false, false).await.unwrap();
        // Listed exactly once: the mount wins, the physical folder is shadowed.
        // Listing both would advertise content that `resolve` can never reach.
        assert_eq!(
            entries.iter().filter(|entry| entry.path == "Team").count(),
            1
        );

        // Descending shows the MOUNT's content, not the shadowed folder's.
        let children = router
            .list_children(Some("Team"), false, false)
            .await
            .unwrap();
        let paths: Vec<&str> = children.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, vec!["Team/Charter.md"]);
    }

    #[tokio::test]
    async fn a_nested_mount_is_visible_through_a_synthetic_ancestor_folder() {
        let vaults = Vaults::new("synthetic-ancestor");
        // Nothing physically exists at Team/ in the root vault, so the folder is
        // purely a consequence of the nested mount's prefix.
        let router = VaultRouter::new(vec![
            Mount::new("vault", "", vaults.vault("root", &[("Root.md", "root")])),
            Mount::new(
                "alpha",
                "Team/Alpha",
                vaults.vault("alpha", &[("Plan.md", "plan")]),
            ),
        ])
        .expect("router");

        let root = router.list_children(None, false, false).await.unwrap();
        assert!(root.iter().any(|entry| entry.path == "Team"));

        // Listing the synthetic ancestor reports the mount below it even though the
        // owning (root) mount has no such directory to list -- otherwise a nested
        // mount would be undiscoverable from above.
        let team = router
            .list_children(Some("Team"), false, false)
            .await
            .unwrap();
        let paths: Vec<&str> = team.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, vec!["Team/Alpha"]);

        // A folder that is neither physical nor a mount ancestor still errors, with
        // the owning backend's own wording.
        assert!(router
            .list_children(Some("Nowhere"), false, false)
            .await
            .is_err());
    }

    // -----------------------------------------------------------------------
    // Grep scoping
    // -----------------------------------------------------------------------

    fn grep(glob: Option<&str>) -> BackendRequest {
        BackendRequest::Recall(RecallRequest::Grep {
            query: "charter".to_string(),
            regex: false,
            case_sensitive: false,
            glob: glob.map(ToOwned::to_owned),
            context_lines: 0,
            limit: 10,
        })
    }

    #[test]
    fn literal_glob_prefix_only_accepts_unambiguous_directory_scopes() {
        assert_eq!(literal_glob_prefix("Team/**/*.md").as_deref(), Some("Team"));
        assert_eq!(
            literal_glob_prefix("Team/Alpha/*.md").as_deref(),
            Some("Team/Alpha")
        );
        assert_eq!(
            literal_glob_prefix("Team/Alpha/notes.md").as_deref(),
            Some("Team/Alpha")
        );
        // No directory component, a negation, a metacharacter or an alternation in
        // the leading segment, or a traversal: refused rather than guessed at.
        for ambiguous in [
            "*.md",
            "**/*.md",
            "!Team/**",
            "Te*m/**",
            "{Team,Other}/**",
            "../**/*.md",
            "",
        ] {
            assert!(
                literal_glob_prefix(ambiguous).is_none(),
                "{ambiguous:?} was accepted"
            );
        }
    }

    #[tokio::test]
    async fn an_unscoped_grep_is_refused_on_a_multi_mount_router() {
        let vaults = Vaults::new("grep-unscoped");
        let router = two_mounts(&vaults);
        // No glob at all, and globs that filter by basename across the whole walk:
        // none of them narrow to a mount, so answering from one would present
        // partial results as complete.
        for request in [grep(None), grep(Some("*.md")), grep(Some("**/*.md"))] {
            let error = router.execute(request).await.expect_err("unscoped");
            assert!(matches!(
                error,
                RouterError::FederationUnsupported {
                    operation: "grep_search",
                    ..
                }
            ));
            assert!(error.to_string().contains("glob"));
        }
    }

    #[tokio::test]
    async fn a_glob_scoped_grep_routes_to_one_mount_and_reports_logical_paths() {
        let vaults = Vaults::new("grep-scoped");
        let router = two_mounts(&vaults);
        if !grep_available(&router) {
            return;
        }
        let matches = router
            .execute(grep(Some("Team/**/*.md")))
            .await
            .unwrap()
            .into_grep_matches()
            .unwrap();
        assert_eq!(matches.len(), 1);
        // Reported in the LOGICAL namespace, not the mount's own.
        assert_eq!(matches[0].path, "Team/Charter.md");
    }

    #[tokio::test]
    async fn a_scope_containing_another_mount_is_refused() {
        let vaults = Vaults::new("grep-nested");
        let router = VaultRouter::new(vec![
            Mount::new(
                "vault",
                "",
                vaults.vault("root", &[("Team/Note.md", "charter")]),
            ),
            Mount::new(
                "alpha",
                "Team/Alpha",
                vaults.vault("alpha", &[("Plan.md", "charter")]),
            ),
        ])
        .expect("router");

        // "Team/**" resolves to the root mount, but the Team/Alpha mount lives
        // inside that scope, so a single-mount run would silently miss it.
        let error = router
            .execute(grep(Some("Team/**/*.md")))
            .await
            .expect_err("nested mount in scope");
        assert!(matches!(error, RouterError::FederationUnsupported { .. }));

        // Scoping tightly enough to name exactly one mount does work.
        if !grep_available(&router) {
            return;
        }
        let matches = router
            .execute(grep(Some("Team/Alpha/*.md")))
            .await
            .unwrap()
            .into_grep_matches()
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "Team/Alpha/Plan.md");
    }

    // -----------------------------------------------------------------------
    // Fan-out
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_health_probe_requires_every_mount() {
        let vaults = Vaults::new("health");
        let router = two_mounts(&vaults);
        assert!(router
            .execute(BackendRequest::health_overview())
            .await
            .is_ok());

        // A startup gate is only useful if it fails when ANY mount is missing.
        let router = VaultRouter::new(vec![
            Mount::new("vault", "", vaults.vault("root2", &[])),
            Mount::new(
                "team",
                "Team",
                Arc::new(FilesystemVaultBackend::new(vaults.absent("never-created"))),
            ),
        ])
        .expect("router");
        let error = router
            .execute(BackendRequest::health_overview())
            .await
            .expect_err("unreachable mount");
        // Reported with the backend's own wording, unchanged.
        assert!(error
            .to_string()
            .contains("vault path does not exist or is not a directory"));
    }
}
