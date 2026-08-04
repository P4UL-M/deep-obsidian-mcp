//! `couchdb export` and `couchdb restore`: a verified snapshot of a LiveSync vault on
//! the filesystem, and the way back.
//!
//! # What this is for
//!
//! A CouchDB mount is the one vault whose content the user cannot inspect, diff or back
//! up with ordinary tools: it lives as chunk documents in a database, reassembled by a
//! sidecar. Before an agent is allowed to write into such a vault, there has to be a
//! way to answer "what did it look like before" and "put it back" — and that way has to
//! be checkable rather than trusted.
//!
//! So export produces a plain directory tree plus a manifest, and restore writes a tree
//! back through the *same guarded write path* the MCP tools use. The pair is verifiable
//! by construction: export, mutate, restore, export again, and compare the two trees.
//! If they are byte-identical, the round trip is proven for that vault. That comparison
//! is what the round-trip test does, and what a user can do by hand with `diff -r`.
//!
//! # Why the tree is deterministic, and has no timestamps
//!
//! Two exports of an unchanged vault produce byte-identical output, `manifest.json`
//! included. That is deliberate and it is the property the whole design is built
//! around: "compare two exports" is only a verification if the comparison has no
//! false positives, and a wall-clock `exportedAt` field would make every export differ
//! from every other. Provenance does not suffer for it — each entry carries its CouchDB
//! revision and its content hash, which identify the snapshot far more precisely than a
//! timestamp does.
//!
//! # What is deliberately not here
//!
//! No deletion. Restore writes and skips; it never removes a note that the export did
//! not contain. "Make the vault match this tree exactly" would mean deleting a note a
//! colleague added on another device since the export, and that is a destructive
//! judgement a backup tool has no business making silently. A user who wants it can
//! read the report and delete in Obsidian.
//!
//! No conflict resolution. A conflicted entry exports its WINNING revision — the one
//! every read serves — and `manifest.json` records that it was conflicted along with
//! the sibling revisions. Picking a winner needs a merge policy this tool does not have.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use deep_obsidian_backend::sidecar::EntryKind;
use deep_obsidian_backend::{
    BackendRequest, BaseVersion, CouchDbVaultBackend, EntryContent, VaultBackend,
};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_core::{content_hash, ContentHasher};
use deep_obsidian_server::mounts::MountBackends;
use deep_obsidian_types::{MountBackendConfig, ResolvedServiceConfig};
use serde::{Deserialize, Serialize};

/// Name of the manifest inside an export directory.
///
/// Lives at the root of the tree rather than beside it so that one directory is the
/// whole self-describing snapshot. It is skipped when restoring, so an export directory
/// can be handed straight back to `restore` without the manifest being mistaken for a
/// note.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Format version of [`ExportManifest`].
///
/// Bumped when the shape changes. `restore` refuses a version it does not know rather
/// than guessing at fields, because the manifest is what decides a file's STORAGE KIND
/// and getting that wrong is not recoverable through this tool.
pub const MANIFEST_VERSION: u32 = 1;

/// The self-describing part of an export.
///
/// Carries the mount id and nothing else about the connection: no URL, no database
/// name, no user. The mount id is sufficient provenance — it names the entry in the
/// config that produced this snapshot — and it is unambiguously not a secret, which is
/// a property worth having by construction rather than by review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportManifest {
    pub version: u32,
    /// The mount this snapshot came from.
    pub mount: String,
    /// One row per exported entry, ordered by path so the file is deterministic.
    pub entries: Vec<ExportEntry>,
}

/// One exported entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportEntry {
    /// Vault-relative path, logical to the mount (no mount prefix).
    pub path: String,
    /// The CouchDB revision this content came from.
    pub rev: String,
    /// Canonical content hash of the exported bytes, so a tree can be verified without
    /// re-reading the remote.
    pub hash: String,
    pub size: u64,
    /// `text` or `binary`. **Authoritative on restore**: it decides whether the entry
    /// is written back as a LiveSync `plain` or `newnote` document, and inferring it
    /// from the bytes can get it wrong for a UTF-8-decodable binary.
    pub kind: ExportKind,
    /// The entry had unreconciled sibling revisions at export time; the winning
    /// revision is what was exported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conflicted: bool,
    /// The sibling revisions, recorded so a conflict is auditable after the fact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_revisions: Vec<String>,
}

