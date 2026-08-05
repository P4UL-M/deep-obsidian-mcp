//! `algolia seed`, `dump`, `restore`, `status`, `retract` and `key`: the operations that
//! only mean something against an Algolia-backed mount.
//!
//! # What these are for
//!
//! An Algolia mount is the one vault where **the index is the only copy**. A filesystem
//! mount is already a directory tree; a CouchDB mount at least has a database an
//! administrator can dump. A shared corpus in a search index has neither, and it is
//! authored through the mount by several participants at once. So three questions have to
//! have answers before anyone should be asked to put a wiki there:
//!
//! * *how does existing content get in?* — [`seed`], a one-shot local→index import;
//! * *how do I get it back out?* — [`dump`], which materializes the whole corpus to disk,
//!   and [`restore`], which writes such a tree back through the guarded write path;
//! * *how do I withdraw something?* — [`retract`], the single destructive operation.
//!
//! Plus [`status`], which reports what an operator needs to see, and [`key`], which
//! derives the read-only key a teammate mounts the corpus with.
//!
//! Ported from PR #40's `share` family. The command semantics are unchanged; what moved is
//! the *addressing*. #40 had a `shared[]` config array and selected by index name; here a
//! mount is the configuration unit, so every command takes `--mount <id>` exactly like
//! `couchdb export`/`couchdb restore` do. That is the whole reason for the rename.
//!
//! # The two deviations from #40 worth knowing about
//!
//! **`seed` no longer refuses to import the mount's own prefix.** #40 rejected a seed
//! whose source prefix lay inside the shared mount's virtual namespace, on the grounds
//! that no local files could be there. Under a mount table that reasoning inverts: the
//! mount SHADOWS a real local folder, and importing `<vault>/<mount_at>` into the mount
//! that now covers that prefix is precisely the migration path — "I had `_Wiki/` on disk,
//! I want it shared". So it is the DEFAULT source, and the guard is gone.
//!
//! **The dump manifest carries no timestamp.** #40 wrote `deep-obsidian-dump.json` with
//! `dumpedAtMs` and `appId`. Both are dropped, for the reason spelled out in
//! [`crate::couchdb_transfer`]: two dumps of an unchanged corpus must be byte-identical or
//! "dump, mutate, restore, dump again, compare" is not a verification. Provenance does not
//! suffer — every entry carries the version id and content hash that produced its bytes,
//! which locates the snapshot far more precisely than a clock reading does.
//!
//! # What is deliberately not here
//!
//! No `set-key`. #40 had one because it also had to write `keyRef` back into its
//! `shared[]` block; a mount's `apiKeyRef` is an ordinary [`SecretRef`] resolved by the
//! same machinery as every other secret, so storing it is not an Algolia-specific
//! operation and does not belong in an Algolia-specific command family. See the module
//! docs of [`parse_parent_key_ref`] for what a user does instead today.
//!
//! No deletion in `restore`. Same rule as `couchdb restore`: it writes and skips, and
//! never removes a note the dump did not contain. On a SHARED corpus that restraint
//! matters more, not less — the note missing from your snapshot is most likely one a
//! colleague added since.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use deep_obsidian_backend::algolia::{
    reads, versioning, AlgoliaMountStatus, AlgoliaVaultBackend, ALGOLIA_NO_BINARY_MESSAGE,
};
use deep_obsidian_backend::{BackendRequest, BaseVersion, VaultBackend};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_core::{content_hash, ContentHasher};
use deep_obsidian_server::mounts::MountBackends;
use deep_obsidian_types::{MountBackendConfig, MountConfig, ResolvedServiceConfig, SecretRef};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

/// Name of the manifest inside a dump directory. Same file name as a couchdb export's,
/// deliberately: an operator holding a directory should not have to remember which backend
/// produced it to know where its manifest is.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Format version of [`DumpManifest`]. `restore` refuses a version it does not know.
pub const MANIFEST_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Mount resolution
// ---------------------------------------------------------------------------

/// The named mount's config entry, checked to be an Algolia one.
fn algolia_mount(config: &ResolvedServiceConfig, mount_id: &str) -> Result<MountConfig> {
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
    if !matches!(mount.backend, MountBackendConfig::Algolia { .. }) {
        bail!(
            "mount {mount_id:?} is a {} backend, not an algolia one; the `algolia` commands only \
             apply to an Algolia-backed shared corpus",
            mount.backend.kind_name()
        );
    }
    Ok(mount)
}

/// Build the named algolia mount's backend from the resolved config.
///
/// Through [`MountBackends`] rather than a hand-rolled construction, for the same reason
/// `couchdb export` does it: the API key, the cache directory, the retention rule and the
/// mount's own `writable` flag are then resolved by exactly the code the service uses. A
/// CLI that assembled its credentials differently from the server would be a second
/// implementation of the one thing that must not have two.
fn algolia_backend_for_mount(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    resolver: &SecretResolver,
) -> Result<std::sync::Arc<dyn VaultBackend>> {
    algolia_mount(config, mount_id)?;
    let backends = MountBackends::build_with_resolver(config, resolver);
    let entry = backends
        .entries()
        .iter()
        .find(|entry| entry.mount.id == mount_id)
        .ok_or_else(|| anyhow!("mount {mount_id:?} could not be built"))?;
    Ok(entry.backend.clone())
}

/// The concrete Algolia backend behind a mount, or a clear failure.
///
/// A mount whose API key is missing is built as a refusing stub rather than an Algolia
/// backend, so this is where that becomes a message naming the mount instead of a
/// confusing "not supported" further down.
fn require_algolia<'backend>(
    backend: &'backend std::sync::Arc<dyn VaultBackend>,
    mount_id: &str,
) -> Result<&'backend AlgoliaVaultBackend> {
    backend.as_algolia().ok_or_else(|| {
        anyhow!(
            "mount {mount_id:?} could not be initialized as an Algolia vault (most likely its \
             `apiKeyRef` secret is not stored). Run `deep-obsidian-mcp doctor` for the specific \
             reason."
        )
    })
}

/// Resolve `--mount` to `(backend arc, concrete backend)` in one step.
///
/// The `Arc` has to be returned alongside the reference because it OWNS the backend: a
/// helper that returned only the borrow would be borrowing from a temporary.
fn connect(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    resolver: &SecretResolver,
) -> Result<std::sync::Arc<dyn VaultBackend>> {
    let backend = algolia_backend_for_mount(config, mount_id, resolver)?;
    require_algolia(&backend, mount_id)?;
    Ok(backend)
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Join `relative` onto `root`, refusing anything that would land outside it.
///
/// The paths come from a remote, so they are untrusted input: a note record whose path
/// contained `../` would otherwise let a dump write anywhere the process can reach.
/// Checked lexically, before any directory is created.
fn safe_destination(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.trim().is_empty() {
        bail!("the index returned a note with an empty path");
    }
    if Path::new(relative).is_absolute() {
        bail!("refusing to write the note {relative:?}: the path is absolute");
    }
    let mut destination = root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!(
                "refusing to write the note {relative:?}: its path contains a segment that would \
                 escape the target directory"
            );
        }
        destination.push(segment);
    }
    Ok(destination)
}

/// True for a path this mount can hold at all. Markdown only; see
/// [`ALGOLIA_NO_BINARY_MESSAGE`].
fn is_markdown(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("md"))
}

/// Every file under `root`, as `/`-joined paths relative to it, manifest and dot-entries
/// skipped.
///
/// Hidden entries are skipped because the backend refuses a path with a dot-prefixed
/// segment outright, so including them would produce a refusal for a file no dump could
/// have written either.
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
// Seed
// ---------------------------------------------------------------------------

/// What seeding one note would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeedAction {
    /// No note at the path in the index: imported as a create.
    Create,
    /// The index holds this path with different content: imported as a new version, the
    /// superseded one going to history.
    Update,
    /// The index already holds exactly these bytes.
    Unchanged,
    /// Not a `.md` file, so the corpus has no shape for it. Skipped and named.
    SkippedBinary,
    /// The note's own frontmatter says `share: false`. Skipped and named.
    SkippedOptOut,
}

impl SeedAction {
    fn writes(self) -> bool {
        matches!(self, SeedAction::Create | SeedAction::Update)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedItem {
    /// The path INSIDE the mount, i.e. what the index will key the note by.
    pub path: String,
    pub action: SeedAction,
}

/// What a seed would do, computed without writing anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedPlan {
    pub mount: String,
    pub from_dir: PathBuf,
    /// The index holds no note records yet. Reported because a first import is the one
    /// case where the whole corpus is about to come into being from this machine, and an
    /// operator should be told that rather than left to infer it from the counts.
    pub first_import: bool,
    pub items: Vec<SeedItem>,
}

impl SeedPlan {
    pub fn changed(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.action.writes())
            .count()
    }

