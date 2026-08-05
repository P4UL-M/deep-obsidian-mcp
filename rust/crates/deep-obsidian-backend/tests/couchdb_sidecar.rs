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
    MutationRequest, RecallRequest, VaultBackend, COUCHDB_READ_ONLY_MESSAGE,
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
///
/// `GrepSearch` is present on a READ-ONLY mount, which is the point of asserting it
/// here: line search is a read, so it is not one of the things `writable` gates.
#[tokio::test]
async fn writes_are_refused_with_the_experimental_read_only_message_but_grep_is_served() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    let (supervisor, backend) = backend(&couch);

    let descriptor = backend.descriptor();
    assert!(descriptor.supports(Capability::BinaryRead));
    assert!(descriptor.supports(Capability::Watch));
    assert!(descriptor.supports(Capability::GrepSearch));
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

    // Grep is SERVED, not refused: the fixture's `# Alpha` heading comes back with its
    // vault-relative path and its line number, from a mount with no files on disk.
    let outcome = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: "Alpha".to_string(),
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
            limit: 10,
        }))
        .await
        .expect("grep must be served")
        .into_grep_outcome()
        .expect("grep outcome");
    assert_eq!(
        outcome
            .matches
            .iter()
            .map(|item| (
                item.path.as_str(),
                item.line_number,
                item.line_text.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![("Notes/Alpha.md", 1, "# Alpha")]
    );
    // A full scan, so it carries ripgrep's own honesty shape.
    assert!(outcome.exhausted);
    assert_eq!(outcome.candidate_count, None);

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
    // Reads are unchanged by the mode, and so is grep: both are reads, and `writable`
    // gates writes. A mount that lost line search by staying read-only would be a
    // capability set that describes the wrong axis.
    assert!(descriptor.supports(Capability::BinaryRead));
    assert!(descriptor.supports(Capability::GrepSearch));
    assert!(read_only.descriptor().supports(Capability::GrepSearch));

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

    supervisor
        .ensure_ready()
        .await
        .expect("the first handshake");
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
    assert_eq!(health.starts, 2, "the restart must be counted: {health:?}");
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

    supervisor
        .ensure_ready()
        .await
        .expect("the first handshake");
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
    let status = degraded.compatibility.as_ref().expect("a verdict").status;
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

    let text = poll_until_ok(
        "a read after the child re-handshook a recovered remote",
        || backend.execute(BackendRequest::read_text("Notes/Alpha.md")),
    )
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

// ---------------------------------------------------------------------------
// Virtual grep: differential parity against REAL ripgrep
// ---------------------------------------------------------------------------

// The centrepiece of the CouchDB grep. Everything below seeds ONE corpus into two
// vaults — a temporary directory and the mock CouchDB — and asserts that the same
// `RecallRequest::Grep` produces IDENTICAL `GrepMatch`es through
// `FilesystemVaultBackend` (which spawns the real `rg`) and through
// `CouchDbVaultBackend` (which imitates it).
//
// A divergence here is a bug in the imitation, never a reason to weaken the
// comparison. The two accommodations that ARE made are documented at their use:
// results are sorted before comparison (ripgrep's inter-file order is parallel-walk
// nondeterministic), and no compared case truncates at `limit` (a truncated set under
// a nondeterministic order is an arbitrary set). The globs compared all lack a path
// separator, because ripgrep anchors those at the process CWD rather than at the
// searched tree — see `virtual_grep::GlobFilter`.

/// The three markdown notes `smallVault` already holds, with their exact bodies.
///
/// Duplicated from `test/fixtures.mjs` rather than derived, because the filesystem
/// half of the differential has to hold BYTE-IDENTICAL content and the fixture is the
/// definition of it. If the fixture changes, this test fails loudly on the corpus
/// comparison below rather than silently comparing two different vaults.
const FIXTURE_NOTES: &[(&str, &str)] = &[
    ("Notes/Alpha.md", "# Alpha\n\nFirst note body.\n"),
    ("Beta.md", "Beta note, single chunk.\n"),
    ("Conflicted.md", "Winning revision content.\n"),
    // The pre-chunking entry: whole content inline in `data`, no children. It is part
    // of the corpus a grep sees, so it is part of the filesystem half too.
    ("Legacy.md", "Legacy inline note body.\n"),
];

/// The notes pushed on top, each one present to exercise something specific.
const DIFFERENTIAL_NOTES: &[(&str, &str)] = &[
    // Several matches in one note, on NON-adjacent lines.
    (
        "Scan/Multiple.md",
        "needle one\nfiller\nneedle two\nfiller\nneedle three\n",
    ),
    // Matches on ADJACENT lines: every context window overlaps its neighbours, which
    // is where a "smart" deduplicating context would diverge.
    (
        "Scan/Adjacent.md",
        "before\nneedle A\nneedle B\nneedle C\nafter\n",
    ),
    // Unicode, so byte offsets and case folding are both exercised.
    (
        "Scan/Unicode.md",
        "éà café needle 你好 needle\nsecond café line\n",
    ),
    // A literal `.*`, which a fixed-string search must find and a regex search must
    // not confuse with "any characters".
    ("Scan/Regexish.md", "literal .* needle\nregex ab needle\n"),
    // Case variants of the pattern, for the case-sensitive/insensitive pair.
    (
        "Scan/Casing.md",
        "NEEDLE upper\nNeedle title\nneedle lower\n",
    ),
    // Excluded by the default `*.md` glob and included by `*.txt` — the corpus-scoping
    // half of the comparison.
    ("Scan/Excluded.txt", "needle in a text file\n"),
    // CRLF: the `\r` stays in `line_text` and in a submatch, and leaves the context
    // lines. Both halves are asserted separately below as well.
    (
        "Scan/Crlf.md",
        "first line\r\ncrlf needle here\r\nthird line\r\n",
    ),
    // Blank lines and a trailing terminator, so line NUMBERING is compared and not
    // just line content.
    ("Scan/Blank.md", "needle top\n\n\nneedle bottom\n\n"),
    // No trailing newline at all: the last line still counts, on both paths.
    ("Scan/NoTrailer.md", "first\nneedle last"),
    // Nested deeper than one level, so `**` and basename globbing are exercised.
    ("Scan/Deep/Nested.md", "deep needle nested\n"),
    // An EMPTY note. Ripgrep opens it and reports nothing (not even for an empty
    // pattern); so must the imitation, which is why it does not skip a `size == 0`
    // manifest entry. Both sides must agree that it contributes no match.
    ("Scan/Empty.md", ""),
];

/// The comparison key: unique per match, because every submatch on one line arrives
/// as a single event on both paths.
fn sort_matches(matches: &mut [deep_obsidian_backend::GrepMatch]) {
    matches.sort_by(|left, right| {
        (left.path.as_str(), left.line_number).cmp(&(right.path.as_str(), right.line_number))
    });
}

/// Render a match so an assertion failure names the exact field that diverged.
fn render(matches: &[deep_obsidian_backend::GrepMatch]) -> Vec<String> {
    matches
        .iter()
        .map(|item| {
            format!(
                "{}:{} text={:?} submatches={:?} before={:?} after={:?}",
                item.path,
                item.line_number,
                item.line_text,
                item.submatches
                    .iter()
                    .map(|sub| (sub.start, sub.end, sub.text.as_str()))
                    .collect::<Vec<_>>(),
                item.context_before
                    .iter()
                    .map(|line| (line.line_number, line.line_text.as_str()))
                    .collect::<Vec<_>>(),
                item.context_after
                    .iter()
                    .map(|line| (line.line_number, line.line_text.as_str()))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// A temp directory holding the whole corpus, for the ripgrep half.
fn seed_filesystem_corpus() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "couchdb-grep-parity-{}-{nanos}",
        std::process::id()
    ));
    for (path, text) in FIXTURE_NOTES.iter().chain(DIFFERENTIAL_NOTES.iter()) {
        let absolute = root.join(path);
        std::fs::create_dir_all(absolute.parent().expect("note parent")).expect("mkdir");
        std::fs::write(&absolute, text.as_bytes()).expect("seed a note");
    }
    root
}

/// One grep case, named so a failure says which case diverged.
struct Case {
    name: &'static str,
    query: &'static str,
    regex: bool,
    case_sensitive: bool,
    glob: Option<&'static str>,
    context_lines: usize,
}

/// Run one case against one backend.
async fn grep_case(
    backend: &dyn VaultBackend,
    case: &Case,
    limit: usize,
) -> Vec<deep_obsidian_backend::GrepMatch> {
    let mut matches = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: case.query.to_string(),
            regex: case.regex,
            case_sensitive: case.case_sensitive,
            glob: case.glob.map(str::to_string),
            context_lines: case.context_lines,
            limit,
        }))
        .await
        .unwrap_or_else(|error| panic!("[{}] grep failed: {error}", case.name))
        .into_grep_outcome()
        .expect("grep outcome")
        .matches;
    sort_matches(&mut matches);
    matches
}