/// How an entry is stored, mirroring the sidecar's own distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportKind {
    /// A LiveSync `plain` entry: stored as text.
    Text,
    /// A LiveSync `newnote` entry: stored as base64 chunks.
    Binary,
}

/// What an export did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub mount: String,
    pub out_dir: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub text_entries: usize,
    pub binary_entries: usize,
    pub conflicted: Vec<String>,
    /// A single hash over every `(path, hash)` pair, so two exports can be compared
    /// with one string instead of a directory walk.
    pub tree_hash: String,
}

/// What restoring one file did, or would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreAction {
    /// No entry at the path: written as a create.
    Created,
    /// The remote already holds exactly these bytes.
    Unchanged,
    /// Differing content, overwritten because `--force` was given.
    Overwritten,
    /// Differing content, left alone because `--force` was not given.
    RefusedDiffers,
    /// The file's storage kind could not be established. See
    /// [`resolve_kind`].
    RefusedUnknownKind,
}

impl RestoreAction {
    /// True when this action left the remote untouched *and* the user probably wanted
    /// it not to. Drives the command's exit status.
    fn is_refusal(self) -> bool {
        matches!(
            self,
            RestoreAction::RefusedDiffers | RestoreAction::RefusedUnknownKind
        )
    }
}

/// One file's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOutcome {
    pub path: String,
    pub action: RestoreAction,
    /// Present on a refusal: why, in one line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub mount: String,
    pub from_dir: PathBuf,
    pub dry_run: bool,
    pub created: usize,
    pub overwritten: usize,
    pub unchanged: usize,
    pub refused: usize,
    pub outcomes: Vec<RestoreOutcome>,
}

impl RestoreReport {
    pub fn ok(&self) -> bool {
        self.refused == 0
    }
}

// ---------------------------------------------------------------------------
// Mount resolution
// ---------------------------------------------------------------------------

/// Build the named couchdb mount's backend from the resolved config.
///
/// Goes through [`MountBackends`], not a hand-rolled construction, so the secrets, the
/// sidecar location, the chunking options and the mount's own `writable` flag are
/// resolved by exactly the code the service uses. A CLI that built its credentials
/// differently from the server would be a second implementation of the one thing that
/// must not have two.
fn couchdb_backend_for_mount(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    resolver: &SecretResolver,
) -> Result<std::sync::Arc<dyn VaultBackend>> {
    let mount = config
        .mount_table()
        .into_iter()
        .find(|mount| mount.id == mount_id)
        .ok_or_else(|| {
            let available = config
                .mount_table()
                .into_iter()
                .map(|mount| format!("{} ({})", mount.id, mount.backend.kind_name()))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!("no mount named {mount_id:?} in the config; mounts are: {available}")
        })?;
    if !matches!(mount.backend, MountBackendConfig::Couchdb { .. }) {
        bail!(
            "mount {mount_id:?} is a {} backend, not a couchdb one; `couchdb export` and \
             `couchdb restore` only apply to a CouchDB (Self-hosted LiveSync) mount",
            mount.backend.kind_name()
        );
    }

    let backends = MountBackends::build_with_resolver(config, resolver);
    let entry = backends
        .entries()
        .iter()
        .find(|entry| entry.mount.id == mount_id)
        .ok_or_else(|| anyhow!("mount {mount_id:?} could not be built"))?;
    Ok(entry.backend.clone())
}

/// The concrete couchdb backend behind a mount, or a clear failure.
///
/// A mount whose secret is missing or whose sidecar bundle is absent is built as a
/// refusing stub rather than a couchdb backend, so this is where that becomes a
/// message naming the mount instead of a confusing "not supported" later on.
fn require_couchdb<'backend>(
    backend: &'backend std::sync::Arc<dyn VaultBackend>,
    mount_id: &str,
) -> Result<&'backend CouchDbVaultBackend> {
    backend.as_couchdb().ok_or_else(|| {
        anyhow!(
            "mount {mount_id:?} could not be initialized as a CouchDB vault (a missing \
                 secret, or a missing sidecar bundle). Run `deep-obsidian-mcp doctor` for the \
                 specific reason."
        )
    })
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Walk the whole mount and write every entry to `out_dir`.
pub async fn export(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    out_dir: &Path,
) -> Result<ExportReport> {
    export_with_resolver(config, mount_id, out_dir, &SecretResolver::new()).await
}

