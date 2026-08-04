//! End-to-end: the REAL sidecar bundle against the REAL mock CouchDB fixture.
//!
//! # Why this is not `#[ignore]`
//!
//! Nothing here reaches the network or a developer's machine state: the fixture
//! CouchDB is a local HTTP server on an ephemeral port, started as a child process
//! from the sidecar's own test utilities. The only external requirement is a Node
//! runtime and a built bundle, and both are checked at the top of each test — a
//! missing one SKIPS with a message naming exactly what to do, the same shape as the
//! pre-existing ripgrep gate. `#[ignore]` would have meant these never ran by
//! default, which for the one suite that exercises the actual protocol against the
//! actual sidecar is the wrong default.
//!
//! # Why the mock is reused rather than reimplemented
//!
//! `test/mock-couch.mjs`'s endpoint set was discovered empirically against
//! upstream's real request shapes (PouchDB's HTTP adapter, `_bulk_get`, `_changes`
//! long-poll). A second hand-written emulator in Rust would drift from it silently,
//! and at that point these tests would be asserting against a fiction rather than
//! against LiveSync's storage format. `test/mock-couch-server.mjs` (npm script
//! `mock-couch`) exposes it over a pipe so a non-Node parent can drive it.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use deep_obsidian_backend::sidecar::{
    CompatibilityStatus, SidecarConfig, SidecarCredentials, SidecarError, SidecarLaunch,
    SidecarSupervisor,
};
use deep_obsidian_backend::{
    BackendRequest, Capability, CouchDbVaultBackend, ManifestRequest, MutationRequest,
    RecallRequest, VaultBackend, COUCHDB_GREP_UNSUPPORTED_MESSAGE, COUCHDB_READ_ONLY_MESSAGE,
};
use secrecy::SecretString;

/// The password the fixture's mock CouchDB accepts. Any value works (the mock does
/// not check it unless `--auth-status` is set); a distinctive one makes the
/// "no secret leaked" assertions meaningful.
const FIXTURE_PASSWORD: &str = "s3cr3t-password-value";

/// The sidecar package directory.
fn sidecar_dir() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is this crate; the sidecar lives at the repo root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|candidate| {
            candidate
                .join("sidecar/livesync-sidecar/package.json")
                .is_file()
        })
        .map(|root| root.join("sidecar/livesync-sidecar"))
        .expect("the livesync-sidecar package to be in this checkout")
}

fn bundle_path() -> PathBuf {
    sidecar_dir().join("dist/sidecar.mjs")
}

/// The prerequisites, or a message saying what is missing.
///
/// Both are actionable: `node` is a documented requirement of this backend, and the
/// bundle is one `npm run build` away.
fn prerequisites() -> Result<(), String> {
    let node = Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !node.map(|status| status.success()).unwrap_or(false) {
        return Err(
            "`node` is not available on PATH (Node 20+ is required by the livesync \
                    sidecar backend)"
                .to_string(),
        );
    }
    if !bundle_path().is_file() {
        return Err(format!(
            "{} is missing; run `npm ci && npm run build` in sidecar/livesync-sidecar",
            bundle_path().display()
        ));
    }
    Ok(())
}

/// Skip the current test with a clear reason, returning `true` when it must not run.
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

/// A running mock CouchDB, driven over its stdio protocol.
///
/// Dropping it closes stdin, which is the child's stop signal, and then kills it —
/// so a panicking test cannot leave an orphan HTTP server holding a port.
struct MockCouch {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
    url: String,
    database: String,
}

