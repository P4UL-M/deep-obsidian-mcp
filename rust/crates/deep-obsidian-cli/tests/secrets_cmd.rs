//! `secrets set` and `secrets check`, at both layers that can break.
//!
//! # The property this file exists for
//!
//! **A rotation writes to the reference the config FILE contains, and to nothing else.** Every
//! other assertion here is in service of that one. `mounts add` derives
//! `mount-<id>-<purpose>` when it creates a credential, so the tempting implementation of
//! `secrets set` derives the same id — and that implementation is silently wrong for every
//! config whose reference was hand-written or renamed, because the config would keep pointing
//! at the value nobody updated. So the fixtures below use references that a derivation would
//! NOT produce, and `rotation_writes_to_the_exact_reference_the_config_holds` asserts both
//! halves: the value landed at the declared id, and the derived id does not exist.
//!
//! # The two layers
//!
//! The library layer owns the semantics — which reference, which refusal, what the report
//! says — tested against a temp config file and a temp encrypted secrets file.
//!
//! The **argv** layer owns the wiring, tested by spawning the REAL binary, for the reason
//! `tests/mounts_cmd.rs` does: `couchdb export` was unreachable from the command line from
//! the commit that introduced it while every hermetic test passed, because the arg normalizer
//! ate the subcommand. `secrets set --field password` adds two fresh chances to repeat that —
//! `--field` and `--target` both take bare-word values that would be promoted to `--vault`.
//!
//! The argv layer also buys something the library layer cannot: it runs `secrets set` in one
//! process and `secrets check` in a SECOND one, against the same encrypted file. That pair is
//! genuine cross-process evidence that a rotation is visible to another process — the
//! property a keyring canary proves for the keyring, done for the store a headless install
//! actually uses.
//!
//! # What is NOT tested here, and why
//!
//! Writes to the OS keyring. A `SecretResolver` routes by reference SHAPE, so a config
//! carrying an `osKeyring` reference reaches the developer's real login keychain no matter
//! which resolver a test builds — which on macOS means an authorization dialog and a run that
//! hangs until someone clicks it. So `osKeyring` references appear only on `--dry-run` paths,
//! which read the reference and store nothing; that is enough to prove the kind is taken from
//! the reference rather than chosen. The masked prompt is likewise not driven (`rpassword`
//! needs a tty): `SecretReader::from_lines` is the same call sequence without one.

use std::path::{Path, PathBuf};

use deep_obsidian_cli::mounts_cmd::SecretReader;
use deep_obsidian_cli::secrets_cmd::{
    check_with_resolver, render_check_report, render_set_report, set_mount_with_resolver,
    set_target_with_resolver,
};
use deep_obsidian_cli::{SecretField, SecretTarget};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_types::SecretRef;

/// A value long and distinctive enough that a `contains` assertion cannot pass by accident,
/// and unmistakable if it ever appears in a report.
const NEW_SECRET: &str = "rotated-value-sentinel-8f3a1c-must-never-be-printed";

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
        "dob-secrets-cmd-{prefix}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// One test's world: a config file whose references are deliberately NOT the ones any
/// command would derive, and a temp encrypted secrets file.
struct Fixture {
    base: PathBuf,
    config_path: PathBuf,
    secrets_path: PathBuf,
    resolver: SecretResolver,
}

