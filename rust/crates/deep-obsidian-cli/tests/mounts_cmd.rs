//! `mounts add` / `mounts list` / `mounts remove`, at both layers that can break.
//!
//! # The two layers, and why both
//!
//! The library layer (`add_with_resolver` and friends) owns the semantics: the legacy
//! migration, the full-table validation, secret handling, the probe decision, the removal
//! refusals. Those are tested against a temp config file and a temp secrets file, with the
//! Algolia mock standing in for a remote.
//!
//! The **argv** layer owns the wiring, and it is tested by spawning the REAL binary. That
//! is not belt-and-braces: `couchdb export` was unreachable from the command line from the
//! commit that introduced it, because the arg normalizer promoted the word `couchdb` to
//! `--vault couchdb`, and every hermetic test kept passing because none of them went
//! through argv. `mounts add filesystem` nests one level deeper than that bug did, and
//! `--vault-path` is a flag the normalizer already rewrites — two fresh chances to repeat
//! it. Hence the subprocess tests at the bottom.
//!
//! # What is NOT tested here, and why
//!
//! The OS keyring. Every secret path runs with `prefer_os_keyring = false`, so a test run
//! never writes into the developer's login keychain (a `SecretResolver` routes by
//! reference SHAPE, so a temp secrets file alone would not prevent that). The masked
//! interactive prompt is likewise not driven: `rpassword` needs a tty. The stdin route
//! that `--password-stdin` / `--api-key-stdin` take IS driven, through
//! `SecretReader::from_lines`, which is the same code path with the same call order.

use std::path::{Path, PathBuf};

use deep_obsidian_algolia::mock::spawn_mock;
use deep_obsidian_cli::cli::{MountsAddCommon, MountsAddKind};
use deep_obsidian_cli::mounts_cmd::{add_with_resolver, list, remove, SecretReader};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_types::SecretRef;

const API_KEY: &str = "test-mounts-api-key";

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
        "dob-mounts-cmd-{prefix}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// One test's world: a root vault, a folder for a second mount, and a config file.
struct Fixture {
    base: PathBuf,
    config_path: PathBuf,
    resolver: SecretResolver,
}

impl Fixture {
    /// A LEGACY config: `vaultPath` only, plus one key this build has never heard of, so
    /// every write is also a retention test.
    fn legacy(name: &str) -> Self {
        let base = temp_dir(name);
        std::fs::create_dir_all(base.join("vault")).expect("vault dir");
        std::fs::create_dir_all(base.join("team")).expect("team dir");
        let config_path = base.join("config.json");
        std::fs::write(
            &config_path,
            format!(
                r#"{{
  "vaultPath": {vault},
  "indexDir": {index},
  "transport": "http",
  "somethingThisBuildDoesNotKnow": {{ "keep": "me" }}
}}
"#,
                vault = json_string(&base.join("vault")),
                index = json_string(&base.join("index")),
            ),
        )
        .expect("write legacy config");
        Self {
            resolver: SecretResolver::with_encrypted_file_path(base.join("secrets.json")),
            base,
            config_path,
        }
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config_path).expect("read config")
    }

    fn config_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.config_text()).expect("config is json")
    }

    fn backup_path(&self) -> PathBuf {
        self.config_path.with_extension("json.bak")
    }
}

fn json_string(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("json string")
}

fn common(id: &str, mount_at: &str) -> MountsAddCommon {
    MountsAddCommon {
        id: id.to_string(),
        mount_at: mount_at.to_string(),
        keep_anyway: false,
        // Every library-level test passes `--yes`. The prompt itself is covered at the
        // argv layer, where the subprocess gets a closed stdin and so takes the refusal
        // branch — which is the branch worth proving, because it must write nothing.
        yes: true,
    }
}

fn filesystem(id: &str, mount_at: &str, vault_path: &Path) -> MountsAddKind {
    MountsAddKind::Filesystem {
        common: common(id, mount_at),
        vault_path: vault_path.to_path_buf(),
    }
}

fn algolia(id: &str, mount_at: &str, base_url: &str, index_name: &str) -> MountsAddKind {
    MountsAddKind::Algolia {
        common: common(id, mount_at),
        app_id: "TESTAPP".to_string(),
        index_name: index_name.to_string(),
        base_url: Some(base_url.to_string()),
        api_key_stdin: true,
        writable: false,
        participant_id: Some("tester@fixture".to_string()),
    }
}

/// The resolved vault path and index dir a config produces, through the real loader.
///
/// The equivalence the legacy migration has to preserve. Deliberately NOT a byte
/// comparison of the file: the migration rewrites the file on purpose (that is the whole
/// point), and what must be unchanged is what the SERVER resolves from it.
fn resolved_shape(config_path: &Path) -> (Option<PathBuf>, PathBuf) {
    let file = deep_obsidian_config::read_config_file(config_path)
        .expect("load config")
        .expect("config exists");
    let resolved =
        deep_obsidian_config::normalize_service_config(deep_obsidian_types::ServiceConfigInput {
            vault_path: file.vault_path,
            mounts: file.mounts,
            experimental: file.experimental,
            index_dir: file.index_dir,
            transport: file.transport,
            stdio_mode: file.stdio_mode,
            http: file.http,
            auto_reindex: file.auto_reindex,
            embedding: file.embedding,
            artifact_embedding: file.artifact_embedding,
            auth: file.auth,
            federated_rerank: file.federated_rerank,
            config_file_path: Some(config_path.to_path_buf()),
        })
        .expect("the config resolves");
    (resolved.vault_path.clone(), resolved.index_dir.clone())
}

// ---------------------------------------------------------------------------
// Legacy migration
// ---------------------------------------------------------------------------