/// **The differential parity test.** Same corpus, same params, two backends, byte-equal
/// matches.
#[tokio::test]
async fn the_virtual_grep_is_byte_identical_to_real_ripgrep_over_the_same_corpus() {
    require_prerequisites!();
    let ripgrep = deep_obsidian_backend::resolve_ripgrep();
    if !ripgrep.is_file() {
        eprintln!(
            "skipping: ripgrep (rg) was not resolved to a real binary, so there is no \
             reference implementation to compare against; install ripgrep or set \
             DEEP_OBSIDIAN_RIPGREP"
        );
        return;
    }

    let root = seed_filesystem_corpus();
    let filesystem = deep_obsidian_backend::FilesystemVaultBackend::with_ripgrep(&root, &ripgrep);

    let mut couch = MockCouch::start("small");
    for (path, text) in DIFFERENTIAL_NOTES {
        couch.push_note(path, text);
    }
    let (supervisor, couchdb) = backend(&couch);

    // Gate zero: the two vaults hold the SAME corpus. Without this the comparison
    // below could pass by both sides finding nothing.
    let mut fixture_paths: Vec<String> = FIXTURE_NOTES
        .iter()
        .chain(DIFFERENTIAL_NOTES.iter())
        .map(|(path, _)| (*path).to_string())
        .filter(|path| path.ends_with(".md"))
        .collect();
    fixture_paths.sort();
    let mut walked = couchdb
        .execute(BackendRequest::walk_markdown())
        .await
        .expect("walk the couchdb corpus")
        .into_markdown_files()
        .expect("markdown files");
    walked.sort();
    assert_eq!(
        walked, fixture_paths,
        "the couchdb vault must hold exactly the corpus the filesystem vault holds"
    );

    // `limit` is far above the match count of every case, so nothing truncates: a
    // truncated comparison would be comparing two arbitrary subsets, because ripgrep's
    // inter-file order is nondeterministic.
    const NO_TRUNCATION: usize = 500;
    let cases = [
        Case {
            name: "fixed string, default glob, no context",
            query: "needle",
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
        },
        Case {
            name: "fixed string with one context line either side",
            query: "needle",
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 1,
        },
        Case {
            name: "fixed string with a context window wider than the notes",
            query: "needle",
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 4,
        },
        Case {
            name: "case sensitive",
            query: "NEEDLE",
            regex: false,
            case_sensitive: true,
            glob: None,
            context_lines: 1,
        },
        Case {
            name: "case insensitive over the same variants",
            query: "NEEDLE",
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
        },
        Case {
            name: "fixed string of regex metacharacters",
            query: ".*",
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 1,
        },
        Case {
            name: "regex with a quantifier",
            query: "needle .*",
            regex: true,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
        },
        Case {
            name: "regex anchored at the line start",
            query: "^needle",
            regex: true,
            case_sensitive: false,
            glob: None,
            context_lines: 1,
        },
        Case {
            name: "regex anchored at the line end, which CRLF defeats",
            query: "needle$",
            regex: true,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
        },
        Case {
            name: "regex matching the carriage return of a CRLF line",
            query: "here\\r$",
            regex: true,
            case_sensitive: false,
            glob: None,
            context_lines: 1,
        },
        Case {
            name: "regex word boundary",
            query: "\\bneedle\\b",
            regex: true,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
        },
        Case {
            name: "unicode literal with case folding",
            query: "CAFÉ",
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 1,
        },
        Case {
            name: "explicit **/*.md glob",
            query: "needle",
            regex: false,
            case_sensitive: false,
            glob: Some("**/*.md"),
            context_lines: 1,
        },
        Case {
            name: "a non-markdown glob reaches the text file",
            query: "needle",
            regex: false,
            case_sensitive: false,
            glob: Some("*.txt"),
            context_lines: 1,
        },
        Case {
            name: "a basename glob",
            query: "needle",
            regex: false,
            case_sensitive: false,
            glob: Some("Adjacent.md"),
            context_lines: 2,
        },
        Case {
            name: "a brace-alternation glob",
            query: "needle",
            regex: false,
            case_sensitive: false,
            glob: Some("{Adjacent,Casing}.md"),
            context_lines: 0,
        },
        Case {
            name: "a character-class glob",
            query: "needle",
            regex: false,
            case_sensitive: false,
            glob: Some("[BN]*.md"),
            context_lines: 0,
        },
        Case {
            name: "a pattern with no match anywhere",
            query: "haystack-that-is-absent",
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 2,
        },
    ];

    for case in &cases {
        let expected = grep_case(&filesystem, case, NO_TRUNCATION).await;
        let actual = grep_case(&couchdb, case, NO_TRUNCATION).await;
        assert_eq!(
            render(&actual),
            render(&expected),
            "[{}] the virtual grep diverged from ripgrep",
            case.name
        );
        assert_eq!(
            actual, expected,
            "[{}] the virtual grep diverged from ripgrep",
            case.name
        );
    }

    // Every case above must actually have exercised something: a suite of eighteen
    // empty comparisons would pass.
    let hits = grep_case(&couchdb, &cases[1], NO_TRUNCATION).await;
    assert!(
        hits.len() >= 12,
        "the corpus must produce a substantial match set: {}",
        hits.len()
    );

    supervisor.shutdown().await;
    std::fs::remove_dir_all(&root).ok();
}