/// [`export`] against an explicit secret store.
///
/// Exists for the same reason `MountBackends::build_with_resolver` does: a test must be
/// able to point at a temp secrets file instead of mutating `XDG_CONFIG_HOME`, which is
/// process-global and would race every other test that reads the default secrets path.
pub async fn export_with_resolver(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    out_dir: &Path,
    resolver: &SecretResolver,
) -> Result<ExportReport> {
    let backend = couchdb_backend_for_mount(config, mount_id, resolver)?;
    let couchdb = require_couchdb(&backend, mount_id)?;

    // The FULL manifest, not `WalkMarkdown`: that filters to `.md` and would silently
    // drop every attachment and every non-`.md` text entry, so the "restore" of such an
    // export would delete-by-omission exactly the files hardest to recreate.
    let entries = couchdb
        .manifest_entries()
        .await
        .with_context(|| format!("could not list mount {mount_id:?}"))?;

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("could not create {}", out_dir.display()))?;

    let mut rows: Vec<ExportEntry> = Vec::new();
    let mut bytes_total: u64 = 0;
    let mut conflicted: Vec<String> = Vec::new();

    // Sorted, so the tree is written in a fixed order and the manifest is
    // deterministic.
    let mut listable: Vec<_> = entries
        .iter()
        .filter(|entry| !entry.deleted && !matches!(entry.kind, EntryKind::Internal))
        .collect();
    listable.sort_by(|left, right| left.path.cmp(&right.path));

    for entry in listable {
        let read = backend
            .execute(BackendRequest::read_bytes(&entry.path))
            .await
            .with_context(|| format!("could not read {}", entry.path))?
            .into_bytes()
            .map_err(|error| anyhow!("{error}"))?;

        // The revision is read through `stat` rather than taken from the manifest so
        // that the recorded rev is the one that produced these exact bytes.
        let stat = couchdb
            .stat_entry(&entry.path)
            .await
            .with_context(|| format!("could not stat {}", entry.path))?;

        let destination = safe_destination(out_dir, &entry.path)?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        std::fs::write(&destination, &read)
            .with_context(|| format!("could not write {}", destination.display()))?;

        let mut conflict_revisions = Vec::new();
        if stat.conflicted {
            conflicted.push(entry.path.clone());
            // Recorded rather than resolved: the losing revisions are what an audit
            // needs and what this tool must not silently discard.
            if let Ok(detail) = couchdb.conflicts(&entry.path).await {
                conflict_revisions = detail
                    .conflicts
                    .iter()
                    .map(|revision| revision.rev.clone())
                    .collect();
            }
        }

        bytes_total = bytes_total.saturating_add(read.len() as u64);
        rows.push(ExportEntry {
            path: entry.path.clone(),
            rev: stat.rev,
            hash: content_hash(&read),
            size: read.len() as u64,
            kind: match entry.kind {
                EntryKind::Binary => ExportKind::Binary,
                // `markdown` upstream means "stored as text", not "is a .md file".
                _ => ExportKind::Text,
            },
            conflicted: stat.conflicted,
            conflict_revisions,
        });
    }

    let text_entries = rows
        .iter()
        .filter(|row| matches!(row.kind, ExportKind::Text))
        .count();
    let manifest = ExportManifest {
        version: MANIFEST_VERSION,
        mount: mount_id.to_string(),
        entries: rows,
    };
    let tree_hash = tree_hash(&manifest);
    // Pretty-printed with a trailing newline: this file is meant to be read and diffed
    // by a human, and a trailing newline is what every other tool expects.
    let serialized = format!("{}\n", serde_json::to_string_pretty(&manifest)?);
    std::fs::write(out_dir.join(MANIFEST_FILE), serialized)
        .with_context(|| format!("could not write the {MANIFEST_FILE}"))?;

    Ok(ExportReport {
        mount: mount_id.to_string(),
        out_dir: out_dir.to_path_buf(),
        files: manifest.entries.len(),
        bytes: bytes_total,
        text_entries,
        binary_entries: manifest.entries.len() - text_entries,
        conflicted,
        tree_hash,
    })
}