    fn count(&self, action: SeedAction) -> usize {
        self.items
            .iter()
            .filter(|item| item.action == action)
            .count()
    }
}

/// What a seed did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedReport {
    pub mount: String,
    pub from_dir: PathBuf,
    pub dry_run: bool,
    pub first_import: bool,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped: Vec<SeedItem>,
    /// Local files removed after a verified import (`--move`).
    pub moved_out: Vec<String>,
    /// Local files `--move` deliberately KEPT: their content no longer matches what the
    /// index holds, so deleting them would discard the difference.
    pub kept_drifted: Vec<String>,
    pub items: Vec<SeedItem>,
}

/// Where a seed reads from when `--from` is not given.
///
/// The mount's own logical folder inside the ROOT vault: an algolia mount at `_Wiki`
/// shadows `<vault>/_Wiki`, and that directory is exactly the content a user wants to
/// migrate into the corpus. Read with `std::fs` rather than through the router on purpose
/// — the router would resolve `_Wiki` to the algolia mount and hand back the REMOTE's
/// files, which is the opposite of the question being asked.
///
/// A root-mounted algolia mount is impossible (config rejects it), so `mount_at` is always
/// a non-empty prefix here and the join always names a real subdirectory.
fn default_seed_source(config: &ResolvedServiceConfig, mount: &MountConfig) -> PathBuf {
    config.vault_path.join(&mount.mount_at)
}

/// Compute what a seed would import.
///
/// Create/update only. Nothing is ever deleted from the index to match the source tree:
/// the note in the corpus that is not in your folder is most likely a colleague's, and
/// removal is [`retract`]'s job precisely so that it cannot happen as a side effect of an
/// import.
pub async fn plan_seed(
    backend: &AlgoliaVaultBackend,
    mount_id: &str,
    from_dir: &Path,
    local: &BTreeMap<String, Vec<u8>>,
) -> Result<SeedPlan> {
    let remote = remote_hashes(backend).await?;
    let mut items = Vec::new();
    for (path, bytes) in local {
        let action = if !is_markdown(path) {
            SeedAction::SkippedBinary
        } else if opts_out_of_sharing(bytes) {
            SeedAction::SkippedOptOut
        } else {
            match remote.get(path.as_str()) {
                None => SeedAction::Create,
                Some(hash) if *hash == content_hash(bytes) => SeedAction::Unchanged,
                Some(_) => SeedAction::Update,
            }
        };
        items.push(SeedItem {
            path: path.clone(),
            action,
        });
    }
    Ok(SeedPlan {
        mount: mount_id.to_string(),
        from_dir: from_dir.to_path_buf(),
        first_import: remote.is_empty(),
        items,
    })
}

/// `path -> contentHash` for every live note in the index.
async fn remote_hashes(backend: &AlgoliaVaultBackend) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for path in reads::walk_markdown(backend)
        .await
        .map_err(|error| anyhow!("could not list the index: {error}"))?
    {
        let head = versioning::fetch_head(backend, &path)
            .await
            .map_err(|error| anyhow!("could not read the head of {path}: {error}"))?;
        if let Some(head) = head.filter(|head| !head.deleted) {
            map.insert(path, head.content_hash);
        }
    }
    Ok(map)
}

/// `share: false` in the note's own frontmatter.
///
/// Kept from #40, and it is the only per-note opt-out there is. A user seeding a folder
/// that contains one private note needs a way to say so that travels WITH the note rather
/// than living in a CLI invocation they have to remember next time. Non-UTF-8 bytes cannot
/// carry frontmatter and are not opted out here — they are refused as non-Markdown
/// instead, which is the more accurate refusal.
fn opts_out_of_sharing(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .map(|text| {
            deep_obsidian_backend::algolia::records_build::parse_frontmatter_fields(text).share
                == Some(false)
        })
        .unwrap_or(false)
}

/// Read the source tree into `path -> bytes`, keyed by the path the note will have INSIDE
/// the mount.
fn read_source_tree(from_dir: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    if !from_dir.is_dir() {
        bail!(
            "{} is not a directory; pass --from <folder> to name the local folder to import",
            from_dir.display()
        );
    }
    let mut relatives = Vec::new();
    collect_files(from_dir, from_dir, &mut relatives)?;
    relatives.sort();
    let mut tree = BTreeMap::new();
    for relative in relatives {
        let absolute = safe_destination(from_dir, &relative)?;
        let bytes = std::fs::read(&absolute)
            .with_context(|| format!("could not read {}", absolute.display()))?;
        tree.insert(relative, bytes);
    }
    Ok(tree)
}

/// One-shot import of a local folder into an Algolia mount.
///
/// `move_files` deletes the local original of each note AFTER re-reading the index and
/// confirming it now holds exactly those bytes. The confirmation is per file and it is the
/// point: an import that half-succeeded must not be followed by a deletion that assumed it
/// fully did.
pub async fn seed(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    from: Option<&Path>,
    dry_run: bool,
    move_files: bool,
) -> Result<SeedReport> {
    seed_with_resolver(
        config,
        mount_id,
        from,
        dry_run,
        move_files,
        &SecretResolver::new(),
    )
    .await
}

/// [`seed`] against an explicit secret store. Exists so a test can point at a temp secrets
/// file instead of mutating `XDG_CONFIG_HOME`, which is process-global.
pub async fn seed_with_resolver(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    from: Option<&Path>,
    dry_run: bool,
    move_files: bool,
    resolver: &SecretResolver,
) -> Result<SeedReport> {
    let mount = algolia_mount(config, mount_id)?;
    let from_dir = from
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_seed_source(config, &mount));
    let arc = connect(config, mount_id, resolver)?;
    let backend = require_algolia(&arc, mount_id)?;
    // Checked BEFORE the source tree is read, so a user pointing a seed at a read-only
    // mount is told immediately.
    if !dry_run && !backend.is_writable() {
        bail!(
            "mount {mount_id:?} is not writable: set \"writable\": true on the mount and restart \
             to allow an import. (`--dry-run` works on a read-only mount and reports exactly what \
             a writable one would do.)"
        );
    }

    let local = read_source_tree(&from_dir)?;
    let plan = plan_seed(backend, mount_id, &from_dir, &local).await?;

    let mut report = SeedReport {
        mount: mount_id.to_string(),
        from_dir: from_dir.clone(),
        dry_run,
        first_import: plan.first_import,
        created: 0,
        updated: 0,
        unchanged: plan.count(SeedAction::Unchanged),
        skipped: plan
            .items
            .iter()
            .filter(|item| !item.action.writes() && item.action != SeedAction::Unchanged)
            .cloned()
            .collect(),
        moved_out: Vec::new(),
        kept_drifted: Vec::new(),
        items: plan.items.clone(),
    };
    if dry_run {
        report.created = plan.count(SeedAction::Create);
        report.updated = plan.count(SeedAction::Update);
        return Ok(report);
    }

    for item in plan.items.iter().filter(|item| item.action.writes()) {
        let Some(bytes) = local.get(&item.path) else {
            continue;
        };
        let text = std::str::from_utf8(bytes).with_context(|| {
            format!(
                "{} is not valid UTF-8, so it cannot be stored as a note",
                item.path
            )
        })?;
        // BaseVersion::Unobserved: a seed asserts no precondition, so a note a colleague
        // edited between the plan and this write is superseded rather than reported as a
        // fork. That is the honest shape for an import — it forked away from nothing, it
        // simply arrived — and the previous version is still in history either way.
        arc.execute(BackendRequest::write_text(&item.path, text))
            .await
            .map_err(|error| anyhow!("could not import {}: {error}", item.path))?;
        match item.action {
            SeedAction::Create => report.created += 1,
            _ => report.updated += 1,
        }
    }

    if move_files {
        // A FRESH read of the index, not the plan: the guard has to be "the corpus now
        // holds exactly these bytes", and the plan was computed before the writes.
        let verified = remote_hashes(backend).await?;
        for item in plan
            .items
            .iter()
            .filter(|item| item.action.writes() || item.action == SeedAction::Unchanged)
        {
            let Some(bytes) = local.get(&item.path) else {
                continue;
            };
            if verified.get(&item.path) != Some(&content_hash(bytes)) {
                report.kept_drifted.push(item.path.clone());
                continue;
            }
            let absolute = safe_destination(&from_dir, &item.path)?;
            match std::fs::remove_file(&absolute) {
                Ok(()) => {
                    report.moved_out.push(item.path.clone());
                    prune_empty_parents(&from_dir, &absolute);
                }
                Err(error) => report.kept_drifted.push(format!("{} ({error})", item.path)),
            }
        }
    }
    Ok(report)
}