/// The `\r` asymmetry, pinned on the CouchDB path with real content flowing through
/// the sidecar.
///
/// `line_text` keeps the carriage return (it is ripgrep's `lines.text` minus the `\n`)
/// while the CONTEXT lines have it stripped, because context has never come from
/// ripgrep — the rg path slices the note itself. The differential test above compares
/// the two implementations; this one states the fact, so a future reader who
/// "normalizes" either half is told what they broke rather than left to infer it from
/// a parity failure.
#[tokio::test]
async fn crlf_survives_in_the_matched_line_and_is_stripped_from_context() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    couch.push_note("Crlf.md", "before\r\ncrlf needle here\r\nafter\r\n");
    let (supervisor, backend) = backend(&couch);

    let outcome = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: "needle".to_string(),
            regex: false,
            case_sensitive: false,
            glob: Some("Crlf.md".to_string()),
            context_lines: 1,
            limit: 10,
        }))
        .await
        .expect("grep")
        .into_grep_outcome()
        .expect("grep outcome");
    assert_eq!(outcome.matches.len(), 1);
    let found = &outcome.matches[0];
    assert_eq!(found.line_text, "crlf needle here\r");
    assert_eq!(found.context_before[0].line_text, "before");
    assert_eq!(found.context_after[0].line_text, "after");

    supervisor.shutdown().await;
}

