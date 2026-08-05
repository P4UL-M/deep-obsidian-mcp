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
    SidecarMode, SidecarSupervisor,
};
use deep_obsidian_backend::{
    BackendError, BackendRequest, BaseVersion, Capability, CouchDbVaultBackend, ManifestRequest,
    MutationRequest, RecallRequest, VaultBackend, COUCHDB_GREP_UNSUPPORTED_MESSAGE,
    COUCHDB_READ_ONLY_MESSAGE,
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
    /// A read-only fixture: every mutating request is refused and recorded, which
    /// is what the read-only proofs assert on.
    fn start(vault: &str) -> Self {
        Self::start_with(vault, false)
    }

    /// A fixture that ACCEPTS writes with CouchDB's real 409 semantics. Needed for
    /// the write tests and for nothing else, so it is opt-in here exactly as it is
    /// opt-in in the mock itself.
    fn start_writable(vault: &str) -> Self {
        Self::start_with(vault, true)
    }

    fn start_with(vault: &str, writable: bool) -> Self {
        let mut command = Command::new("node");
        command
            .arg("test/mock-couch-server.mjs")
            .arg("--vault")
            .arg(vault);
        if writable {
            command.arg("--writable");
        }
        let mut child = command
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

    /// Answer the next `count` mutating requests 500 WITHOUT applying them: a
    /// transient remote failure whose precondition is still valid on retry.
    fn fail_next_writes(&mut self, count: u32) {
        let reply = self.command(serde_json::json!({
            "command": "fail-next-writes", "count": count
        }));
        assert_eq!(reply["ok"], serde_json::json!(true), "{reply}");
    }

    /// APPLY the next `count` entry-root PUTs and then answer 500: the write lands and
    /// the client never hears about it. The only way to reach the ambiguous-conflict
    /// path from outside.
    fn drop_next_entry_put_responses(&mut self, count: u32) {
        let reply = self.command(serde_json::json!({
            "command": "drop-next-entry-put-responses", "count": count
        }));
        assert_eq!(reply["ok"], serde_json::json!(true), "{reply}");
    }

    /// Answer the next `count` requests of ANY kind 500: a remote outage.
    ///
    /// Set to a count larger than any operation's request budget to open an outage that
    /// lasts until it is cleared, and to `0` to close it. Explicit open/close rather
    /// than a duration: the fixture keeps its port for the whole test, so recovery is
    /// observed by polling the operation rather than by sleeping out a guessed window.
    fn fail_next_requests(&mut self, count: u32) {
        let reply = self.command(serde_json::json!({
            "command": "fail-next-requests", "count": count
        }));
        assert_eq!(reply["ok"], serde_json::json!(true), "{reply}");
    }

    /// Destroy the socket for the next `count` requests: a connection DROP rather than a
    /// 500. Distinct from [`Self::fail_next_requests`] because the two arrive at the
    /// sidecar's HTTP client as different failures, and only one of them is a response.
    fn destroy_next_requests(&mut self, count: u32) {
        let reply = self.command(serde_json::json!({
            "command": "destroy-next-requests", "count": count
        }));
        assert_eq!(reply["ok"], serde_json::json!(true), "{reply}");
    }
}

/// The deadline every recovery assertion in this file is bounded by.
///
/// Recovery here is never a background loop -- the supervisor restarts lazily, inside
/// the next call -- so the poll below RE-ISSUES the operation each time rather than
/// watching a status field, which would never flip on its own.
const RECOVERY_DEADLINE: Duration = Duration::from_secs(30);

/// Poll `attempt` until it succeeds, or panic naming the last failure.
///
/// No bare sleeps anywhere in the resilience tests: a sleep long enough for a slow CI
/// box is a sleep every developer pays on every run, and one short enough not to hurt
/// is a flake. The deadline is the only timing this file contains.
async fn poll_until_ok<T, E, F, Fut>(what: &str, mut attempt: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let deadline = std::time::Instant::now() + RECOVERY_DEADLINE;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        match attempt().await {
            Ok(value) => return value,
            Err(error) => {
                last = error.to_string();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    panic!("{what} did not recover within {RECOVERY_DEADLINE:?}; last failure: {last}");
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
    config_with_mode(couch, SidecarMode::ReadOnly)
}

fn config_with_mode(couch: &MockCouch, mode: SidecarMode) -> SidecarConfig {
    SidecarConfig {
        launch: SidecarLaunch {
            node: PathBuf::from("node"),
            bundle: bundle_path(),
        },
        credentials: credentials(couch),
        mode,
        options: None,
        request_timeout: Duration::from_secs(30),
        restart_backoff_base: Duration::from_millis(20),
    }
}

/// A READ-ONLY backend over a freshly started fixture.
fn backend(couch: &MockCouch) -> (Arc<SidecarSupervisor>, CouchDbVaultBackend) {
    let supervisor = SidecarSupervisor::new(config(couch));
    (
        supervisor.clone(),
        CouchDbVaultBackend::from_supervisor(supervisor),
    )
}

/// A WRITABLE backend over a freshly started fixture. The backend derives its
/// writability from the supervisor's mode, so naming the mode here is the only way
/// to get one.
fn writable_backend(couch: &MockCouch) -> (Arc<SidecarSupervisor>, CouchDbVaultBackend) {
    let supervisor = SidecarSupervisor::new(config_with_mode(couch, SidecarMode::ReadWrite));
    (
        supervisor.clone(),
        CouchDbVaultBackend::from_supervisor(supervisor),
    )
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
// Guarded writes
// ---------------------------------------------------------------------------

/// Read a note's text and the opaque version it was read at — the pair the write path
/// threads.
async fn read_versioned(backend: &CouchDbVaultBackend, path: &str) -> (String, Option<String>) {
    backend
        .execute(BackendRequest::read_text(path))
        .await
        .unwrap_or_else(|error| panic!("read {path}: {error}"))
        .into_versioned_text()
        .expect("versioned text")
}

/// A writable mount advertises write capabilities; a read-only one does not, and the
/// capability set is derived from the sidecar's mode rather than declared beside it.
#[tokio::test]
async fn write_capabilities_follow_the_mounts_mode() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");

    let (read_only_supervisor, read_only) = backend(&couch);
    assert!(!read_only.is_writable());
    let descriptor = read_only.descriptor();
    assert!(!descriptor.supports(Capability::BinaryWrite));
    assert!(!descriptor.supports(Capability::Upload));

    let (writable_supervisor, writable) = writable_backend(&couch);
    assert!(writable.is_writable());
    let descriptor = writable.descriptor();
    assert!(descriptor.supports(Capability::BinaryWrite));
    assert!(descriptor.supports(Capability::Upload));
    // Reads are unchanged by the mode, and grep is still absent: writability says
    // nothing about ripgrep.
    assert!(descriptor.supports(Capability::BinaryRead));
    assert!(!descriptor.supports(Capability::GrepSearch));

    read_only_supervisor.shutdown().await;
    writable_supervisor.shutdown().await;
}

/// The happy path: read, write back under the observed revision, read again.
///
/// Also the proof that the revision a read hands out is the one a write accepts — if
/// the two disagreed, this would fail with a conflict rather than succeed.
#[tokio::test]
async fn a_write_guarded_by_the_revision_a_read_returned_lands() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    let (text, version) = read_versioned(&backend, "Beta.md").await;
    let version = version.expect("a couchdb read must carry its revision");
    assert!(!text.is_empty());

    let updated = "Beta note, rewritten by the agent.\n";
    let response = backend
        .execute(BackendRequest::write_text_guarded(
            "Beta.md",
            updated,
            BaseVersion::Version(version.clone()),
        ))
        .await
        .expect("a write under the observed revision must land");
    assert!(
        matches!(
            response,
            deep_obsidian_backend::BackendResponse::Mutation(
                deep_obsidian_backend::MutationResponse::Written { created: false }
            )
        ),
        "overwriting an existing note is not a create: {response:?}"
    );

    let (after, after_version) = read_versioned(&backend, "Beta.md").await;
    assert_eq!(after, updated, "the write must be readable back exactly");
    assert_ne!(
        after_version.as_deref(),
        Some(version.as_str()),
        "the revision must have moved"
    );

    supervisor.shutdown().await;
}

/// The heart of the slice: a write whose precondition is stale FAILS, and the note is
/// left exactly as the other writer left it.
///
/// This is the case a filesystem mount cannot detect — it would rename over the other
/// writer's content and report success. Here the storage adjudicates, the loser is
/// told, and no version is lost.
#[tokio::test]
async fn a_write_whose_precondition_went_stale_is_refused_and_loses_nothing() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    // Both "clients" read the same base revision.
    let (_, base) = read_versioned(&backend, "Beta.md").await;
    let base = base.expect("a revision");

    // The first writer lands.
    let winner = "written by the client that got there first\n";
    backend
        .execute(BackendRequest::write_text_guarded(
            "Beta.md",
            winner,
            BaseVersion::Version(base.clone()),
        ))
        .await
        .expect("the first write must land");

    // The second writer still holds the ORIGINAL revision, exactly as it would if it
    // had checked `expectedHash` a moment before the first writer committed.
    let error = backend
        .execute(BackendRequest::write_text_guarded(
            "Beta.md",
            "written by the client that was too late\n",
            BaseVersion::Version(base.clone()),
        ))
        .await
        .expect_err("a stale precondition must be refused, never overwritten");

    // Structurally a version conflict...
    assert!(
        matches!(error, BackendError::VersionConflict { .. }),
        "unexpected error: {error:?}"
    );
    let message = error.to_string();
    // ...reported in the taxonomy a caller already handles for a stale expectedHash,
    // so a client does not need a new branch to do the right thing.
    assert!(
        message.starts_with("hash conflict for Beta.md:"),
        "the wording must land in the existing hash-conflict taxonomy: {message}"
    );
    assert!(
        message.contains("nothing was written"),
        "the message must say the write did not happen: {message}"
    );
    assert!(
        message.contains(&base),
        "the message must name the precondition that failed: {message}"
    );

    // The winner's content survived untouched. Nothing was merged, nothing was lost.
    let (after, _) = read_versioned(&backend, "Beta.md").await;
    assert_eq!(after, winner);

    supervisor.shutdown().await;
}

/// A caller that observed "nothing is here" gets create-only semantics, so a
/// concurrent create is reported rather than clobbered.
#[tokio::test]
async fn an_absent_precondition_is_create_only() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    // A genuine create.
    let response = backend
        .execute(BackendRequest::write_text_guarded(
            "Notes/Brand New.md",
            "# Brand New\n\nfresh\n",
            BaseVersion::Absent,
        ))
        .await
        .expect("a create-only write to a free path must land");
    assert!(
        matches!(
            response,
            deep_obsidian_backend::BackendResponse::Mutation(
                deep_obsidian_backend::MutationResponse::Written { created: true }
            )
        ),
        "a first write reports created: {response:?}"
    );

    // The same claim against a path that is now occupied must fail: the caller's
    // observation is no longer true.
    let error = backend
        .execute(BackendRequest::write_text_guarded(
            "Notes/Brand New.md",
            "# Brand New\n\nsomeone else got here first\n",
            BaseVersion::Absent,
        ))
        .await
        .expect_err("create-only over an existing entry must be refused");
    assert!(
        matches!(error, BackendError::VersionConflict { .. }),
        "unexpected error: {error:?}"
    );

    // A soft-deleted entry OCCUPIES its path: `Removed.md` looks free to a listing but
    // is a live document with a revision, so create-only must lose there too. Reported
    // rather than silently resurrected, because "create" and "bring back" are
    // different intents.
    let error = backend
        .execute(BackendRequest::write_text_guarded(
            "Removed.md",
            "resurrected by accident\n",
            BaseVersion::Absent,
        ))
        .await
        .expect_err("create-only over a tombstone must be refused");
    assert!(
        matches!(error, BackendError::VersionConflict { .. }),
        "unexpected error: {error:?}"
    );

    supervisor.shutdown().await;
}