/// A `vaultPath`-only config becomes an explicit root mount plus the new one, and resolves
/// to exactly the same vault path and index directory it did before.
///
/// Three properties in one test because they are one event: the migration, the `.bak`, and
/// the retention of a key this build cannot interpret. A config edit that silently dropped
/// a future key would corrupt an install written by a newer build.
#[tokio::test]
async fn legacy_migration_adds_a_root_mount_and_preserves_what_it_cannot_read() {
    let fixture = Fixture::legacy("legacy");
    let before = resolved_shape(&fixture.config_path);
    let original = fixture.config_text();

    let report = add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("team", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("the migration succeeds");

    assert!(report.written);
    assert_eq!(report.migrated_root.as_deref(), Some("vault"));
    // A second mount is a multi-mount table, which needs the flag; `--yes` stood in for the
    // confirmation and the flag was ENABLED, not assumed.
    assert_eq!(report.experimental_enabled, vec!["multiVault".to_string()]);
    assert!(report.probe.ok, "{:?}", report.probe);

    let json = fixture.config_json();
    let mounts = json["mounts"].as_array().expect("a mount table");
    assert_eq!(mounts.len(), 2);
    assert_eq!(mounts[0]["id"], "vault");
    assert_eq!(mounts[0]["mountAt"], "");
    assert_eq!(
        mounts[0]["backend"]["vaultPath"],
        fixture.base.join("vault").display().to_string()
    );
    assert_eq!(mounts[1]["id"], "team");
    assert_eq!(mounts[1]["mountAt"], "Team");
    // The two are mutually exclusive on input, so the migration must drop `vaultPath`.
    assert!(json["vaultPath"].is_null(), "{}", fixture.config_text());
    // The key this build does not know survived the load → modify → save round trip.
    assert_eq!(json["somethingThisBuildDoesNotKnow"]["keep"], "me");

    // The 7a safety net engaged: the previous file is still readable, byte for byte.
    assert_eq!(
        std::fs::read_to_string(fixture.backup_path()).expect("a backup exists"),
        original,
        "the .bak must hold the pre-migration content"
    );
    assert_eq!(
        report.backup_path.as_deref(),
        Some(fixture.backup_path().as_path())
    );

    // The equivalence proven in slice 2a, re-proven through this command: an explicit root
    // mount resolves exactly as the `vaultPath` it replaces.
    let after = resolved_shape(&fixture.config_path);
    assert_eq!(before, after);
}

/// Only the flags an addition actually needs are enabled — never a backend flag it does
/// not use.
///
/// The rule mirrors `normalize_service_config`'s own: the backend kind decides
/// `couchdbVaults` / `algoliaVaults`, and a table of more than one mount decides
/// `multiVault`. A second FILESYSTEM mount therefore turns on exactly one flag.
#[tokio::test]
async fn only_the_experimental_flags_this_addition_needs_are_enabled() {
    let fixture = Fixture::legacy("flags");
    let report = add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("team", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("add");
    assert_eq!(report.experimental_enabled, vec!["multiVault".to_string()]);

    let json = fixture.config_json();
    assert_eq!(json["experimental"]["multiVault"], true);
    assert_eq!(json["experimental"]["couchdbVaults"], false);
    assert_eq!(json["experimental"]["algoliaVaults"], false);
}

/// The global `--index-dir` sets the NEW mount's own index directory, and leaves the
/// config's top-level `indexDir` alone.
///
/// The flag is reused rather than redeclared (a subcommand redeclaring a global clap
/// argument is a duplicate-argument panic), so which of the two it means is a decision
/// worth pinning.
#[tokio::test]
async fn the_index_dir_flag_sets_the_new_mounts_index_not_the_root_one() {
    let fixture = Fixture::legacy("index-dir");
    let elsewhere = fixture.base.join("elsewhere");
    let report = add_with_resolver(
        &fixture.config_path,
        Some(&elsewhere),
        &filesystem("team", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("add");
    assert_eq!(report.index_dir, elsewhere);
    let json = fixture.config_json();
    assert_eq!(
        json["mounts"][1]["backend"]["indexDir"],
        elsewhere.display().to_string()
    );
    assert_eq!(
        json["indexDir"],
        fixture.base.join("index").display().to_string(),
        "the top-level indexDir must be untouched"
    );
}

// ---------------------------------------------------------------------------
// Full-table validation
// ---------------------------------------------------------------------------

/// A duplicate id is refused by the REAL loader, and nothing is written.
///
/// The message must carry the loader's own words: `main` prints only the outermost error,
/// so a `with_context` would have replaced "duplicate mount id" with "the table is not
/// valid" and lost the actionable half.
#[tokio::test]
async fn a_duplicate_id_is_refused_and_nothing_is_written() {
    let fixture = Fixture::legacy("dup-id");
    add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("team", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("first add");
    let after_first = fixture.config_text();

    let error = add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("team", "Other", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect_err("a duplicate id is refused");
    let message = error.to_string();
    assert!(message.contains("duplicate mount id"), "{message}");
    assert!(message.contains("team"), "{message}");
    assert!(message.contains("Nothing was written"), "{message}");
    assert_eq!(
        fixture.config_text(),
        after_first,
        "the config was rewritten"
    );
}

/// A duplicate `mountAt` is refused the same way. Same single implementation, different
/// rule — which is the point: neither is restated in this crate.
#[tokio::test]
async fn a_duplicate_mount_at_is_refused() {
    let fixture = Fixture::legacy("dup-at");
    add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("team", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("first add");
    let after_first = fixture.config_text();

    let error = add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("team-two", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect_err("a duplicate mountAt is refused");
    assert!(error.to_string().contains("Team"), "{error}");
    assert_eq!(
        fixture.config_text(),
        after_first,
        "a refused add must not have touched the file"
    );
}

/// An invalid id never reaches the file: the id rule lives in the config crate and is
/// enforced here by calling it.
#[tokio::test]
async fn an_invalid_mount_id_is_refused_by_the_loaders_own_rule() {
    let fixture = Fixture::legacy("bad-id");
    let error = add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("Team Alpha", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect_err("an invalid id is refused");
    assert!(error.to_string().contains("mount id"), "{error}");
    assert!(!fixture.backup_path().exists(), "nothing should be written");
}

// ---------------------------------------------------------------------------
// Probe: filesystem
// ---------------------------------------------------------------------------

/// A vault directory that does not exist fails the probe, and the config is NOT written.
#[tokio::test]
async fn a_missing_directory_fails_the_probe_and_aborts() {
    let fixture = Fixture::legacy("no-dir");
    let original = fixture.config_text();
    let error = add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("gone", "Gone", &fixture.base.join("does-not-exist")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect_err("a missing directory aborts");
    let message = error.to_string();
    assert!(message.contains("did not pass its probe"), "{message}");
    assert!(message.contains("--keep-anyway"), "{message}");
    assert_eq!(fixture.config_text(), original);
    assert!(!fixture.backup_path().exists());
}

/// `--keep-anyway` writes the same mount, and says so.
#[tokio::test]
async fn keep_anyway_writes_a_mount_that_failed_its_probe() {
    let fixture = Fixture::legacy("keep-anyway");
    let kind = MountsAddKind::Filesystem {
        common: MountsAddCommon {
            keep_anyway: true,
            ..common("gone", "Gone")
        },
        vault_path: fixture.base.join("does-not-exist"),
    };
    let report = add_with_resolver(
        &fixture.config_path,
        None,
        &kind,
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("--keep-anyway writes anyway");
    assert!(report.written);
    assert!(!report.probe.ok);
    assert!(
        report
            .messages
            .iter()
            .any(|message| message.contains("--keep-anyway") && message.contains("degraded")),
        "{:?}",
        report.messages
    );
    assert_eq!(fixture.config_json()["mounts"][1]["id"], "gone");
}

// ---------------------------------------------------------------------------
// Secrets and the remote probe
// ---------------------------------------------------------------------------

/// An algolia mount stores its key in the secret store and puts only the REFERENCE in the
/// config file.
///
/// The assertion that matters is the negative one, made against the persisted BYTES: the
/// key must not appear anywhere in the file, whatever the serializer decided to do.
#[tokio::test]
async fn an_algolia_mount_persists_a_reference_and_never_the_key() {
    let fixture = Fixture::legacy("algolia-ref");
    let (base_url, _mock) = spawn_mock().await;

    let report = add_with_resolver(
        &fixture.config_path,
        None,
        &algolia("wiki", "_Wiki", &base_url, "wiki-ref"),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(vec![API_KEY.to_string()]),
    )
    .await
    .expect("the algolia mount is added");

    assert!(report.written, "{:?}", report.messages);
    assert!(report.probe.ok, "{:?}", report.probe);
    assert_eq!(
        report.experimental_enabled,
        vec!["algoliaVaults".to_string(), "multiVault".to_string()]
    );

    let text = fixture.config_text();
    assert!(
        !text.contains(API_KEY),
        "the API key reached the config file: {text}"
    );
    let reference = SecretRef::EncryptedFile {
        id: "mount-wiki-api-key".to_string(),
    };
    assert_eq!(
        fixture.config_json()["mounts"][1]["backend"]["apiKeyRef"],
        serde_json::json!({ "kind": "encryptedFile", "id": "mount-wiki-api-key" })
    );
    // ...and the reference resolves to the value that never touched the file.
    let stored = fixture
        .resolver
        .get(&reference)
        .expect("read the stored secret")
        .expect("the secret is present");
    assert_eq!(secrecy::ExposeSecret::expose_secret(&stored), API_KEY);
    // The report names the reference, which is what an operator needs to rotate it, and
    // still carries no value.
    assert_eq!(
        report.secret_refs,
        vec!["encryptedFile id=mount-wiki-api-key".to_string()]
    );
    assert!(!serde_json::to_string(&report)
        .expect("serialize the report")
        .contains(API_KEY));
}

/// A remote that cannot be reached aborts the add AND removes the credential it had just
/// stored.
///
/// The cleanup is the load-bearing half. Leaving the secret behind would orphan a
/// credential in the operator's store that no config references and nothing will ever
/// clean up; and it is safe to remove precisely because validation already proved the
/// mount id is not in the table, so the id-keyed reference cannot be a live mount's.
#[tokio::test]
async fn an_unreachable_remote_aborts_and_removes_the_credential_it_stored() {
    let fixture = Fixture::legacy("algolia-down");
    let original = fixture.config_text();

    let error = add_with_resolver(
        &fixture.config_path,
        None,
        // Port 1 on loopback: nothing listens and the connection fails immediately.
        &algolia("wiki", "_Wiki", "http://127.0.0.1:1/", "wiki-down"),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(vec![API_KEY.to_string()]),
    )
    .await
    .expect_err("an unreachable index aborts");
    let message = error.to_string();
    assert!(message.contains("did not pass its probe"), "{message}");
    assert!(
        message.contains("removed the credential stored at"),
        "{message}"
    );
    assert!(!message.contains(API_KEY), "{message}");

    assert_eq!(fixture.config_text(), original, "the config was written");
    assert!(!fixture.backup_path().exists());
    assert!(
        fixture
            .resolver
            .get(&SecretRef::EncryptedFile {
                id: "mount-wiki-api-key".to_string(),
            })
            .expect("read the store")
            .is_none(),
        "the credential was left behind"
    );
}

/// `--keep-anyway` on an unreachable remote KEEPS the credential, because the mount that
/// references it is now in the config.
#[tokio::test]
async fn keep_anyway_on_an_unreachable_remote_keeps_the_credential() {
    let fixture = Fixture::legacy("algolia-keep");
    let mut kind = algolia("wiki", "_Wiki", "http://127.0.0.1:1/", "wiki-keep");
    if let MountsAddKind::Algolia { common, .. } = &mut kind {
        common.keep_anyway = true;
    }
    let report = add_with_resolver(
        &fixture.config_path,
        None,
        &kind,
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(vec![API_KEY.to_string()]),
    )
    .await
    .expect("--keep-anyway writes anyway");
    assert!(report.written);
    assert!(!report.probe.ok);
    assert!(fixture
        .resolver
        .get(&SecretRef::EncryptedFile {
            id: "mount-wiki-api-key".to_string(),
        })
        .expect("read the store")
        .is_some());
}

/// A couchdb mount with `--e2ee` reads TWO secrets from stdin, in the documented order,
/// and persists both as references only.
///
/// The order is what makes the non-interactive path usable, and it is the thing a script
/// can get wrong invisibly: swapped, each secret would be stored under the other's
/// reference and the failure would surface much later as an authentication error. So it is
/// asserted by reading each reference back.
///
/// `--keep-anyway` is used because this fixture has no CouchDB and no sidecar, so the
/// probe cannot pass — which is exactly the state the next test exercises.
#[tokio::test]
async fn a_couchdb_mount_reads_password_then_passphrase_and_persists_only_references() {
    let fixture = Fixture::legacy("couchdb-secrets");
    let kind = MountsAddKind::Couchdb {
        common: MountsAddCommon {
            keep_anyway: true,
            ..common("archive", "Archive")
        },
        url: "https://couch.invalid".to_string(),
        database: "vault".to_string(),
        username: Some("reader".to_string()),
        password_stdin: true,
        writable: false,
        e2ee: true,
        sidecar_path: None,
    };
    let report = add_with_resolver(
        &fixture.config_path,
        None,
        &kind,
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(vec![
            "the-password".to_string(),
            "the-passphrase".to_string(),
        ]),
    )
    .await
    .expect("--keep-anyway writes the mount");

    assert!(report.written);
    assert_eq!(
        report.experimental_enabled,
        vec!["couchdbVaults".to_string(), "multiVault".to_string()]
    );

    let text = fixture.config_text();
    for secret in ["the-password", "the-passphrase"] {
        assert!(
            !text.contains(secret),
            "{secret} reached the config: {text}"
        );
    }
    let backend = &fixture.config_json()["mounts"][1]["backend"];
    assert_eq!(
        backend["passwordRef"],
        serde_json::json!({ "kind": "encryptedFile", "id": "mount-archive-password" })
    );
    assert_eq!(
        backend["e2ee"]["passphraseRef"],
        serde_json::json!({ "kind": "encryptedFile", "id": "mount-archive-e2ee-passphrase" })
    );
    // Path obfuscation is a second, independent passphrase and is deliberately not guessed
    // at: a wrong one makes every read look like corruption.
    assert!(backend["e2ee"]["obfuscatePassphraseRef"].is_null());
    // `username` stays plaintext on purpose: a CouchDB user name is an identifier.
    assert_eq!(backend["username"], "reader");

    for (id, expected) in [
        ("mount-archive-password", "the-password"),
        ("mount-archive-e2ee-passphrase", "the-passphrase"),
    ] {
        let stored = fixture
            .resolver
            .get(&SecretRef::EncryptedFile { id: id.to_string() })
            .expect("read the store")
            .unwrap_or_else(|| panic!("{id} was not stored"));
        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&stored),
            expected,
            "{id} holds the wrong secret — the stdin order was not honoured"
        );
    }
}

/// Without `--keep-anyway`, a couchdb mount that cannot handshake aborts and BOTH secrets
/// it stored are removed.
#[tokio::test]
async fn a_couchdb_mount_that_cannot_handshake_removes_both_secrets() {
    let fixture = Fixture::legacy("couchdb-abort");
    let original = fixture.config_text();
    let kind = MountsAddKind::Couchdb {
        common: common("archive", "Archive"),
        url: "http://127.0.0.1:1".to_string(),
        database: "vault".to_string(),
        username: None,
        password_stdin: true,
        writable: false,
        e2ee: true,
        sidecar_path: None,
    };
    let error = add_with_resolver(
        &fixture.config_path,
        None,
        &kind,
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(vec!["pw".to_string(), "pp".to_string()]),
    )
    .await
    .expect_err("no CouchDB and no sidecar means no handshake");
    let message = error.to_string();
    assert!(message.contains("did not pass its probe"), "{message}");
    assert_eq!(fixture.config_text(), original);
    for id in ["mount-archive-password", "mount-archive-e2ee-passphrase"] {
        assert!(
            fixture
                .resolver
                .get(&SecretRef::EncryptedFile { id: id.to_string() })
                .expect("read the store")
                .is_none(),
            "{id} was orphaned in the secret store"
        );
        assert!(
            message.contains(id),
            "the abort did not name {id}: {message}"
        );
    }
}

/// Running out of stdin lines is an error that names the secret it was waiting for, not a
/// silently empty credential.
#[tokio::test]
async fn a_missing_stdin_line_names_the_secret_it_was_waiting_for() {
    let fixture = Fixture::legacy("stdin-short");
    let kind = MountsAddKind::Couchdb {
        common: common("archive", "Archive"),
        url: "https://couch.invalid".to_string(),
        database: "vault".to_string(),
        username: None,
        password_stdin: true,
        writable: false,
        e2ee: true,
        sidecar_path: None,
    };
    let error = add_with_resolver(
        &fixture.config_path,
        None,
        &kind,
        false,
        &fixture.resolver,
        false,
        // Only the password; the E2EE passphrase is missing.
        &mut SecretReader::from_lines(vec!["pw".to_string()]),
    )
    .await
    .expect_err("a missing line is an error");
    let message = error.to_string();
    assert!(message.contains("E2EE passphrase"), "{message}");
    assert!(message.contains("one line per secret"), "{message}");
}

/// `--dry-run` validates and reports, and touches neither the config nor the secret store.
#[tokio::test]
async fn dry_run_validates_and_writes_nothing() {
    let fixture = Fixture::legacy("dry-run");
    let original = fixture.config_text();
    let report = add_with_resolver(
        &fixture.config_path,
        None,
        &algolia("wiki", "_Wiki", "http://127.0.0.1:1/", "wiki-dry"),
        true,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("dry-run validates");
    assert!(!report.written);
    assert!(report.dry_run);
    assert!(report.secret_refs.is_empty());
    assert_eq!(report.probe.kind, "skipped");
    assert_eq!(fixture.config_text(), original);
    assert!(fixture
        .resolver
        .get(&SecretRef::EncryptedFile {
            id: "mount-wiki-api-key".to_string(),
        })
        .expect("read the store")
        .is_none());
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// A legacy config lists its implicit root as such, from the same helper the migration
/// uses — so what `list` shows is what `add` would write.
#[test]
fn list_reports_a_legacy_config_as_an_implicit_root() {
    let fixture = Fixture::legacy("list-legacy");
    let report = list(&fixture.config_path).expect("list a legacy config");
    assert!(report.implicit);
    assert_eq!(report.mounts.len(), 1);
    let root = &report.mounts[0];
    assert_eq!(root.id, "vault");
    assert!(root.root);
    assert_eq!(root.mount_at, "");
    assert_eq!(root.kind, "filesystem");
    assert!(root.writable);
    assert_eq!(
        root.index_dir.as_deref(),
        Some(fixture.base.join("index").as_path())
    );
    assert!(report.unresolved.is_none());
    // The rendering is `doctor`'s, reused rather than reimplemented.
    assert!(
        root.line.starts_with("mount vault at / (filesystem): "),
        "{}",
        root.line
    );
    assert!(report.experimental.is_empty());

    let text = deep_obsidian_cli::mounts_cmd::render_list_report(&report);
    assert!(text.contains("IMPLICIT"), "{text}");
    assert!(text.contains("experimental: (none enabled)"), "{text}");
}

/// After an add, `list` reports both mounts, their writability and the flags that are on.
#[tokio::test]
async fn list_reports_a_declared_table_with_its_experimental_flags() {
    let fixture = Fixture::legacy("list-declared");
    let (base_url, _mock) = spawn_mock().await;
    add_with_resolver(
        &fixture.config_path,
        None,
        &algolia("wiki", "_Wiki", &base_url, "wiki-list"),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(vec![API_KEY.to_string()]),
    )
    .await
    .expect("add an algolia mount");

    let report = list(&fixture.config_path).expect("list");
    assert!(!report.implicit);
    assert_eq!(report.mounts.len(), 2);
    assert!(report.mounts[0].writable, "a filesystem mount is writable");
    assert!(
        !report.mounts[1].writable,
        "the algolia mount was added read-only"
    );
    assert_eq!(
        report.mounts[1].index_dir.as_deref(),
        Some(
            fixture
                .base
                .join("index")
                .join("mounts")
                .join("wiki")
                .as_path()
        )
    );
    assert_eq!(
        report.experimental,
        vec!["multiVault".to_string(), "algoliaVaults".to_string()]
    );

    let text = deep_obsidian_cli::mounts_cmd::render_list_report(&report);
    assert!(text.contains("— read-only"), "{text}");
    assert!(text.contains("multiVault, algoliaVaults"), "{text}");
    // Still no credential, and not even the index name, in what an operator pastes.
    assert!(!text.contains(API_KEY), "{text}");
}

/// `list` still reports a config the loader REFUSES, with the reason attached.
///
/// The property that matters most for this command: a broken table is exactly when an
/// operator needs to see what is declared, so refusing would be backwards. Only the index
/// directories go missing, because they are derived from the resolved root.
#[test]
fn list_reports_a_table_the_loader_refuses_and_says_why() {
    let fixture = Fixture::legacy("list-broken");
    // Two mounts at the same prefix, and no experimental flag — a config the server would
    // refuse to start on, of the shape a hand edit produces.
    std::fs::write(
        &fixture.config_path,
        format!(
            r#"{{
  "indexDir": {index},
  "mounts": [
    {{ "id": "vault", "mountAt": "", "backend": {{ "kind": "filesystem", "vaultPath": {vault} }} }},
    {{ "id": "team", "mountAt": "Team", "backend": {{ "kind": "filesystem", "vaultPath": {team} }} }}
  ]
}}
"#,
            index = json_string(&fixture.base.join("index")),
            vault = json_string(&fixture.base.join("vault")),
            team = json_string(&fixture.base.join("team")),
        ),
    )
    .expect("write a broken config");

    let report = list(&fixture.config_path).expect("list must not refuse a broken config");
    assert!(!report.implicit);
    assert_eq!(report.mounts.len(), 2, "both declared mounts are reported");
    assert_eq!(report.mounts[1].id, "team");
    // Derived from the resolved root, which does not exist here.
    assert!(report.mounts[1].index_dir.is_none());
    assert!(report.root_index_dir.is_none());
    let reason = report.unresolved.clone().expect("the loader's complaint");
    assert!(reason.contains("multiVault"), "{reason}");

    let text = deep_obsidian_cli::mounts_cmd::render_list_report(&report);
    assert!(text.contains("WARNING"), "{text}");
    assert!(text.contains("does NOT resolve"), "{text}");
    assert!(text.contains("mount team at /Team"), "{text}");
}

/// The migration narration survives the error path.
///
/// `--id vault` collides with the root mount the migration itself invents, so the bare
/// loader error would name a mount the operator never wrote. The conversion is reported
/// first, which is the only thing that makes the collision explainable.
#[tokio::test]
async fn a_collision_with_the_invented_root_explains_where_it_came_from() {
    let fixture = Fixture::legacy("collide-root");
    let original = fixture.config_text();
    let error = add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("vault", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect_err("the id collides with the migrated root");
    let message = error.to_string();
    assert!(
        message.contains("converted the legacy `vaultPath`"),
        "the narration was discarded: {message}"
    );
    assert!(message.contains("duplicate mount id"), "{message}");
    assert_eq!(fixture.config_text(), original);
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

/// A three-mount table: the root, a filesystem mount, and an algolia mount with a stored
/// credential.
async fn fixture_with_three_mounts(name: &str) -> (Fixture, tokio::task::JoinHandle<()>) {
    let fixture = Fixture::legacy(name);
    let (base_url, mock) = spawn_mock().await;
    add_with_resolver(
        &fixture.config_path,
        None,
        &filesystem("team", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("add the filesystem mount");
    add_with_resolver(
        &fixture.config_path,
        None,
        &algolia("wiki", "_Wiki", &base_url, &format!("wiki-{name}")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(vec![API_KEY.to_string()]),
    )
    .await
    .expect("add the algolia mount");
    (fixture, mock)
}

/// The root cannot be removed while other mounts remain: they resolve by longest prefix
/// beneath it, so the table would lose its floor.
#[tokio::test]
async fn remove_refuses_the_root_while_other_mounts_remain() {
    let (fixture, _mock) = fixture_with_three_mounts("remove-root").await;
    let original = fixture.config_text();
    let error = remove(&fixture.config_path, "vault", false, true, false)
        .expect_err("the root cannot go while others remain");
    let message = error.to_string();
    assert!(message.contains("ROOT mount"), "{message}");
    assert!(message.contains("2 other mount(s)"), "{message}");
    assert_eq!(fixture.config_text(), original);
}

/// The last mount cannot be removed: a config needs a root mount, and there is no valid
/// table to write. Refused with a named message rather than silently converted back to a
/// legacy `vaultPath`.
#[tokio::test]
async fn remove_refuses_the_last_mount() {
    let (fixture, _mock) = fixture_with_three_mounts("remove-last").await;
    remove(&fixture.config_path, "wiki", false, true, false).expect("remove the algolia mount");
    remove(&fixture.config_path, "team", false, true, false).expect("remove the filesystem mount");
    let original = fixture.config_text();

    let error = remove(&fixture.config_path, "vault", false, true, false)
        .expect_err("the last mount cannot go");
    let message = error.to_string();
    assert!(message.contains("only mount"), "{message}");
    assert!(message.contains("needs a root mount"), "{message}");
    assert!(message.contains("Edit the file directly"), "{message}");
    assert_eq!(fixture.config_text(), original);
}

/// An unknown id names the ids that do exist rather than failing bare.
#[tokio::test]
async fn remove_names_the_declared_ids_when_the_id_is_unknown() {
    let (fixture, _mock) = fixture_with_three_mounts("remove-unknown").await;
    let error =
        remove(&fixture.config_path, "nope", false, true, false).expect_err("no such mount");
    let message = error.to_string();
    assert!(message.contains("'vault'"), "{message}");
    assert!(message.contains("'team'"), "{message}");
    assert!(message.contains("'wiki'"), "{message}");
}

/// Removing a remote mount says the remote data was not touched, keeps the credential, and
/// names the reference. The index directory stays put unless asked for.
#[tokio::test]
async fn remove_keeps_the_remote_data_the_index_and_the_credential() {
    let (fixture, _mock) = fixture_with_three_mounts("remove-keeps").await;
    let index_dir = fixture.base.join("index").join("mounts").join("wiki");
    std::fs::create_dir_all(&index_dir).expect("index dir");
    std::fs::write(index_dir.join("index.sqlite"), b"not really").expect("index file");

    let report =
        remove(&fixture.config_path, "wiki", false, true, false).expect("remove the algolia mount");
    assert!(report.written);
    assert!(!report.index_purged);
    assert_eq!(report.index_dir.as_deref(), Some(index_dir.as_path()));
    assert_eq!(
        report.secret_refs,
        vec!["encryptedFile id=mount-wiki-api-key".to_string()]
    );
    let text = deep_obsidian_cli::mounts_cmd::render_remove_report(&report);
    assert!(text.contains("NOTHING was deleted"), "{text}");
    assert!(text.contains("left this mount's index in place"), "{text}");
    assert!(text.contains("KEPT"), "{text}");
    assert!(text.contains("mount-wiki-api-key"), "{text}");

    assert!(
        index_dir.exists(),
        "the index was deleted without --purge-index"
    );
    assert!(
        fixture
            .resolver
            .get(&SecretRef::EncryptedFile {
                id: "mount-wiki-api-key".to_string()
            })
            .expect("read the store")
            .is_some(),
        "the credential was deleted"
    );
    // And the mount is gone from the file, with the other two intact.
    let json = fixture.config_json();
    let ids: Vec<&str> = json["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .map(|mount| mount["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec!["vault", "team"]);
    // The experimental flags are NOT reset: turning one off because its last user went away
    // would be a second, unasked-for change.
    assert_eq!(json["experimental"]["algoliaVaults"], true);
    // The write kept the previous file, and the unknown key.
    assert!(fixture.backup_path().exists());
    assert_eq!(json["somethingThisBuildDoesNotKnow"]["keep"], "me");
}

/// `--purge-index` deletes the mount's index directory, and says exactly what it deleted.
#[tokio::test]
async fn remove_purge_index_deletes_only_that_mounts_index() {
    let (fixture, _mock) = fixture_with_three_mounts("remove-purge").await;
    let root_index = fixture.base.join("index");
    let mount_index = root_index.join("mounts").join("wiki");
    std::fs::create_dir_all(&mount_index).expect("index dir");
    std::fs::write(mount_index.join("index.sqlite"), b"x").expect("index file");
    std::fs::write(root_index.join("index.sqlite"), b"root").expect("root index file");

    let report = remove(&fixture.config_path, "wiki", true, true, false).expect("remove");
    assert!(report.index_purged);
    assert!(
        !mount_index.exists(),
        "the mount index survived --purge-index"
    );
    assert!(
        root_index.join("index.sqlite").exists(),
        "the ROOT index must never be touched"
    );
    let text = deep_obsidian_cli::mounts_cmd::render_remove_report(&report);
    assert!(text.contains("deleted"), "{text}");
    assert!(text.contains("mounts/wiki"), "{text}");
}

/// `--dry-run` reports the removal and changes nothing.
#[tokio::test]
async fn remove_dry_run_changes_nothing() {
    let (fixture, _mock) = fixture_with_three_mounts("remove-dry").await;
    let original = fixture.config_text();
    let report = remove(&fixture.config_path, "wiki", true, true, true).expect("dry-run remove");
    assert!(!report.written);
    assert!(!report.index_purged);
    assert_eq!(fixture.config_text(), original);
}

/// `mounts remove` on a legacy config refuses, naming what the config actually is.
#[test]
fn remove_refuses_a_legacy_config() {
    let fixture = Fixture::legacy("remove-legacy");
    let error =
        remove(&fixture.config_path, "vault", false, true, false).expect_err("nothing to remove");
    let message = error.to_string();
    assert!(message.contains("declares no mount table"), "{message}");
    assert!(message.contains("`mounts add`"), "{message}");
}

/// A TOML config is edited as TOML, and its backup is named `.toml.bak`.
///
/// Both formats are supported and the writer dispatches on the extension, so the one thing
/// that could go wrong is the BACKUP: a `config.json.bak` holding TOML is a trap, because
/// the obvious way to use a backup is to rename it back.
#[tokio::test]
async fn a_toml_config_is_edited_as_toml_and_backed_up_as_toml() {
    let fixture = Fixture::legacy("toml");
    let toml_path = fixture.base.join("config.toml");
    std::fs::write(
        &toml_path,
        format!(
            "vaultPath = {vault}\nindexDir = {index}\n",
            vault = json_string(&fixture.base.join("vault")),
            index = json_string(&fixture.base.join("index")),
        ),
    )
    .expect("write a toml config");
    let original = std::fs::read_to_string(&toml_path).expect("read");

    let report = add_with_resolver(
        &toml_path,
        None,
        &filesystem("team", "Team", &fixture.base.join("team")),
        false,
        &fixture.resolver,
        false,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .await
    .expect("add to a toml config");

    let backup = fixture.base.join("config.toml.bak");
    assert_eq!(report.backup_path.as_deref(), Some(backup.as_path()));
    assert_eq!(
        std::fs::read_to_string(&backup).expect("the backup exists"),
        original
    );
    assert!(
        !fixture.base.join("config.json.bak").exists(),
        "a TOML config must not be backed up under a .json name"
    );
    // ...and the rewritten file is still TOML, so it loads.
    let text = std::fs::read_to_string(&toml_path).expect("read");
    assert!(text.contains("[[mounts]]"), "{text}");
    assert_eq!(
        list(&toml_path).expect("list the toml config").mounts.len(),
        2
    );
}

/// `mounts remove` can REPAIR a table the loader refuses.
///
/// Dropping one of two mounts from a config whose `experimental.multiVault` was edited out
/// leaves a single valid root, so refusing to remove from a broken config would make the
/// repair impossible with this command. The pre-removal resolution failure becomes a note,
/// not an error; the POST-removal table is still validated, because that is what gets
/// written.
#[test]
fn remove_can_repair_a_config_the_loader_refuses() {
    let fixture = Fixture::legacy("remove-repair");
    std::fs::write(
        &fixture.config_path,
        format!(
            r#"{{
  "indexDir": {index},
  "mounts": [
    {{ "id": "vault", "mountAt": "", "backend": {{ "kind": "filesystem", "vaultPath": {vault} }} }},
    {{ "id": "team", "mountAt": "Team", "backend": {{ "kind": "filesystem", "vaultPath": {team} }} }}
  ]
}}
"#,
            index = json_string(&fixture.base.join("index")),
            vault = json_string(&fixture.base.join("vault")),
            team = json_string(&fixture.base.join("team")),
        ),
    )
    .expect("write a broken config");
    assert!(list(&fixture.config_path)
        .expect("list")
        .unresolved
        .is_some());

    let report = remove(&fixture.config_path, "team", true, true, false)
        .expect("removing a mount must be able to fix the table");
    assert!(report.written);
    // The index dir could not be derived, so --purge-index says so rather than guessing.
    assert!(report.index_dir.is_none());
    assert!(!report.index_purged);
    let text = deep_obsidian_cli::mounts_cmd::render_remove_report(&report);
    assert!(text.contains("does not resolve as it stands"), "{text}");
    assert!(text.contains("--purge-index: skipped"), "{text}");

    // And the config now resolves.
    let after = list(&fixture.config_path).expect("list");
    assert!(after.unresolved.is_none(), "{:?}", after.unresolved);
    assert_eq!(after.mounts.len(), 1);
}

// ---------------------------------------------------------------------------
// argv
// ---------------------------------------------------------------------------

/// Run the real binary, returning `(succeeded, stdout, stderr)`.
///
/// stdin is CLOSED rather than inherited, which is load-bearing twice: a command that
/// prompts reads EOF and takes the safe (refusing) branch instead of hanging the suite,
/// and that is exactly the branch worth proving for the experimental-flag confirmation.
fn run(config_path: &Path, args: &[&str]) -> (bool, String, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .arg("--config")
        .arg(config_path)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// The `mounts` family is reachable **through the real binary's argv**, values and all.
///
/// One process-spawning test rather than one per subcommand: what can break here is the
/// shared argv plumbing — clap's derive, `normalize_cli_args`, the dispatch in
/// `commands::run` and the `--json` switch — and spawning a process per subcommand would
/// pay for the same coverage several times.
///
/// The flag-value assertions are the specific regression guard. `--mount-at Team` and
/// `--vault-path <dir>` are the two shapes the normalizer could eat: the first as an
/// unknown value flag whose value becomes `--vault Team`, the second through the existing
/// `--vault-path` → `--vault` rewrite. Both would surface as clap complaining about the
/// flag the user *did* pass.
#[test]
fn cli_argv_reaches_the_mounts_subcommands() {
    let fixture = Fixture::legacy("argv");

    // `mounts list` on a legacy config: the family is reached and the implicit root shown.
    let (ok, stdout, stderr) = run(&fixture.config_path, &["mounts", "list"]);
    assert!(ok, "mounts list failed\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("IMPLICIT"), "{stdout}");
    assert!(stdout.contains("mount vault at / (filesystem)"), "{stdout}");

    // `mounts add filesystem`: three nested words AND two value flags whose values must
    // survive. `--yes` stands in for the experimental confirmation.
    let team = fixture.base.join("team");
    let (ok, stdout, stderr) = run(
        &fixture.config_path,
        &[
            "mounts",
            "add",
            "filesystem",
            "--id",
            "team",
            "--mount-at",
            "Team",
            "--vault-path",
            team.to_str().expect("utf-8 path"),
            "--yes",
        ],
    );
    assert!(
        ok,
        "mounts add filesystem failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    // The values arrived intact rather than being promoted to `--vault`.
    assert!(
        stdout.contains("mount team at /Team (filesystem)"),
        "{stdout}"
    );
    assert!(stdout.contains(&team.display().to_string()), "{stdout}");
    assert!(
        stdout.contains("enabled experimental.multiVault"),
        "{stdout}"
    );

    // `--json` reaches the same command and switches the rendering.
    let (ok, stdout, stderr) = run(&fixture.config_path, &["mounts", "list", "--json"]);
    assert!(ok, "mounts list --json failed\nstderr: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json report");
    assert_eq!(parsed["implicit"], false);
    assert_eq!(parsed["mounts"][1]["id"], "team");
    assert_eq!(parsed["mounts"][1]["mountAt"], "Team");

    // `--mount-at=Team` (the `=` form) on a second mount: the other half of value handling.
    let other = fixture.base.join("other");
    std::fs::create_dir_all(&other).expect("other dir");
    let (ok, stdout, stderr) = run(
        &fixture.config_path,
        &[
            "mounts",
            "add",
            "filesystem",
            "--id=other",
            "--mount-at=Other",
            &format!("--vault-path={}", other.display()),
            "--yes",
        ],
    );
    assert!(
        ok,
        "the `=` flag form failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("mount other at /Other"), "{stdout}");

    // `mounts remove`, and its refusals, through argv.
    let (ok, stdout, stderr) = run(
        &fixture.config_path,
        &["mounts", "remove", "--id", "other", "--yes"],
    );
    assert!(
        ok,
        "mounts remove failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("unmounted 'other'"), "{stdout}");

    let (ok, _stdout, stderr) = run(
        &fixture.config_path,
        &["mounts", "remove", "--id", "vault", "--yes"],
    );
    assert!(!ok, "removing the root must fail");
    assert!(stderr.contains("ROOT mount"), "{stderr}");
}

/// Without `--yes`, the experimental confirmation is asked — and a closed stdin is not a
/// yes.
///
/// The property: a flag is never enabled silently, and a declined confirmation writes
/// nothing at all.
#[test]
fn cli_argv_never_enables_an_experimental_flag_without_an_answer() {
    let fixture = Fixture::legacy("argv-prompt");
    let original = fixture.config_text();
    let team = fixture.base.join("team");

    let (ok, stdout, stderr) = run(
        &fixture.config_path,
        &[
            "mounts",
            "add",
            "filesystem",
            "--id",
            "team",
            "--mount-at",
            "Team",
            "--vault-path",
            team.to_str().expect("utf-8 path"),
        ],
    );
    assert!(!ok, "an unanswered confirmation must not succeed");
    // The question named the flag, the word "EXPERIMENTAL", and what it turns on.
    assert!(stdout.contains("experimental.multiVault"), "{stdout}");
    assert!(stdout.contains("EXPERIMENTAL"), "{stdout}");
    assert!(stdout.contains("behaviour may change"), "{stdout}");
    assert!(stderr.contains("was not enabled"), "{stderr}");
    assert_eq!(fixture.config_text(), original, "the config was written");
    assert!(!fixture.backup_path().exists());
}

/// `mounts --help` reaches clap's derived help rather than the hand-written summary, and
/// every other command keeps the summary it has always printed.
#[test]
fn cli_argv_mounts_help_is_claps_and_the_rest_is_unchanged() {
    let mounts_help = std::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .args(["mounts", "--help"])
        .output()
        .expect("run --help");
    let text = String::from_utf8_lossy(&mounts_help.stdout).to_string();
    assert!(
        text.contains("Usage: deep-obsidian-mcp mounts"),
        "mounts --help did not reach clap: {text}"
    );
    for subcommand in ["add", "list", "remove"] {
        assert!(text.contains(subcommand), "{text}");
    }

    // Per-kind help exists and names the kind's own required flags.
    let kind_help = std::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .args(["mounts", "add", "couchdb", "--help"])
        .output()
        .expect("run --help");
    let text = String::from_utf8_lossy(&kind_help.stdout).to_string();
    assert!(text.contains("--url"), "{text}");
    assert!(text.contains("--database"), "{text}");
    assert!(text.contains("--password-stdin"), "{text}");
    // And it says the password is never a flag value.
    assert!(text.contains("prompted masked"), "{text}");

    // The global summary still answers for everything else, and now advertises `mounts`.
    let doctor_help = std::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .args(["doctor", "--help"])
        .output()
        .expect("run --help");
    let text = String::from_utf8_lossy(&doctor_help.stdout).to_string();
    assert!(
        text.starts_with("Usage:\n  deep-obsidian-mcp [serve]"),
        "{text}"
    );
    assert!(text.contains("mounts add filesystem --id"), "{text}");
    assert!(text.contains("mounts remove --id <id>"), "{text}");
}
