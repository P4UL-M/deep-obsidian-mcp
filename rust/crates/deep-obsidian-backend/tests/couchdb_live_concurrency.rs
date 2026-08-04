//! Two concurrent clients against a REAL CouchDB.
//!
//! # What this proves that nothing else does
//!
//! The roadmap asks for "two concurrent clients: Obsidian/LiveSync and Deep Obsidian".
//! Without driving the Obsidian GUI, the honest proxy is two *independent* clients of the
//! same vault: two `CouchDbVaultBackend`s over two separate sidecar child processes, each
//! with its own CouchDB connection. Neither knows the other exists, which is exactly the
//! relationship Deep Obsidian has with a phone running LiveSync.
//!
//! Two layers already cover parts of this and are deliberately not repeated here:
//!
//! * the sidecar's own `live-couch.test.mjs` replays the whole compare-and-swap matrix
//!   against real CouchDB revision hashes, including two sidecars racing one base
//!   revision;
//! * `couchdb_sidecar.rs` covers the Rust conflict mapping hermetically, against the mock.
//!
//! So what is left, and what this file asserts, is that the two meet correctly: that the
//! Rust guarded-write path applied to REAL revision hashes yields exactly one winner and
//! one `BackendError::VersionConflict` naming the winner's revision, and that a write by
//! one client becomes visible to the other. A mock cannot prove the first (its revisions
//! are fabricated) and cannot prove the second at all.
//!
//! # Gating
//!
//! Skips, never fails, without `DEEP_OBSIDIAN_COUCHDB_URL`. The hermetic suites are the
//! contract and CI must not require a container.
//!
//! ```sh
//! docker run -d --name couch -p 5984:5984 \
//!   -e COUCHDB_USER=admin -e COUCHDB_PASSWORD=pw couchdb:3
//! DEEP_OBSIDIAN_COUCHDB_URL=http://127.0.0.1:5984 \
//! DEEP_OBSIDIAN_COUCHDB_USER=admin DEEP_OBSIDIAN_COUCHDB_PASSWORD=pw \
//!   cargo test -p deep-obsidian-backend --test couchdb_live_concurrency -- --nocapture
//! ```
//!
//! The scratch database is created and dropped by this test and is never the one named in
//! the environment.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use deep_obsidian_backend::sidecar::{
    SidecarConfig, SidecarCredentials, SidecarLaunch, SidecarMode, SidecarSupervisor,
};
use deep_obsidian_backend::{
    BackendError, BackendRequest, BaseVersion, CouchDbVaultBackend, VaultBackend,
};
use secrecy::SecretString;

/// Created and dropped by this file. Never the configured database.
const SCRATCH_DATABASE: &str = "deep-obsidian-rust-concurrency";

