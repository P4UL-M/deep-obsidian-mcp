//! `algolia seed`/`dump`/`restore`/`status`/`retract` against the in-process mock Algolia.
//!
//! Hermetic: no network, no credentials, no `#[ignore]`. The mock speaks the same REST
//! surface the real account does, which is enough for every property these commands own —
//! the round trip, the refusal semantics, and the purge.
//!
//! The centrepiece is `seed → dump → compare bytes`. That is the whole reason the dump
//! format has no timestamp in it: "import a folder, dump it back, the bytes are identical"
//! is only a verification if the comparison has no false positives. It also happens to
//! test something a unit test cannot reach — that a note survives being split into chunk
//! records and reassembled from them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use deep_obsidian_algolia::mock::spawn_mock;
use deep_obsidian_cli::algolia_cmd::{
    dump_with_resolver, restore_with_resolver, retract_with_resolver, seed_with_resolver,
    status_with_resolver, DumpManifest, RestoreAction, SeedAction, MANIFEST_FILE,
};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_types::{
    AuthConfig, AutoReindexConfig, EmbeddingConfig, ExperimentalConfig, HttpConfig,
    MountBackendConfig, MountConfig, ResolvedServiceConfig, SecretRef, StdioMode, TransportMode,
};

const MOUNT_ID: &str = "wiki";
const MOUNT_AT: &str = "_Wiki";
const SECRET_ID: &str = "algolia-wiki";
const API_KEY: &str = "test-api-key-value";