/// Remove now-empty directories between `file`'s parent and `root`, best effort.
///
/// Stops at the first directory that is not empty, and never removes `root` itself: the
/// source folder is what the user pointed at, and a `--move` that made it vanish would
/// look like the tool deleted more than it did.
fn prune_empty_parents(root: &Path, file: &Path) {
    let mut parent = file.parent();
    while let Some(directory) = parent {
        if directory == root || !directory.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(directory).is_err() {
            break;
        }
        parent = directory.parent();
    }
}

// ---------------------------------------------------------------------------
// Dump
// ---------------------------------------------------------------------------

/// The self-describing part of a dump.
///
/// Carries the mount id and nothing else about the connection: no app id, no index name,
/// no key. The mount id is sufficient provenance — it names the config entry that produced
/// the snapshot — and it is unambiguously not a secret, which is a property worth having
/// by construction rather than by review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpManifest {
    pub version: u32,
    pub mount: String,
    /// One row per note, ordered by path so the file is deterministic.
    pub entries: Vec<DumpEntry>,
}

/// One dumped note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpEntry {
    /// Path INSIDE the mount (no mount prefix), as the index keys it.
    pub path: String,
    /// The head version these bytes came from.
    pub version_id: String,
    /// Canonical hash of the dumped bytes, so a tree can be verified without re-reading
    /// the index.
    pub hash: String,
    pub size: u64,
    /// The head records a divergence: some version was pushed against a base that was not
    /// the head, and the content it forked away from is in the history index. Recorded
    /// rather than resolved — picking a winner needs a merge policy this tool has not got.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_divergence: bool,
    /// The reassembled body's hash did not match the hash the note record declares.
    ///
    /// Kept from #40, where it was a real finding rather than defensive programming: a
    /// body is stored as chunk records and reassembled on read, so a lost or duplicated
    /// chunk shows up here and NOWHERE ELSE. A dump is the one moment every note is read
    /// end to end, which makes it the right place to notice.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hash_mismatch: bool,
}

/// What a dump did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpReport {
    pub mount: String,
    pub out_dir: PathBuf,
    pub notes: usize,
    pub bytes: u64,
    pub divergent: Vec<String>,
    pub hash_mismatches: Vec<String>,
    /// A single hash over every `(path, hash)` pair, so two dumps can be compared with one
    /// string instead of a directory walk.
    pub tree_hash: String,
}

/// Materialize every live note of the mount to `out_dir`, with a manifest.
pub async fn dump(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    out_dir: &Path,
) -> Result<DumpReport> {
    dump_with_resolver(config, mount_id, out_dir, &SecretResolver::new()).await
}

/// [`dump`] against an explicit secret store.
pub async fn dump_with_resolver(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    out_dir: &Path,
    resolver: &SecretResolver,
) -> Result<DumpReport> {
    let arc = connect(config, mount_id, resolver)?;
    let backend = require_algolia(&arc, mount_id)?;

    let mut paths = reads::walk_markdown(backend)
        .await
        .map_err(|error| anyhow!("could not list mount {mount_id:?}: {error}"))?;
    // Sorted, so the tree is written in a fixed order and the manifest is deterministic.
    paths.sort();

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("could not create {}", out_dir.display()))?;

    let mut entries: Vec<DumpEntry> = Vec::new();
    let mut bytes_total: u64 = 0;
    for path in paths {
        // `read_note` rather than the boundary's `ReadText`, because it hands back the head
        // RECORD alongside the body — the declared content hash and the divergence flag
        // both live there, and a second round trip to fetch them would be a second chance
        // for the head to have moved underneath the bytes just read.
        let hydrated = match reads::read_note(backend, &path).await {
            Ok(hydrated) => hydrated,
            // A note that vanished between the listing and the read is a real outcome on a
            // SHARED corpus: a colleague retracted it. Skipping it is right; failing the
            // whole dump because someone else was working would make the command unusable
            // exactly when it matters.
            Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => continue,
            Err(error) => bail!("could not read {path}: {error}"),
        };
        let body = hydrated.content;
        let hash = content_hash(body.as_bytes());

        let destination = safe_destination(out_dir, &path)?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        std::fs::write(&destination, body.as_bytes())
            .with_context(|| format!("could not write {}", destination.display()))?;

        bytes_total = bytes_total.saturating_add(body.len() as u64);
        entries.push(DumpEntry {
            path: path.clone(),
            version_id: hydrated.note.version_id,
            hash: hash.clone(),
            size: body.len() as u64,
            has_divergence: hydrated.note.has_divergence,
            hash_mismatch: hash != hydrated.note.content_hash,
        });
    }

    let manifest = DumpManifest {
        version: MANIFEST_VERSION,
        mount: mount_id.to_string(),
        entries,
    };
    let tree_hash = tree_hash(&manifest);
    let serialized = format!("{}\n", serde_json::to_string_pretty(&manifest)?);
    std::fs::write(out_dir.join(MANIFEST_FILE), serialized)
        .with_context(|| format!("could not write the {MANIFEST_FILE}"))?;

    Ok(DumpReport {
        mount: mount_id.to_string(),
        out_dir: out_dir.to_path_buf(),
        notes: manifest.entries.len(),
        bytes: bytes_total,
        divergent: manifest
            .entries
            .iter()
            .filter(|entry| entry.has_divergence)
            .map(|entry| entry.path.clone())
            .collect(),
        hash_mismatches: manifest
            .entries
            .iter()
            .filter(|entry| entry.hash_mismatch)
            .map(|entry| entry.path.clone())
            .collect(),
        tree_hash,
    })
}