impl MockCouch {
    fn start(vault: &str) -> Self {
        let mut child = Command::new("node")
            .arg("test/mock-couch-server.mjs")
            .arg("--vault")
            .arg(vault)
            .current_dir(sidecar_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited so a fixture failure is visible in the test output.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the mock CouchDB fixture server");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut handshake = String::new();
        stdout
            .read_line(&mut handshake)
            .expect("read the fixture handshake line");
        let handshake: serde_json::Value =
            serde_json::from_str(handshake.trim()).expect("parse the fixture handshake");
        Self {
            url: handshake["url"].as_str().expect("fixture url").to_string(),
            database: handshake["database"]
                .as_str()
                .expect("fixture database")
                .to_string(),
            child,
            stdout,
        }
    }

    /// Issue one command and read its reply.
    fn command(&mut self, request: serde_json::Value) -> serde_json::Value {
        let stdin = self.child.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{request}").expect("write a fixture command");
        stdin.flush().expect("flush the fixture command");
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read a fixture reply");
        serde_json::from_str(line.trim()).expect("parse a fixture reply")
    }

    /// Add a note and release any held change feed, as a real edit would.
    fn push_note(&mut self, path: &str, text: &str) {
        let reply = self.command(serde_json::json!({
            "command": "push-note", "path": path, "text": text
        }));
        assert_eq!(reply["ok"], serde_json::json!(true), "push-note: {reply}");
    }

    /// Every request that would have MUTATED the remote.
    fn writes(&mut self) -> Vec<serde_json::Value> {
        let reply = self.command(serde_json::json!({"command": "writes"}));
        reply["writes"].as_array().cloned().unwrap_or_default()
    }
}

impl Drop for MockCouch {
    fn drop(&mut self) {
        // Closing stdin is the documented stop signal; the kill is the backstop.
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn credentials(couch: &MockCouch) -> SidecarCredentials {
    SidecarCredentials {
        url: couch.url.clone(),
        database: couch.database.clone(),
        username: "vaultuser".to_string(),
        password: SecretString::new(FIXTURE_PASSWORD.to_string()),
        e2ee_passphrase: None,
        e2ee_obfuscate_passphrase: None,
    }
}

fn config(couch: &MockCouch) -> SidecarConfig {
    SidecarConfig {
        launch: SidecarLaunch {
            node: PathBuf::from("node"),
            bundle: bundle_path(),
        },
        credentials: credentials(couch),
        options: None,
        request_timeout: Duration::from_secs(30),
        restart_backoff_base: Duration::from_millis(20),
    }
}

/// A backend over a freshly started fixture.
fn backend(couch: &MockCouch) -> (Arc<SidecarSupervisor>, CouchDbVaultBackend) {
    let supervisor = SidecarSupervisor::new(config(couch));
    (supervisor.clone(), CouchDbVaultBackend::new(supervisor))
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// The real sidecar's handshake against the real fixture: `ok`, and the pinning
/// triple the supervisor enforces MATCHES the one the sidecar advertises.
///
/// This is the test that would catch a `SUPPORTED` drift between the TypeScript and
/// the Rust — the one failure mode the version pinning exists to prevent.
#[tokio::test]
async fn the_real_sidecar_handshake_succeeds_and_the_pinning_triple_agrees() {
    require_prerequisites!();
    let couch = MockCouch::start("small");
    let supervisor = SidecarSupervisor::new(config(&couch));

    supervisor
        .ensure_ready()
        .await
        .expect("the real sidecar must hand-shake against the fixture");
    let health = supervisor.health();
    assert!(health.is_ready(), "{health:?}");
    assert_eq!(
        health.compatibility.as_ref().map(|c| c.status),
        Some(CompatibilityStatus::Ok)
    );
    assert_eq!(health.starts, 1);
    assert_eq!(health.consecutive_failures, 0);

    supervisor.shutdown().await;
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// `manifest` → `ListChildren` / `WalkMarkdown` / `TopLevelFolders`, driven through
/// the public `execute` surface against the fixture vault.
#[tokio::test]
async fn manifest_listings_come_from_the_fixture_vault() {
    require_prerequisites!();
    let couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    let markdown = backend
        .execute(BackendRequest::walk_markdown())
        .await
        .expect("walk markdown")
        .into_markdown_files()
        .expect("markdown files");
    // The fixture's two live notes plus the conflicted one (whose winning revision
    // is served). The soft-deleted `Removed.md` is a tombstone and must NOT appear.
    assert!(markdown.contains(&"Beta.md".to_string()), "{markdown:?}");
    assert!(
        markdown.contains(&"Notes/Alpha.md".to_string()),
        "{markdown:?}"
    );
    assert!(
        markdown.contains(&"Conflicted.md".to_string()),
        "a conflicted note is served: {markdown:?}"
    );
    assert!(
        !markdown.contains(&"Removed.md".to_string()),
        "a soft-deleted entry must not be listed: {markdown:?}"
    );
    // Sorted, because the order fixes note and chunk ids.
    let mut sorted = markdown.clone();
    sorted.sort();
    assert_eq!(markdown, sorted);

    let folders = backend
        .execute(BackendRequest::top_level_folders())
        .await
        .expect("top level folders")
        .into_folders()
        .expect("folders");
    assert!(folders.contains(&"Notes".to_string()), "{folders:?}");

    // Folders are SYNTHESIZED from path prefixes: a LiveSync vault is a flat map.
    let children = backend
        .execute(BackendRequest::Manifest(ManifestRequest::ListChildren {
            path: Some("Notes".to_string()),
            include_hidden: false,
            include_ignored: false,
        }))
        .await
        .expect("list children")
        .into_children()
        .expect("children");
    assert!(
        children.iter().any(|child| child.path == "Notes/Alpha.md"),
        "{children:?}"
    );

    supervisor.shutdown().await;
}

/// `read` for text and for a binary attachment, and `stat` for metadata.
#[tokio::test]
async fn reads_and_stats_return_the_fixture_content() {
    require_prerequisites!();
    let couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    // Text, reassembled from the fixture's TWO chunks.
    let text = backend
        .execute(BackendRequest::read_text("Notes/Alpha.md"))
        .await
        .expect("read Alpha")
        .into_text()
        .expect("text");
    assert_eq!(text, "# Alpha\n\nFirst note body.\n");

    // Binary, from base64 chunks that are deliberately split mid-quantum in the
    // fixture — so this also proves fragments are concatenated before decoding.
    let bytes = backend
        .execute(BackendRequest::read_bytes("assets/logo.png"))
        .await
        .expect("read the attachment")
        .into_bytes()
        .expect("bytes");
    assert_eq!(
        bytes,
        vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01, 0x02, 0x03]
    );

    let size = backend
        .execute(BackendRequest::stat("Beta.md"))
        .await
        .expect("stat Beta")
        .into_size_bytes()
        .expect("size");
    assert_eq!(size, "Beta note, single chunk.\n".len() as u64);

    // A missing path arrives as an IO NotFound, because the server branches on
    // `io_kind()` rather than on wording.
    let error = backend
        .execute(BackendRequest::read_text("Nope.md"))
        .await
        .expect_err("a missing note must fail");
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotFound));

    supervisor.shutdown().await;
}

// ---------------------------------------------------------------------------
// Capabilities and refusals
// ---------------------------------------------------------------------------

/// The capability set, and the refusals that follow from it.
#[tokio::test]
async fn writes_and_grep_are_refused_with_the_experimental_read_only_message() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    let descriptor = backend.descriptor();
    assert!(descriptor.supports(Capability::BinaryRead));
    assert!(descriptor.supports(Capability::Watch));
    assert!(!descriptor.supports(Capability::GrepSearch));
    assert!(!descriptor.supports(Capability::BinaryWrite));
    assert!(!descriptor.supports(Capability::Upload));