/// `limit` truncates the OUTPUT without the outcome claiming the search was bounded —
/// the same shape the ripgrep path has always had, where `limit` breaks out of the
/// event loop and the outcome is still `exhaustive`.
///
/// Asserted on the CouchDB side only, deliberately: ripgrep's file order is
/// nondeterministic, so WHICH matches survive a truncation is not comparable. Here it
/// is: the scan runs in sorted path order, so the survivors are the alphabetically
/// first ones, every run.
#[tokio::test]
async fn the_limit_truncates_deterministically_without_claiming_to_be_bounded() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    couch.push_note("AAA.md", "needle 1\nneedle 2\nneedle 3\n");
    couch.push_note("ZZZ.md", "needle 4\n");
    let (supervisor, backend) = backend(&couch);

    let outcome = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: "needle".to_string(),
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 0,
            limit: 2,
        }))
        .await
        .expect("grep")
        .into_grep_outcome()
        .expect("grep outcome");
    assert_eq!(
        outcome
            .matches
            .iter()
            .map(|item| (item.path.as_str(), item.line_number))
            .collect::<Vec<_>>(),
        vec![("AAA.md", 1), ("AAA.md", 2)],
        "the first note in path order fills the budget"
    );
    // Truncated, and still not candidate-bounded: `exhausted` is about the SHORTLIST,
    // and there was none. The ripgrep path reports exactly this under a limit.
    assert!(outcome.exhausted);
    assert_eq!(outcome.candidate_count, None);

    supervisor.shutdown().await;
}