/// The protected-template policy is the vault's, not the filesystem's: a writable
/// couchdb mount refuses the same paths with core's byte-identical wording.
#[tokio::test]
async fn protected_template_paths_are_refused_with_cores_wording() {
    require_prerequisites!();
    let mut couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    for path in ["Templates/T.md", "Notes/Template/T.md", "templates/t.md"] {
        let error = backend
            .execute(BackendRequest::write_text(path, "body"))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("writes to protected template folders are forbidden: {path}"),
            "for {path}"
        );
    }

    // Refused ABOVE the remote: not one write request was issued.
    supervisor.shutdown().await;
    assert_eq!(couch.writes(), Vec::<serde_json::Value>::new());
}

/// A transient `remote-error` is retried once under the same precondition, and the
/// write lands. Retrying is safe by construction (content-addressed chunks, entry root
/// last), which is why the policy lives here rather than in the sidecar.
#[tokio::test]
async fn a_transient_remote_error_is_retried_once_and_the_write_lands() {
    require_prerequisites!();
    let mut couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    let (_, base) = read_versioned(&backend, "Beta.md").await;
    // The next mutating request is answered 500 WITHOUT being applied, so the retry
    // starts from an unchanged remote and its original precondition still holds.
    couch.fail_next_writes(1);

    let updated = "survived a transient remote failure\n";
    backend
        .execute(BackendRequest::write_text_guarded(
            "Beta.md",
            updated,
            BaseVersion::from_read(base),
        ))
        .await
        .expect("a transient remote error must be retried, not surfaced");

    let (after, _) = read_versioned(&backend, "Beta.md").await;
    assert_eq!(after, updated);

    supervisor.shutdown().await;
}