    let write = backend
        .execute(BackendRequest::write_text("New.md", "body"))
        .await
        .expect_err("writes must be refused");
    assert_eq!(write.to_string(), COUCHDB_READ_ONLY_MESSAGE);

    let upload = backend
        .execute(BackendRequest::Mutation(
            MutationRequest::CommitUploadStream {
                path: "New.png".to_string(),
                expected_hash: None,
                max_bytes: 16,
                chunks: deep_obsidian_backend::UploadChunks::new(std::iter::empty()),
            },
        ))
        .await
        .expect_err("uploads must be refused");
    assert_eq!(upload.to_string(), COUCHDB_READ_ONLY_MESSAGE);

    let grep = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: "Alpha".to_string(),
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
            limit: 10,
        }))
        .await
        .expect_err("grep must be refused");
    assert_eq!(grep.to_string(), COUCHDB_GREP_UNSUPPORTED_MESSAGE);

    // The decisive proof, at the transport: after all of the above plus the reads,
    // the remote saw NOT ONE mutating request. The sidecar is read-only
    // structurally, not by convention.
    let _ = backend.execute(BackendRequest::read_text("Beta.md")).await;
    supervisor.shutdown().await;
    assert_eq!(
        couch.writes(),
        Vec::<serde_json::Value>::new(),
        "the sidecar must never write to the remote"
    );
}