/// One hash over every `(path, hash)` pair.
///
/// Deliberately over the manifest's own rows rather than over the files: it is then a
/// statement about the CONTENT of the snapshot, independent of how the filesystem
/// happened to store it, and two exports can be compared without walking either tree.
fn tree_hash(manifest: &ExportManifest) -> String {
    let mut hasher = ContentHasher::new();
    for entry in &manifest.entries {
        hasher.update(entry.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finish()
}

/// Join `relative` onto `root`, refusing anything that would land outside it.
///
/// The paths come from a remote, so they are untrusted input: a vault entry whose path
/// contained `../` would otherwise let an export write anywhere the process can reach.
/// Checked lexically before any directory is created.
fn safe_destination(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.trim().is_empty() {
        bail!("the remote returned an entry with an empty path");
    }
    let mut destination = root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!(
                "refusing to write the remote entry {relative:?}: its path contains a segment \
                 that would escape the output directory"
            );
        }
        destination.push(segment);
    }
    if std::path::Path::new(relative).is_absolute() {
        bail!("refusing to write the remote entry {relative:?}: the path is absolute");
    }
    Ok(destination)
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// Write a previously exported tree back into the mount.
///
/// # Refusal semantics
///
/// Conservative on purpose, because the failure mode of a restore is destroying work
/// that was not in the snapshot:
///
/// * a path with no entry on the remote is **created**;
/// * a path whose remote content is byte-identical is **skipped** (so a restore is
///   idempotent and a re-run reports nothing to do);
/// * a path whose remote content DIFFERS is **refused** unless `force`, and the refusal
///   names the path. That is the whole safety property: the default cannot overwrite an
///   edit made after the export;
/// * every write goes through the same revision-guarded path the MCP tools use, so even
///   with `force` a note edited between this restore's read and its write is refused
///   rather than clobbered.
///
/// `dry_run` performs every read and every comparison and no write, so the report is
/// exactly what a real run would do.
pub async fn restore(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    from_dir: &Path,
    dry_run: bool,
    force: bool,
) -> Result<RestoreReport> {
    restore_with_resolver(
        config,
        mount_id,
        from_dir,
        dry_run,
        force,
        &SecretResolver::new(),
    )
    .await
}

/// [`restore`] against an explicit secret store. See [`export_with_resolver`].
pub async fn restore_with_resolver(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    from_dir: &Path,
    dry_run: bool,
    force: bool,
    resolver: &SecretResolver,
) -> Result<RestoreReport> {
    let backend = couchdb_backend_for_mount(config, mount_id, resolver)?;
    let couchdb = require_couchdb(&backend, mount_id)?;
    // Checked BEFORE any read, so a user pointing a restore at a read-only mount is
    // told immediately instead of after a full tree walk.
    if !dry_run && !couchdb.is_writable() {
        bail!(
            "mount {mount_id:?} is not writable: set \"writable\": true on the mount and restart \
             to allow a restore. (`--dry-run` works on a read-only mount and reports exactly what \
             a writable one would do.)"
        );
    }

    let manifest = read_manifest(from_dir)?;
    let expected: BTreeMap<&str, &ExportEntry> = manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .entries
                .iter()
                .map(|entry| (entry.path.as_str(), entry))
                .collect()
        })
        .unwrap_or_default();

    let mut files = Vec::new();
    collect_files(from_dir, from_dir, &mut files)?;
    files.sort();

    let mut report = RestoreReport {
        mount: mount_id.to_string(),
        from_dir: from_dir.to_path_buf(),
        dry_run,
        created: 0,
        overwritten: 0,
        unchanged: 0,
        refused: 0,
        outcomes: Vec::new(),
    };

    for relative in files {
        let absolute = safe_destination(from_dir, &relative)?;
        let bytes = std::fs::read(&absolute)
            .with_context(|| format!("could not read {}", absolute.display()))?;

        let kind = match resolve_kind(&relative, expected.get(relative.as_str()).copied(), force) {
            Ok(kind) => kind,
            Err(reason) => {
                report.refused += 1;
                report.outcomes.push(RestoreOutcome {
                    path: relative,
                    action: RestoreAction::RefusedUnknownKind,
                    reason: Some(reason),
                });
                continue;
            }
        };

        // The remote as it is NOW. Byte comparison, never a text one: a text entry
        // whose stored form differs only in line endings must not look permanently
        // changed and refuse forever.
        let current = couchdb
            .read_bytes_and_version(&relative)
            .await
            .with_context(|| format!("could not read {relative} from the remote"))?;

        let (action, base_version) = match &current {
            None => (RestoreAction::Created, BaseVersion::Absent),
            Some((existing, rev)) if existing == &bytes => {
                (RestoreAction::Unchanged, BaseVersion::Version(rev.clone()))
            }
            Some((_, rev)) if force => (
                RestoreAction::Overwritten,
                BaseVersion::Version(rev.clone()),
            ),
            Some(_) => (RestoreAction::RefusedDiffers, BaseVersion::Unobserved),
        };

        match action {
            RestoreAction::Unchanged => {
                report.unchanged += 1;
                report.outcomes.push(RestoreOutcome {
                    path: relative,
                    action,
                    reason: None,
                });
                continue;
            }
            RestoreAction::RefusedDiffers => {
                report.refused += 1;
                report.outcomes.push(RestoreOutcome {
                    path: relative,
                    action,
                    reason: Some(
                        "the remote holds different content than the snapshot; pass --force to \
                         overwrite it"
                            .to_string(),
                    ),
                });
                continue;
            }
            _ => {}
        }

        if !dry_run {
            // Text is validated as UTF-8 HERE rather than lossily converted: an entry
            // the manifest calls text but whose bytes are not valid UTF-8 means the
            // snapshot and the tree disagree, and writing a replacement-character
            // version of the file would be a silent corruption dressed as a restore.
            let content = match kind {
                ExportKind::Text => {
                    EntryContent::Text(std::str::from_utf8(&bytes).with_context(|| {
                        format!(
                            "{relative} is recorded as a text entry but is not valid UTF-8; \
                             refusing to write a lossy conversion of it"
                        )
                    })?)
                }
                ExportKind::Binary => EntryContent::Binary(&bytes),
            };
            couchdb
                .write_entry(&relative, content, base_version)
                .await
                .with_context(|| format!("could not restore {relative}"))?;
        }
        match action {
            RestoreAction::Created => report.created += 1,
            RestoreAction::Overwritten => report.overwritten += 1,
            _ => {}
        }
        report.outcomes.push(RestoreOutcome {
            path: relative,
            action,
            reason: None,
        });
    }

    Ok(report)
}