impl Fixture {
    /// A four-mount table plus auth, embedding and artifact-embedding references.
    ///
    /// Every reference is hand-written and none matches `mount-<id>-<purpose>`,
    /// `http-auth-token` or `openai-embedding` — the ids the wizard and `mounts add` derive.
    /// A derivation-based implementation therefore fails every rotation assertion here rather
    /// than passing three of them by coincidence.
    ///
    /// One unknown top-level key rides along so the config-immutability assertions also prove
    /// nothing rewrote the file "harmlessly".
    fn multi(name: &str) -> Self {
        let base = temp_dir(name);
        for dir in ["vault", "plain"] {
            std::fs::create_dir_all(base.join(dir)).expect("vault dir");
        }
        let config_path = base.join("config.json");
        std::fs::write(
            &config_path,
            format!(
                r#"{{
  "mounts": [
    {{
      "id": "vault",
      "mountAt": "",
      "backend": {{ "kind": "filesystem", "vaultPath": {vault} }}
    }},
    {{
      "id": "team",
      "mountAt": "Team",
      "backend": {{
        "kind": "couchdb",
        "url": "https://couch.example",
        "database": "teamvault",
        "username": "sync",
        "passwordRef": {{ "kind": "encryptedFile", "id": "hand-written-team-pw" }},
        "e2ee": {{
          "passphraseRef": {{ "kind": "encryptedFile", "id": "hand-written-team-e2ee" }},
          "obfuscatePassphraseRef": {{ "kind": "encryptedFile", "id": "hand-written-team-obf" }}
        }}
      }}
    }},
    {{
      "id": "bare",
      "mountAt": "Bare",
      "backend": {{
        "kind": "couchdb",
        "url": "https://couch.example",
        "database": "barevault",
        "passwordRef": {{ "kind": "encryptedFile", "id": "hand-written-bare-pw" }}
      }}
    }},
    {{
      "id": "wiki",
      "mountAt": "_Wiki",
      "backend": {{
        "kind": "algolia",
        "appId": "APPID123",
        "indexName": "shared-wiki",
        "apiKeyRef": {{ "kind": "encryptedFile", "id": "hand-written-wiki-key" }}
      }}
    }},
    {{
      "id": "plain",
      "mountAt": "Plain",
      "backend": {{ "kind": "filesystem", "vaultPath": {plain} }}
    }}
  ],
  "experimental": {{ "multiVault": true, "couchdbVaults": true, "algoliaVaults": true }},
  "indexDir": {index},
  "auth": {{ "enabled": true, "tokenRef": {{ "kind": "encryptedFile", "id": "hand-written-token" }} }},
  "embedding": {{
    "model": "nomic-embed-text",
    "apiKeyRef": {{ "kind": "encryptedFile", "id": "hand-written-embedding-key" }}
  }},
  "artifactEmbedding": {{
    "apiKeyRef": {{ "kind": "encryptedFile", "id": "hand-written-artifact-key" }}
  }},
  "somethingThisBuildDoesNotKnow": {{ "keep": "me" }}
}}
"#,
                vault = json_string(&base.join("vault")),
                plain = json_string(&base.join("plain")),
                index = json_string(&base.join("index")),
            ),
        )
        .expect("write config");
        let secrets_path = base.join("secrets.json");
        Self {
            resolver: SecretResolver::with_encrypted_file_path(secrets_path.clone()),
            base,
            config_path,
            secrets_path,
        }
    }

    /// A LEGACY config: `vaultPath` only, so no mount has a credential.
    fn legacy(name: &str) -> Self {
        let base = temp_dir(name);
        std::fs::create_dir_all(base.join("vault")).expect("vault dir");
        let config_path = base.join("config.json");
        std::fs::write(
            &config_path,
            format!(
                "{{\n  \"vaultPath\": {vault}\n}}\n",
                vault = json_string(&base.join("vault"))
            ),
        )
        .expect("write config");
        let secrets_path = base.join("secrets.json");
        Self {
            resolver: SecretResolver::with_encrypted_file_path(secrets_path.clone()),
            base,
            config_path,
            secrets_path,
        }
    }

    /// A config whose couchdb password lives in the OS KEYRING, under an account no
    /// derivation would produce. Only ever used on `--dry-run` paths; see the module docs.
    fn keyring_backed(name: &str) -> Self {
        let base = temp_dir(name);
        std::fs::create_dir_all(base.join("vault")).expect("vault dir");
        let config_path = base.join("config.json");
        std::fs::write(
            &config_path,
            format!(
                r#"{{
  "mounts": [
    {{
      "id": "vault",
      "mountAt": "",
      "backend": {{ "kind": "filesystem", "vaultPath": {vault} }}
    }},
    {{
      "id": "team",
      "mountAt": "Team",
      "backend": {{
        "kind": "couchdb",
        "url": "https://couch.example",
        "database": "teamvault",
        "passwordRef": {{
          "kind": "osKeyring",
          "service": "some-other-service",
          "account": "hand-written-keychain-account"
        }}
      }}
    }}
  ],
  "experimental": {{ "multiVault": true, "couchdbVaults": true }}
}}
"#,
                vault = json_string(&base.join("vault")),
            ),
        )
        .expect("write config");
        let secrets_path = base.join("secrets.json");
        Self {
            resolver: SecretResolver::with_encrypted_file_path(secrets_path.clone()),
            base,
            config_path,
            secrets_path,
        }
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config_path).expect("read config")
    }

    fn backup_path(&self) -> PathBuf {
        self.config_path.with_extension("json.bak")
    }

    /// The ids the encrypted secrets file currently holds. The file stores ciphertext, so
    /// the KEYS are the only thing a test can look at — which is exactly what the exact-ref
    /// property is about.
    fn stored_ids(&self) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(&self.secrets_path) else {
            return Vec::new();
        };
        let file: serde_json::Value = serde_json::from_str(&text).expect("secrets file is json");
        file["items"]
            .as_object()
            .expect("items object")
            .keys()
            .cloned()
            .collect()
    }

    fn stored_value(&self, id: &str) -> Option<String> {
        self.resolver
            .get(&SecretRef::EncryptedFile { id: id.to_string() })
            .expect("read the store")
            .map(|value| secrecy::ExposeSecret::expose_secret(&value).to_string())
    }

    fn put(&self, id: &str, value: &str) {
        self.resolver
            .put(
                &SecretRef::EncryptedFile { id: id.to_string() },
                secrecy::SecretString::from(value.to_string()),
            )
            .expect("seed the store");
    }
}

fn json_string(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("json string")
}

/// The seam a masked prompt is driven through in a test. One line, one secret.
fn one(value: &str) -> SecretReader {
    SecretReader::from_lines(vec![value.to_string()])
}

// ---------------------------------------------------------------------------
// The exact-reference property
// ---------------------------------------------------------------------------