/// A unique temp directory. `SystemTime` alone is not unique across concurrent tests on
/// macOS, so a counter disambiguates.
fn temp_dir(prefix: &str) -> PathBuf {
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "dob-algolia-cmd-{prefix}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// One test's world: a mock Algolia, a root vault with a `_Wiki/` folder in it, an algolia
/// mount covering that prefix, and a temp secrets file holding the key.
struct Fixture {
    base: PathBuf,
    base_url: String,
    index_name: String,
    resolver: SecretResolver,
    _mock: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(name: &str) -> Self {
        let base = temp_dir(name);
        let (base_url, handle) = spawn_mock().await;
        // The secrets file is per-fixture: the default path is process-global and would
        // race every other test that reads it.
        let resolver = SecretResolver::with_encrypted_file_path(base.join("secrets.json"));
        resolver
            .put(
                &SecretRef::EncryptedFile {
                    id: SECRET_ID.to_string(),
                },
                secrecy::SecretString::new(API_KEY.to_string()),
            )
            .expect("store the api key");
        std::fs::create_dir_all(base.join("root-vault").join(MOUNT_AT)).expect("vault dirs");
        Self {
            base,
            base_url,
            // Per-fixture index name so two tests never share a mock index.
            index_name: format!("wiki-{name}"),
            resolver,
            _mock: handle,
        }
    }

    fn vault(&self) -> PathBuf {
        self.base.join("root-vault")
    }

    /// The local folder the algolia mount shadows — the default seed source.
    fn shadowed(&self) -> PathBuf {
        self.vault().join(MOUNT_AT)
    }

    fn write_local(&self, relative: &str, content: &str) {
        let path = self.shadowed().join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
        std::fs::write(path, content).expect("write local note");
    }

    fn config(&self, writable: bool) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            federated_rerank: true,
            vault_path: self.vault(),
            mounts: vec![
                MountConfig {
                    recall_weight: None,
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: self.vault(),
                        index_dir: None,
                    },
                },
                MountConfig {
                    recall_weight: None,
                    id: MOUNT_ID.to_string(),
                    mount_at: MOUNT_AT.to_string(),
                    backend: MountBackendConfig::Algolia {
                        app_id: "TESTAPP".to_string(),
                        index_name: self.index_name.clone(),
                        api_key_ref: SecretRef::EncryptedFile {
                            id: SECRET_ID.to_string(),
                        },
                        base_url: Some(self.base_url.clone()),
                        writable,
                        participant_id: Some("tester@fixture".to_string()),
                        cache: None,
                        retention: None,
                        index_dir: None,
                    },
                },
            ],
            index_dir: self.base.join("index"),
            transport: TransportMode::Stdio,
            stdio_mode: StdioMode::Auto,
            http: HttpConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                mcp_path: "/mcp".to_string(),
                health_path: "/healthz".to_string(),
            },
            auto_reindex: AutoReindexConfig {
                enabled: false,
                debounce_ms: 0,
                interval_ms: 0,
            },
            embedding: EmbeddingConfig::default(),
            artifact_embedding: EmbeddingConfig::default(),
            experimental: ExperimentalConfig {
                multi_vault: true,
                algolia_vaults: true,
                ..ExperimentalConfig::default()
            },
            auth: AuthConfig::default(),
            config_file_path: None,
        }
    }

    /// Write the fixture's config to disk so the real binary can be pointed at it, and
    /// return the path.
    ///
    /// Only [`the argv test`](cli_argv_reaches_the_algolia_subcommands) needs this: every
    /// other test in this file drives the library functions directly, which is faster and
    /// lets it assert on typed reports instead of on rendered text.
    ///
    /// The secrets file has to travel too, and it cannot: `SecretResolver::new()` inside the
    /// child reads the default path. So the child is given the API key through
    /// `$DEEP_OBSIDIAN_ALGOLIA_API_KEY`, which is the documented override and takes
    /// precedence over `apiKeyRef` — the one credential path a subprocess can be handed
    /// without touching the user's keyring.
    fn write_config_file(&self, writable: bool) -> PathBuf {
        let path = self.base.join("config.json");
        let config = self.config(writable);
        // `vaultPath` is deliberately ABSENT: a persisted config may set it or `mounts`,
        // never both (`ConfigError::VaultPathAndMountsBothSet`), because both spell "where
        // the vault root is" and preferring one silently would let a user who added `mounts`
        // keep serving the old vault with no signal. The root mount carries the path.
        let persisted = serde_json::json!({
            "indexDir": config.index_dir,
            "transport": "stdio",
            "experimental": { "multiVault": true, "algoliaVaults": true },
            "mounts": config.mounts,
        });
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&persisted).expect("serialize config"),
        )
        .expect("write config");
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Every file under `root` as `relative -> bytes`, the manifest included.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, directory: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).expect("read dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .expect("under root")
                .components()
                .map(|component| component.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");
            out.insert(relative, std::fs::read(&path).expect("read file"));
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

// ---------------------------------------------------------------------------
// The round trip
// ---------------------------------------------------------------------------

/// THE property: seed a folder into the index, dump it back, and the bytes are identical.
///
/// This is the backup-and-exit story in one assertion. It also exercises the part of the
/// storage no unit test reaches — each note is split into chunk records on the way in and
/// reassembled from them on the way out — so a chunker that lost a line or a reassembler
/// that duplicated one fails here.
#[tokio::test]
async fn seed_then_dump_reproduces_the_source_bytes() {
    let fixture = Fixture::start("roundtrip").await;
    // Deliberately varied: a nested folder, headings the section chunker splits on, a body
    // long enough to produce several chunks, blank lines, and trailing whitespace shapes.
    let sources: Vec<(&str, String)> = vec![
        (
            "Alpha.md",
            "# Alpha\n\nfirst\n\n## Beta\nsecond line\n".to_string(),
        ),
        (
            "Decisions/Gamma.md",
            format!(
                "---\ntype: decision\nstatus: accepted\n---\n\n# Gamma\n\n{}\n\n## Detail\n{}\n",
                (1..40)
                    .map(|line| format!("body line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                (1..40)
                    .map(|line| format!("detail line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        ),
        (
            "NoTrailingNewline.md",
            "# Terse\n\njust one line".to_string(),
        ),
    ];
    for (path, content) in &sources {
        fixture.write_local(path, content);
    }

    let config = fixture.config(true);
    let seeded = seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed");
    assert!(seeded.first_import, "a virgin index is a first import");
    assert_eq!(seeded.created, sources.len());
    assert_eq!(seeded.updated, 0);
    assert!(seeded.skipped.is_empty(), "{:?}", seeded.skipped);

    let out = fixture.base.join("dump");
    let dumped = dump_with_resolver(&config, MOUNT_ID, &out, &fixture.resolver)
        .await
        .expect("dump");
    assert_eq!(dumped.notes, sources.len());
    // No chunk was lost or duplicated on the way through the index.
    assert!(
        dumped.hash_mismatches.is_empty(),
        "reassembly diverged from the recorded hashes: {:?}",
        dumped.hash_mismatches
    );
    assert!(dumped.divergent.is_empty(), "{:?}", dumped.divergent);

    for (path, content) in &sources {
        let round_tripped =
            std::fs::read_to_string(out.join(path)).unwrap_or_else(|_| panic!("dumped {path}"));
        assert_eq!(&round_tripped, content, "{path} did not round-trip");
    }

    // ...and a SECOND dump of the unchanged corpus is byte-identical to the first,
    // manifest included. That is what makes "dump, mutate, restore, dump, compare" a
    // verification rather than an approximation.
    let again = fixture.base.join("dump-again");
    let second = dump_with_resolver(&config, MOUNT_ID, &again, &fixture.resolver)
        .await
        .expect("second dump");
    assert_eq!(second.tree_hash, dumped.tree_hash);
    assert_eq!(tree(&out), tree(&again), "two dumps must be identical");

    // The manifest is well-formed, sorted, and names no connection detail.
    let manifest: DumpManifest =
        serde_json::from_str(&std::fs::read_to_string(out.join(MANIFEST_FILE)).expect("manifest"))
            .expect("parse manifest");
    assert_eq!(manifest.mount, MOUNT_ID);
    let paths: Vec<&str> = manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "manifest rows are ordered by path");
    for entry in &manifest.entries {
        assert!(!entry.version_id.is_empty(), "{entry:?}");
        assert!(!entry.hash_mismatch, "{entry:?}");
    }
}

/// A dump and a status report leak nothing: not the API key, not the app id, not the index
/// name, not the base URL. The same guarantee `couchdb export`'s manifest test pins.
#[tokio::test]
async fn dump_and_status_output_carry_no_secrets() {
    let fixture = Fixture::start("noleak").await;
    fixture.write_local("Alpha.md", "# Alpha\n\nbody\n");
    let config = fixture.config(true);
    seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed");

    let out = fixture.base.join("dump");
    let dumped = dump_with_resolver(&config, MOUNT_ID, &out, &fixture.resolver)
        .await
        .expect("dump");
    let status = status_with_resolver(&config, MOUNT_ID, &fixture.resolver)
        .await
        .expect("status");

    let manifest = std::fs::read_to_string(out.join(MANIFEST_FILE)).expect("manifest");
    let surfaces = [
        manifest,
        serde_json::to_string(&dumped).expect("dump json"),
        deep_obsidian_cli::algolia_cmd::render_dump_report(&dumped),
        serde_json::to_string(&status).expect("status json"),
        deep_obsidian_cli::algolia_cmd::render_status_report(&status),
    ];
    for surface in &surfaces {
        for forbidden in [API_KEY, "TESTAPP", fixture.index_name.as_str()] {
            assert!(
                !surface.contains(forbidden),
                "{forbidden:?} must not appear in:\n{surface}"
            );
        }
        // The mock's URL is a localhost origin, i.e. exactly the sort of connection detail
        // that must not travel with a report an operator pastes into an issue.
        assert!(
            !surface.contains(&fixture.base_url),
            "the base url must not appear in:\n{surface}"
        );
    }
    assert!(status.reachable);
    assert_eq!(status.notes, 1);
    assert!(status.writable);
    assert_eq!(status.mount_at, MOUNT_AT);
}

// ---------------------------------------------------------------------------
// Restore refusal semantics
// ---------------------------------------------------------------------------

/// Restore creates what is missing, skips what is identical, and REFUSES what differs
/// unless `--force`. The default cannot bury an edit made after the dump.
#[tokio::test]
async fn restore_creates_skips_and_refuses_drift_until_forced() {
    let fixture = Fixture::start("refusal").await;
    fixture.write_local("Alpha.md", "# Alpha\n\noriginal\n");
    fixture.write_local("Beta.md", "# Beta\n\nuntouched\n");
    let config = fixture.config(true);
    seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed");

    let snapshot = fixture.base.join("snapshot");
    dump_with_resolver(&config, MOUNT_ID, &snapshot, &fixture.resolver)
        .await
        .expect("dump");

    // Somebody edits Alpha in the corpus AFTER the snapshot was taken. That edit is what
    // the default must protect.
    std::fs::write(
        fixture.shadowed().join("Alpha.md"),
        "# Alpha\n\nedited by a colleague\n",
    )
    .expect("local edit");
    seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("re-seed the colleague's edit");
    // ...and a note the snapshot does not know about at all, to prove restore never deletes.
    fixture.write_local("Late.md", "# Late\n\nadded after the dump\n");
    seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed the late note");

    let refused = restore_with_resolver(
        &config,
        MOUNT_ID,
        &snapshot,
        false,
        false,
        &fixture.resolver,
    )
    .await
    .expect("restore");
    assert!(!refused.ok(), "a drifted note must make the report not-ok");
    assert_eq!(refused.refused, 1);
    assert_eq!(refused.unchanged, 1, "Beta was identical");
    assert_eq!(refused.created, 0);
    assert_eq!(refused.superseded, 0);
    let alpha = refused
        .outcomes
        .iter()
        .find(|outcome| outcome.path == "Alpha.md")
        .expect("Alpha in the report");
    assert_eq!(alpha.action, RestoreAction::RefusedDiffers);
    assert!(
        alpha
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("--force"),
        "{alpha:?}"
    );
    // The corpus still holds the colleague's edit: nothing was written.
    let check = fixture.base.join("check");
    dump_with_resolver(&config, MOUNT_ID, &check, &fixture.resolver)
        .await
        .expect("dump");
    assert_eq!(
        std::fs::read_to_string(check.join("Alpha.md")).expect("Alpha"),
        "# Alpha\n\nedited by a colleague\n"
    );
    // ...and the note the snapshot never contained is untouched. A restore writes and
    // skips; it never deletes by omission.
    assert!(check.join("Late.md").is_file());

    // `--force` supersedes. It does not destroy: the colleague's version is still in
    // history, which is exactly why the action is called `Superseded`.
    let forced =
        restore_with_resolver(&config, MOUNT_ID, &snapshot, false, true, &fixture.resolver)
            .await
            .expect("forced restore");
    assert!(forced.ok());
    assert_eq!(forced.superseded, 1);
    assert_eq!(forced.unchanged, 1);
    let after = fixture.base.join("after");
    dump_with_resolver(&config, MOUNT_ID, &after, &fixture.resolver)
        .await
        .expect("dump");
    assert_eq!(
        std::fs::read_to_string(after.join("Alpha.md")).expect("Alpha"),
        "# Alpha\n\noriginal\n"
    );
    assert!(after.join("Late.md").is_file(), "still never deleted");

    // A re-run is now idempotent: everything is identical and nothing is refused.
    let again = restore_with_resolver(
        &config,
        MOUNT_ID,
        &snapshot,
        false,
        false,
        &fixture.resolver,
    )
    .await
    .expect("idempotent restore");
    assert!(again.ok());
    assert_eq!(again.unchanged, 2);
    assert_eq!(again.created, 0);
}

/// A `--dry-run` restore reads and compares everything and writes nothing, and it works on
/// a READ-ONLY mount — so an operator can find out what a restore would do before turning
/// writes on.
#[tokio::test]
async fn a_dry_run_restore_writes_nothing_and_works_read_only() {
    let fixture = Fixture::start("dryrun").await;
    fixture.write_local("Alpha.md", "# Alpha\n\nv1\n");
    let writable = fixture.config(true);
    seed_with_resolver(&writable, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed");
    let snapshot = fixture.base.join("snapshot");
    dump_with_resolver(&writable, MOUNT_ID, &snapshot, &fixture.resolver)
        .await
        .expect("dump");
    // A note only the snapshot has: a dry run must report it as a CREATE without making one.
    std::fs::write(snapshot.join("Ghost.md"), "# Ghost\n").expect("write");

    let read_only = fixture.config(false);
    let planned = restore_with_resolver(
        &read_only,
        MOUNT_ID,
        &snapshot,
        true,
        false,
        &fixture.resolver,
    )
    .await
    .expect("a dry run works on a read-only mount");
    assert!(planned.dry_run);
    assert_eq!(planned.created, 1, "Ghost would be created");
    assert_eq!(planned.unchanged, 1);
    // Nothing landed.
    let check = fixture.base.join("check");
    dump_with_resolver(&writable, MOUNT_ID, &check, &fixture.resolver)
        .await
        .expect("dump");
    assert!(!check.join("Ghost.md").exists(), "a dry run wrote a note");

    // A REAL restore against the read-only mount is refused before anything is read, and
    // the refusal names the setting that lifts it.
    let error = restore_with_resolver(
        &read_only,
        MOUNT_ID,
        &snapshot,
        false,
        false,
        &fixture.resolver,
    )
    .await
    .expect_err("a read-only mount refuses a restore");
    assert!(error.to_string().contains("\"writable\": true"), "{error}");
}

/// A non-Markdown file in a restore tree is refused with the STORAGE's own message, and
/// `--force` does not lift it: nothing makes an Algolia index hold a PDF.
#[tokio::test]
async fn a_non_markdown_file_is_refused_even_with_force() {
    let fixture = Fixture::start("binary").await;
    let config = fixture.config(true);
    let snapshot = fixture.base.join("snapshot");
    std::fs::create_dir_all(snapshot.join("Assets")).expect("dirs");
    std::fs::write(snapshot.join("Alpha.md"), "# Alpha\n").expect("write");
    std::fs::write(snapshot.join("Assets").join("logo.png"), [0x89, 0x50]).expect("write");

    for force in [false, true] {
        let report =
            restore_with_resolver(&config, MOUNT_ID, &snapshot, true, force, &fixture.resolver)
                .await
                .expect("restore plans");
        let refusal = report
            .outcomes
            .iter()
            .find(|outcome| outcome.path == "Assets/logo.png")
            .expect("the png in the report");
        assert_eq!(
            refusal.action,
            RestoreAction::RefusedBinary,
            "--force={force} must not lift a storage-level refusal"
        );
        let reason = refusal.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("MARKDOWN ONLY"), "{reason}");
        // The Markdown note in the same tree is still planned normally: one refusal does
        // not abort the run.
        assert_eq!(report.created, 1);
    }
}

// ---------------------------------------------------------------------------
// Seed
// ---------------------------------------------------------------------------

/// A seed skips what the corpus cannot hold and what the author opted out of, imports the
/// rest, and is idempotent. Nothing is ever deleted from the index to match the folder.
#[tokio::test]
async fn a_seed_skips_binaries_and_opt_outs_and_is_idempotent() {
    let fixture = Fixture::start("skips").await;
    fixture.write_local("Public.md", "# Public\n\nshared\n");
    fixture.write_local("Private.md", "---\nshare: false\n---\n# Private\n");
    std::fs::write(fixture.shadowed().join("logo.png"), [0x89, 0x50]).expect("write");
    let config = fixture.config(true);

    let first = seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed");
    assert_eq!(first.created, 1, "only the public note");
    let skipped: BTreeMap<&str, SeedAction> = first
        .skipped
        .iter()
        .map(|item| (item.path.as_str(), item.action))
        .collect();
    assert_eq!(skipped.get("Private.md"), Some(&SeedAction::SkippedOptOut));
    assert_eq!(skipped.get("logo.png"), Some(&SeedAction::SkippedBinary));

    // Re-seeding an unchanged folder writes nothing.
    let second = seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("re-seed");
    assert!(!second.first_import, "the index is no longer virgin");
    assert_eq!(second.created, 0);
    assert_eq!(second.updated, 0);
    assert_eq!(second.unchanged, 1);

    // An edited note is an UPDATE, and the superseded version goes to history rather than
    // being destroyed.
    fixture.write_local("Public.md", "# Public\n\nrevised\n");
    let third = seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("update seed");
    assert_eq!(third.updated, 1);
    let status = status_with_resolver(&config, MOUNT_ID, &fixture.resolver)
        .await
        .expect("status");
    assert_eq!(status.notes, 1);
    assert!(
        status.superseded_versions >= 1,
        "the previous version must be in history: {status:?}"
    );
}

/// `--dry-run` on a seed reports the plan and imports nothing, on a read-only mount too.
#[tokio::test]
async fn a_dry_run_seed_imports_nothing() {
    let fixture = Fixture::start("seeddry").await;
    fixture.write_local("Alpha.md", "# Alpha\n");
    let read_only = fixture.config(false);
    let planned = seed_with_resolver(&read_only, MOUNT_ID, None, true, false, &fixture.resolver)
        .await
        .expect("a dry run works on a read-only mount");
    assert!(planned.dry_run && planned.first_import);
    assert_eq!(planned.created, 1);

    let status = status_with_resolver(&read_only, MOUNT_ID, &fixture.resolver)
        .await
        .expect("status");
    assert_eq!(status.notes, 0, "a dry run imported a note");

    let error = seed_with_resolver(&read_only, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect_err("a read-only mount refuses a seed");
    assert!(error.to_string().contains("\"writable\": true"), "{error}");
}

/// `--move` deletes each local original only after a FRESH read of the index confirms it
/// holds exactly those bytes, and prunes the emptied parents without touching the source
/// root.
///
/// # What this does not cover
///
/// The `kept_drifted` branch. Reaching it needs a file to change between the write and the
/// verification read, which is a two-writer race a single-threaded test cannot stage: any
/// local edit made before the call is simply imported by that same call, so the verification
/// then matches. The branch is straight-line code over a `BTreeMap` lookup, and the more
/// valuable assertion here is the one that IS made — that the verification reads the index
/// again rather than trusting the plan it computed before writing.
#[tokio::test]
async fn move_removes_verified_originals_and_prunes_emptied_parents() {
    let fixture = Fixture::start("move").await;
    fixture.write_local("Nested/Alpha.md", "# Alpha\n\nbody\n");
    fixture.write_local("Beta.md", "# Beta\n\nbody\n");
    let config = fixture.config(true);

    let report = seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed");
    assert_eq!(report.created, 2);
    // Beta is edited locally after that import. The `--move` run below must import the edit
    // FIRST and only then verify — if it verified against the plan it computed before
    // writing, or against the pre-edit hashes, Beta would be reported as drifted.
    std::fs::write(fixture.shadowed().join("Beta.md"), "# Beta\n\nEDITED\n").expect("local edit");

    let moved = seed_with_resolver(&config, MOUNT_ID, None, false, true, &fixture.resolver)
        .await
        .expect("seed --move");
    assert_eq!(moved.updated, 1, "the local edit was imported");
    assert!(
        moved.kept_drifted.is_empty(),
        "the verification read stale hashes: {:?}",
        moved.kept_drifted
    );
    assert_eq!(moved.moved_out.len(), 2, "{:?}", moved.moved_out);
    assert!(!fixture.shadowed().join("Beta.md").exists());
    assert!(
        !fixture.shadowed().join("Nested").exists(),
        "empty parent pruned"
    );
    // The source root itself survives: a `--move` that made the folder the user pointed at
    // vanish would look like the tool deleted more than it did.
    assert!(fixture.shadowed().is_dir());

    // The index is now the only copy, and it has both notes.
    let status = status_with_resolver(&config, MOUNT_ID, &fixture.resolver)
        .await
        .expect("status");
    assert_eq!(status.notes, 2);
}

// ---------------------------------------------------------------------------
// Retract
// ---------------------------------------------------------------------------

/// Retract destroys one note and its whole history, and leaves every other note intact.
#[tokio::test]
async fn retract_purges_one_note_and_its_history_only() {
    let fixture = Fixture::start("retract").await;
    fixture.write_local("Doomed.md", "# Doomed\n\nv1\n");
    fixture.write_local("Keeper.md", "# Keeper\n\nv1\n");
    let config = fixture.config(true);
    seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed");
    // Give both notes a history, so the purge has something to purge and the survivor has
    // something that must NOT be purged.
    for round in 2..=3 {
        fixture.write_local("Doomed.md", &format!("# Doomed\n\nv{round}\n"));
        fixture.write_local("Keeper.md", &format!("# Keeper\n\nv{round}\n"));
        seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
            .await
            .expect("re-seed");
    }
    let before = status_with_resolver(&config, MOUNT_ID, &fixture.resolver)
        .await
        .expect("status");
    assert_eq!(before.notes, 2);
    assert!(before.superseded_versions >= 4, "{before:?}");

    // A dry run reports what would go and removes nothing.
    let planned = retract_with_resolver(&config, MOUNT_ID, "Doomed.md", true, &fixture.resolver)
        .await
        .expect("dry run");
    assert!(planned.dry_run);
    assert_eq!(planned.versions_removed, 3, "head plus two superseded");
    assert_eq!(planned.head_participant_id, "tester@fixture");
    assert_eq!(
        status_with_resolver(&config, MOUNT_ID, &fixture.resolver)
            .await
            .expect("status")
            .notes,
        2,
        "a dry run retracted a note"
    );

    // The mounted form of the path addresses the same note, so a user pasting a path out
    // of `list_children` does not have to translate it.
    let report = retract_with_resolver(
        &config,
        MOUNT_ID,
        &format!("{MOUNT_AT}/Doomed.md"),
        false,
        &fixture.resolver,
    )
    .await
    .expect("retract");
    assert_eq!(report.path, "Doomed.md");
    assert!(!report.dry_run);

    let after = status_with_resolver(&config, MOUNT_ID, &fixture.resolver)
        .await
        .expect("status");
    assert_eq!(after.notes, 1, "only the keeper is left");
    // The keeper's history survived: the purge was keyed on ONE noteId.
    assert!(
        after.superseded_versions >= 2,
        "the survivor's history was destroyed too: {after:?}"
    );

    // Nothing of the retracted note remains: not the note, not its chunks, not a version.
    let out = fixture.base.join("dump");
    let dumped = dump_with_resolver(&config, MOUNT_ID, &out, &fixture.resolver)
        .await
        .expect("dump");
    assert_eq!(dumped.notes, 1);
    assert!(!out.join("Doomed.md").exists());
    assert_eq!(
        std::fs::read_to_string(out.join("Keeper.md")).expect("keeper"),
        "# Keeper\n\nv3\n"
    );

    // A second retraction of the same path reports absence rather than pretending.
    let error = retract_with_resolver(&config, MOUNT_ID, "Doomed.md", false, &fixture.resolver)
        .await
        .expect_err("a retracted note is gone");
    assert!(error.to_string().contains("no note at"), "{error}");
}

/// A read-only mount refuses a retraction, and the refusal names the setting.
#[tokio::test]
async fn a_read_only_mount_refuses_a_retraction() {
    let fixture = Fixture::start("retractro").await;
    fixture.write_local("Alpha.md", "# Alpha\n");
    seed_with_resolver(
        &fixture.config(true),
        MOUNT_ID,
        None,
        false,
        false,
        &fixture.resolver,
    )
    .await
    .expect("seed");
    let error = retract_with_resolver(
        &fixture.config(false),
        MOUNT_ID,
        "Alpha.md",
        false,
        &fixture.resolver,
    )
    .await
    .expect_err("a read-only mount refuses");
    assert!(error.to_string().contains("\"writable\": true"), "{error}");
    // A dry run on a read-only mount is still fine: it is a read.
    retract_with_resolver(
        &fixture.config(false),
        MOUNT_ID,
        "Alpha.md",
        true,
        &fixture.resolver,
    )
    .await
    .expect("a dry run is a read");
}

// ---------------------------------------------------------------------------
// Mount addressing
// ---------------------------------------------------------------------------

/// Naming a mount of the wrong kind, or one that does not exist, fails with a message that
/// lists what IS available — the same shape `couchdb export` uses.
#[tokio::test]
async fn a_wrong_or_missing_mount_is_named_along_with_the_alternatives() {
    let fixture = Fixture::start("addressing").await;
    let config = fixture.config(true);

    let error = status_with_resolver(&config, "vault", &fixture.resolver)
        .await
        .expect_err("the root mount is a filesystem one");
    assert!(error.to_string().contains("not an algolia one"), "{error}");

    let error = status_with_resolver(&config, "nope", &fixture.resolver)
        .await
        .expect_err("no such mount");
    let message = error.to_string();
    assert!(message.contains("no mount named"), "{message}");
    assert!(message.contains(MOUNT_ID), "{message}");
    assert!(message.contains("vault"), "{message}");
}

/// A status report on an UNREACHABLE mount describes the mount rather than failing like
/// one: that is the whole point of a status command.
#[tokio::test]
async fn status_reports_an_unreachable_mount_instead_of_failing() {
    let fixture = Fixture::start("unreachable").await;
    let mut config = fixture.config(true);
    if let Some(MountBackendConfig::Algolia { base_url, .. }) = config
        .mounts
        .iter_mut()
        .find(|mount| mount.id == MOUNT_ID)
        .map(|mount| &mut mount.backend)
    {
        // Port 1 on loopback: nothing listens, and the connection fails immediately.
        *base_url = Some("http://127.0.0.1:1/".to_string());
    }
    let status = status_with_resolver(&config, MOUNT_ID, &fixture.resolver)
        .await
        .expect("status describes a broken mount");
    assert!(!status.reachable);
    assert_eq!(status.notes, 0);
    assert!(!status.main_provisioned);
    let text = deep_obsidian_cli::algolia_cmd::render_status_report(&status);
    assert!(text.contains("reachable    NO"), "{text}");
    assert!(text.contains("apiKeyRef"), "{text}");
}

/// A mount whose API key secret is not stored degrades to a refusing stub, and the CLI says
/// so by naming the mount rather than reporting "not supported".
#[tokio::test]
async fn a_mount_with_no_stored_key_is_named_in_the_failure() {
    let fixture = Fixture::start("nokey").await;
    let mut config = fixture.config(true);
    if let Some(MountBackendConfig::Algolia { api_key_ref, .. }) = config
        .mounts
        .iter_mut()
        .find(|mount| mount.id == MOUNT_ID)
        .map(|mount| &mut mount.backend)
    {
        *api_key_ref = SecretRef::EncryptedFile {
            id: "never-stored".to_string(),
        };
    }
    let error = status_with_resolver(&config, MOUNT_ID, &fixture.resolver)
        .await
        .expect_err("a mount with no key cannot be inspected");
    let message = error.to_string();
    assert!(message.contains(MOUNT_ID), "{message}");
    assert!(message.contains("apiKeyRef"), "{message}");
    assert!(message.contains("doctor"), "{message}");
}

// ---------------------------------------------------------------------------
// argv
// ---------------------------------------------------------------------------

/// Run the real binary against a fixture's config file, returning
/// `(succeeded, stdout, stderr)`.
///
/// # Why `tokio::process` and not `std::process`
///
/// The mock Algolia server runs as a task on THIS test's runtime, and `#[tokio::test]`
/// gives a current-thread runtime. A blocking `std::process::Command::output()` therefore
/// parks the only thread the runtime has: the child's HTTP request reaches a server nobody
/// is polling, and the test deadlocks until it is killed. (Observed, not theorized.)
/// Awaiting `tokio::process` keeps the mock being driven while the child runs.
///
/// stdin is closed rather than inherited, so a command that PROMPTS — `algolia retract`
/// without `--yes` — reads EOF and takes the safe branch instead of blocking forever. That
/// is also what makes the abort path testable.
async fn run(config_path: &Path, args: &[&str]) -> (bool, String, String) {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .arg("--config")
        .arg(config_path)
        .args(args)
        // The documented override, and the only credential path a subprocess can be handed
        // without touching the user's real keyring or default secrets file.
        .env("DEEP_OBSIDIAN_ALGOLIA_API_KEY", API_KEY)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("run the binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// The `algolia` subcommands are reachable **through the real binary's argv**.
///
/// # Why this test exists at all
///
/// Every other test in this file calls `seed_with_resolver` / `status_with_resolver` and
/// friends directly, and so does the couchdb suite. That is a genuine blind spot, and it had
/// already cost something: `couchdb export` was UNREACHABLE from the command line from the
/// commit that introduced it, because `couchdb` was missing from `normalize_cli_args`'s
/// known-command list and got promoted to `--vault couchdb` — after which clap rejected
/// `export` as an unrecognized TOP-LEVEL subcommand. Every library-level test passed
/// throughout, because none of them went through argv.
///
/// `normalize_cli_args` now has unit tests, which is the right layer for the logic. This is
/// the layer that proves the wiring: clap's derive, the normalizer, the dispatch in
/// `commands::run`, and the `--json` plumbing, all in one process the way a user runs it.
///
/// One test rather than one per subcommand: what can break here is the argv plumbing, which
/// is shared, and spawning a process per subcommand would pay for the same coverage six
/// times.
#[tokio::test]
async fn cli_argv_reaches_the_algolia_subcommands() {
    let fixture = Fixture::start("argv").await;
    fixture.write_local("Alpha.md", "# Alpha\n\nbody\n");
    let config = fixture.config(true);
    seed_with_resolver(&config, MOUNT_ID, None, false, false, &fixture.resolver)
        .await
        .expect("seed a note to look at");

    let config_path = fixture.write_config_file(true);

    // `status`: the subcommand is reached, the mount is addressed by id, and the report is
    // the rendered one rather than a clap error.
    let (ok, stdout, stderr) = run(&config_path, &["algolia", "status", "--mount", MOUNT_ID]).await;
    assert!(
        ok,
        "algolia status failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains(&format!("mount '{MOUNT_ID}'")), "{stdout}");
    assert!(stdout.contains("notes        1"), "{stdout}");
    assert!(stdout.contains("reachable    yes"), "{stdout}");
    // ...and it still leaks nothing, at the layer a user actually sees.
    for forbidden in [API_KEY, "TESTAPP", fixture.index_name.as_str()] {
        assert!(!stdout.contains(forbidden), "{forbidden} in {stdout}");
    }

    // `--json` reaches the same command and switches the rendering.
    let (ok, stdout, stderr) = run(
        &config_path,
        &["algolia", "status", "--mount", MOUNT_ID, "--json"],
    )
    .await;
    assert!(ok, "algolia status --json failed\nstderr: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json report");
    assert_eq!(parsed["mount"], MOUNT_ID);
    assert_eq!(parsed["notes"], 1);

    // A value flag whose value is a PATH survives normalization — the case that was broken
    // for `--out` and `--from` as well as `--mount`.
    let out = fixture.base.join("argv-dump");
    let (ok, stdout, stderr) = run(
        &config_path,
        &[
            "algolia",
            "dump",
            "--mount",
            MOUNT_ID,
            "--out",
            out.to_str().expect("utf-8 path"),
        ],
    )
    .await;
    assert!(
        ok,
        "algolia dump failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(out.join("Alpha.md").is_file(), "{stdout}");
    assert!(out.join(MANIFEST_FILE).is_file(), "{stdout}");

    // A `--dry-run` restore is reachable and writes nothing.
    let (ok, stdout, stderr) = run(
        &config_path,
        &[
            "algolia",
            "restore",
            "--mount",
            MOUNT_ID,
            "--from",
            out.to_str().expect("utf-8 path"),
            "--dry-run",
        ],
    )
    .await;
    assert!(
        ok,
        "algolia restore failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("would restore"), "{stdout}");

    // `retract` without `--yes` PROMPTS: with stdin closed the answer is not a yes, so it
    // must abort rather than destroy anything. This is the one place the confirmation gate
    // can be tested at all, and it is worth testing — it guards the only destructive
    // operation in the family.
    let (ok, stdout, _) = run(
        &config_path,
        &[
            "algolia", "retract", "--mount", MOUNT_ID, "--path", "Alpha.md",
        ],
    )
    .await;
    assert!(ok, "an aborted retraction is not a failure: {stdout}");
    assert!(stdout.contains("aborted"), "{stdout}");
    let (_, stdout, _) = run(&config_path, &["algolia", "status", "--mount", MOUNT_ID]).await;
    assert!(
        stdout.contains("notes        1"),
        "the note was retracted anyway: {stdout}"
    );

    // A bad `--parent-key-ref` is rejected by the parser, not by a panic, and the message
    // names the accepted forms.
    let (ok, _, stderr) = run(
        &config_path,
        &[
            "algolia",
            "key",
            "--mount",
            MOUNT_ID,
            "--parent-key-ref",
            "nonsense",
        ],
    )
    .await;
    assert!(!ok, "a bad parent key reference must fail");
    assert!(stderr.contains("keyring:"), "{stderr}");

    // And the sibling family is reachable too — the bug this test was written for was
    // `couchdb`'s, and a regression there must not be invisible from here.
    let (ok, _, stderr) = run(
        &config_path,
        &["couchdb", "export", "--mount", "nope", "--out", "/tmp/x"],
    )
    .await;
    assert!(!ok, "no such mount must fail");
    assert!(
        stderr.contains("no mount named") || stderr.contains("nope"),
        "couchdb export did not reach its own argument parsing: {stderr}"
    );
}