/// The one ambiguous case, and the only place a conflict does not become an error.
///
/// The write LANDS and its response is dropped, so the retry meets a revision that is
/// its own. A revision cannot tell that apart from a competing writer — the content
/// can. Since the remote already holds exactly the requested bytes, the write is
/// reported as the no-op it is instead of as a failure for something that succeeded.
#[tokio::test]
async fn a_write_whose_response_was_lost_is_reported_as_the_no_op_it_is() {
    require_prerequisites!();
    let mut couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    let (_, base) = read_versioned(&backend, "Beta.md").await;
    // Apply the next entry-root PUT and then answer 500: the write lands, the client
    // never hears about it.
    couch.drop_next_entry_put_responses(1);

    let desired = "landed but unacknowledged\n";
    backend
        .execute(BackendRequest::write_text_guarded(
            "Beta.md",
            desired,
            BaseVersion::from_read(base),
        ))
        .await
        .expect("a write that demonstrably landed must not be reported as a conflict");

    // And it really is the requested content, not a guess.
    let (after, _) = read_versioned(&backend, "Beta.md").await;
    assert_eq!(after, desired);

    supervisor.shutdown().await;
}

/// The same lost-response machinery, but the content does NOT match: the carve-out
/// must not fire, and the conflict must be reported.
///
/// This is what keeps the previous test from being a loophole — the discriminator is
/// byte-equality with the requested content, not "a retry happened".
#[tokio::test]
async fn an_ambiguous_conflict_whose_content_differs_is_still_a_conflict() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    let (_, base) = read_versioned(&backend, "Beta.md").await;
    let base = base.expect("a revision");
    // A competing writer lands first, so the stale precondition below can only be a
    // genuine conflict.
    backend
        .execute(BackendRequest::write_text_guarded(
            "Beta.md",
            "the other client's content\n",
            BaseVersion::Version(base.clone()),
        ))
        .await
        .expect("the competing write must land");

    let error = backend
        .execute(BackendRequest::write_text_guarded(
            "Beta.md",
            "my content, which is not what is there\n",
            BaseVersion::Version(base),
        ))
        .await
        .expect_err("differing content must still conflict");
    assert!(matches!(error, BackendError::VersionConflict { .. }));
    // The other client's content is intact.
    let (after, _) = read_versioned(&backend, "Beta.md").await;
    assert_eq!(after, "the other client's content\n");

    supervisor.shutdown().await;
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