/// A traversal or otherwise unusable path is refused WITHOUT touching the sidecar,
/// which is what the upload mint needs: it validates before issuing a token.
#[tokio::test]
async fn resolve_path_rejects_traversal_without_the_sidecar() {
    require_prerequisites!();
    let couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    for path in ["../escape.md", "/absolute.md", "has:colon.md"] {
        assert!(
            backend
                .execute(BackendRequest::resolve_path(path))
                .await
                .is_err(),
            "{path} must be refused"
        );
    }
    assert!(backend
        .execute(BackendRequest::resolve_path("Notes/Alpha.md"))
        .await
        .is_ok());

    // Never started a child: path validation is pure.
    assert_eq!(supervisor.health().starts, 0);
}

// ---------------------------------------------------------------------------
// Watch
// ---------------------------------------------------------------------------

/// A live edit on the remote arrives as a `ChangeEvent` on the backend's stream.
#[tokio::test]
async fn a_live_edit_arrives_on_the_change_stream() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    // Ready first, so `watch` is armed before the edit is pushed.
    supervisor.ensure_ready().await.expect("handshake");
    let mut stream = backend.changes(None);
    // Give the supervisor's watch-arming task time to complete its catch-up and
    // subscribe. Polling rather than sleeping blind: the loop below is the actual
    // assertion and this only avoids pushing into an unarmed feed.
    for _ in 0..100 {
        if supervisor.health().watching {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        supervisor.health().watching,
        "the change feed must arm: {:?}",
        supervisor.health()
    );

    couch.push_note("Fresh.md", "A newly edited note.\n");

    let event = tokio::time::timeout(Duration::from_secs(20), stream.recv())
        .await
        .expect("a change notification must arrive within 20s")
        .expect("the stream must not close");
    match event {
        deep_obsidian_backend::ChangeEvent::Change(reason) => {
            assert!(
                reason.starts_with("livesync:"),
                "the reason names the provider: {reason}"
            );
        }
        other => panic!("expected a Change event, got {other:?}"),
    }

    supervisor.shutdown().await;
}

// ---------------------------------------------------------------------------
// Compatibility failures → degraded readiness
// ---------------------------------------------------------------------------

/// A `locked` milestone: the handshake SUCCEEDS, the mount is NOT ready, and every
/// data method refuses with the status. That combination is what makes the mount
/// degrade while the vault root keeps serving.
#[tokio::test]
async fn a_locked_milestone_degrades_the_mount_instead_of_failing_it() {
    require_prerequisites!();
    let couch = MockCouch::start("locked");
    let (supervisor, backend) = backend(&couch);

    // The child starts and hand-shakes...
    supervisor
        .ensure_started()
        .await
        .expect("a locked remote must still complete the handshake");
    assert_eq!(supervisor.health().starts, 1);
    // ...but the mount is not serveable, and the reason is precise.
    let error = supervisor
        .ensure_ready()
        .await
        .expect_err("a locked remote is not serveable");
    assert_eq!(error.status(), Some(CompatibilityStatus::Locked));
    assert!(!supervisor.health().is_ready());

    // A data method refuses, naming the status rather than a generic failure.
    let refusal = backend
        .execute(BackendRequest::walk_markdown())
        .await
        .expect_err("a locked remote must refuse data methods")
        .to_string();
    assert!(refusal.contains("locked"), "{refusal}");
    assert!(refusal.contains("mid-rebuild"), "{refusal}");

    // Health reports unreachable rather than erroring, so a startup gate that runs
    // `health_overview` across mounts does not take the server down.
    let health = backend
        .execute(BackendRequest::health_overview())
        .await
        .expect("health must answer even when the remote is not serveable");
    assert!(matches!(
        health,
        deep_obsidian_backend::BackendResponse::Health(
            deep_obsidian_backend::HealthResponse::Overview { reachable: false }
        )
    ));

    supervisor.shutdown().await;
}