/// One hash over every `(path, hash)` pair.
///
/// Over the manifest's rows rather than over the files, so it is a statement about the
/// CONTENT of the snapshot and two dumps can be compared without walking either tree.
fn tree_hash(manifest: &DumpManifest) -> String {
    let mut hasher = ContentHasher::new();
    for entry in &manifest.entries {
        hasher.update(entry.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finish()
}

/// Read a dump manifest, or `None` when the directory has none.
///
/// A missing manifest is tolerated (a hand-assembled tree is a legitimate input) but an
/// unknown VERSION is not: refusing beats guessing at fields whose meaning may have moved.
fn read_manifest(from_dir: &Path) -> Result<Option<DumpManifest>> {
    let path = from_dir.join(MANIFEST_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let manifest: DumpManifest = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a readable dump manifest", path.display()))?;
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "{} declares manifest version {} but this build understands {MANIFEST_VERSION}; \
             re-dump with this build rather than restoring from a format it cannot read",
            path.display(),
            manifest.version
        );
    }
    Ok(Some(manifest))
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// What restoring one note did, or would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreAction {
    /// No live note at the path: written as a create.
    Created,
    /// The index already holds exactly these bytes.
    Unchanged,
    /// Differing content, written anyway because `--force` was given.
    ///
    /// Named `Superseded` rather than `Overwritten` on purpose, and the difference is not
    /// cosmetic: a write here APPENDS a version and moves the head pointer. The content
    /// that was there is in the history index and a versioned read can still serve it. On
    /// a couchdb mount `--force` genuinely overwrites a revision; here nothing is lost, and
    /// a label that claimed otherwise would make operators more afraid of the flag than
    /// the flag deserves.
    Superseded,
    /// Differing content, left alone because `--force` was not given.
    RefusedDiffers,
    /// Not a `.md` path. This corpus has no record shape for binary content, so the
    /// refusal is a fact about the storage rather than something a flag lifts — `--force`
    /// does NOT lift it.
    RefusedBinary,
}

impl RestoreAction {
    fn is_refusal(self) -> bool {
        matches!(
            self,
            RestoreAction::RefusedDiffers | RestoreAction::RefusedBinary
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOutcome {
    pub path: String,
    pub action: RestoreAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub mount: String,
    pub from_dir: PathBuf,
    pub dry_run: bool,
    pub created: usize,
    pub superseded: usize,
    pub unchanged: usize,
    pub refused: usize,
    pub outcomes: Vec<RestoreOutcome>,
}

impl RestoreReport {
    pub fn ok(&self) -> bool {
        self.refused == 0
    }
}

/// Write a previously dumped tree back into the mount.
///
/// # Refusal semantics
///
/// Deliberately the same shape as `couchdb restore`, because an operator should not have
/// to learn two:
///
/// * a path with no live note in the index is **created**;
/// * a path whose remote content is byte-identical is **skipped**, so a restore is
///   idempotent and a re-run reports nothing to do;
/// * a path whose remote content DIFFERS is **refused** unless `force`, and the refusal
///   names it. That is the whole safety property: the default cannot bury an edit made
///   after the dump;
/// * a non-`.md` path is refused regardless of `force`, with
///   [`ALGOLIA_NO_BINARY_MESSAGE`]. Refused HERE rather than in the backend so the report
///   names every such file at once instead of failing on the first;
/// * every write goes through the backend's guarded, fork-aware path, so a note edited
///   between this restore's read and its write forks and records a divergence rather than
///   silently winning.
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

/// [`restore`] against an explicit secret store.
pub async fn restore_with_resolver(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    from_dir: &Path,
    dry_run: bool,
    force: bool,
    resolver: &SecretResolver,
) -> Result<RestoreReport> {
    let arc = connect(config, mount_id, resolver)?;
    let backend = require_algolia(&arc, mount_id)?;
    if !dry_run && !backend.is_writable() {
        bail!(
            "mount {mount_id:?} is not writable: set \"writable\": true on the mount and restart \
             to allow a restore. (`--dry-run` works on a read-only mount and reports exactly what \
             a writable one would do.)"
        );
    }
    // Read for its side effect of REFUSING an unknown version. Its rows are not otherwise
    // needed: unlike a couchdb export there is no storage kind to recover from them (this
    // corpus stores one thing), so a tree with no manifest restores identically.
    let _ = read_manifest(from_dir)?;

    let mut files = Vec::new();
    collect_files(from_dir, from_dir, &mut files)?;
    files.sort();

    let mut report = RestoreReport {
        mount: mount_id.to_string(),
        from_dir: from_dir.to_path_buf(),
        dry_run,
        created: 0,
        superseded: 0,
        unchanged: 0,
        refused: 0,
        outcomes: Vec::new(),
    };

    for relative in files {
        if !is_markdown(&relative) {
            report.refused += 1;
            report.outcomes.push(RestoreOutcome {
                path: relative,
                action: RestoreAction::RefusedBinary,
                reason: Some(ALGOLIA_NO_BINARY_MESSAGE.to_string()),
            });
            continue;
        }
        let absolute = safe_destination(from_dir, &relative)?;
        let bytes = std::fs::read(&absolute)
            .with_context(|| format!("could not read {}", absolute.display()))?;
        let text = std::str::from_utf8(&bytes).with_context(|| {
            format!(
                "{relative} is not valid UTF-8; refusing to write a lossy conversion of it into \
                 the shared corpus"
            )
        })?;

        let head = versioning::fetch_head(backend, &relative)
            .await
            .map_err(|error| anyhow!("could not read {relative} from the index: {error}"))?
            .filter(|head| !head.deleted);

        let (action, base) = match &head {
            None => (RestoreAction::Created, BaseVersion::Absent),
            Some(head) if head.content_hash == content_hash(&bytes) => (
                RestoreAction::Unchanged,
                BaseVersion::Version(head.version_id.clone()),
            ),
            Some(head) if force => (
                RestoreAction::Superseded,
                // The observed head, so the write CONTINUES that version's line instead of
                // forking off it. A forced restore is an intentional supersession, not a
                // disagreement, and marking every one of them `hasDivergence` would fill
                // `conflicted_paths()` with notes nobody disagreed about.
                BaseVersion::Version(head.version_id.clone()),
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
                        "the index holds different content than the snapshot; pass --force to \
                         supersede it (the current version is kept in history either way)"
                            .to_string(),
                    ),
                });
                continue;
            }
            _ => {}
        }

        if !dry_run {
            arc.execute(BackendRequest::write_text_guarded(&relative, text, base))
                .await
                .map_err(|error| anyhow!("could not restore {relative}: {error}"))?;
        }
        match action {
            RestoreAction::Created => report.created += 1,
            RestoreAction::Superseded => report.superseded += 1,
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

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// What `algolia status` reports.
///
/// The mount id and the observed state, and nothing about the connection: no app id, no
/// index name, no base URL, no key. Same discipline as the dump manifest, and it is tested
/// — an operator pasting a status report into an issue must not be pasting a credential or
/// even the coordinates of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusReport {
    pub mount: String,
    pub mount_at: String,
    pub writable: bool,
    pub reachable: bool,
    pub main_provisioned: bool,
    pub history_provisioned: bool,
    pub notes: usize,
    pub superseded_versions: usize,
    pub divergent: Vec<String>,
    pub retention_min_versions: usize,
    pub retention_max_age_days: u64,
    pub cache_entries: usize,
    pub cache_bytes: u64,
}

pub async fn status(config: &ResolvedServiceConfig, mount_id: &str) -> Result<StatusReport> {
    status_with_resolver(config, mount_id, &SecretResolver::new()).await
}

pub async fn status_with_resolver(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    resolver: &SecretResolver,
) -> Result<StatusReport> {
    let mount = algolia_mount(config, mount_id)?;
    let arc = connect(config, mount_id, resolver)?;
    let backend = require_algolia(&arc, mount_id)?;
    let AlgoliaMountStatus {
        reachable,
        main_provisioned,
        history_provisioned,
        notes,
        superseded_versions,
        divergent_paths,
        retention,
        cache,
    } = backend.status().await;
    Ok(StatusReport {
        mount: mount_id.to_string(),
        mount_at: mount.mount_at.clone(),
        writable: backend.is_writable(),
        reachable,
        main_provisioned,
        history_provisioned,
        notes,
        superseded_versions,
        divergent: divergent_paths,
        retention_min_versions: retention.0,
        retention_max_age_days: retention.1,
        cache_entries: cache.0,
        cache_bytes: cache.1,
    })
}

// ---------------------------------------------------------------------------
// Retract
// ---------------------------------------------------------------------------

/// What a retraction removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetractReport {
    pub mount: String,
    pub path: String,
    pub dry_run: bool,
    /// The head version at the moment of the purge, so the operation is auditable after
    /// the fact from the command's own output.
    pub head_version_id: String,
    pub head_participant_id: String,
    /// How many versions were destroyed, the head included.
    pub versions_removed: usize,
}

/// Permanently remove a note and its entire history.
///
/// The ONE destructive operation in this family, and the reason it is a CLI command and
/// never an MCP tool: an agent cannot judge whether a human wanted a shared corpus's
/// history destroyed. Confirmation is the caller's job — see the `commands.rs` dispatch,
/// which prompts unless `--yes`.
pub async fn retract(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    path: &str,
    dry_run: bool,
) -> Result<RetractReport> {
    retract_with_resolver(config, mount_id, path, dry_run, &SecretResolver::new()).await
}

pub async fn retract_with_resolver(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    path: &str,
    dry_run: bool,
    resolver: &SecretResolver,
) -> Result<RetractReport> {
    let mount = algolia_mount(config, mount_id)?;
    let arc = connect(config, mount_id, resolver)?;
    let backend = require_algolia(&arc, mount_id)?;
    // Accepted in either form: `_Wiki/Foo.md` (index-relative, what the index keys) and
    // `_Wiki/Foo.md` under a mount at `_Wiki` would collide, so the MOUNT PREFIX is
    // stripped when present. A user reading a path out of `list_children` sees the mounted
    // form and must not have to translate it.
    let remote = strip_mount_prefix(&mount.mount_at, path);
    let head = versioning::fetch_head(backend, &remote)
        .await
        .map_err(|error| anyhow!("could not read {remote}: {error}"))?
        .ok_or_else(|| {
            anyhow!(
                "no note at {remote:?} in mount {mount_id:?} (a note already retracted leaves \
                 nothing behind, so this is also what a second retraction reports)"
            )
        })?;
    let versions = arc
        .execute(BackendRequest::note_versions(&remote))
        .await
        .map_err(|error| anyhow!("could not read the history of {remote}: {error}"))?
        .into_note_history()
        .map_err(|error| anyhow!("{error}"))?
        .versions
        .len();

    let report = RetractReport {
        mount: mount_id.to_string(),
        path: remote.clone(),
        dry_run,
        head_version_id: head.version_id,
        head_participant_id: head.participant_id,
        versions_removed: versions,
    };
    if dry_run {
        return Ok(report);
    }
    backend
        .retract_note(&remote)
        .await
        .map_err(|error| anyhow!("could not retract {remote}: {error}"))?;
    Ok(report)
}

/// Strip a mount's logical prefix from a user-supplied path, when it carries one.
///
/// `_Wiki/Foo.md` and `Foo.md` both address the same note on a mount at `_Wiki`. The
/// prefix is stripped rather than required because a path copied out of `list_children`
/// carries it and a path read out of a dump manifest does not, and a user should not have
/// to know which is which.
fn strip_mount_prefix(mount_at: &str, path: &str) -> String {
    let trimmed = path.trim().trim_start_matches('/');
    if mount_at.is_empty() {
        return trimmed.to_string();
    }
    trimmed
        .strip_prefix(&format!("{mount_at}/"))
        .unwrap_or(trimmed)
        .to_string()
}

// ---------------------------------------------------------------------------
// Secured keys
// ---------------------------------------------------------------------------

/// Where `algolia key` reads its PARENT key from.
///
/// A `SecretRef` is a JSON object in the config file, so it has no natural command-line
/// spelling; these three forms are it. Deliberately not a JSON blob on the command line:
/// a secret reference in shell history is fine, a mistyped one that silently resolves to
/// nothing is not, so the forms are short enough to get right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentKeyRef {
    /// `mount` (or the flag omitted): the mount's own configured `apiKeyRef`.
    ///
    /// The default because it is the case that must FAIL LOUDLY. A writable mount's key
    /// can write, and a secured key derived from it would read a narrow slice while
    /// writing anywhere — so the common first attempt has to hit the refusal and its
    /// explanation, rather than a user having to know to ask for it.
    Mount,
    /// `keyring:<service>/<account>`
    Keyring { service: String, account: String },
    /// `file:<id>` — an entry in the encrypted secrets file.
    File { id: String },
    /// `env:<VAR>` — an environment variable. The form the demo script and the live tests
    /// use, where a search-only key is minted for the occasion and never stored.
    Env { name: String },
}

/// Parse a `--parent-key-ref` value.
///
/// # Storing a key in the first place
///
/// There is no `algolia set-key`, and no generic `secrets set` command either as of this
/// slice. A mount's API key reaches the process one of two ways: the `apiKeyRef` secret,
/// written by `setup-service --wizard` or placed in the encrypted secrets file directly;
/// or `$DEEP_OBSIDIAN_ALGOLIA_API_KEY`, which SHADOWS the configured reference and is
/// logged at `warn` when it does. `env:` here exists so the second path composes with key
/// derivation without a key ever being stored.
pub fn parse_parent_key_ref(spec: &str) -> Result<ParentKeyRef> {
    let spec = spec.trim();
    if spec.is_empty() || spec == "mount" {
        return Ok(ParentKeyRef::Mount);
    }
    if let Some(rest) = spec.strip_prefix("keyring:") {
        let (service, account) = rest.split_once('/').ok_or_else(|| {
            anyhow!("keyring references are `keyring:<service>/<account>`, got {spec:?}")
        })?;
        if service.trim().is_empty() || account.trim().is_empty() {
            bail!("keyring references are `keyring:<service>/<account>`, got {spec:?}");
        }
        return Ok(ParentKeyRef::Keyring {
            service: service.to_string(),
            account: account.to_string(),
        });
    }
    if let Some(id) = spec.strip_prefix("file:") {
        if id.trim().is_empty() {
            bail!("encrypted-file references are `file:<id>`, got {spec:?}");
        }
        return Ok(ParentKeyRef::File { id: id.to_string() });
    }
    if let Some(name) = spec.strip_prefix("env:") {
        if name.trim().is_empty() {
            bail!("environment references are `env:<VAR>`, got {spec:?}");
        }
        return Ok(ParentKeyRef::Env {
            name: name.to_string(),
        });
    }
    bail!(
        "unrecognized parent key reference {spec:?}; use `mount` (the mount's own apiKeyRef), \
         `keyring:<service>/<account>`, `file:<id>`, or `env:<VAR>`"
    )
}

/// The Algolia ACLs that a parent key must NOT have, and why the check exists at all.
///
/// # The finding this encodes
///
/// A secured API key INHERITS its parent's ACLs, and the `filters` restriction it carries
/// applies to **search only**. Verified against a live account in PR #40. So deriving a
/// "read-only teammate key" from a mount's WRITE key produces a key that reads exactly the
/// slice you scoped it to and can write ANYWHERE in the index — the opposite of what the
/// operation is for, and impossible to notice from the key itself.
///
/// Hence: the parent is inspected, not trusted. `Err` is a refusal with the remedy in it;
/// `Ok(warnings)` is a usable parent plus anything the teammate will trip over.
pub fn classify_parent_key(acls: &[String]) -> Result<Vec<String>, String> {
    let write: Vec<&str> = acls
        .iter()
        .map(String::as_str)
        .filter(|acl| deep_obsidian_algolia::WRITE_ACLS.contains(acl))
        .collect();
    if !write.is_empty() {
        return Err(format!(
            "refusing to derive a teammate key from a parent that can WRITE (ACLs: {write:?}).\n\n\
             A secured key inherits its parent's ACLs, and its `filters` restriction applies to \
             SEARCH ONLY — the derived key would read the slice you scoped it to while writing \
             anywhere in the index.\n\n\
             Create a search-only key for this index (Algolia dashboard > API Keys > New, ACLs: \
             search and browse), then pass it with --parent-key-ref env:VAR, \
             --parent-key-ref keyring:<service>/<account>, or --parent-key-ref file:<id>."
        ));
    }
    if !acls.iter().any(|acl| acl == "search") {
        return Err(format!(
            "the parent key cannot search (ACLs: {acls:?}); a teammate key derived from it would \
             be useless"
        ));
    }
    let mut warnings = Vec::new();
    if !acls.iter().any(|acl| acl == "browse") {
        // `browse` is a separate ACL from `search`, and several mount reads enumerate
        // exhaustively. Without it the teammate gets a bare 403 from those, so it is said
        // up front rather than discovered.
        warnings.push(format!(
            "the parent key lacks the `browse` ACL (has: {acls:?}). Reads that enumerate \
             exhaustively will fail with 403 for the teammate: list_children on the mount ROOT \
             (subfolders of a named folder still work), note_history, and `algolia dump`. Add \
             `browse` for a fully usable read-only mount."
        ));
    }
    Ok(warnings)
}

/// The `filters` expression scoping a secured key to one folder.
///
/// Built from the `folders.lvlN` facets [`deep_obsidian_algolia::records::folder_facets`]
/// emits, which is why the depth limit is real rather than arbitrary: only `lvl0`, `lvl1`
/// and `lvl2` are declared as facets in the index settings, so a folder four levels down
/// cannot be expressed as a filter at all. Refused with that reason rather than silently
/// scoped to its grandparent, which would hand out a key with more access than asked for.
pub fn prefix_filter(prefix: &str) -> Result<String> {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() {
        bail!("--prefix requires a vault-relative folder, e.g. _Wiki or _Wiki/Decisions");
    }
    let depth = trimmed.split('/').count();
    if depth > 3 {
        bail!(
            "--prefix {prefix:?} is {depth} levels deep, but only three levels of folder facet \
             exist in this index (folders.lvl0..lvl2), so a key cannot be scoped to it. Scope to \
             one of its first three levels instead, or restructure the corpus."
        );
    }
    Ok(format!(
        "folders.lvl{}:{}",
        depth - 1,
        quote_filter_value(trimmed)
    ))
}

/// Quote a value for an Algolia `filters` expression. Backslashes first, then quotes, so
/// the escape itself cannot be smuggled in.
fn quote_filter_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Percent-encode a restriction payload for the secured-key message.
fn percent_encode(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => character.to_string(),
            other => other
                .to_string()
                .into_bytes()
                .iter()
                .map(|byte| format!("%{byte:02X}"))
                .collect(),
        })
        .collect()
}