/// A binary upload lands through the sidecar and reads back byte-identical.
#[tokio::test]
async fn an_upload_round_trips_binary_bytes() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    // Deliberately not a multiple of 3, so the base64 padding path is exercised, and
    // deliberately containing bytes no UTF-8 decoder would survive.
    let bytes: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0x00, 0xff, 0xfe, 0x01, 0x02];
    let outcome = backend
        .execute(BackendRequest::Mutation(
            MutationRequest::CommitUploadStream {
                path: "assets/uploaded.png".to_string(),
                expected_hash: None,
                max_bytes: 1024,
                // Two chunks, so the collector's concatenation is exercised rather
                // than a single-buffer shortcut.
                chunks: deep_obsidian_backend::UploadChunks::new(
                    vec![Ok(bytes[..4].to_vec()), Ok(bytes[4..].to_vec())].into_iter(),
                ),
            },
        ))
        .await
        .expect("an upload to a writable mount must land")
        .into_upload_outcome()
        .expect("upload outcome");
    assert!(outcome.created);
    assert_eq!(outcome.bytes_written, bytes.len());
    // The canonical hash, so the endpoint's reported hash is the same string the tool
    // layer would compute over the same bytes.
    assert_eq!(outcome.hash, deep_obsidian_core::content_hash(&bytes));

    let read_back = backend
        .execute(BackendRequest::read_bytes("assets/uploaded.png"))
        .await
        .expect("read the uploaded artifact")
        .into_bytes()
        .expect("bytes");
    assert_eq!(
        read_back, bytes,
        "an upload must round-trip byte-identically"
    );

    supervisor.shutdown().await;
}

/// A multi-megabyte upload round-trips, so the reported ceiling is a measured one.
///
/// # Why this test exists rather than a claim about `DEFAULT_MAX_UPLOAD_BYTES`
///
/// The configured cap is 100 MiB, but a CouchDB upload is not a stream: the sidecar needs
/// the whole content to run upstream's chunker over it, so the bytes are held once in the
/// collector and again as base64 in a single JSON-RPC line. The practical ceiling is
/// therefore memory and Node's line handling, not the configured number — and the honest
/// thing to report is a size that has actually been exercised end to end.
///
/// 4 MiB of NON-COMPRESSIBLE bytes: incompressible so the content-defined chunker
/// produces many distinct chunks and the real `_bulk_docs` batching path runs, rather
/// than one chunk repeated.
#[tokio::test]
async fn a_multi_megabyte_upload_round_trips() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    // A cheap xorshift keeps this deterministic without a dependency: the same bytes
    // every run, so a failure is reproducible.
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let bytes: Vec<u8> = (0..4 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xff) as u8
        })
        .collect();

    let outcome = backend
        .execute(BackendRequest::Mutation(
            MutationRequest::CommitUploadStream {
                path: "assets/large.bin".to_string(),
                expected_hash: None,
                max_bytes: deep_obsidian_backend::UPLOAD_COLLECT_ADVISORY_BYTES,
                // Streamed in 64 KiB chunks, as a real HTTP body pump would deliver it.
                chunks: deep_obsidian_backend::UploadChunks::new(
                    bytes
                        .chunks(64 * 1024)
                        .map(|chunk| Ok(chunk.to_vec()))
                        .collect::<Vec<_>>()
                        .into_iter(),
                ),
            },
        ))
        .await
        .expect("a multi-megabyte upload must land")
        .into_upload_outcome()
        .expect("upload outcome");
    assert!(outcome.created);
    assert_eq!(outcome.bytes_written, bytes.len());
    assert_eq!(outcome.hash, deep_obsidian_core::content_hash(&bytes));

    let read_back = backend
        .execute(BackendRequest::read_bytes("assets/large.bin"))
        .await
        .expect("read the large artifact")
        .into_bytes()
        .expect("bytes");
    assert_eq!(
        read_back.len(),
        bytes.len(),
        "the round trip must preserve the length"
    );
    assert_eq!(
        read_back, bytes,
        "a multi-megabyte upload must round-trip byte-identically"
    );

    supervisor.shutdown().await;
}

/// A stale `expectedHash` on an upload is refused with the SAME wording the filesystem
/// backend uses, because the upload endpoint's 409 body is frozen public behaviour.
#[tokio::test]
async fn an_upload_with_a_stale_expected_hash_conflicts_with_the_frozen_wording() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    let existing = backend
        .execute(BackendRequest::read_bytes("assets/logo.png"))
        .await
        .expect("read the fixture artifact")
        .into_bytes()
        .expect("bytes");

    let error = backend
        .execute(BackendRequest::Mutation(
            MutationRequest::CommitUploadStream {
                path: "assets/logo.png".to_string(),
                expected_hash: Some("fnv1a64:0000000000000000".to_string()),
                max_bytes: 1024,
                chunks: deep_obsidian_backend::UploadChunks::new(std::iter::once(Ok(
                    b"replacement".to_vec(),
                ))),
            },
        ))
        .await
        .expect_err("a stale expected hash must conflict");
    assert!(matches!(error, BackendError::HashConflict { .. }));
    assert_eq!(
        error.to_string(),
        format!(
            "hash conflict: expected fnv1a64:0000000000000000, found {}",
            deep_obsidian_core::content_hash(&existing)
        )
    );

    // The destination is untouched by a rejected commit.
    let after = backend
        .execute(BackendRequest::read_bytes("assets/logo.png"))
        .await
        .expect("read again")
        .into_bytes()
        .expect("bytes");
    assert_eq!(after, existing);

    supervisor.shutdown().await;
}

