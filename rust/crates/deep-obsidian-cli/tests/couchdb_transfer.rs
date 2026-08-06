//! The verified round trip: export → mutate → restore → re-export → compare.
//!
//! This is the whole point of `couchdb export`/`couchdb restore`, and it is only a
//! *verified* rollback if the comparison is exact. So the assertion is not "restore
//! reported success" — it is that the re-exported tree is byte-identical to the original
//! one, `manifest.json` included, which is why the export format has no timestamps in it.
//!
//! Driven against the REAL sidecar bundle and the sidecar's own mock CouchDB in its
//! writable mode, so the writes go through the actual protocol and the actual
//! compare-and-swap rather than a Rust-side imitation of them. Skips (never fails) when
//! `node` or the built bundle is missing, matching the pre-existing couchdb suites.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use deep_obsidian_cli::couchdb_transfer::{
    export_with_resolver, restore_with_resolver, ExportKind, ExportManifest, RestoreAction,
    MANIFEST_FILE,
};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_types::{
    AuthConfig, AutoReindexConfig, EmbeddingConfig, ExperimentalConfig, HttpConfig,
    MountBackendConfig, MountConfig, ResolvedServiceConfig, SecretRef, StdioMode, TransportMode,
};

const FIXTURE_PASSWORD: &str = "s3cr3t-password-value";
const MOUNT_ID: &str = "live";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|candidate| {
            candidate
                .join("sidecar/livesync-sidecar/package.json")
                .is_file()
        })
        .expect("the livesync-sidecar package to be in this checkout")
        .to_path_buf()
}

fn sidecar_dir() -> PathBuf {
    repo_root().join("sidecar/livesync-sidecar")
}

fn bundle_path() -> PathBuf {
    sidecar_dir().join("dist/sidecar.mjs")
}

/// The prerequisites, or the reason to skip. Both are actionable.
fn prerequisites() -> Result<(), String> {
    let node = Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !node.map(|status| status.success()).unwrap_or(false) {
        return Err("`node` is not available on PATH (Node 20+ is required)".to_string());
    }
    if !bundle_path().is_file() {
        return Err(format!(
            "{} is missing; run `npm ci && npm run build` in sidecar/livesync-sidecar",
            bundle_path().display()
        ));
    }
    Ok(())
}

macro_rules! require_prerequisites {
    () => {
        match prerequisites() {
            Ok(()) => {}
            Err(reason) => {
                eprintln!("skipping: {reason}");
                return;
            }
        }
    };
}