/// A derived key, plus what the operator has to be told about it.
///
/// `key` is meant to be shared — that is the whole point — so it is printed. The PARENT
/// never appears in this struct, in `Debug`, or in the rendered output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedKeyReport {
    pub mount: String,
    /// The secured key. Shareable by construction: it can only search, and only inside
    /// `filters`.
    pub key: String,
    /// The restriction that was baked in, or `None` for an unscoped read-only key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<String>,
    /// The parent's ACLs, verified to contain no write ACL. Reported so the check is
    /// auditable from the output.
    pub parent_acls: Vec<String>,
    pub warnings: Vec<String>,
}

/// Derive a scoped, read-only secured API key for a teammate.
pub async fn derive_key(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    parent: &ParentKeyRef,
    prefix: Option<&str>,
) -> Result<DerivedKeyReport> {
    derive_key_with_resolver(config, mount_id, parent, prefix, &SecretResolver::new()).await
}

pub async fn derive_key_with_resolver(
    config: &ResolvedServiceConfig,
    mount_id: &str,
    parent: &ParentKeyRef,
    prefix: Option<&str>,
    resolver: &SecretResolver,
) -> Result<DerivedKeyReport> {
    let mount = algolia_mount(config, mount_id)?;
    let (app_id, base_url, api_key_ref) = match &mount.backend {
        MountBackendConfig::Algolia {
            app_id,
            base_url,
            api_key_ref,
            ..
        } => (app_id.clone(), base_url.clone(), api_key_ref.clone()),
        // Unreachable: `algolia_mount` has already established the variant.
        _ => bail!("mount {mount_id:?} is not an algolia mount"),
    };
    let parent_key = resolve_parent_key(parent, &api_key_ref, resolver)?;
    let filters = prefix.map(prefix_filter).transpose()?;

    let probe = deep_obsidian_algolia::AlgoliaClient::new(
        &app_id,
        parent_key.expose_secret(),
        base_url.as_deref(),
    );
    let acls = probe
        .key_acls(parent_key.expose_secret())
        .await
        .map_err(|error| anyhow!("cannot inspect the parent key's ACLs: {error}"))?;
    let warnings = classify_parent_key(&acls).map_err(|refusal| anyhow!("{refusal}"))?;

    let restrictions = filters
        .as_deref()
        .map(|filters| format!("filters={}", percent_encode(filters)))
        .unwrap_or_default();
    Ok(DerivedKeyReport {
        mount: mount_id.to_string(),
        key: deep_obsidian_algolia::generate_secured_api_key(
            parent_key.expose_secret(),
            &restrictions,
        ),
        filters,
        parent_acls: acls,
        warnings,
    })
}