/// An oversize body is refused DURING collection, so it never reaches the remote —
/// the same `PayloadTooLarge` (413) taxonomy a filesystem mount produces.
#[tokio::test]
async fn an_oversize_upload_is_refused_before_the_remote_is_touched() {
    require_prerequisites!();
    let mut couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    let error = backend
        .execute(BackendRequest::Mutation(
            MutationRequest::CommitUploadStream {
                path: "assets/too-big.bin".to_string(),
                expected_hash: None,
                max_bytes: 4,
                chunks: deep_obsidian_backend::UploadChunks::new(std::iter::once(Ok(
                    b"12345".to_vec()
                ))),
            },
        ))
        .await
        .expect_err("an oversize body must be rejected");
    assert!(matches!(error, BackendError::PayloadTooLarge));
    assert_eq!(error.to_string(), "upload exceeds maximum allowed size");

    supervisor.shutdown().await;
    assert_eq!(
        couch.writes(),
        Vec::<serde_json::Value>::new(),
        "an oversize body must never reach the remote"
    );
}

// ---------------------------------------------------------------------------
// Conflict exposure
// ---------------------------------------------------------------------------

/// Conflicted paths come off the already-collected manifest, and the per-path
/// enumeration works on a READ-ONLY mount — which is exactly where it matters most.
#[tokio::test]
async fn conflicts_are_enumerable_on_a_read_only_mount() {
    require_prerequisites!();
    let couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    let conflicted = backend
        .conflicted_paths()
        .await
        .expect("conflicted paths must be listable");
    assert_eq!(
        conflicted,
        // `Some`, not `None`: a LiveSync vault genuinely has the notion, so even an
        // empty answer here would be a real one.
        Some(vec!["Conflicted.md".to_string()]),
        "the fixture has exactly one conflicted entry"
    );

    let detail = backend
        .conflicts("Conflicted.md")
        .await
        .expect("per-path conflicts must be listable read-only");
    assert!(!detail.winning.is_empty());
    assert!(
        !detail.conflicts.is_empty(),
        "a conflicted entry must report its siblings: {detail:?}"
    );

    // A healthy entry reports no siblings rather than failing.
    let healthy = backend
        .conflicts("Beta.md")
        .await
        .expect("healthy conflicts");
    assert!(healthy.conflicts.is_empty(), "{healthy:?}");

    supervisor.shutdown().await;
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
        mode: SidecarMode::ReadOnly,
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

// ---------------------------------------------------------------------------
// Resilience
// ---------------------------------------------------------------------------
//
// The four faults the roadmap names: the child process dying, the remote going away
// mid-session, a cursor across a restart, and recovery from each. All of them against
// the REAL sidecar and the REAL fixture CouchDB, because that is the only place where
// "the supervision claims to catch up" can be distinguished from "the supervision
// claims to catch up".
//
// # What is deliberately NOT asserted here
//
// The child dying *mid-request* -- the `call_tracked` retry-once path and its
// "first attempt unobserved" flag -- is not reachable from outside: killing the child
// between two calls is, killing it while one is in flight is a race no test can win
// reliably. The HTTP-layer analogue of that ambiguity IS covered, by
// `a_write_whose_response_was_lost_is_reported_as_the_no_op_it_is`, which drops a
// response the remote already applied.

/// Kill `pid` outright, the way a crash or an OOM would.
///
/// SIGKILL rather than the `shutdown` RPC: a graceful stop is a message the child
/// answers, which proves the protocol works and says nothing about supervision. What
/// supervision has to survive is a death with no notice, and only a signal produces
/// one. `child_pid` is the supervisor's own view of its own child, so nothing else on
/// the machine can be hit.
fn kill_child(pid: u32) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -9 {pid} failed");
}

/// A child killed outright is restarted by the next call, and the read still answers.
///
/// This is the load-bearing supervision claim: everything else in this section assumes
/// a dead child comes back.
#[tokio::test]
async fn a_child_killed_outright_is_restarted_by_the_next_call() {
    require_prerequisites!();
    let couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    supervisor.ensure_ready().await.expect("the first handshake");
    let first_pid = supervisor.child_pid().expect("a running child has a pid");
    assert_eq!(supervisor.health().starts, 1);

    // The content a read answers BEFORE the death, so the post-restart read can be
    // asserted to be the real thing rather than an empty success.
    let before = backend
        .execute(BackendRequest::read_text("Notes/Alpha.md"))
        .await
        .expect("the pre-death read")
        .into_text()
        .expect("text");
    assert!(!before.is_empty());

    kill_child(first_pid);

    // The next call restarts and re-hand-shakes. Polled rather than assumed to be the
    // FIRST attempt: whether the reader task has already seen EOF when the call lands
    // decides whether the restart happens in `live_connection` or in the retry inside
    // `call_tracked`, and both are correct.
    let after = poll_until_ok("a read after the child was killed", || {
        backend.execute(BackendRequest::read_text("Notes/Alpha.md"))
    })
    .await
    .into_text()
    .expect("text");
    assert_eq!(
        after, before,
        "a restart must not change what a read answers"
    );

    // Health REPORTS the restart rather than hiding it: an operator looking at a mount
    // that keeps restarting has to be able to see that it is.
    let health = supervisor.health();
    assert_eq!(
        health.starts, 2,
        "the restart must be counted: {health:?}"
    );
    assert_eq!(
        health.consecutive_failures, 0,
        "a SUCCESSFUL restart clears the failure count: {health:?}"
    );
    assert!(health.is_ready(), "{health:?}");
    let second_pid = supervisor.child_pid().expect("a running child has a pid");
    assert_ne!(
        second_pid, first_pid,
        "the restart must be a NEW process, not a revived handle"
    );

    supervisor.shutdown().await;
}