/// A grep is a full corpus read, and this measures it: the notes/second the scan
/// achieves through the sidecar, printed so the cost documented on
/// `CouchDbVaultBackend::grep` is a measured figure rather than a claim.
///
/// Not a performance ASSERTION — a threshold would be a flake on a loaded CI box. The
/// assertion is on correctness (every note was scanned); the timing is reported.
#[tokio::test]
async fn a_grep_reads_the_whole_corpus_and_reports_its_throughput() {
    require_prerequisites!();
    let mut couch = MockCouch::start("small");
    const PUSHED: usize = 40;
    for index in 0..PUSHED {
        couch.push_note(
            &format!("Bulk/Note{index:03}.md"),
            &format!("line one\nneedle in note {index}\nline three\n"),
        );
    }
    let (supervisor, backend) = backend(&couch);
    // Warm the handshake so the measurement is the scan, not the child's startup.
    backend
        .execute(BackendRequest::read_text("Beta.md"))
        .await
        .expect("warm read");

    let started = std::time::Instant::now();
    let outcome = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: "needle in note".to_string(),
            regex: false,
            case_sensitive: false,
            glob: Some("Bulk/**/*.md".to_string()),
            context_lines: 1,
            limit: 500,
        }))
        .await
        .expect("grep")
        .into_grep_outcome()
        .expect("grep outcome");
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.matches.len(),
        PUSHED,
        "every pushed note must be scanned"
    );
    eprintln!(
        "virtual grep: {PUSHED} notes read and matched in {elapsed:?} ({:.0} notes/sec through the sidecar)",
        PUSHED as f64 / elapsed.as_secs_f64()
    );

    supervisor.shutdown().await;
}

/// The passphrase the committed E2EE fixture was generated with.
///
/// Part of the DATA, not a configuration choice: HKDF derives the content key from
/// passphrase + salt, so this value and `test/fixtures/e2ee-written-vault.json` are
/// generated together (`npm run fixtures:e2ee`). See `test/e2ee-fixture.mjs`.
const E2EE_PASSPHRASE: &str = "correct-horse-battery-staple";