/// A rotation writes to the reference the config CONTAINS, not to a derived one.
///
/// The critical property of this whole slice, asserted in both directions: the value is
/// readable at the hand-written id, and the id `mounts add` would have derived
/// (`mount-team-password`) does not exist in the store at all. The second half is what
/// catches an implementation that writes to both.
///
/// The same run proves the config is not modified — byte-identical, and no `.bak` — because
/// "rotation touches the store only" is the other half of the same contract.
#[test]
fn rotation_writes_to_the_exact_reference_the_config_holds() {
    let fixture = Fixture::multi("exact-ref");
    let before = fixture.config_text();

    let report = set_mount_with_resolver(
        &fixture.config_path,
        "team",
        None,
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect("rotate the couchdb password");

    // The field defaulted by KIND: a couchdb mount's password.
    assert_eq!(report.subject, "mounts.team.passwordRef");
    assert_eq!(report.kind, "encryptedFile");
    assert_eq!(report.reference, "encryptedFile id=hand-written-team-pw");
    assert!(report.stored);

    // Half one: the value is at the declared id.
    assert_eq!(
        fixture.stored_value("hand-written-team-pw").as_deref(),
        Some(NEW_SECRET)
    );
    // Half two, the one with teeth: nothing was written to the DERIVED id.
    let ids = fixture.stored_ids();
    assert_eq!(
        ids,
        vec!["hand-written-team-pw".to_string()],
        "a rotation wrote somewhere other than the declared reference: {ids:?}"
    );

    // The config was not touched, and no backup was made because nothing was written.
    assert_eq!(fixture.config_text(), before, "the config was modified");
    assert!(!fixture.backup_path().exists(), "a .bak was written");
}

/// Every rotatable field goes to its own declared reference, and only that one.
///
/// Runs all four rotations against one fixture in one test: what is being checked is that
/// the field → reference mapping has no crossed wires, and four one-rotation tests would
/// prove each mapping in isolation while missing a swap between two of them.
#[test]
fn every_field_and_target_rotates_its_own_declared_reference() {
    let fixture = Fixture::multi("per-field");

    for (mount, field, subject, id) in [
        (
            "team",
            Some(SecretField::Password),
            "mounts.team.passwordRef",
            "hand-written-team-pw",
        ),
        (
            "team",
            Some(SecretField::E2eePassphrase),
            "mounts.team.e2ee.passphraseRef",
            "hand-written-team-e2ee",
        ),
        (
            "wiki",
            None,
            "mounts.wiki.apiKeyRef",
            "hand-written-wiki-key",
        ),
        (
            "wiki",
            Some(SecretField::ApiKey),
            "mounts.wiki.apiKeyRef",
            "hand-written-wiki-key",
        ),
    ] {
        let value = format!("{NEW_SECRET}-{id}");
        let report = set_mount_with_resolver(
            &fixture.config_path,
            mount,
            field,
            &fixture.resolver,
            false,
            &mut one(&value),
        )
        .unwrap_or_else(|error| panic!("rotate {mount}/{field:?}: {error}"));
        assert_eq!(report.subject, subject);
        assert_eq!(fixture.stored_value(id).as_deref(), Some(value.as_str()));
    }

    for (target, subject, id) in [
        (
            SecretTarget::AuthToken,
            "auth.tokenRef",
            "hand-written-token",
        ),
        (
            SecretTarget::EmbeddingApiKey,
            "embedding.apiKeyRef",
            "hand-written-embedding-key",
        ),
    ] {
        let value = format!("{NEW_SECRET}-{id}");
        let report = set_target_with_resolver(
            &fixture.config_path,
            target,
            &fixture.resolver,
            false,
            &mut one(&value),
        )
        .unwrap_or_else(|error| panic!("rotate {subject}: {error}"));
        assert_eq!(report.subject, subject);
        assert_eq!(fixture.stored_value(id).as_deref(), Some(value.as_str()));
    }

    // Exactly the five declared ids, and no derived one anywhere.
    let mut ids = fixture.stored_ids();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "hand-written-embedding-key".to_string(),
            "hand-written-team-e2ee".to_string(),
            "hand-written-team-pw".to_string(),
            "hand-written-token".to_string(),
            "hand-written-wiki-key".to_string(),
        ],
        "a rotation wrote outside the declared references"
    );

    // Rotating the auth token says the thing an operator must act on.
    let report = set_target_with_resolver(
        &fixture.config_path,
        SecretTarget::AuthToken,
        &fixture.resolver,
        false,
        &mut one("another-token"),
    )
    .expect("rotate again");
    let text = render_set_report(&report);
    assert!(text.contains("401"), "{text}");
    assert!(text.contains("restart the server"), "{text}");
}

// ---------------------------------------------------------------------------
// Kind preservation and the absence of a fallback
// ---------------------------------------------------------------------------