/// Decide how a file must be stored, preferring the manifest over inference.
///
/// # Why inference is a refusal rather than a guess
///
/// A file's storage kind is not cosmetic: `text` becomes a LiveSync `plain` entry and
/// `binary` a `newnote`, and the plugin treats the two differently. "Valid UTF-8 means
/// text" gets that wrong for every binary that happens to decode — and once written
/// under the wrong kind, this tool cannot tell it was wrong, so nothing will ever fix
/// it. A refusal costs a flag; a silent misclassification costs the entry.
///
/// So: the manifest decides when it has a row. Without one, a Markdown-ish extension is
/// accepted as text (unambiguous by convention), and anything else is refused unless
/// `force` — in which case the bytes decide, and the user has said they accept that.
fn resolve_kind(
    relative: &str,
    manifest_entry: Option<&ExportEntry>,
    force: bool,
) -> Result<ExportKind, String> {
    if let Some(entry) = manifest_entry {
        return Ok(entry.kind);
    }
    let is_markdown = relative
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("md"));
    if is_markdown {
        return Ok(ExportKind::Text);
    }
    if force {
        return Ok(ExportKind::Binary);
    }
    Err(format!(
        "{relative} has no row in {MANIFEST_FILE} and is not a .md file, so whether it must be \
         stored as text or as a binary attachment cannot be established; storing it under the \
         wrong kind is not reversible with this tool. Restore from an unmodified export \
         directory, or pass --force to store it as a binary attachment."
    ))
}