/// An edit made while the child was DOWN arrives after the restart, through the
/// `changesSince` catch-up.
///
/// This is the claim the supervision was built for and the one that cannot be inferred
/// from any other test: `watch` only ever delivers from the moment it is armed, so an
/// edit during an outage is invisible to it. Only the catch-up replay can find it, and
/// the catch-up reports itself as `livesync:resume-catchup` -- a reason no live
/// notification ever carries, which is what makes this assertion unambiguous.
#[tokio::test]
async fn an_edit_made_while_the_child_was_down_arrives_through_the_catch_up() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    supervisor.ensure_ready().await.expect("the first handshake");
    let mut stream = backend.changes(None);
    // Arm the feed before the outage: an unarmed feed would make the catch-up below
    // trivially explainable by "it was never watching".
    for _ in 0..200 {
        if supervisor.health().watching {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        supervisor.health().watching,
        "the change feed must arm before the outage: {:?}",
        supervisor.health()
    );
    let cursor_before = supervisor.cursor();
    assert!(
        cursor_before.is_some(),
        "arming `watch` must record a cursor, or there is nothing to catch up FROM"
    );

    let pid = supervisor.child_pid().expect("a running child has a pid");
    kill_child(pid);

    // The edit lands with NO client connected. Nothing is listening on the change feed
    // at this moment, which is precisely the gap the catch-up exists to close.
    couch.push_note("Outage.md", "Written while the sidecar was dead.\n");

    // Any call restarts the child; the restart's own catch-up is what finds the edit.
    poll_until_ok("a read after the child was killed", || {
        backend.execute(BackendRequest::read_text("Notes/Alpha.md"))
    })
    .await;

    // The catch-up announces itself. Drained with a deadline rather than taking the
    // first event: a re-armed `watch` may also deliver, and the assertion is about the
    // catch-up specifically.
    let deadline = std::time::Instant::now() + RECOVERY_DEADLINE;
    let mut seen: Vec<String> = Vec::new();
    let mut caught_up = false;
    while std::time::Instant::now() < deadline && !caught_up {
        match tokio::time::timeout(Duration::from_millis(500), stream.recv()).await {
            Ok(Some(deep_obsidian_backend::ChangeEvent::Change(reason))) => {
                caught_up = reason == "livesync:resume-catchup";
                seen.push(reason);
            }
            Ok(Some(other)) => panic!("expected a Change event, got {other:?}"),
            Ok(None) => panic!("the change stream must survive a restart, not close"),
            Err(_) => {}
        }
    }
    assert!(
        caught_up,
        "the restart must replay `changesSince` and report it as a change; saw {seen:?}"
    );

    // The catch-up must have carried the PAYLOAD, not merely fired. Without this the test
    // would pass on any non-empty replay page, which proves the mechanism ran and says
    // nothing about whether the edit made during the outage survived it -- and that edit
    // is the entire claim.
    let paths = poll_until_ok("the manifest after the catch-up", || {
        supervisor.collect_manifest()
    })
    .await
    .into_iter()
    .map(|entry| entry.path)
    .collect::<Vec<_>>();
    assert!(
        paths.iter().any(|path| path == "Outage.md"),
        "the edit made during the outage must be in the vault afterwards; saw {paths:?}"
    );
    let recovered = backend
        .execute(BackendRequest::read_text("Outage.md"))
        .await
        .expect("the edit made during the outage must be readable")
        .into_text()
        .expect("text");
    assert_eq!(recovered, "Written while the sidecar was dead.\n");

    // And the feed is armed AGAIN, so the next live edit does not need another outage
    // to be noticed.
    assert!(
        supervisor.health().watching,
        "the restart must re-arm `watch`: {:?}",
        supervisor.health()
    );
    // The cursor moved past what it was before the outage: the catch-up consumed the
    // pages it replayed rather than replaying them forever.
    assert_ne!(
        supervisor.cursor(),
        cursor_before,
        "the catch-up must advance the cursor"
    );

    supervisor.shutdown().await;
}