/// The reference's own STORE is preserved: the kind is read off the reference, never chosen.
///
/// The `osKeyring` half runs as a dry run on purpose — see the module docs — which is enough
/// for the claim being made: the reported kind comes from the reference, and the encrypted
/// file (the store a fallback would have used) was never even created.
#[test]
fn rotation_preserves_the_store_the_reference_names() {
    let file_backed = Fixture::multi("kind-file");
    let report = set_mount_with_resolver(
        &file_backed.config_path,
        "bare",
        None,
        &file_backed.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect("rotate an encryptedFile reference");
    assert_eq!(report.kind, "encryptedFile");
    let text = render_set_report(&report);
    assert!(text.contains("Rotation is not migration"), "{text}");

    let keyring_backed = Fixture::keyring_backed("kind-keyring");
    let report = set_mount_with_resolver(
        &keyring_backed.config_path,
        "team",
        None,
        &keyring_backed.resolver,
        // Dry run: the reference is read and reported, nothing is stored anywhere.
        true,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .expect("dry-run an osKeyring reference");
    assert_eq!(report.kind, "osKeyring");
    assert_eq!(
        report.reference,
        "osKeyring service=some-other-service account=hand-written-keychain-account"
    );
    assert!(!report.stored);
    assert!(
        !keyring_backed.secrets_path.exists(),
        "an osKeyring reference must never reach the encrypted file"
    );
}

/// A store that cannot be written to is REPORTED, never worked around.
///
/// The failure is produced without a keyring and without a new seam: the encrypted secrets
/// file is pointed inside a path whose parent is an existing regular FILE, so
/// `EncryptedFileStore`'s `create_dir_all` fails and `put` returns an I/O error. What must
/// then happen is nothing: no value anywhere else, no config change. A fallback here would
/// orphan the value, because the config still points at this reference.
#[test]
fn a_failed_store_write_is_reported_and_nothing_else_is_touched() {
    let fixture = Fixture::multi("no-fallback");
    let blocker = fixture.base.join("not-a-directory");
    std::fs::write(&blocker, "this is a regular file").expect("write blocker");
    let broken = SecretResolver::with_encrypted_file_path(blocker.join("secrets.json"));
    let before = fixture.config_text();

    let error = set_mount_with_resolver(
        &fixture.config_path,
        "team",
        None,
        &broken,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("a failed store write must be an error");
    let message = error.to_string();

    assert!(
        message.contains("could not store the new value at encryptedFile id=hand-written-team-pw"),
        "{message}"
    );
    // It names the refusal to fall back, and says why.
    assert!(message.contains("does NOT fall back"), "{message}");
    assert!(message.contains("would be ignored"), "{message}");
    // And it points at the way to actually change stores.
    assert!(message.contains("change the reference itself"), "{message}");
    // The secret is not in the message.
    assert!(!message.contains(NEW_SECRET), "{message}");

    assert_eq!(fixture.config_text(), before);
    assert!(fixture.stored_ids().is_empty(), "something was stored");
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Every refusal, and the substance each one has to carry.
///
/// Grouped because they share a fixture and each is one assertion about wording; the
/// behavioural claim they share — nothing is stored and the config is untouched — is checked
/// once at the end for all of them.
#[test]
fn the_refusals_say_what_to_do_instead() {
    let fixture = Fixture::multi("refusals");
    let before = fixture.config_text();

    // An unknown mount lists the ids that do exist.
    let message = set_mount_with_resolver(
        &fixture.config_path,
        "nope",
        None,
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("unknown mount")
    .to_string();
    assert!(message.contains("no mount with id \"nope\""), "{message}");
    assert!(message.contains("'team'"), "{message}");

    // A filesystem mount has no credential at all — a different situation from a wrong field.
    let message = set_mount_with_resolver(
        &fixture.config_path,
        "plain",
        None,
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("filesystem mount")
    .to_string();
    assert!(message.contains("stores no credential"), "{message}");

    // ... and naming a field explicitly reaches the same refusal rather than a confusing one.
    let message = set_mount_with_resolver(
        &fixture.config_path,
        "plain",
        Some(SecretField::Password),
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("filesystem mount with an explicit field")
    .to_string();
    assert!(message.contains("stores no credential"), "{message}");

    // A field that belongs to the OTHER backend kind names the fields this kind has.
    let message = set_mount_with_resolver(
        &fixture.config_path,
        "team",
        Some(SecretField::ApiKey),
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("api-key on a couchdb mount")
    .to_string();
    assert!(message.contains("has no apiKeyRef"), "{message}");
    assert!(
        message.contains("`password` and `e2ee-passphrase`"),
        "{message}"
    );

    let message = set_mount_with_resolver(
        &fixture.config_path,
        "wiki",
        Some(SecretField::Password),
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("password on an algolia mount")
    .to_string();
    assert!(message.contains("has no passwordRef"), "{message}");
    assert!(message.contains("`api-key`"), "{message}");

    // THE refusal this slice was asked for: a mount configured without e2ee. Adding one is a
    // config change, and the message has to say so rather than inventing a reference.
    let message = set_mount_with_resolver(
        &fixture.config_path,
        "bare",
        Some(SecretField::E2eePassphrase),
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("e2ee-passphrase on a mount without e2ee")
    .to_string();
    assert!(
        message.contains("not configured for end-to-end encryption"),
        "{message}"
    );
    assert!(message.contains("MOUNT-CONFIG change"), "{message}");
    assert!(message.contains("mounts add couchdb --e2ee"), "{message}");

    // Nothing was stored by any of the above, and the config is untouched.
    assert!(fixture.stored_ids().is_empty());
    assert_eq!(fixture.config_text(), before);
    assert!(!fixture.backup_path().exists());
}

/// A target with no reference in the config is refused, naming the command that creates one.
#[test]
fn a_target_with_no_reference_is_refused() {
    let fixture = Fixture::legacy("no-target");

    let message = set_target_with_resolver(
        &fixture.config_path,
        SecretTarget::AuthToken,
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("no auth configured")
    .to_string();
    assert!(message.contains("`auth.tokenRef` is unset"), "{message}");
    assert!(message.contains("setup-service --auth"), "{message}");
    // It says what `secrets set` is FOR, so the reader does not go looking for a flag that
    // would make this invocation work.
    assert!(
        message.contains("`secrets set` only replaces the value"),
        "{message}"
    );

    let message = set_target_with_resolver(
        &fixture.config_path,
        SecretTarget::EmbeddingApiKey,
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("no embedding key configured")
    .to_string();
    assert!(
        message.contains("`embedding.apiKeyRef` is unset"),
        "{message}"
    );
    assert!(message.contains("--wizard"), "{message}");

    // A legacy config has no mount credentials at all, and says so rather than "unknown id".
    let message = set_mount_with_resolver(
        &fixture.config_path,
        "vault",
        None,
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect_err("legacy config")
    .to_string();
    assert!(message.contains("declares no mount table"), "{message}");
    assert!(message.contains("secrets check"), "{message}");

    assert!(fixture.stored_ids().is_empty());
}

/// A dry run reads nothing and stores nothing.
///
/// "Reads nothing" is the load-bearing half: prompting for a credential and then discarding
/// it would teach an operator to type a secret into a command that does nothing with it, and
/// on the stdin path it would silently consume the line. So the reader handed in here is
/// EMPTY — a run that tried to read one secret would fail.
#[test]
fn a_dry_run_reads_nothing_and_stores_nothing() {
    let fixture = Fixture::multi("dry-run");
    let before = fixture.config_text();

    let report = set_mount_with_resolver(
        &fixture.config_path,
        "team",
        None,
        &fixture.resolver,
        true,
        &mut SecretReader::from_lines(Vec::new()),
    )
    .expect("dry run");

    assert!(!report.stored);
    assert!(report.dry_run);
    assert_eq!(report.reference, "encryptedFile id=hand-written-team-pw");
    let text = render_set_report(&report);
    assert!(
        text.contains("no value was read and nothing was stored"),
        "{text}"
    );
    assert!(fixture.stored_ids().is_empty());
    assert_eq!(fixture.config_text(), before);
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// The doctor-style table: every reference the config holds, and where it resolves.
///
/// Deliberately seeded so the run has all three outcomes present at once — resolved, MISSING,
/// and a reference no command can rotate — because the table's job is to be readable when it
/// is mixed, which is the only time anyone runs it.
#[test]
fn check_reports_every_reference_and_flags_the_missing_ones() {
    let fixture = Fixture::multi("check");
    // Three of the seven references exist; the rest are missing.
    fixture.put("hand-written-team-pw", "pw");
    fixture.put("hand-written-token", "token");
    fixture.put("hand-written-artifact-key", "artifact");

    let report = check_with_resolver(&fixture.config_path, &fixture.resolver).expect("check");

    let subjects: Vec<&str> = report
        .entries
        .iter()
        .map(|entry| entry.subject.as_str())
        .collect();
    assert_eq!(
        subjects,
        vec![
            // In config order, mounts first: the two filesystem mounts contribute nothing.
            "mounts.team.passwordRef",
            "mounts.team.e2ee.passphraseRef",
            "mounts.team.e2ee.obfuscatePassphraseRef",
            "mounts.bare.passwordRef",
            "mounts.wiki.apiKeyRef",
            "auth.tokenRef",
            "embedding.apiKeyRef",
            // Included even though `set` cannot address it: `doctor` already checks it, so
            // omitting it would make this table strictly weaker than the one it models.
            "artifactEmbedding.apiKeyRef",
        ]
    );

    let status = |subject: &str| -> &str {
        report
            .entries
            .iter()
            .find(|entry| entry.subject == subject)
            .map(|entry| entry.status.as_str())
            .unwrap_or_else(|| panic!("{subject} is not in the table"))
    };
    assert_eq!(status("mounts.team.passwordRef"), "ok");
    assert_eq!(status("auth.tokenRef"), "ok");
    assert_eq!(status("artifactEmbedding.apiKeyRef"), "ok");
    assert_eq!(status("mounts.team.e2ee.passphraseRef"), "missing");
    assert_eq!(status("mounts.bare.passwordRef"), "missing");
    assert_eq!(status("mounts.wiki.apiKeyRef"), "missing");
    assert_eq!(status("embedding.apiKeyRef"), "missing");
    assert!(
        !report.ok,
        "a MISSING reference must make the report not-ok"
    );

    // Every reference reports its kind and its descriptor, never a value — and every LINE
    // leads with the bracketed verdict, the shape `doctor` prints its checks in. Pinned
    // because the verdict used to be the LAST field, where a long `osKeyring service=…
    // account=…` pushed it out of its column exactly on the rows that had something to say.
    for entry in &report.entries {
        assert_eq!(entry.kind, "encryptedFile");
        assert!(entry
            .reference
            .starts_with("encryptedFile id=hand-written-"));
        assert!(
            entry.line.starts_with("[ok     ] ") || entry.line.starts_with("[MISSING] "),
            "a check line must lead with its verdict: {}",
            entry.line
        );
        assert!(entry.line.contains(&entry.subject), "{}", entry.line);
        assert!(entry.line.ends_with(&entry.reference), "{}", entry.line);
    }

    // The rotate hint is the command that actually works, and absent for the two references
    // no command can address.
    let rotate = |subject: &str| -> Option<String> {
        report
            .entries
            .iter()
            .find(|entry| entry.subject == subject)
            .expect("entry")
            .rotate_with
            .clone()
    };
    assert_eq!(
        rotate("mounts.wiki.apiKeyRef").as_deref(),
        Some("secrets set --mount wiki --field api-key")
    );
    assert_eq!(
        rotate("mounts.team.e2ee.passphraseRef").as_deref(),
        Some("secrets set --mount team --field e2ee-passphrase")
    );
    assert_eq!(
        rotate("embedding.apiKeyRef").as_deref(),
        Some("secrets set --target embedding-api-key")
    );
    assert_eq!(rotate("mounts.team.e2ee.obfuscatePassphraseRef"), None);
    assert_eq!(rotate("artifactEmbedding.apiKeyRef"), None);

    let text = render_check_report(&report);
    assert!(text.contains("MISSING"), "{text}");
    assert!(
        text.contains("8 reference(s) checked, 3 resolved."),
        "{text}"
    );
    // A missing reference is followed by the way to fix it...
    assert!(
        text.contains("rotate with: deep-obsidian-mcp secrets set --mount wiki --field api-key"),
        "{text}"
    );
    // ... and the environment-override caveat is always present, because without it a
    // MISSING line reads as "the server is broken" when it need not be.
    assert!(text.contains("DEEP_OBSIDIAN_AUTH_TOKEN"), "{text}");
    assert!(
        text.contains("reports the STORE, not the effective value"),
        "{text}"
    );

    // A hand-configured reference that is missing says so instead of naming a command.
    fixture.put("hand-written-team-e2ee", "passphrase");
    let report = check_with_resolver(&fixture.config_path, &fixture.resolver).expect("check again");
    let text = render_check_report(&report);
    assert!(text.contains("no command rotates this reference"), "{text}");
}

/// `check` on a config that references no secret at all still succeeds and says so.
#[test]
fn check_on_a_config_with_no_references_is_ok_and_explicit() {
    let fixture = Fixture::legacy("check-empty");
    let report = check_with_resolver(&fixture.config_path, &fixture.resolver).expect("check");
    assert!(report.entries.is_empty());
    assert!(report.ok);
    let text = render_check_report(&report);
    assert!(text.contains("references no secret at all"), "{text}");
}

/// A rotation is visible to `check` immediately, and `check` is what proves it.
///
/// The in-process half of the cross-process gate; the two-process half is at the argv layer.
#[test]
fn check_sees_a_rotation() {
    let fixture = Fixture::multi("check-after-set");
    assert_eq!(
        check_with_resolver(&fixture.config_path, &fixture.resolver)
            .expect("check")
            .entries
            .iter()
            .filter(|entry| entry.status == "ok")
            .count(),
        0
    );

    set_mount_with_resolver(
        &fixture.config_path,
        "wiki",
        None,
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect("rotate");

    let report = check_with_resolver(&fixture.config_path, &fixture.resolver).expect("check");
    let entry = report
        .entries
        .iter()
        .find(|entry| entry.subject == "mounts.wiki.apiKeyRef")
        .expect("entry");
    assert_eq!(entry.status, "ok");
}

// ---------------------------------------------------------------------------
// The value never leaves the store
// ---------------------------------------------------------------------------

/// No rendering of any report ever carries the value — text or JSON, set or check.
///
/// A `grep` test rather than a review note: every message in this module is a template with a
/// secret in scope, so the one that interpolates it by mistake is one edit away at all times.
#[test]
fn no_report_or_rendering_ever_carries_the_value() {
    let fixture = Fixture::multi("no-leak");

    let set_report = set_mount_with_resolver(
        &fixture.config_path,
        "team",
        None,
        &fixture.resolver,
        false,
        &mut one(NEW_SECRET),
    )
    .expect("rotate");
    let check_report = check_with_resolver(&fixture.config_path, &fixture.resolver).expect("check");

    for rendering in [
        render_set_report(&set_report),
        serde_json::to_string_pretty(&set_report).expect("set json"),
        render_check_report(&check_report),
        serde_json::to_string_pretty(&check_report).expect("check json"),
    ] {
        assert!(
            !rendering.contains(NEW_SECRET),
            "a report carried the secret: {rendering}"
        );
    }

    // The store holds it, so the assertions above are about the reports and not about a
    // rotation that quietly did nothing.
    assert_eq!(
        fixture.stored_value("hand-written-team-pw").as_deref(),
        Some(NEW_SECRET)
    );
}

// ---------------------------------------------------------------------------
// argv
// ---------------------------------------------------------------------------

/// Run the real binary with `XDG_CONFIG_HOME` pointed at a temp directory.
///
/// The env var matters: the spawned process builds `SecretResolver::new()`, whose encrypted
/// file is `default_secrets_path()` — i.e. `$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json`.
/// Without the override, an argv-level rotation would write into the developer's real
/// secrets file. With it, two separate processes share one temp store, which is what makes
/// the cross-process assertion below possible.
///
/// stdin is supplied explicitly rather than inherited: `None` closes it, so a command that
/// prompts reads EOF and takes its refusing branch instead of hanging the suite.
fn run(
    config_path: &Path,
    xdg: &Path,
    stdin: Option<&str>,
    args: &[&str],
) -> (bool, String, String) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .env("XDG_CONFIG_HOME", xdg)
        .arg("--config")
        .arg(config_path)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the binary");
    if let Some(stdin_text) = stdin {
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(stdin_text.as_bytes())
            .expect("write stdin");
    }
    let output = child.wait_with_output().expect("run the binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// The `secrets` family is reachable through the real binary's argv, and a rotation in one
/// process is visible to a `check` in another.
///
/// One process-spawning test for the whole family rather than one per subcommand: what can
/// break here is the shared plumbing — clap's derive, `normalize_cli_args`, the dispatch, the
/// `--json` switch — and spawning a process per subcommand pays for the same coverage
/// repeatedly.
///
/// The cross-process claim is the interesting one. `secrets set --stdin` runs in process A;
/// `secrets check` then runs in process B, with no shared memory and no shared handle, and
/// sees the rotated reference resolve. That is the verification the roadmap's final slice
/// asked for, made a user-visible command.
#[test]
fn cli_argv_rotates_in_one_process_and_verifies_in_another() {
    let fixture = Fixture::multi("argv");
    let xdg = fixture.base.join("xdg");
    std::fs::create_dir_all(&xdg).expect("xdg dir");
    let before = fixture.config_text();

    // `secrets check` first: the table renders, and the command exits NON-ZERO because every
    // reference is missing.
    let (ok, stdout, stderr) = run(&fixture.config_path, &xdg, None, &["secrets", "check"]);
    assert!(!ok, "check must exit non-zero with missing references");
    assert!(stdout.contains("mounts.team.passwordRef"), "{stdout}");
    assert!(stdout.contains("MISSING"), "{stdout}");
    assert!(
        stdout.contains("8 reference(s) checked, 0 resolved."),
        "{stdout}"
    );
    assert!(stderr.is_empty(), "{stderr}");

    // Rotate the algolia key through argv, value on stdin. `--field` is given explicitly so
    // its bare-word VALUE has to survive the normalizer.
    let (ok, stdout, stderr) = run(
        &fixture.config_path,
        &xdg,
        Some(&format!("{NEW_SECRET}\n")),
        &[
            "secrets", "set", "--mount", "wiki", "--field", "api-key", "--stdin",
        ],
    );
    assert!(ok, "secrets set failed\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("rotated mounts.wiki.apiKeyRef"), "{stdout}");
    assert!(
        stdout.contains("encryptedFile id=hand-written-wiki-key"),
        "{stdout}"
    );
    // The value is nowhere in the output.
    assert!(!stdout.contains(NEW_SECRET), "{stdout}");
    assert!(!stderr.contains(NEW_SECRET), "{stderr}");

    // It landed in the store the OTHER process will read, at the declared id and nowhere
    // else. (`$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json`, per `default_secrets_path`.)
    let shared_secrets = xdg.join("deep-obsidian-mcp").join("secrets.json");
    let text = std::fs::read_to_string(&shared_secrets).expect("the child wrote a secrets file");
    assert!(text.contains("hand-written-wiki-key"), "{text}");
    assert!(
        !text.contains("mount-wiki-api-key"),
        "a derived id was written: {text}"
    );
    assert!(!text.contains(NEW_SECRET), "plaintext in the store: {text}");

    // A SECOND process now sees it — cross-process verification of the rotation.
    let (ok, stdout, stderr) = run(
        &fixture.config_path,
        &xdg,
        None,
        &["secrets", "check", "--json"],
    );
    assert!(
        !ok,
        "still non-zero: the other seven references are missing"
    );
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json report");
    let entries = parsed["entries"].as_array().expect("entries");
    let wiki = entries
        .iter()
        .find(|entry| entry["subject"] == "mounts.wiki.apiKeyRef")
        .expect("the wiki entry");
    assert_eq!(wiki["status"], "ok");
    assert_eq!(wiki["kind"], "encryptedFile");
    assert!(!stdout.contains(NEW_SECRET), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");

    // Not one of those runs touched the config.
    assert_eq!(fixture.config_text(), before);
    assert!(!fixture.backup_path().exists());
}

/// What `--stdin` actually stores, for the three shapes a shell produces.
///
/// Driven through the real binary because this is the one property `from_lines` cannot check:
/// only a spawned process exercises `SecretReader::from_stdin`, and the question is exactly
/// what its `text.lines()` does to a real pipe. Asserted by DECRYPTING the child's store, so
/// the claim is about the bytes that landed rather than about a message.
///
/// The contract, which the `--stdin` flag doc states: the first line, newline stripped,
/// nothing else changed. A password may legitimately end in a space, so trimming one off
/// would store a secret nobody typed.
#[test]
fn cli_argv_stdin_stores_the_first_line_verbatim() {
    let fixture = Fixture::multi("argv-stdin");
    let xdg = fixture.base.join("xdg");
    std::fs::create_dir_all(&xdg).expect("xdg dir");
    let child_store = SecretResolver::with_encrypted_file_path(
        xdg.join("deep-obsidian-mcp").join("secrets.json"),
    );
    let stored = || -> Option<String> {
        child_store
            .get(&SecretRef::EncryptedFile {
                id: "hand-written-wiki-key".to_string(),
            })
            .expect("read the child's store")
            .map(|value| secrecy::ExposeSecret::expose_secret(&value).to_string())
    };

    for (label, stdin, expected) in [
        // `echo value |` — the common case.
        ("trailing newline", "plain-value\n", "plain-value"),
        // `printf '%s' value |` — no trailing newline at all.
        (
            "no trailing newline",
            "unterminated-value",
            "unterminated-value",
        ),
        // Trailing space KEPT: it is part of the credential, not noise.
        ("trailing space", "value-with-space \n", "value-with-space "),
        // Only the FIRST line is the secret; a second is ignored rather than concatenated.
        ("extra lines", "first-line\nsecond-line\n", "first-line"),
    ] {
        let (ok, stdout, stderr) = run(
            &fixture.config_path,
            &xdg,
            Some(stdin),
            &["secrets", "set", "--mount", "wiki", "--stdin"],
        );
        assert!(ok, "{label} failed\nstdout: {stdout}\nstderr: {stderr}");
        assert_eq!(stored().as_deref(), Some(expected), "{label}");
    }

    // A blank line is REFUSED rather than stored over a working credential: an empty password
    // would fail later as an authentication error instead of now as a typo. The previous
    // value survives, which is the half that matters.
    let (ok, _stdout, stderr) = run(
        &fixture.config_path,
        &xdg,
        Some("\n"),
        &["secrets", "set", "--mount", "wiki", "--stdin"],
    );
    assert!(!ok, "a blank line must be refused");
    assert!(stderr.contains("is required and was empty"), "{stderr}");
    assert_eq!(stored().as_deref(), Some("first-line"));

    // Empty stdin says what to pipe rather than reporting a mounts-add-shaped order.
    let (ok, _stdout, stderr) = run(
        &fixture.config_path,
        &xdg,
        Some(""),
        &["secrets", "set", "--mount", "wiki", "--stdin"],
    );
    assert!(!ok, "an empty stdin must be refused");
    assert!(stderr.contains("no value left on stdin"), "{stderr}");
    assert!(
        stderr.contains("`secrets set --stdin` reads one line"),
        "{stderr}"
    );
    assert_eq!(stored().as_deref(), Some("first-line"));
}

/// The refusals reach the operator through argv too, and clap enforces the mutually
/// exclusive addressing.
#[test]
fn cli_argv_reports_the_refusals() {
    let fixture = Fixture::multi("argv-refusals");
    let xdg = fixture.base.join("xdg");
    std::fs::create_dir_all(&xdg).expect("xdg dir");

    // An unknown mount.
    let (ok, _stdout, stderr) = run(
        &fixture.config_path,
        &xdg,
        Some("x\n"),
        &["secrets", "set", "--mount", "nope", "--stdin"],
    );
    assert!(!ok);
    assert!(stderr.contains("no mount with id \"nope\""), "{stderr}");

    // A field that does not belong to the mount's kind.
    let (ok, _stdout, stderr) = run(
        &fixture.config_path,
        &xdg,
        Some("x\n"),
        &[
            "secrets", "set", "--mount", "team", "--field", "api-key", "--stdin",
        ],
    );
    assert!(!ok);
    assert!(stderr.contains("has no apiKeyRef"), "{stderr}");

    // A `--field` value clap does not know.
    let (ok, _stdout, stderr) = run(
        &fixture.config_path,
        &xdg,
        None,
        &["secrets", "set", "--mount", "team", "--field", "banana"],
    );
    assert!(!ok);
    assert!(stderr.contains("invalid value 'banana'"), "{stderr}");
    // The error names the vocabulary, which is the only place it is enumerated for the user.
    assert!(stderr.contains("e2ee-passphrase"), "{stderr}");

    // Neither `--mount` nor `--target`: clap's own group error, not a panic.
    let (ok, _stdout, stderr) = run(&fixture.config_path, &xdg, None, &["secrets", "set"]);
    assert!(!ok);
    assert!(
        stderr.contains("--mount") && stderr.contains("--target"),
        "{stderr}"
    );

    // Both at once is rejected as mutually exclusive.
    let (ok, _stdout, stderr) = run(
        &fixture.config_path,
        &xdg,
        None,
        &[
            "secrets",
            "set",
            "--mount",
            "team",
            "--target",
            "auth-token",
        ],
    );
    assert!(!ok);
    assert!(stderr.contains("cannot be used with"), "{stderr}");

    // Nothing was written to the shared store by any refusal.
    assert!(!xdg.join("deep-obsidian-mcp").join("secrets.json").exists());
}

/// `secrets --help` keeps the hand-written summary, which now documents the family.
///
/// Only `mounts` falls through to clap's derived help (see `commands::run`), so this also
/// guards that exception from spreading by accident.
#[test]
fn cli_argv_secrets_help_is_the_hand_written_summary() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .args(["secrets", "--help"])
        .output()
        .expect("run --help");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        text.starts_with("Usage:\n  deep-obsidian-mcp [serve]"),
        "{text}"
    );
    assert!(
        text.contains("secrets set --mount <id> [--field password|e2ee-passphrase|api-key]"),
        "{text}"
    );
    assert!(text.contains("secrets check [--json]"), "{text}");
    // The two claims the family lives or dies by.
    assert!(text.contains("never\nmodifies the config file"), "{text}");
    assert!(text.contains("rotation is not migration"), "{text}");
}