/// Resolve a [`ParentKeyRef`] to the key itself.
///
/// `ParentKeyRef::Mount` goes through the same environment-shadowing rule the server
/// applies, because a user who has `$DEEP_OBSIDIAN_ALGOLIA_API_KEY` set is running the
/// mount on that key and "the mount's key" must mean the same thing in both places.
fn resolve_parent_key(
    parent: &ParentKeyRef,
    api_key_ref: &SecretRef,
    resolver: &SecretResolver,
) -> Result<SecretString> {
    let missing =
        |what: String| anyhow!("the parent key {what} is not set, so nothing to derive from");
    match parent {
        ParentKeyRef::Mount => {
            if let Ok(from_env) = std::env::var(deep_obsidian_backend::ALGOLIA_API_KEY_ENV) {
                let from_env = from_env.trim().to_string();
                if !from_env.is_empty() {
                    return Ok(SecretString::new(from_env));
                }
            }
            resolver
                .get(api_key_ref)
                .map_err(|error| anyhow!("the mount's apiKeyRef could not be read: {error}"))?
                .ok_or_else(|| missing("referenced by the mount's apiKeyRef".to_string()))
        }
        ParentKeyRef::Keyring { service, account } => resolver
            .get(&SecretRef::OsKeyring {
                service: service.clone(),
                account: account.clone(),
            })
            .map_err(|error| anyhow!("the keyring reference could not be read: {error}"))?
            .ok_or_else(|| missing(format!("keyring:{service}/{account}"))),
        ParentKeyRef::File { id } => resolver
            .get(&SecretRef::EncryptedFile { id: id.clone() })
            .map_err(|error| anyhow!("the encrypted-file reference could not be read: {error}"))?
            .ok_or_else(|| missing(format!("file:{id}"))),
        ParentKeyRef::Env { name } => {
            let value = std::env::var(name).unwrap_or_default().trim().to_string();
            if value.is_empty() {
                return Err(missing(format!("env:{name}")));
            }
            Ok(SecretString::new(value))
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_seed_report(report: &SeedReport) -> String {
    let mut lines = vec![
        format!(
            "{} mount '{}'",
            if report.dry_run {
                "would seed"
            } else {
                "seeded"
            },
            report.mount
        ),
        format!("  from         {}", report.from_dir.display()),
        format!(
            "  created {}  updated {}  unchanged {}  skipped {}",
            report.created,
            report.updated,
            report.unchanged,
            report.skipped.len()
        ),
    ];
    if report.first_import {
        lines.push(
            "  FIRST IMPORT into this index: it held no notes, so the corpus is coming into \
             being from this machine"
                .to_string(),
        );
    }
    for item in &report.skipped {
        lines.push(format!(
            "  skipped      {} ({})",
            item.path,
            match item.action {
                SeedAction::SkippedBinary =>
                    "not a .md file; this corpus stores Markdown only, so keep it on a \
                     filesystem mount and link to it",
                SeedAction::SkippedOptOut => "its frontmatter says `share: false`",
                _ => "unspecified",
            }
        ));
    }
    for path in &report.moved_out {
        lines.push(format!("  removed local  {path}"));
    }
    for path in &report.kept_drifted {
        lines.push(format!(
            "  kept local     {path} (drifted since the import)"
        ));
    }
    if !report.moved_out.is_empty() {
        lines.push(format!(
            "  {} local file(s) removed; the index now holds the only copy — back it up with \
             `algolia dump`",
            report.moved_out.len()
        ));
    }
    if !report.dry_run {
        lines.push(
            "  nothing was deleted from the index: a seed only creates and updates, so a note \
             in the corpus that is not in this folder is left alone"
                .to_string(),
        );
    }
    lines.join("\n")
}

pub fn render_dump_report(report: &DumpReport) -> String {
    let mut lines = vec![
        format!("dumped mount '{}'", report.mount),
        format!("  to           {}", report.out_dir.display()),
        format!("  notes        {}", report.notes),
        format!("  bytes        {}", report.bytes),
        format!("  tree hash    {}", report.tree_hash),
    ];
    if report.divergent.is_empty() {
        lines.push("  divergence   none".to_string());
    } else {
        lines.push(format!(
            "  divergence   {} note(s) dumped at their CURRENT head; the version each forked \
             away from is in the history index and was not dumped",
            report.divergent.len()
        ));
        for path in &report.divergent {
            lines.push(format!("               - {path}"));
        }
    }
    for path in &report.hash_mismatches {
        lines.push(format!(
            "  WARNING      {path}: the reassembled body does not match the hash its record \
             declares, so a chunk record is missing or duplicated"
        ));
    }
    lines.push(
        "  verify       re-dump into another directory and compare the tree hashes, or `diff -r` \
         the two trees"
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
            "  created {}  superseded {}  unchanged {}  refused {}",
            report.created, report.superseded, report.unchanged, report.refused
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
            "  nothing was written for the refused entries; re-run with --force only if you \
             intend the snapshot's content to become the new head (the current version is kept \
             in history)"
                .to_string(),
        );
    }
    lines.join("\n")
}

pub fn render_status_report(report: &StatusReport) -> String {
    let mut lines = vec![
        format!(
            "mount '{}' at '{}'{}",
            report.mount,
            report.mount_at,
            if report.writable {
                " (writable)"
            } else {
                " (read-only)"
            }
        ),
        format!(
            "  reachable    {}",
            if report.reachable { "yes" } else { "NO" }
        ),
        format!(
            "  provisioned  main {}, history {}",
            yes_no(report.main_provisioned),
            yes_no(report.history_provisioned)
        ),
        format!("  notes        {}", report.notes),
        format!(
            "  history      {} superseded version(s)",
            report.superseded_versions
        ),
        format!(
            "  retention    the {} most recent versions, plus anything younger than {} days",
            report.retention_min_versions, report.retention_max_age_days
        ),
        format!(
            "  cache        {} note(s), {} bytes",
            report.cache_entries, report.cache_bytes
        ),
    ];
    if report.divergent.is_empty() {
        lines.push("  divergence   none".to_string());
    } else {
        lines.push(format!("  divergence   {} note(s)", report.divergent.len()));
        for path in &report.divergent {
            lines.push(format!(
                "               - {path}  (resolve_divergence reconciles it)"
            ));
        }
    }
    if !report.reachable {
        lines.push(
            "  the index did not answer: check the network, the app id, and whether the \
             apiKeyRef secret is still valid. An index nobody has written to yet is also \
             unreachable in this sense — it does not exist until its first write."
                .to_string(),
        );
    } else if !report.main_provisioned && report.notes > 0 {
        lines.push(
            "  the index holds records but carries no faceting settings, so folder listings \
             will fail. Settings are applied on a write; the next one repairs it."
                .to_string(),
        );
    }
    lines.join("\n")
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

pub fn render_retract_report(report: &RetractReport) -> String {
    let mut lines = vec![
        format!(
            "{} {} from mount '{}'",
            if report.dry_run {
                "would retract"
            } else {
                "retracted"
            },
            report.path,
            report.mount
        ),
        format!(
            "  head         {} by {}",
            report.head_version_id, report.head_participant_id
        ),
        format!("  versions     {}", report.versions_removed),
    ];
    lines.push(if report.dry_run {
        "  (dry run: nothing was removed)".to_string()
    } else {
        "  the note, its chunks and its ENTIRE history are gone; this is not recoverable \
         through this tool"
            .to_string()
    });
    lines.join("\n")
}

pub fn render_derived_key_report(report: &DerivedKeyReport) -> String {
    let mut lines = vec![format!(
        "secured read-only key for mount '{}' (parent ACLs verified write-free: {:?})",
        report.mount, report.parent_acls
    )];
    match &report.filters {
        Some(filters) => lines.push(format!("  scoped to    {filters}")),
        None => lines.push(
            "  scoped to    the WHOLE index (no --prefix given): the holder can search every \
             note in it"
                .to_string(),
        ),
    }
    lines.push(String::new());
    lines.push(report.key.clone());
    lines.push(String::new());
    lines.push(
        "The teammate supplies this as their mount's apiKeyRef secret, or via \
         $DEEP_OBSIDIAN_ALGOLIA_API_KEY. Writes are refused by Algolia, not by this server."
            .to_string(),
    );
    for warning in &report.warnings {
        lines.push(format!("WARNING: {warning}"));
    }
    lines.join("\n")
}

/// The prompt text for `algolia retract` without `--yes`. Here rather than in the dispatch
/// so it can be asserted on.
pub fn retract_confirmation(report: &RetractReport) -> String {
    format!(
        "This permanently deletes {} and all {} of its versions from mount '{}'. It cannot be \
         undone. Proceed?",
        report.path, report.versions_removed, report.mount
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn entry(path: &str) -> DumpEntry {
        DumpEntry {
            path: path.to_string(),
            version_id: "v1-abcd".to_string(),
            hash: "fnv1a64:0".to_string(),
            size: 1,
            has_divergence: false,
            hash_mismatch: false,
        }
    }

    /// A path from the index that would escape the target directory is refused, not
    /// normalized. Note paths come from a remote, so they are untrusted.
    #[test]
    fn a_traversing_remote_path_is_refused() {
        let root = Path::new("/tmp/dump");
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

    /// The manifest round-trips, omits its empty flags, and carries nothing about the
    /// connection — no app id, no index name, no key.
    #[test]
    fn the_manifest_round_trips_and_names_no_connection_detail() {
        let manifest = DumpManifest {
            version: MANIFEST_VERSION,
            mount: "wiki".to_string(),
            entries: vec![entry("A.md")],
        };
        let text = serde_json::to_string(&manifest).expect("serialize");
        assert!(!text.contains("hasDivergence"), "{text}");
        assert!(!text.contains("hashMismatch"), "{text}");
        for secretish in ["appId", "indexName", "apiKey", "baseUrl", "key"] {
            assert!(
                !text.contains(secretish),
                "{secretish} must not appear: {text}"
            );
        }
        assert_eq!(
            serde_json::from_str::<DumpManifest>(&text).expect("deserialize"),
            manifest
        );
    }

    /// The tree hash is over CONTENT, so it survives a rename of the mount and changes
    /// when any byte or any path does.
    #[test]
    fn the_tree_hash_tracks_content_only() {
        let manifest = |entries: Vec<DumpEntry>| DumpManifest {
            version: MANIFEST_VERSION,
            mount: "wiki".to_string(),
            entries,
        };
        let base = manifest(vec![entry("A.md")]);
        let mut renamed = base.clone();
        renamed.mount = "other".to_string();
        assert_eq!(tree_hash(&base), tree_hash(&renamed));
        // The version id is NOT part of it: the same bytes re-pushed under a new version
        // are the same snapshot, which is what makes "dump, restore, dump, compare" work.
        let mut reversioned = base.clone();
        reversioned.entries[0].version_id = "v2-ffff".to_string();
        assert_eq!(tree_hash(&base), tree_hash(&reversioned));

        let mut changed = base.clone();
        changed.entries[0].hash = "fnv1a64:1".to_string();
        assert_ne!(tree_hash(&base), tree_hash(&changed));
        let mut moved = base.clone();
        moved.entries[0].path = "B.md".to_string();
        assert_ne!(tree_hash(&base), tree_hash(&moved));
    }

    /// An unknown manifest version is refused rather than read loosely.
    #[test]
    fn an_unknown_manifest_version_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "algolia-cmd-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join(MANIFEST_FILE),
            r#"{"version": 99, "mount": "wiki", "entries": []}"#,
        )
        .expect("write manifest");
        let error = read_manifest(&dir).expect_err("an unknown version must be refused");
        assert!(error.to_string().contains("version 99"), "{error}");

        // A directory with no manifest is fine: a hand-assembled tree is legitimate.
        std::fs::remove_file(dir.join(MANIFEST_FILE)).expect("remove");
        assert!(read_manifest(&dir).expect("no manifest is ok").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// THE key-derivation safety property: a write-capable parent is refused, and the
    /// refusal explains the inheritance rule that makes it necessary.
    #[test]
    fn a_write_capable_parent_key_is_refused_with_the_reason() {
        for write_acl in deep_obsidian_algolia::WRITE_ACLS {
            let acls = vec!["search".to_string(), (*write_acl).to_string()];
            let refusal = classify_parent_key(&acls)
                .expect_err("a parent that can write must never yield a teammate key");
            assert!(refusal.contains("WRITE"), "{refusal}");
            assert!(refusal.contains(write_acl), "{refusal}");
            // The reason, not just the verdict: a user told only "refused" would go
            // looking for a flag to override it.
            assert!(refusal.contains("inherits"), "{refusal}");
            assert!(refusal.contains("SEARCH ONLY"), "{refusal}");
            assert!(refusal.contains("search-only key"), "{refusal}");
        }
    }

    /// A search-only parent is accepted; a missing `browse` is a WARNING, not a refusal,
    /// because such a key still serves most reads.
    #[test]
    fn a_search_only_parent_is_accepted_and_missing_browse_only_warns() {
        let full = vec!["search".to_string(), "browse".to_string()];
        assert_eq!(classify_parent_key(&full), Ok(Vec::new()));

        let warnings = classify_parent_key(&["search".to_string()]).expect("still usable");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("browse"), "{warnings:?}");
        assert!(warnings[0].contains("note_history"), "{warnings:?}");

        // A parent that cannot search at all is useless, and that IS a refusal.
        let refusal =
            classify_parent_key(&["browse".to_string()]).expect_err("no search is useless");
        assert!(refusal.contains("cannot search"), "{refusal}");
    }

    /// `--prefix` becomes the facet level its depth implies, and a folder deeper than the
    /// declared facets is refused rather than silently widened to its grandparent.
    #[test]
    fn a_prefix_maps_onto_the_facet_level_its_depth_implies() {
        assert_eq!(
            prefix_filter("_Wiki").expect("depth 1"),
            "folders.lvl0:\"_Wiki\""
        );
        assert_eq!(
            prefix_filter("/_Wiki/Decisions/").expect("depth 2, slashes trimmed"),
            "folders.lvl1:\"_Wiki/Decisions\""
        );
        assert_eq!(
            prefix_filter("a/b/c").expect("depth 3"),
            "folders.lvl2:\"a/b/c\""
        );
        let refusal = prefix_filter("a/b/c/d").expect_err("deeper than lvl2 must be refused");
        assert!(refusal.to_string().contains("three levels"), "{refusal}");
        assert!(prefix_filter("  ").is_err());
        // A quote in a folder name cannot terminate the filter expression early.
        assert_eq!(
            prefix_filter("Odd \"name\"").expect("quoted"),
            "folders.lvl0:\"Odd \\\"name\\\"\""
        );
    }

    #[test]
    fn parent_key_references_parse_or_say_what_they_should_look_like() {
        assert_eq!(parse_parent_key_ref("mount").unwrap(), ParentKeyRef::Mount);
        assert_eq!(parse_parent_key_ref("  ").unwrap(), ParentKeyRef::Mount);
        assert_eq!(
            parse_parent_key_ref("keyring:deep-obsidian-mcp/algolia-wiki").unwrap(),
            ParentKeyRef::Keyring {
                service: "deep-obsidian-mcp".to_string(),
                account: "algolia-wiki".to_string()
            }
        );
        assert_eq!(
            parse_parent_key_ref("file:algolia-wiki").unwrap(),
            ParentKeyRef::File {
                id: "algolia-wiki".to_string()
            }
        );
        assert_eq!(
            parse_parent_key_ref("env:MY_KEY").unwrap(),
            ParentKeyRef::Env {
                name: "MY_KEY".to_string()
            }
        );
        for bad in ["keyring:noslash", "file:", "env:", "whatever"] {
            let error = parse_parent_key_ref(bad).expect_err("{bad} must be refused");
            assert!(
                error.to_string().contains("keyring:") || error.to_string().contains("references"),
                "{error}"
            );
        }
    }

    /// The mount prefix is optional on a path, in either direction.
    #[test]
    fn a_path_addresses_the_same_note_with_or_without_the_mount_prefix() {
        assert_eq!(strip_mount_prefix("_Wiki", "_Wiki/Foo.md"), "Foo.md");
        assert_eq!(strip_mount_prefix("_Wiki", "Foo.md"), "Foo.md");
        assert_eq!(strip_mount_prefix("_Wiki", "/_Wiki/Foo.md"), "Foo.md");
        assert_eq!(strip_mount_prefix("", "Foo.md"), "Foo.md");
        // A note whose own folder happens to be named like the mount is not mangled: only
        // ONE leading prefix segment is stripped.
        assert_eq!(
            strip_mount_prefix("_Wiki", "_Wiki/_Wiki/Foo.md"),
            "_Wiki/Foo.md"
        );
    }

    /// The status report names no connection detail. An operator pasting one into an issue
    /// must not be pasting a credential or the coordinates of one.
    #[test]
    fn a_status_report_carries_no_connection_detail() {
        let report = StatusReport {
            mount: "wiki".to_string(),
            mount_at: "_Wiki".to_string(),
            writable: true,
            reachable: true,
            main_provisioned: true,
            history_provisioned: false,
            notes: 3,
            superseded_versions: 4,
            divergent: vec!["A.md".to_string()],
            retention_min_versions: 5,
            retention_max_age_days: 90,
            cache_entries: 2,
            cache_bytes: 100,
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let text = render_status_report(&report);
        for secretish in [
            "appId",
            "app_id",
            "indexName",
            "apiKey",
            "baseUrl",
            "algolia.net",
        ] {
            assert!(!json.contains(secretish), "{secretish} in {json}");
            assert!(!text.contains(secretish), "{secretish} in {text}");
        }
        assert!(text.contains("resolve_divergence"), "{text}");
    }

    /// `share: false` opts a note out; anything else does not.
    #[test]
    fn frontmatter_opts_a_note_out_of_a_seed() {
        assert!(opts_out_of_sharing(b"---\nshare: false\n---\n# Private\n"));
        assert!(!opts_out_of_sharing(b"---\nshare: true\n---\n# Public\n"));
        assert!(!opts_out_of_sharing(b"# No frontmatter\n"));
        // Non-UTF-8 cannot carry frontmatter, and is refused as non-Markdown elsewhere
        // rather than treated as opted out here.
        assert!(!opts_out_of_sharing(&[0xff, 0xfe, 0x00]));
    }

    #[test]
    fn markdown_is_recognized_case_insensitively() {
        for path in ["A.md", "a/B.MD", "deep/nest/c.Md"] {
            assert!(is_markdown(path), "{path}");
        }
        for path in ["A.png", "noextension", "A.md.png", "A.markdown"] {
            assert!(!is_markdown(path), "{path}");
        }
    }

    /// Percent-encoding covers multi-byte characters a naive `char as u32` would mangle.
    #[test]
    fn the_restriction_payload_percent_encodes_every_byte() {
        assert_eq!(
            percent_encode("folders.lvl0:\"_Wiki\""),
            "folders.lvl0%3A%22_Wiki%22"
        );
        // é is two UTF-8 bytes and must become two escapes.
        assert_eq!(percent_encode("é"), "%C3%A9");
    }

    /// The refusal for a non-Markdown file in a restore tree is the STORAGE's message, and
    /// `--force` is not offered as a way around it.
    #[test]
    fn a_binary_restore_refusal_does_not_offer_force() {
        let refusal = RestoreOutcome {
            path: "Assets/logo.png".to_string(),
            action: RestoreAction::RefusedBinary,
            reason: Some(ALGOLIA_NO_BINARY_MESSAGE.to_string()),
        };
        assert!(refusal.action.is_refusal());
        let reason = refusal.reason.expect("a reason");
        assert!(reason.contains("MARKDOWN ONLY"), "{reason}");
        assert!(!reason.contains("--force"), "{reason}");
    }

    /// `--move`'s empty-parent pruning stops at the source root: a `--move` that removed
    /// the folder the user pointed at would look like the tool deleted more than it did.
    #[test]
    fn pruning_empty_parents_never_removes_the_source_root() {
        let root = std::env::temp_dir().join(format!(
            "algolia-cmd-prune-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("nested dirs");
        let file = nested.join("note.md");
        std::fs::write(&file, "x").expect("write");
        std::fs::remove_file(&file).expect("remove");
        prune_empty_parents(&root, &file);
        assert!(root.is_dir(), "the source root survives");
        assert!(!root.join("a").exists(), "empty parents are pruned");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A dump entry keeps `versionId` even though the tree hash ignores it: the hash
    /// answers "is this the same content", the field answers "which version produced it",
    /// and an audit needs both.
    #[test]
    fn a_seed_plan_counts_only_writes_as_changes() {
        let plan = SeedPlan {
            mount: "wiki".to_string(),
            from_dir: PathBuf::from("/vault/_Wiki"),
            first_import: false,
            items: vec![
                SeedItem {
                    path: "A.md".to_string(),
                    action: SeedAction::Create,
                },
                SeedItem {
                    path: "B.md".to_string(),
                    action: SeedAction::Unchanged,
                },
                SeedItem {
                    path: "C.md".to_string(),
                    action: SeedAction::SkippedOptOut,
                },
                SeedItem {
                    path: "d.png".to_string(),
                    action: SeedAction::SkippedBinary,
                },
                SeedItem {
                    path: "E.md".to_string(),
                    action: SeedAction::Update,
                },
            ],
        };
        assert_eq!(plan.changed(), 2);
        assert_eq!(plan.count(SeedAction::Unchanged), 1);
        assert_eq!(plan.count(SeedAction::SkippedBinary), 1);
        assert!(!SeedAction::Unchanged.writes());
        assert!(!SeedAction::SkippedOptOut.writes());
    }

    /// The default seed source is the mount's own folder in the ROOT vault — the local
    /// directory the mount shadows, which is exactly what a migration wants to import.
    #[test]
    fn the_default_seed_source_is_the_folder_the_mount_shadows() {
        let mount = MountConfig {
            recall_weight: None,
            id: "wiki".to_string(),
            mount_at: "_Wiki".to_string(),
            backend: MountBackendConfig::Algolia {
                app_id: "APP".to_string(),
                index_name: "wiki".to_string(),
                api_key_ref: SecretRef::EncryptedFile {
                    id: "k".to_string(),
                },
                base_url: None,
                writable: true,
                participant_id: None,
                cache: None,
                retention: None,
                index_dir: None,
            },
        };
        let config = ResolvedServiceConfig {
            federated_rerank: true,
            vault_path: PathBuf::from("/vault"),
            mounts: vec![mount.clone()],
            experimental: Default::default(),
            index_dir: PathBuf::from("/idx"),
            transport: deep_obsidian_types::TransportMode::Stdio,
            stdio_mode: deep_obsidian_types::StdioMode::Auto,
            http: Default::default(),
            auto_reindex: Default::default(),
            embedding: Default::default(),
            artifact_embedding: Default::default(),
            auth: Default::default(),
            config_file_path: None,
        };
        assert_eq!(
            default_seed_source(&config, &mount),
            PathBuf::from("/vault/_Wiki")
        );
    }

    /// Unused-import guard for the set the module needs; also documents that a dump's
    /// paths are collected into a sorted, de-duplicated set before writing.
    #[test]
    fn collected_files_are_relative_and_slash_joined() {
        let root = std::env::temp_dir().join(format!(
            "algolia-cmd-collect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("Sub")).expect("dirs");
        std::fs::write(root.join("A.md"), "a").expect("write");
        std::fs::write(root.join("Sub").join("B.md"), "b").expect("write");
        std::fs::write(root.join(MANIFEST_FILE), "{}").expect("write");
        std::fs::write(root.join(".hidden.md"), "h").expect("write");
        let mut files = Vec::new();
        collect_files(&root, &root, &mut files).expect("collect");
        let found: BTreeSet<String> = files.into_iter().collect();
        assert_eq!(
            found,
            ["A.md", "Sub/B.md"]
                .iter()
                .map(|path| path.to_string())
                .collect::<BTreeSet<String>>(),
            "the manifest and dot-entries are skipped"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