/// `cleaned` and `unknown-schema` fail closed too, each with its own remediation —
/// the point of a status enum rather than one "incompatible" error.
#[tokio::test]
async fn cleaned_and_unknown_schema_fail_closed_with_distinct_remediations() {
    require_prerequisites!();
    for (vault, expected_status, expected_remediation) in [
        ("cleaned", CompatibilityStatus::Cleaned, "resync"),
        (
            "unknown-schema",
            CompatibilityStatus::UnknownSchema,
            "refuses to guess",
        ),
    ] {
        let couch = MockCouch::start(vault);
        let supervisor = SidecarSupervisor::new(config(&couch));

        // The handshake completes: a remote problem is a status, not a crash.
        supervisor
            .ensure_started()
            .await
            .unwrap_or_else(|error| panic!("{vault} must still hand-shake: {error}"));

        let error = match supervisor.ensure_ready().await {
            Err(error) => error,
            Ok(()) => panic!("{vault} must not be reported as serveable"),
        };
        assert_eq!(
            error.status(),
            Some(expected_status),
            "{vault} classified as {:?}",
            error.status()
        );
        // Each status carries its OWN remediation; that is the whole reason the
        // sidecar reports a status enum rather than one generic failure.
        let message = error.to_string();
        assert!(
            message.contains(expected_remediation),
            "{vault} remediation missing {expected_remediation:?}: {message}"
        );

        supervisor.shutdown().await;
    }
}

/// No secret ever reaches the child's command line.
///
/// Asserted against the ACTUAL argv the supervisor spawns, not a transcription of
/// it, so appending an argument in future cannot quietly invalidate the test.
#[tokio::test]
async fn no_secret_reaches_the_spawned_command_line() {
    require_prerequisites!();
    let couch = MockCouch::start("small");
    let supervisor = SidecarSupervisor::new(config(&couch));
    supervisor.ensure_ready().await.expect("handshake");

    let argv = format!("{:?}", supervisor.command_line());
    assert!(!argv.contains(FIXTURE_PASSWORD), "password in argv: {argv}");
    assert_eq!(
        supervisor.command_line().len(),
        2,
        "argv must be exactly [node, bundle]: {argv}"
    );

    supervisor.shutdown().await;
}

// ---------------------------------------------------------------------------
// The env-gated LIVE test
// ---------------------------------------------------------------------------