/// While the remote is answering 500 to everything, reads FAIL -- and once it answers
/// again, they succeed, with no process restart and no stale content in between.
///
/// # What "no stale content" means on this backend
///
/// It means there is nowhere for stale content to come from. The CouchDB path has no
/// note cache at all: every read is a live `_bulk_get` against the remote, so a read
/// during an outage has no fallback to silently take. That is asserted rather than
/// assumed below -- a read that returned the content the PREVIOUS read returned would
/// be a cache appearing where the design says there is none, and it would be
/// indistinguishable from a fresh read to any caller.
///
/// (The Algolia backend DOES have a hydrated-note cache, and its own honesty rule is
/// different and stronger: the cache is keyed by head version, so serving from it
/// requires a successful head lookup against the live remote. See
/// `an_algolia_mount_that_goes_down_mid_session_fails_honestly_and_recovers` in the
/// server crate's `multi_vault.rs`.)
#[tokio::test]
async fn reads_fail_honestly_during_a_remote_outage_and_recover_when_it_ends() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    let before = backend
        .execute(BackendRequest::read_text("Notes/Alpha.md"))
        .await
        .expect("the pre-outage read")
        .into_text()
        .expect("text");
    assert!(!before.is_empty());
    let starts_before = supervisor.health().starts;

    // Wide enough to outlast any single operation's request budget, so the window is
    // closed by the test rather than by running out.
    couch.fail_next_requests(100_000);

    let error = backend
        .execute(BackendRequest::read_text("Notes/Alpha.md"))
        .await
        .expect_err("a read against a remote answering 500 must FAIL, not answer");
    let message = error.to_string();
    assert!(
        !message.contains(before.trim()),
        "the failure must not smuggle the previous read's content back out: {message}"
    );
    // A manifest walk fails too, rather than reporting an empty vault -- which a caller
    // could not tell from a vault whose notes were all deleted.
    backend
        .execute(BackendRequest::walk_markdown())
        .await
        .expect_err("a manifest walk during an outage must fail rather than report empty");

    couch.fail_next_requests(0);

    // The other shape of the same outage: the socket is DROPPED rather than answered.
    // Asserted separately because it reaches the sidecar's HTTP client as a transport
    // failure with no status code to classify, and a backend that only handled the 5xx
    // would report a dropped connection as something else -- most dangerously as an
    // empty result.
    couch.destroy_next_requests(100_000);
    backend
        .execute(BackendRequest::read_text("Notes/Alpha.md"))
        .await
        .expect_err("a read whose connection is dropped must FAIL, not answer");
    couch.destroy_next_requests(0);

    let recovered = poll_until_ok("a read after the remote outage ended", || {
        backend.execute(BackendRequest::read_text("Notes/Alpha.md"))
    })
    .await
    .into_text()
    .expect("text");
    assert_eq!(recovered, before, "the content is the remote's, unchanged");

    // A remote outage is not a child problem, so the child must not have been recycled
    // for it. Restarting on every 5xx would turn a brief remote blip into a handshake
    // storm.
    assert_eq!(
        supervisor.health().starts,
        starts_before,
        "a remote 500 must not restart the child: {:?}",
        supervisor.health()
    );

    supervisor.shutdown().await;
}

