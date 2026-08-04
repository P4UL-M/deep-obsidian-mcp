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