fn sidecar_dir() -> PathBuf {
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

/// The live CouchDB's connection details, or `None` when the suite must skip.
struct LiveTarget {
    url: String,
    username: String,
    password: String,
}

fn live_target() -> Option<LiveTarget> {
    let url = std::env::var("DEEP_OBSIDIAN_COUCHDB_URL").ok()?;
    if !bundle_path().is_file() {
        eprintln!(
            "skipping: {} is missing; run `npm ci && npm run build` in sidecar/livesync-sidecar",
            bundle_path().display()
        );
        return None;
    }
    Some(LiveTarget {
        url,
        username: std::env::var("DEEP_OBSIDIAN_COUCHDB_USER").unwrap_or_else(|_| "admin".into()),
        password: std::env::var("DEEP_OBSIDIAN_COUCHDB_PASSWORD").unwrap_or_else(|_| "pw".into()),
    })
}

macro_rules! require_live {
    () => {
        match live_target() {
            Some(target) => target,
            None => {
                eprintln!(
                    "skipping: set DEEP_OBSIDIAN_COUCHDB_URL to run the live concurrency tests"
                );
                return;
            }
        }
    };
}

/// A real, seeded, throwaway LiveSync database.
///
/// Dropping this closes the helper's stdin, which is what makes it DELETE the scratch
/// database — so a panicking test cannot leave one behind on someone's server.
struct ScratchVault {
    child: Child,
    database: String,
}

impl ScratchVault {
    fn create(target: &LiveTarget) -> Self {
        let mut child = Command::new("node")
            .arg("test/live-scratch.mjs")
            .args(["--url", &target.url])
            .args(["--user", &target.username])
            .args(["--password", &target.password])
            .args(["--database", SCRATCH_DATABASE])
            .current_dir(sidecar_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the live scratch-vault helper");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut handshake = String::new();
        stdout
            .read_line(&mut handshake)
            .expect("read the scratch handshake");
        let handshake: serde_json::Value =
            serde_json::from_str(handshake.trim()).expect("parse the scratch handshake");
        Self {
            database: handshake["database"]
                .as_str()
                .expect("database")
                .to_string(),
            child,
        }
    }
}

impl Drop for ScratchVault {
    fn drop(&mut self) {
        // Closing stdin is the helper's stop signal, and it reacts by DELETEing the
        // scratch database — a round trip to a real server. So it is given time to
        // finish, or a test run leaves a database behind on someone's CouchDB. Polled
        // rather than slept blind, and still bounded by the kill.
        drop(self.child.stdin.take());
        for _ in 0..60 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One independent client: its own supervisor, its own child process, its own connection.
///
/// Two of these share nothing but the remote, which is the whole point — a shared
/// supervisor would serialize the writes internally and prove nothing about concurrency.
fn client(
    target: &LiveTarget,
    vault: &ScratchVault,
) -> (Arc<SidecarSupervisor>, CouchDbVaultBackend) {
    let supervisor = SidecarSupervisor::new(SidecarConfig {
        launch: SidecarLaunch {
            node: PathBuf::from("node"),
            bundle: bundle_path(),
        },
        credentials: SidecarCredentials {
            url: target.url.clone(),
            database: vault.database.clone(),
            username: target.username.clone(),
            password: SecretString::new(target.password.clone()),
            e2ee_passphrase: None,
            e2ee_obfuscate_passphrase: None,
        },
        mode: SidecarMode::ReadWrite,
        options: None,
        request_timeout: Duration::from_secs(30),
        restart_backoff_base: Duration::from_millis(50),
    });
    (
        supervisor.clone(),
        CouchDbVaultBackend::from_supervisor(supervisor),
    )
}

async fn read_versioned(backend: &CouchDbVaultBackend, path: &str) -> (String, Option<String>) {
    backend
        .execute(BackendRequest::read_text(path))
        .await
        .unwrap_or_else(|error| panic!("read {path}: {error}"))
        .into_versioned_text()
        .expect("versioned text")
}

/// Two independent clients race a guarded write on the same path from the same base
/// revision. Exactly one wins; the loser is told, and no version is lost.
#[tokio::test]
async fn two_independent_clients_racing_one_revision_produce_exactly_one_winner() {
    let target = require_live!();
    let vault = ScratchVault::create(&target);
    let (supervisor_a, alice) = client(&target, &vault);
    let (supervisor_b, bob) = client(&target, &vault);

    // Alice creates the note. Bob then reads it, so both hold the SAME revision — the
    // situation two devices are in after syncing and then editing offline.
    let path = "Notes/Contended.md";
    alice
        .execute(BackendRequest::write_text_guarded(
            path,
            "# Contended\n\nthe original\n",
            BaseVersion::Absent,
        ))
        .await
        .expect("the create must land");

    let (_, alice_base) = read_versioned(&alice, path).await;
    let (_, bob_base) = read_versioned(&bob, path).await;
    let alice_base = alice_base.expect("a real revision");
    let bob_base = bob_base.expect("a real revision");
    assert_eq!(
        alice_base, bob_base,
        "both clients must start from the same revision"
    );
    // A REAL CouchDB revision — `<generation>-<32 hex>` — not a fabricated one. This is
    // the thing the mock cannot give, and therefore the reason this test exists: the Rust
    // conflict mapping has to survive an adjudicator whose revisions it cannot predict.
    let (generation, hash) = alice_base
        .split_once('-')
        .unwrap_or_else(|| panic!("a CouchDB revision is <generation>-<hash>: {alice_base}"));
    assert!(
        generation.parse::<u32>().is_ok(),
        "the revision generation must be numeric: {alice_base}"
    );
    assert!(
        hash.len() == 32 && hash.chars().all(|character| character.is_ascii_hexdigit()),
        "expected a real 32-hex-digit CouchDB revision hash, got {alice_base}"
    );

    // Both write, concurrently, from that revision.
    let alice_text = "# Contended\n\nwritten by client A\n";
    let bob_text = "# Contended\n\nwritten by client B\n";
    let (from_alice, from_bob) = tokio::join!(
        alice.execute(BackendRequest::write_text_guarded(
            path,
            alice_text,
            BaseVersion::Version(alice_base.clone()),
        )),
        bob.execute(BackendRequest::write_text_guarded(
            path,
            bob_text,
            BaseVersion::Version(bob_base.clone()),
        )),
    );

    let winners = [&from_alice, &from_bob]
        .iter()
        .filter(|outcome| outcome.is_ok())
        .count();
    assert_eq!(
        winners, 1,
        "exactly one write may win a contested revision; got A={from_alice:?} B={from_bob:?}"
    );

    // The loser's error names the winner's revision, which is what makes recovery a
    // re-read rather than a guess.
    let (loser, winner_text) = match (&from_alice, &from_bob) {
        (Err(error), Ok(_)) => (error, bob_text),
        (Ok(_), Err(error)) => (error, alice_text),
        other => panic!("expected exactly one failure: {other:?}"),
    };
    assert!(
        matches!(loser, BackendError::VersionConflict { .. }),
        "the loser must get a version conflict, not something generic: {loser:?}"
    );
    let message = loser.to_string();
    assert!(
        message.starts_with(&format!("hash conflict for {path}:")),
        "the loser's error lands in the existing taxonomy: {message}"
    );
    assert!(
        message.contains("nothing was written"),
        "the loser must be told its write did not happen: {message}"
    );

    // No version was lost: the winner's content is what the vault holds, unmerged.
    let (final_text, final_rev) = read_versioned(&alice, path).await;
    assert_eq!(
        final_text, winner_text,
        "the winner's content must be what the vault holds, with no merge"
    );
    // And the losing write created NO conflict branch: a guarded write extends the
    // winning revision only.
    let conflicts = alice
        .conflicts(path)
        .await
        .expect("conflicts must be listable");
    assert!(
        conflicts.conflicts.is_empty(),
        "a lost compare-and-swap must not fork the revision tree: {conflicts:?}"
    );
    assert_eq!(conflicts.winning, final_rev.expect("a revision"));

    // Recovery: the loser re-reads and retries, and now succeeds. This is the documented
    // remedy, so it is asserted rather than assumed.
    let losing_backend = if from_alice.is_err() { &alice } else { &bob };
    let (_, fresh) = read_versioned(losing_backend, path).await;
    losing_backend
        .execute(BackendRequest::write_text_guarded(
            path,
            "# Contended\n\nthe loser, after re-reading\n",
            BaseVersion::from_read(fresh),
        ))
        .await
        .expect("a retry under the fresh revision must succeed");

    supervisor_a.shutdown().await;
    supervisor_b.shutdown().await;
}

/// A write by one client becomes visible to the other, within a bounded wait.
///
/// Two separate processes, two separate connections: this is the property that makes
/// "Deep Obsidian and another LiveSync client on the same vault" workable at all, and
/// nothing hermetic can demonstrate it.
#[tokio::test]
async fn a_write_by_one_client_becomes_visible_to_the_other() {
    let target = require_live!();
    let vault = ScratchVault::create(&target);
    let (supervisor_a, alice) = client(&target, &vault);
    let (supervisor_b, bob) = client(&target, &vault);

    let path = "Notes/Shared.md";
    let body = "# Shared\n\nwritten by A, read by B\n";
    alice
        .execute(BackendRequest::write_text_guarded(
            path,
            body,
            BaseVersion::Absent,
        ))
        .await
        .expect("A's write must land");

    // Bob's read: the content is in CouchDB the moment A's write returns, so this should
    // be immediate. It is polled anyway rather than asserted once, because "eventually
    // visible" is the honest contract for a replicated store and a flaky assertion here
    // would be worse than a slightly slower one.
    let mut seen = None;
    for _ in 0..40 {
        if let Ok(response) = bob.execute(BackendRequest::read_text(path)).await {
            if let Ok(text) = response.into_text() {
                if text == body {
                    seen = Some(text);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        seen.as_deref(),
        Some(body),
        "A's write must become visible to B within 10s"
    );

    // And through B's MANIFEST, not only through a direct read by path — that is what the
    // index refresh walks, so a write invisible there would be a write the other client
    // never indexes.
    let mut listed = false;
    for _ in 0..40 {
        let entries = bob
            .manifest_entries()
            .await
            .expect("B must be able to list the vault");
        if entries.iter().any(|entry| entry.path == path) {
            listed = true;
            break;
        }
        // The backend reuses a very recently collected manifest, so the wait must exceed
        // that window for the next call to actually re-walk.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(listed, "A's write must appear in B's manifest within 20s");

    supervisor_a.shutdown().await;
    supervisor_b.shutdown().await;
}