/// Cursors are NOT persisted across a process restart, and that is safe because a
/// supervisor with no cursor replays from the beginning.
///
/// # What this test claims, precisely
///
/// The `OpaqueCursor` a `changes(after)` call accepts is honoured for the life of the
/// SUPERVISOR: it outlives its child, so a child restart resumes from where the feed
/// had got to (proved by
/// `an_edit_made_while_the_child_was_down_arrives_through_the_catch_up`). Nothing
/// writes it to disk, so a SERVER restart -- or any rebuild of the backend from config
/// -- starts with no cursor.
///
/// That is asserted here rather than treated as a gap, because the consequence is the
/// safe one: `resume_watch` with no cursor calls `changesSince` with no `cursor`
/// parameter, which replays the whole feed, and the mount's index is rebuilt from a
/// full `manifest` at bootstrap anyway. A fresh backend therefore MISSES NOTHING; it
/// only does more work. Persistent cursors would make a restart cheaper and are not
/// claimed by anything in this repository.
#[tokio::test]
async fn a_rebuilt_backend_has_no_cursor_and_replays_everything_rather_than_missing_it() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");

    // --- The first backend, which advances a cursor. ---
    let (first, first_backend) = backend(&couch);
    first.ensure_ready().await.expect("the first handshake");
    let _first_stream = first_backend.changes(None);
    for _ in 0..200 {
        if first.health().watching {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(first.health().watching, "{:?}", first.health());

    couch.push_note("Before.md", "Written while the first backend was up.\n");
    // Wait for the cursor to actually move, so "the first backend had a cursor" is a
    // fact and not a hope.
    let advanced = poll_until_ok("the first backend's cursor to advance", || {
        let first = first.clone();
        async move {
            match first.cursor() {
                Some(cursor) => Ok(cursor),
                None => Err("no cursor yet".to_string()),
            }
        }
    })
    .await;
    assert!(!advanced.is_empty());

    // The whole backend goes away, as a server restart would take it.
    first.shutdown().await;
    drop(first_backend);
    drop(first);

    // A SECOND writer edits the vault while nothing at all is running. The mock IS that
    // second client: it is the only participant in this fixture besides the sidecar.
    couch.push_note("During.md", "Written while no backend existed.\n");

    // --- The rebuilt backend, from the same config. ---
    let (second, second_backend) = backend(&couch);
    assert_eq!(
        second.cursor(),
        None,
        "a backend rebuilt from config starts with NO cursor: they are not persisted"
    );

    // And it misses nothing: the full manifest carries both edits, the one made while
    // the first backend was up and the one made while none existed.
    let paths = poll_until_ok("the rebuilt backend's manifest", || {
        second.collect_manifest()
    })
    .await
    .into_iter()
    .map(|entry| entry.path)
    .collect::<Vec<_>>();
    for expected in ["Before.md", "During.md"] {
        assert!(
            paths.iter().any(|path| path == expected),
            "a rebuilt backend must see {expected}; it saw {paths:?}"
        );
    }
    // Listed AND readable: an entry a manifest names but a read cannot fetch would be a
    // worse answer than an omission.
    let during = second_backend
        .execute(BackendRequest::read_text("During.md"))
        .await
        .expect("the edit made while no backend existed must be readable")
        .into_text()
        .expect("text");
    assert_eq!(during, "Written while no backend existed.\n");

    // The replay starts from the beginning rather than from a cursor nobody stored.
    let replay = second
        .call("changesSince", serde_json::json!({}))
        .await
        .expect("a cursorless changesSince must be accepted");
    let changes = replay["changes"].as_array().expect("changes");
    assert!(
        !changes.is_empty(),
        "a cursorless replay must return the feed from its start: {replay}"
    );
    assert!(
        replay["nextCursor"].is_string(),
        "the replay must hand back a cursor to continue from: {replay}"
    );

    second.shutdown().await;
}

/// A remote that was DOWN at handshake time leaves the mount degraded, and bringing
/// the remote back does not by itself fix it -- the child has to re-handshake.
///
/// # Why this is asserted as a limit rather than as a recovery
///
/// The compatibility verdict is decided once, by `initialize`, and cached on BOTH
/// sides: the sidecar answers `health` from `state.vault.compatibilityStatus` rather
/// than re-probing, and the supervisor answers `ready_connection` from the health it
/// recorded. Nothing re-runs `initialize` while the child is alive, and the restart
/// backoff only runs when the connection has DIED -- an unreachable remote does not kill
/// the child, it only makes its verdict useless.
///
/// So the honest claim is the narrow one, and both halves of it are asserted below: an
/// outage at startup degrades the mount rather than failing the server, and the mount
/// comes back exactly when the child does. An operator whose CouchDB was down when the
/// service started has to restart the service (or wait for a transport failure to
/// recycle the child); that is a real limitation, and a test that polled until the mount
/// healed by itself would have been asserting something this code does not do.
#[tokio::test]
async fn a_remote_down_at_handshake_time_recovers_when_the_child_restarts_and_not_before() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    // The outage is in place BEFORE the first handshake, so `initialize` never sees a
    // working remote. Wide enough to outlast the handshake's whole request budget.
    couch.fail_next_requests(100_000);

    let (supervisor, backend) = backend(&couch);

    // The child starts and hand-shakes: an unreachable remote is a VERDICT, not a
    // crash. This is what keeps the server up and the vault root serving.
    supervisor
        .ensure_started()
        .await
        .expect("an unreachable remote must still complete the handshake");
    let degraded = supervisor.health();
    assert_eq!(degraded.starts, 1);
    assert!(
        !degraded.is_ready(),
        "a remote answering 500 to everything must not be reported serveable: {degraded:?}"
    );
    let status = degraded
        .compatibility
        .as_ref()
        .expect("a verdict")
        .status;
    assert_eq!(
        status,
        CompatibilityStatus::Unreachable,
        "a remote answering 500 to every request classifies as unreachable: {degraded:?}"
    );
    // Reads refuse with the verdict rather than answering an empty vault.
    let error = backend
        .execute(BackendRequest::read_text("Notes/Alpha.md"))
        .await
        .expect_err("a degraded mount must refuse reads");
    assert!(!error.to_string().is_empty());

    couch.fail_next_requests(0);

    // The remote is back, and the mount is STILL degraded -- repeatedly, including
    // through `probe_health`, which is the one call that refreshes the verdict from the
    // child and therefore the most likely place for a self-heal to happen if there were
    // one. A bounded number of attempts, because the assertion is that nothing changes.
    for attempt in 0..10 {
        let health = supervisor.probe_health().await;
        assert!(
            !health.is_ready(),
            "attempt {attempt}: the verdict is cached until a re-handshake, so it must \
             not flip on its own: {health:?}"
        );
        assert_eq!(
            health.compatibility.as_ref().expect("a verdict").status,
            status,
            "attempt {attempt}: the cached verdict must not drift either"
        );
        assert_eq!(health.starts, 1, "nothing may restart the child implicitly");
    }

    // Killing the child is what forces a fresh `initialize` -- and against the recovered
    // remote that one reports `ok`, so the mount serves again with no server restart.
    let pid = supervisor.child_pid().expect("a running child has a pid");
    kill_child(pid);

    let text = poll_until_ok("a read after the child re-handshook a recovered remote", || {
        backend.execute(BackendRequest::read_text("Notes/Alpha.md"))
    })
    .await
    .into_text()
    .expect("text");
    assert_eq!(text, "# Alpha\n\nFirst note body.\n");

    let recovered = supervisor.health();
    assert!(recovered.is_ready(), "{recovered:?}");
    assert_eq!(
        recovered.starts, 2,
        "the recovery is the RESTART, and health says so: {recovered:?}"
    );

    supervisor.shutdown().await;
}