/// Against a REAL CouchDB, named by `DEEP_OBSIDIAN_COUCHDB_URL`.
///
/// ```sh
/// docker run -d --rm -p 5984:5984 -e COUCHDB_USER=admin -e COUCHDB_PASSWORD=pw couchdb:3
/// DEEP_OBSIDIAN_COUCHDB_URL=http://127.0.0.1:5984 \
///   DEEP_OBSIDIAN_COUCHDB_USER=admin DEEP_OBSIDIAN_COUCHDB_PASSWORD=pw \
///   cargo test -p deep-obsidian-backend --test couchdb_sidecar -- --ignored
/// ```
///
/// # What this can and cannot prove
///
/// An EMPTY real CouchDB has no `obsydian_livesync_version` document, so the only
/// honest assertion is that the sidecar CLASSIFIES it — `unknown-schema` (reachable,
/// no schema doc) or `auth-failed`/`unreachable` — rather than crashing, hanging, or
/// reporting `ok`. That is genuinely worth having: it exercises the real PouchDB
/// HTTP adapter against a real CouchDB's real responses, which the mock only
/// approximates.
///
/// It deliberately does NOT seed a vault. Doing so would mean writing LiveSync
/// documents by hand, and a hand-built fixture that satisfies a test proves nothing
/// about the format — the exact trap the sidecar's README calls out. The remaining
/// gaps are therefore still gaps, and this is the plan for closing them:
///
/// 1. Install Self-hosted LiveSync in a scratch Obsidian vault, point it at a
///    throwaway CouchDB, and let it replicate a handful of notes, one attachment,
///    one deletion and one deliberate conflict.
/// 2. Dump the database (`curl .../_all_docs?include_docs=true`) and commit it as a
///    fixture, recording the plugin version in the fixture file.
/// 3. Repeat with E2EE enabled, and again with path obfuscation, which is what
///    would finally exercise a SUCCESSFUL decrypt and obfuscated-id resolution —
///    both unproven today because their ciphertext cannot be synthesized (see the
///    sidecar README's "Deferred to slice 3c").
/// 4. Re-run on every `commonlibVersion` bump; a diff in the dump is the signal
///    that `maxSchemaVersion` needs review.
#[tokio::test]
#[ignore = "requires a real CouchDB; set DEEP_OBSIDIAN_COUCHDB_URL"]
async fn a_real_couchdb_is_classified_rather_than_crashed_on() {
    require_prerequisites!();
    let Ok(url) = std::env::var("DEEP_OBSIDIAN_COUCHDB_URL") else {
        eprintln!("skipping: DEEP_OBSIDIAN_COUCHDB_URL is not set");
        return;
    };
    let username = std::env::var("DEEP_OBSIDIAN_COUCHDB_USER").unwrap_or_else(|_| "admin".into());
    let password = std::env::var("DEEP_OBSIDIAN_COUCHDB_PASSWORD").unwrap_or_else(|_| "pw".into());
    let database =
        std::env::var("DEEP_OBSIDIAN_COUCHDB_DATABASE").unwrap_or_else(|_| "vault".into());

    let supervisor = SidecarSupervisor::new(SidecarConfig {
        launch: SidecarLaunch {
            node: PathBuf::from("node"),
            bundle: bundle_path(),
        },
        credentials: SidecarCredentials {
            url,
            database,
            username,
            password: SecretString::new(password),
            e2ee_passphrase: None,
            e2ee_obfuscate_passphrase: None,
        },
        options: None,
        request_timeout: Duration::from_secs(30),
        restart_backoff_base: Duration::from_millis(50),
    });

    // The handshake must COMPLETE. A remote problem is a status, never a crash or a
    // protocol error, so `ensure_started` succeeding is itself the assertion.
    supervisor
        .ensure_started()
        .await
        .expect("the handshake against a real CouchDB must complete, not crash");

    let health = supervisor.health();
    let compatibility = health
        .compatibility
        .as_ref()
        .expect("a real CouchDB must produce a compatibility verdict");
    eprintln!("real CouchDB classified as: {}", compatibility.describe());
    // An empty CouchDB has no milestone and no version document, so it must NOT be
    // reported as serveable. Anything else here would mean the gate is not gating.
    assert!(
        !compatibility.status.is_ok(),
        "an EMPTY CouchDB must not be reported as serveable: {}",
        compatibility.describe()
    );
    assert!(
        matches!(
            compatibility.status,
            CompatibilityStatus::UnknownSchema
                | CompatibilityStatus::AuthFailed
                | CompatibilityStatus::Unreachable
                | CompatibilityStatus::Unknown
        ),
        "unexpected classification: {}",
        compatibility.describe()
    );

    // Data methods refuse with the same status, rather than returning an empty vault
    // (which a caller could not distinguish from a real empty vault).
    let error = supervisor
        .collect_manifest()
        .await
        .expect_err("data methods must refuse until the remote is serveable");
    assert!(matches!(error, SidecarError::NotReady { .. }));

    supervisor.shutdown().await;
}