/// Grep over a REAL end-to-end-encrypted vault.
///
/// # Why this is not redundant with the parity test
///
/// The parity test proves the MATCHER is ripgrep's. This proves the matcher composes with
/// the READ PATH: the corpus it searches exists only as `h:+` chunks of genuine ciphertext
/// (produced by upstream's own key schedule), so every line it matches was decrypted
/// inside the sidecar on the way out. A virtual grep that had quietly special-cased
/// plaintext chunks — or that read a manifest field instead of the content — would pass
/// every other test in this file and fail this one.
///
/// The fixture note spans many chunks (300 lines), so chunk REASSEMBLY is exercised too:
/// a match on a line that straddles a chunk boundary in the wrong assembly order would
/// land on the wrong line number, and the line numbers are asserted exactly.
#[tokio::test]
async fn grep_reads_through_the_decrypting_read_path_on_an_e2ee_vault() {
    require_prerequisites!();
    let couch = MockCouch::start("e2ee");
    let mut config = config(&couch);
    config.credentials.e2ee_passphrase = Some(SecretString::new(E2EE_PASSPHRASE.to_string()));
    let supervisor = SidecarSupervisor::new(config);
    let backend = CouchDbVaultBackend::from_supervisor(supervisor.clone());

    // `secret line 42` is line 43 of the fixture's encrypted note (its lines are
    // generated `secret line {index}` for index 0..300).
    let outcome = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: "secret line 42:".to_string(),
            regex: false,
            case_sensitive: false,
            glob: None,
            context_lines: 1,
            limit: 10,
        }))
        .await
        .expect("grep must be served over an encrypted vault")
        .into_grep_outcome()
        .expect("grep outcome");
    assert_eq!(
        outcome
            .matches
            .iter()
            .map(|item| (item.path.as_str(), item.line_number))
            .collect::<Vec<_>>(),
        vec![("Notes/Encrypted.md", 43)],
        "the encrypted note's line 43 must be found at its real line number"
    );
    let found = &outcome.matches[0];
    assert!(
        found
            .line_text
            .starts_with("secret line 42: the quick brown fox"),
        "the decrypted line text must be the plaintext: {:?}",
        found.line_text
    );
    // Context came from the same decrypted body, at the neighbouring line numbers.
    assert_eq!(found.context_before[0].line_number, 42);
    assert!(found.context_before[0]
        .line_text
        .starts_with("secret line 41:"));
    assert_eq!(found.context_after[0].line_number, 44);
    assert!(found.context_after[0]
        .line_text
        .starts_with("secret line 43:"));
    assert!(outcome.exhausted);

    // A regex over the same encrypted corpus, to show the whole pattern surface works
    // through decryption rather than only a literal.
    let regexed = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: r"^secret line 29\d: ".to_string(),
            regex: true,
            case_sensitive: true,
            glob: Some("Encrypted.md".to_string()),
            context_lines: 0,
            limit: 50,
        }))
        .await
        .expect("regex grep over an encrypted vault")
        .into_grep_outcome()
        .expect("grep outcome");
    assert_eq!(
        regexed.matches.len(),
        10,
        "lines 290..299 inclusive: {:?}",
        regexed
            .matches
            .iter()
            .map(|item| item.line_number)
            .collect::<Vec<_>>()
    );
    assert_eq!(regexed.matches[0].line_number, 291);
    assert_eq!(regexed.matches[9].line_number, 300);

    // The binary attachment in the same vault is never a grep candidate: it is stored as
    // a `newnote`, and ripgrep would report no line matches for a binary file either.
    assert!(
        !outcome
            .matches
            .iter()
            .any(|item| item.path.contains("encrypted.bin")),
        "an encrypted binary attachment must not be scanned as text"
    );

    supervisor.shutdown().await;
}

/// A note written a moment ago is found by the very next grep.
///
/// # The bug this pins, and how it was found
///
/// The manifest has a two-second reuse window, and grep uses the manifest to decide what
/// "everywhere" means. Served from that cache, a grep issued right after a write scanned a
/// corpus snapshot that predated the write and reported NO MATCHES — while claiming
/// `exhausted: true`, i.e. that it had looked everywhere. The write had landed and a
/// `read_file` of the same path returned the new content, so nothing else was wrong.
///
/// Found by `scripts/demo-multi-backend.sh`, which writes a note through MCP and then
/// greps for a line in it. This test is the same sequence with no sleep anywhere: a
/// tolerance would have hidden exactly the defect it exists to catch.
#[tokio::test]
async fn a_grep_issued_immediately_after_a_write_sees_the_new_note() {
    require_prerequisites!();
    let couch = MockCouch::start_writable("small");
    let (supervisor, backend) = writable_backend(&couch);

    // Warm the manifest cache the way the server does — a listing right before the write
    // is what put a stale snapshot in it.
    let before = backend
        .execute(BackendRequest::walk_markdown())
        .await
        .expect("walk before the write")
        .into_markdown_files()
        .expect("markdown files");
    assert!(!before.contains(&"Team/Standup.md".to_string()));

    backend
        .execute(BackendRequest::write_text(
            "Team/Standup.md",
            "# Standup\n\nFederation status for the team. Owner: demo.\n",
        ))
        .await
        .expect("the write must land");

    // No sleep: the manifest cache is still well inside its window here, which is the
    // whole point.
    let outcome = backend
        .execute(BackendRequest::Recall(RecallRequest::Grep {
            query: "Owner".to_string(),
            regex: false,
            case_sensitive: false,
            glob: Some("Team/**/*.md".to_string()),
            context_lines: 1,
            limit: 10,
        }))
        .await
        .expect("grep")
        .into_grep_outcome()
        .expect("grep outcome");
    assert_eq!(
        outcome
            .matches
            .iter()
            .map(|item| (
                item.path.as_str(),
                item.line_number,
                item.line_text.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![(
            "Team/Standup.md",
            3,
            "Federation status for the team. Owner: demo."
        )],
        "a grep that claims to be exhaustive must see a note written a moment ago"
    );
    assert!(outcome.exhausted);

    supervisor.shutdown().await;
}