/// Read the export manifest, or `None` when the directory has none.
///
/// A missing manifest is tolerated (a hand-assembled tree is a legitimate input) but a
/// manifest of an unknown VERSION is not: it decides storage kinds, so reading it
/// loosely would mean guessing at exactly the field that must not be guessed.
fn read_manifest(from_dir: &Path) -> Result<Option<ExportManifest>> {
    let path = from_dir.join(MANIFEST_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let manifest: ExportManifest = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a readable export manifest", path.display()))?;
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "{} declares manifest version {} but this build understands {MANIFEST_VERSION}; \
             re-export with this build rather than restoring from a format it cannot read",
            path.display(),
            manifest.version
        );
    }
    Ok(Some(manifest))
}

/// Every file under `root`, as `/`-joined paths relative to it, with the manifest and
/// hidden entries skipped.
///
/// Hidden files are skipped because the sidecar cannot serve a path with a dot-prefixed
/// segment at all (it mirrors commonlib's own `isTargetFile`), so including them would
/// produce a refusal for a file the export could never have written either.
fn collect_files(root: &Path, directory: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("could not read {}", directory.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("could not read {}", directory.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("could not stat {}", path.display()))?;
        if file_type.is_dir() {
            collect_files(root, &path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| anyhow!("{} is not under {}", path.display(), root.display()))?
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        if relative == MANIFEST_FILE {
            continue;
        }
        out.push(relative);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_export_report(report: &ExportReport) -> String {
    let mut lines = vec![
        format!("exported mount '{}'", report.mount),
        format!("  to           {}", report.out_dir.display()),
        format!(
            "  entries      {} ({} text, {} binary)",
            report.files, report.text_entries, report.binary_entries
        ),
        format!("  bytes        {}", report.bytes),
        format!("  tree hash    {}", report.tree_hash),
    ];
    if report.conflicted.is_empty() {
        lines.push("  conflicts    none".to_string());
    } else {
        lines.push(format!(
            "  conflicts    {} entr{} exported at their WINNING revision; the losing revisions \
             are recorded in {MANIFEST_FILE} and were not exported",
            report.conflicted.len(),
            if report.conflicted.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ));
        for path in &report.conflicted {
            lines.push(format!("               - {path}"));
        }
    }
    lines.push(
        "  verify       re-export into another directory and compare the tree hashes, or \
         `diff -r` the two trees"
            .to_string(),
    );
    lines.join("\n")
}

pub fn render_restore_report(report: &RestoreReport) -> String {
    let mut lines = vec![
        format!(
            "{} mount '{}'",
            if report.dry_run {
                "would restore"
            } else {
                "restored"
            },
            report.mount
        ),
        format!("  from         {}", report.from_dir.display()),
        format!(
            "  created {}  overwritten {}  unchanged {}  refused {}",
            report.created, report.overwritten, report.unchanged, report.refused
        ),
    ];
    for outcome in &report.outcomes {
        if !outcome.action.is_refusal() {
            continue;
        }
        lines.push(format!(
            "  refused      {}: {}",
            outcome.path,
            outcome.reason.as_deref().unwrap_or("unspecified")
        ));
    }
    if report.refused > 0 {
        lines.push(
            "  nothing was overwritten for the refused entries; re-run with --force only if you \
             intend to discard what the remote holds"
                .to_string(),
        );
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: ExportKind) -> ExportEntry {
        ExportEntry {
            path: path.to_string(),
            rev: "1-abc".to_string(),
            hash: "fnv1a64:0".to_string(),
            size: 1,
            kind,
            conflicted: false,
            conflict_revisions: Vec::new(),
        }
    }

    /// A remote-supplied path that would escape the output directory is refused, not
    /// normalized. The paths come from a remote, so they are untrusted.
    #[test]
    fn a_traversing_remote_path_is_refused() {
        let root = Path::new("/tmp/export");
        for path in ["../escape.md", "Notes/../../escape.md", "/etc/passwd", ""] {
            assert!(
                safe_destination(root, path).is_err(),
                "{path:?} must be refused"
            );
        }
        assert_eq!(
            safe_destination(root, "Notes/Alpha.md").expect("a plain path"),
            root.join("Notes").join("Alpha.md")
        );
    }

    /// The manifest decides the storage kind; without it, only `.md` is unambiguous.
    #[test]
    fn the_storage_kind_comes_from_the_manifest_and_is_never_guessed() {
        // With a manifest row, the row wins — including for a `.md` file recorded as
        // binary, which is exactly the case inference would get wrong.
        assert_eq!(
            resolve_kind("Odd.md", Some(&entry("Odd.md", ExportKind::Binary)), false),
            Ok(ExportKind::Binary)
        );
        assert_eq!(
            resolve_kind(
                "assets/logo.png",
                Some(&entry("assets/logo.png", ExportKind::Binary)),
                false
            ),
            Ok(ExportKind::Binary)
        );

        // Without a row, `.md` is accepted by convention...
        assert_eq!(
            resolve_kind("Notes/New.md", None, false),
            Ok(ExportKind::Text)
        );
        // ...and anything else is REFUSED rather than sniffed, because a
        // UTF-8-decodable binary would be silently stored as a text entry and this
        // tool could never tell afterwards.
        let refusal = resolve_kind("assets/logo.png", None, false)
            .expect_err("an unknown kind must be refused");
        assert!(refusal.contains("cannot be established"), "{refusal}");
        assert!(refusal.contains("--force"), "{refusal}");
        // With `--force` the user has accepted the risk, and binary is the
        // non-destructive choice: it preserves the bytes exactly.
        assert_eq!(
            resolve_kind("assets/logo.png", None, true),
            Ok(ExportKind::Binary)
        );
    }

    /// The tree hash is over content, so it is stable across reorderings of equal data
    /// and changes when any byte does.
    #[test]
    fn the_tree_hash_tracks_content_only() {
        let manifest = |entries: Vec<ExportEntry>| ExportManifest {
            version: MANIFEST_VERSION,
            mount: "live".to_string(),
            entries,
        };
        let base = manifest(vec![entry("A.md", ExportKind::Text)]);
        // The mount name is NOT part of it: the same content exported from a renamed
        // mount is the same snapshot.
        let mut renamed = base.clone();
        renamed.mount = "other".to_string();
        assert_eq!(tree_hash(&base), tree_hash(&renamed));

        // A changed content hash changes it.
        let mut changed = base.clone();
        changed.entries[0].hash = "fnv1a64:1".to_string();
        assert_ne!(tree_hash(&base), tree_hash(&changed));

        // So does a changed path, even with identical content.
        let mut moved = base.clone();
        moved.entries[0].path = "B.md".to_string();
        assert_ne!(tree_hash(&base), tree_hash(&moved));
    }

    /// An unknown manifest version is refused rather than read loosely: it is the field
    /// that decides storage kinds.
    #[test]
    fn an_unknown_manifest_version_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "couchdb-transfer-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join(MANIFEST_FILE),
            r#"{"version": 99, "mount": "live", "entries": []}"#,
        )
        .expect("write manifest");
        let error = read_manifest(&dir).expect_err("an unknown version must be refused");
        assert!(error.to_string().contains("version 99"), "{error}");

        // A directory with no manifest is fine: a hand-assembled tree is legitimate.
        std::fs::remove_file(dir.join(MANIFEST_FILE)).expect("remove");
        assert!(read_manifest(&dir).expect("no manifest is ok").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The manifest round-trips, and `conflicted`/`conflictRevisions` are omitted when
    /// empty so a healthy export carries no noise.
    #[test]
    fn the_manifest_round_trips_and_omits_empty_conflict_fields() {
        let manifest = ExportManifest {
            version: MANIFEST_VERSION,
            mount: "live".to_string(),
            entries: vec![entry("A.md", ExportKind::Text)],
        };
        let text = serde_json::to_string(&manifest).expect("serialize");
        assert!(!text.contains("conflicted"), "{text}");
        assert!(!text.contains("conflictRevisions"), "{text}");
        // And nothing about the connection: no url, no database, no user.
        for secretish in ["url", "database", "username", "password"] {
            assert!(
                !text.contains(secretish),
                "{secretish} must not appear: {text}"
            );
        }
        assert_eq!(
            serde_json::from_str::<ExportManifest>(&text).expect("deserialize"),
            manifest
        );
    }
}