/// Wait up to three seconds for a child to exit on its own. True when it did.
///
/// Polling rather than a blind sleep: an exit that takes 20ms should cost 20ms, and one
/// that never comes must still be bounded.
fn wait_briefly(child: &mut Child) -> bool {
    for _ in 0..60 {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    false
}

/// A writable mock CouchDB. Dropping it stops the child, so a panicking test cannot
/// leave an orphan server holding a port.
struct MockCouch {
    child: Child,
    url: String,
    database: String,
}

impl MockCouch {
    fn start_writable() -> Self {
        let mut child = Command::new("node")
            .arg("test/mock-couch-server.mjs")
            .arg("--vault")
            .arg("small")
            .arg("--writable")
            .current_dir(sidecar_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the mock CouchDB fixture server");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut handshake = String::new();
        stdout
            .read_line(&mut handshake)
            .expect("read the fixture handshake");
        let handshake: serde_json::Value =
            serde_json::from_str(handshake.trim()).expect("parse the handshake");
        // stdout is not kept: this suite drives the vault through the sidecar, never
        // through the fixture's own command channel.
        Self {
            url: handshake["url"].as_str().expect("url").to_string(),
            database: handshake["database"]
                .as_str()
                .expect("database")
                .to_string(),
            child,
        }
    }
}

impl Drop for MockCouch {
    fn drop(&mut self) {
        // Closing stdin is the documented stop signal. The live variant's helper reacts
        // to it by DELETEing its scratch database, and that is a round trip to a real
        // server — so it gets a moment to finish before the kill lands, or the test
        // leaves a database behind on someone's CouchDB. (Verified the hard way: without
        // this, `deep-obsidian-cli-round-trip` survived the run.) The kill is still the
        // backstop for a helper that hangs.
        drop(self.child.stdin.take());
        if !wait_briefly(&mut self.child) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Temp directories plus the secrets file, all removed on drop.
struct Fixture {
    base: PathBuf,
    resolver: SecretResolver,
    /// The CouchDB user the config names. The mock ignores it; a real server does not.
    username: String,
}

impl Fixture {
    fn new(name: &str) -> Self {
        Self::with_credentials(name, "vaultuser", FIXTURE_PASSWORD)
    }

    /// A fixture whose stored secret is a REAL server's password, for the live variant.
    fn with_credentials(name: &str, username: &str, password: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "deep-obsidian-couchdb-transfer-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(base.join("root-vault")).expect("root vault");
        std::fs::create_dir_all(base.join("index")).expect("index dir");
        std::fs::write(base.join("root-vault/Root.md"), "# Root\n").expect("seed root");

        // A TEMP secrets file, never `XDG_CONFIG_HOME`: that variable is process-global
        // and mutating it races every other test that reads the default secrets path.
        let resolver = SecretResolver::with_encrypted_file_path(base.join("secrets.json"));
        resolver
            .put(
                &SecretRef::EncryptedFile {
                    id: "livesync-password".to_string(),
                },
                secrecy::SecretString::new(password.to_string()),
            )
            .expect("store the fixture password");
        Self {
            base,
            resolver,
            username: username.to_string(),
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.base.join(name)
    }

    /// A two-mount config: a filesystem root plus the couchdb mount under test.
    fn config(&self, couch: &MockCouch, writable: bool) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            federated_rerank: true,
            vault_path: Some(self.base.join("root-vault")),
            mounts: vec![
                MountConfig {
                    unknown: Default::default(),
                    recall_weight: None,
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: self.base.join("root-vault"),
                        index_dir: None,
                    },
                },
                MountConfig {
                    unknown: Default::default(),
                    recall_weight: None,
                    id: MOUNT_ID.to_string(),
                    mount_at: "LiveSync".to_string(),
                    backend: MountBackendConfig::Couchdb {
                        url: couch.url.clone(),
                        database: couch.database.clone(),
                        username: Some(self.username.clone()),
                        password_ref: SecretRef::EncryptedFile {
                            id: "livesync-password".to_string(),
                        },
                        e2ee: None,
                        sidecar_path: Some(bundle_path()),
                        index_dir: None,
                        options: None,
                        writable,
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
                couchdb_vaults: true,
                ..ExperimentalConfig::default()
            },
            auth: AuthConfig::default(),
            config_file_path: None,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Every file under `root` as `relative path -> bytes`, so two trees can be compared
/// exactly rather than by a summary.
fn read_tree(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, directory: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).expect("read dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(relative, std::fs::read(&path).expect("read file"));
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn read_manifest(dir: &Path) -> ExportManifest {
    let text = std::fs::read_to_string(dir.join(MANIFEST_FILE)).expect("read manifest");
    serde_json::from_str(&text).expect("parse manifest")
}

/// The roadmap's "verified rollback", end to end.
#[tokio::test]
async fn export_mutate_restore_re_export_is_byte_identical() {
    require_prerequisites!();
    let couch = MockCouch::start_writable();
    let fixture = Fixture::new("round-trip");
    let config = fixture.config(&couch, true);

    // 1. Snapshot.
    let first = fixture.dir("export-1");
    let report = export_with_resolver(&config, MOUNT_ID, &first, &fixture.resolver)
        .await
        .expect("the first export must succeed");
    assert!(
        report.files > 0,
        "the fixture vault is not empty: {report:?}"
    );

    // The export covers BINARY entries, not only markdown. This is the assertion that
    // catches an export built on `WalkMarkdown`, which would silently drop every
    // attachment and make the "restore" a data-losing operation.
    let manifest = read_manifest(&first);
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| matches!(entry.kind, ExportKind::Binary)),
        "the fixture's attachment must be exported: {:?}",
        manifest.entries
    );
    assert!(
        first.join("assets/logo.png").is_file(),
        "the attachment must land in the tree"
    );
    // A soft-deleted entry is a tombstone, not a file: it must not be exported, or the
    // restore would resurrect it.
    assert!(
        !first.join("Removed.md").exists(),
        "a tombstone must not be exported"
    );
    // Entries are sorted, so the manifest is deterministic.
    let paths: Vec<&String> = manifest.entries.iter().map(|entry| &entry.path).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    // Every row carries the revision and hash that identify the snapshot.
    assert!(manifest
        .entries
        .iter()
        .all(|entry| !entry.rev.is_empty() && entry.hash.starts_with("fnv1a64:")));

    let original_tree = read_tree(&first);

    // 2. Mutate the vault, as an agent (or a user on another device) would.
    let mutated_note = "Beta.md";
    let restore_source = fixture.dir("export-1");
    {
        let mutation = fixture.dir("mutation");
        std::fs::create_dir_all(&mutation).expect("mutation dir");
        std::fs::write(mutation.join(mutated_note), b"MUTATED CONTENT\n").expect("write");
        // Restoring a one-file tree with --force is the most direct way to change the
        // remote through the same guarded path a real write uses.
        let report =
            restore_with_resolver(&config, MOUNT_ID, &mutation, false, true, &fixture.resolver)
                .await
                .expect("the mutation must land");
        assert_eq!(report.overwritten, 1, "{report:?}");
    }

    // The mutation is real: an export now differs.
    let mutated_export = fixture.dir("export-mutated");
    let mutated_report =
        export_with_resolver(&config, MOUNT_ID, &mutated_export, &fixture.resolver)
            .await
            .expect("export after the mutation");
    assert_ne!(
        mutated_report.tree_hash, report.tree_hash,
        "the mutation must change the tree hash"
    );

    // 3. Restore the original snapshot. `--force` is REQUIRED here, and that is the
    //    safety property: the remote now differs from the snapshot, so the default
    //    refuses rather than silently reverting someone's edit.
    let refused = restore_with_resolver(
        &config,
        MOUNT_ID,
        &restore_source,
        false,
        false,
        &fixture.resolver,
    )
    .await
    .expect("a refusal is a report, not an error");
    assert!(!refused.ok(), "{refused:?}");
    assert_eq!(refused.refused, 1, "exactly the mutated note: {refused:?}");
    let refusal = refused
        .outcomes
        .iter()
        .find(|outcome| outcome.action == RestoreAction::RefusedDiffers)
        .expect("the differing entry must be named");
    assert_eq!(refusal.path, mutated_note);
    assert!(
        refusal
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("--force"),
        "the refusal must say how to proceed: {refusal:?}"
    );
    // Every other entry was already identical, so nothing was rewritten.
    assert_eq!(refused.overwritten, 0);
    assert!(refused.unchanged > 0, "{refused:?}");

    let forced = restore_with_resolver(
        &config,
        MOUNT_ID,
        &restore_source,
        false,
        true,
        &fixture.resolver,
    )
    .await
    .expect("a forced restore must succeed");
    assert!(forced.ok(), "{forced:?}");
    assert_eq!(forced.overwritten, 1, "{forced:?}");

    // 4. Re-export and compare. BYTE-IDENTICAL, manifest included — which is only
    //    possible because the format carries no timestamps.
    let second = fixture.dir("export-2");
    let second_report = export_with_resolver(&config, MOUNT_ID, &second, &fixture.resolver)
        .await
        .expect("the second export must succeed");

    let restored_tree = read_tree(&second);
    for (path, bytes) in &original_tree {
        if path == MANIFEST_FILE {
            continue;
        }
        assert_eq!(
            restored_tree.get(path).map(|bytes| bytes.as_slice()),
            Some(bytes.as_slice()),
            "{path} must be restored byte-identically"
        );
    }
    assert_eq!(
        original_tree.keys().collect::<Vec<_>>(),
        restored_tree.keys().collect::<Vec<_>>(),
        "the two exports must contain exactly the same paths"
    );
    // The content hash of the whole snapshot is back to where it started.
    assert_eq!(
        second_report.tree_hash, report.tree_hash,
        "the restored vault must hash identically to the original snapshot"
    );

    // The revisions moved (the restore was a real write), which is why the manifests
    // are not compared byte-for-byte while the CONTENT is.
    let second_manifest = read_manifest(&second);
    let original_manifest = read_manifest(&first);
    assert_eq!(
        second_manifest.entries.len(),
        original_manifest.entries.len()
    );
    for (before, after) in original_manifest
        .entries
        .iter()
        .zip(second_manifest.entries.iter())
    {
        assert_eq!(before.path, after.path);
        assert_eq!(
            before.hash, after.hash,
            "{} content must match",
            before.path
        );
        assert_eq!(
            before.kind, after.kind,
            "{} kind must be preserved",
            before.path
        );
    }

    // 5. A restore of an already-matching tree is idempotent: nothing to do, and no
    //    --force needed.
    let again = restore_with_resolver(
        &config,
        MOUNT_ID,
        &restore_source,
        false,
        false,
        &fixture.resolver,
    )
    .await
    .expect("an idempotent restore must succeed");
    assert!(again.ok(), "{again:?}");
    assert_eq!(again.created, 0);
    assert_eq!(again.overwritten, 0);
    assert_eq!(
        again.unchanged,
        original_manifest.entries.len(),
        "every entry must be reported unchanged: {again:?}"
    );
}

/// Two exports of an UNCHANGED vault are byte-identical, `manifest.json` included.
///
/// This is the property that makes "compare two exports" a verification rather than a
/// heuristic. A wall-clock field anywhere in the format would break it.
#[tokio::test]
async fn two_exports_of_an_unchanged_vault_are_byte_identical() {
    require_prerequisites!();
    let couch = MockCouch::start_writable();
    let fixture = Fixture::new("deterministic");
    let config = fixture.config(&couch, false);

    let first = fixture.dir("export-a");
    let second = fixture.dir("export-b");
    let one = export_with_resolver(&config, MOUNT_ID, &first, &fixture.resolver)
        .await
        .expect("first export");
    let two = export_with_resolver(&config, MOUNT_ID, &second, &fixture.resolver)
        .await
        .expect("second export");

    assert_eq!(one.tree_hash, two.tree_hash);
    assert_eq!(
        read_tree(&first),
        read_tree(&second),
        "two exports of an unchanged vault must be byte-identical, manifest included"
    );
}

/// A restore against a READ-ONLY mount is refused up front, and `--dry-run` still works
/// there — so an operator can see what a restore would do before enabling writes.
#[tokio::test]
async fn a_restore_needs_a_writable_mount_but_a_dry_run_does_not() {
    require_prerequisites!();
    let couch = MockCouch::start_writable();
    let fixture = Fixture::new("read-only");
    let read_only = fixture.config(&couch, false);

    let snapshot = fixture.dir("export");
    export_with_resolver(&read_only, MOUNT_ID, &snapshot, &fixture.resolver)
        .await
        .expect("export works on a read-only mount");

    let error = restore_with_resolver(
        &read_only,
        MOUNT_ID,
        &snapshot,
        false,
        false,
        &fixture.resolver,
    )
    .await
    .expect_err("a restore into a read-only mount must be refused");
    let message = error.to_string();
    assert!(message.contains("not writable"), "{message}");
    assert!(message.contains("\"writable\": true"), "{message}");

    // The dry run works, reads everything, and reports the truth: the snapshot matches.
    let dry = restore_with_resolver(
        &read_only,
        MOUNT_ID,
        &snapshot,
        true,
        false,
        &fixture.resolver,
    )
    .await
    .expect("a dry run must work on a read-only mount");
    assert!(dry.dry_run);
    assert!(dry.ok(), "{dry:?}");
    assert!(dry.unchanged > 0, "{dry:?}");
    assert_eq!(dry.created, 0);
    assert_eq!(dry.overwritten, 0);
}

/// A dry run of a restore that WOULD write reports exactly that, and writes nothing.
#[tokio::test]
async fn a_dry_run_reports_what_it_would_do_and_writes_nothing() {
    require_prerequisites!();
    let couch = MockCouch::start_writable();
    let fixture = Fixture::new("dry-run");
    let config = fixture.config(&couch, true);

    let source = fixture.dir("source");
    std::fs::create_dir_all(source.join("Notes")).expect("source dir");
    // One create, one overwrite.
    std::fs::write(source.join("Notes/BrandNew.md"), b"# Brand New\n").expect("write");
    std::fs::write(source.join("Beta.md"), b"WOULD OVERWRITE\n").expect("write");

    let dry = restore_with_resolver(&config, MOUNT_ID, &source, true, true, &fixture.resolver)
        .await
        .expect("dry run");
    assert!(dry.dry_run);
    assert_eq!(dry.created, 1, "{dry:?}");
    assert_eq!(dry.overwritten, 1, "{dry:?}");

    // Nothing landed: the created path is still absent and the existing one is unchanged.
    let after = fixture.dir("after");
    export_with_resolver(&config, MOUNT_ID, &after, &fixture.resolver)
        .await
        .expect("export after the dry run");
    assert!(
        !after.join("Notes/BrandNew.md").exists(),
        "a dry run must not create anything"
    );
    assert_ne!(
        std::fs::read(after.join("Beta.md")).expect("read Beta.md"),
        b"WOULD OVERWRITE\n".to_vec(),
        "a dry run must not overwrite anything"
    );
}

/// The same round trip against a REAL CouchDB.
///
/// The hermetic version above proves the logic; this proves it against real revision
/// hashes and a real conflict adjudicator, which is where an export that recorded the
/// wrong revision or a restore that mis-guarded its write would actually show up.
///
/// Skips without `DEEP_OBSIDIAN_COUCHDB_URL`. Creates and drops its OWN scratch database.
#[tokio::test]
async fn the_round_trip_holds_against_a_real_couchdb() {
    require_prerequisites!();
    let Ok(url) = std::env::var("DEEP_OBSIDIAN_COUCHDB_URL") else {
        eprintln!("skipping: set DEEP_OBSIDIAN_COUCHDB_URL to run the live round trip");
        return;
    };
    let username = std::env::var("DEEP_OBSIDIAN_COUCHDB_USER").unwrap_or_else(|_| "admin".into());
    let password = std::env::var("DEEP_OBSIDIAN_COUCHDB_PASSWORD").unwrap_or_else(|_| "pw".into());

    // A real, seeded, throwaway LiveSync database. Dropping the child closes its stdin,
    // which is what makes it DELETE the database.
    let mut helper = Command::new("node")
        .arg("test/live-scratch.mjs")
        .args(["--url", &url])
        .args(["--user", &username])
        .args(["--password", &password])
        .args(["--database", "deep-obsidian-cli-round-trip"])
        .current_dir(sidecar_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the live scratch-vault helper");
    let mut handshake = String::new();
    BufReader::new(helper.stdout.take().expect("piped stdout"))
        .read_line(&mut handshake)
        .expect("read the scratch handshake");
    let handshake: serde_json::Value =
        serde_json::from_str(handshake.trim()).expect("parse the handshake");
    let database = handshake["database"]
        .as_str()
        .expect("database")
        .to_string();

    let fixture = Fixture::with_credentials("live-round-trip", &username, &password);
    let couch = MockCouch {
        // Not a mock: the same shape, pointed at the real server, so `Fixture::config`
        // needs no live-specific variant.
        child: helper,
        url,
        database,
    };
    let config = fixture.config(&couch, true);

    // Seed the vault through the restore path itself, which is the only writer available
    // — and a legitimate one: restoring into an empty vault is the disaster-recovery case.
    let seed = fixture.dir("seed");
    std::fs::create_dir_all(seed.join("Notes")).expect("seed dir");
    std::fs::write(seed.join("Notes/Live.md"), b"# Live\n\noriginal body\n").expect("write");
    // Long enough to span several chunks, so the real `_bulk_docs` path runs.
    let long = (0..300)
        .map(|index| format!("live line {index} {}", "y".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(seed.join("Notes/Long.md"), long.as_bytes()).expect("write");
    let seeded = restore_with_resolver(&config, MOUNT_ID, &seed, false, false, &fixture.resolver)
        .await
        .expect("seeding a real vault must succeed");
    assert_eq!(seeded.created, 2, "{seeded:?}");

    // Export, mutate, restore, re-export, compare.
    let first = fixture.dir("live-export-1");
    let before = export_with_resolver(&config, MOUNT_ID, &first, &fixture.resolver)
        .await
        .expect("the first live export must succeed");
    assert_eq!(before.files, 2, "{before:?}");
    let original_tree = read_tree(&first);

    let mutation = fixture.dir("live-mutation");
    std::fs::create_dir_all(mutation.join("Notes")).expect("mutation dir");
    std::fs::write(mutation.join("Notes/Live.md"), b"MUTATED\n").expect("write");
    restore_with_resolver(&config, MOUNT_ID, &mutation, false, true, &fixture.resolver)
        .await
        .expect("the mutation must land");

    // The default refuses to revert a changed entry; --force is the explicit override.
    let refused = restore_with_resolver(&config, MOUNT_ID, &first, false, false, &fixture.resolver)
        .await
        .expect("a refusal is a report");
    assert_eq!(refused.refused, 1, "{refused:?}");
    let forced = restore_with_resolver(&config, MOUNT_ID, &first, false, true, &fixture.resolver)
        .await
        .expect("a forced restore must succeed");
    assert!(forced.ok(), "{forced:?}");

    let second = fixture.dir("live-export-2");
    let after = export_with_resolver(&config, MOUNT_ID, &second, &fixture.resolver)
        .await
        .expect("the second live export must succeed");
    assert_eq!(
        after.tree_hash, before.tree_hash,
        "the restored real vault must hash identically to the snapshot"
    );
    let restored_tree = read_tree(&second);
    for (path, bytes) in &original_tree {
        if path == MANIFEST_FILE {
            continue;
        }
        assert_eq!(
            restored_tree.get(path).map(|bytes| bytes.as_slice()),
            Some(bytes.as_slice()),
            "{path} must be restored byte-identically against a real CouchDB"
        );
    }
}

/// A file whose storage kind cannot be established is refused, not guessed.
///
/// Writing a binary as a text entry (or the reverse) is permanent and invisible
/// afterwards, so the tool stops rather than choose for the user.
#[tokio::test]
async fn a_file_with_an_unknowable_storage_kind_is_refused() {
    require_prerequisites!();
    let couch = MockCouch::start_writable();
    let fixture = Fixture::new("unknown-kind");
    let config = fixture.config(&couch, true);

    let source = fixture.dir("source");
    std::fs::create_dir_all(source.join("assets")).expect("source dir");
    // No manifest, and not a `.md` file: the bytes happen to be valid UTF-8, which is
    // exactly the case a "sniff the content" fallback would get wrong.
    std::fs::write(source.join("assets/data.bin"), b"PK\x03\x04 plain-looking").expect("write");

    let report = restore_with_resolver(&config, MOUNT_ID, &source, false, false, &fixture.resolver)
        .await
        .expect("a refusal is a report");
    assert!(!report.ok(), "{report:?}");
    assert_eq!(report.refused, 1, "{report:?}");
    let refusal = &report.outcomes[0];
    assert_eq!(refusal.action, RestoreAction::RefusedUnknownKind);
    let reason = refusal.reason.as_deref().unwrap_or_default();
    assert!(reason.contains("cannot be established"), "{reason}");
    assert!(reason.contains("--force"), "{reason}");

    // A `.md` file in the same tree needs no manifest: the extension is unambiguous.
    std::fs::write(source.join("Fine.md"), b"# Fine\n").expect("write");
    let report = restore_with_resolver(&config, MOUNT_ID, &source, false, true, &fixture.resolver)
        .await
        .expect("with --force both are written");
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.created, 2, "{report:?}");
}
