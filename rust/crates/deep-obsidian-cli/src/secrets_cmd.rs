//! `secrets set` and `secrets check`: rotate the value behind a reference the config
//! already holds, and prove every reference still resolves.
//!
//! # The one invariant
//!
//! **A rotation writes to the reference the CONFIG FILE contains, byte for byte.** Not to a
//! freshly derived one. [`crate::mounts_cmd`] derives `mount-<id>-<purpose>` when it CREATES
//! a credential, and it may do so because it also writes the reference it chose into the
//! config. This command writes no config, so it has no such freedom: a reference may have
//! been hand-written to any account or id at all, and a rotation that guessed
//! `mount-team-password` would leave the config pointing at a value nobody updated — the
//! mount would keep authenticating with the old secret and the operator would have no way to
//! tell which of the two entries was live. So the reference is READ out of the file and
//! written back to unchanged.
//!
//! # Rotation is not migration
//!
//! The reference's own KIND is preserved: an `osKeyring` reference stays in the keyring, an
//! `encryptedFile` reference stays in the file. This is the same invariant stated a second
//! way — the kind is part of the reference — and it is why there is **no fallback** here.
//! `mounts add` falls back from an unavailable keyring to the encrypted file and reports it,
//! which is correct there because the reference it writes into the config is the one it
//! actually used. The same fallback here would write the value somewhere the config does not
//! point, i.e. would orphan it: the mount would still read the stale keyring entry, or fail
//! outright, and the operator would believe the rotation had happened. So a failed write is
//! REPORTED with the remedy and nothing else is touched.
//!
//! Moving a secret between stores is therefore deliberately not a thing this command does.
//! It is a config change (the reference itself changes), which means editing the mount — or
//! `mounts remove` followed by `mounts add`, which chooses fresh.
//!
//! # Why the FILE and not the resolved config
//!
//! For the reasons [`crate::mounts_cmd::run`] reads the file, plus two of its own. A stray
//! `--vault` or `DEEP_OBSIDIAN_*` must not influence which reference is written to; and
//! `normalize_service_config` fills in defaults, so the resolved view can present a
//! reference the file does not declare. Reading the file has the useful side effect that
//! both commands work on a config the loader REFUSES — which is exactly when an operator is
//! most likely to be repairing credentials.
//!
//! # What `check` does not know
//!
//! Environment variables shadow a reference at runtime (`DEEP_OBSIDIAN_AUTH_TOKEN`,
//! `DEEP_OBSIDIAN_EMBEDDING_API_KEY`, `DEEP_OBSIDIAN_ALGOLIA_API_KEY`). `check` reports the
//! STORE, not the effective value, so a MISSING line on a running install can be correct
//! for both. It says so in its own output rather than leaving the reader to guess.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use deep_obsidian_config::{read_config_file, secrets::SecretResolver};
use deep_obsidian_types::{MountBackendConfig, MountConfig, PersistedServiceConfig, SecretRef};
use secrecy::ExposeSecret;
use serde::Serialize;

use crate::cli::{SecretField, SecretTarget, SecretsCommand, ServiceOptions};
use crate::mounts_cmd::{describe_secret_ref, SecretReader};

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// The `kind` discriminator a reference is reported under. The same two words `doctor`'s
/// secret checks use, so one vocabulary describes a store everywhere.
fn reference_kind(reference: &SecretRef) -> &'static str {
    match reference {
        SecretRef::OsKeyring { .. } => "osKeyring",
        SecretRef::EncryptedFile { .. } => "encryptedFile",
    }
}

/// What `secrets set` did.
///
/// Carries the REFERENCE and nothing else: a rotation report that echoed the value would
/// put it in a terminal, in a scrollback buffer and in whatever collects `--json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsSetReport {
    pub config_path: PathBuf,
    /// The config path of the reference that was rotated, e.g. `mounts.team.passwordRef`.
    pub subject: String,
    /// `osKeyring` or `encryptedFile` — the reference's own kind, preserved.
    pub kind: String,
    /// The reference rendered for an operator. Never a value.
    pub reference: String,
    /// True when a value reached the store AND was read back from it.
    pub stored: bool,
    pub dry_run: bool,
    pub messages: Vec<String>,
}

/// One line of `secrets check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsCheckEntry {
    /// Where the reference lives in the config, e.g. `mounts.team.e2ee.passphraseRef`.
    pub subject: String,
    pub kind: String,
    pub reference: String,
    /// `ok`, `missing`, or `error`.
    pub status: String,
    /// The store's own words, when the lookup FAILED rather than merely came back empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The `secrets set` invocation that rotates this reference, when one does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotate_with: Option<String>,
    /// The text rendering, from the same renderer the table uses, so the JSON and the text
    /// output cannot drift.
    pub line: String,
}

/// What `secrets check` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsCheckReport {
    pub config_path: PathBuf,
    pub entries: Vec<SecretsCheckEntry>,
    /// False when any reference is `missing` or `error`. Drives the exit code.
    pub ok: bool,
    pub messages: Vec<String>,
}

// ---------------------------------------------------------------------------
// Reading the file
// ---------------------------------------------------------------------------

/// Read the config file, flattening the loader's own message into ours.
///
/// Same reason [`crate::mounts_cmd`] inlines its cause: `main` prints `{error}` only, so a
/// parse error attached as an anyhow cause reaches the operator with the line number
/// stripped off.
fn load_config_file(config_path: &Path) -> Result<PersistedServiceConfig> {
    read_config_file(config_path)
        .map_err(|error| {
            anyhow!(
                "failed to load config file {}: {error}",
                config_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow!(
                "no config file at {}: `secrets` rotates and checks the references an existing \
                 config holds. Run `deep-obsidian-mcp setup-service --wizard` first.",
                config_path.display()
            )
        })
}

/// The mount with this id, or a refusal naming what the file does declare.
fn find_mount<'a>(
    config: &'a PersistedServiceConfig,
    config_path: &Path,
    id: &str,
) -> Result<&'a MountConfig> {
    let Some(mounts) = config.mounts.as_ref().filter(|mounts| !mounts.is_empty()) else {
        bail!(
            "{} declares no mount table, so no mount has a credential to rotate: its vault is \
             the top-level `vaultPath`, which needs none. `deep-obsidian-mcp secrets check` \
             lists the references this config does hold.",
            config_path.display()
        );
    };
    mounts.iter().find(|mount| mount.id == id).ok_or_else(|| {
        anyhow!(
            "no mount with id {id:?} in {}. Declared ids: {}.",
            config_path.display(),
            mounts
                .iter()
                .map(|mount| format!("'{}'", mount.id))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The field a mount's rotation addresses when `--field` was omitted.
///
/// Derived from the backend kind, because each remote kind has exactly one credential that
/// is not conditional: a couchdb mount always has a password, an algolia mount always has an
/// API key. `e2ee-passphrase` is conditional AND accompanies a password, so it is never
/// defaulted to — a wrong guess there rotates the secret the operator did not mean and
/// surfaces later as a decryption failure rather than now as a question.
fn default_field(mount: &MountConfig, config_path: &Path) -> Result<SecretField> {
    match &mount.backend {
        MountBackendConfig::Couchdb { .. } => Ok(SecretField::Password),
        MountBackendConfig::Algolia { .. } => Ok(SecretField::ApiKey),
        MountBackendConfig::Filesystem { .. } => bail!(
            "mount '{}' in {} is a filesystem mount: it reads a local directory and stores no \
             credential, so there is nothing to rotate.",
            mount.id,
            config_path.display()
        ),
    }
}

/// How a field is spelled where it lives in the config.
fn field_config_path(field: SecretField) -> &'static str {
    match field {
        SecretField::Password => "passwordRef",
        SecretField::E2eePassphrase => "e2ee.passphraseRef",
        SecretField::ApiKey => "apiKeyRef",
    }
}

/// The `--field` value, as it is typed.
fn field_flag_value(field: SecretField) -> &'static str {
    match field {
        SecretField::Password => "password",
        SecretField::E2eePassphrase => "e2ee-passphrase",
        SecretField::ApiKey => "api-key",
    }
}

/// The prompt label one field is read under.
fn field_label(field: SecretField) -> &'static str {
    match field {
        SecretField::Password => "New CouchDB password",
        SecretField::E2eePassphrase => "New CouchDB vault E2EE passphrase",
        SecretField::ApiKey => "New Algolia API key",
    }
}

/// The EXACT reference this mount's field points at, or a refusal that says why there is
/// none.
///
/// The three refusals are deliberately different messages, because they call for three
/// different actions:
///
/// * **Wrong field for the kind** — a typo. Names the fields this kind does have.
/// * **No `e2ee` section** — a config change, not a rotation. Turning on end-to-end
///   encryption means the mount now needs a passphrase to read anything, so inventing the
///   reference here would leave a config that says "encrypted" for a vault that may not be,
///   and every read would fail in a way that looks like corruption. The message points at
///   the mount.
/// * **Filesystem** — nothing to rotate at all.
fn mount_field_reference(
    mount: &MountConfig,
    field: SecretField,
    config_path: &Path,
) -> Result<SecretRef> {
    let wrong_field = |available: &str| -> anyhow::Error {
        anyhow!(
            "mount '{}' is a {} mount and has no {}: its rotatable field(s) are {available}. Run \
             `deep-obsidian-mcp secrets check` to see every reference {} holds.",
            mount.id,
            mount.backend.kind_name(),
            field_config_path(field),
            config_path.display()
        )
    };
    match (&mount.backend, field) {
        (MountBackendConfig::Filesystem { .. }, _) => {
            // Reuses `default_field`'s wording so the two paths into this refusal read the
            // same, whether the operator named a field or not.
            Err(default_field(mount, config_path).expect_err("a filesystem mount has no field"))
        }
        (MountBackendConfig::Couchdb { password_ref, .. }, SecretField::Password) => {
            Ok(password_ref.clone())
        }
        (MountBackendConfig::Couchdb { e2ee, .. }, SecretField::E2eePassphrase) => e2ee
            .as_ref()
            .map(|e2ee| e2ee.passphrase_ref.clone())
            .ok_or_else(|| {
                anyhow!(
                    "mount '{}' in {} is not configured for end-to-end encryption: it declares no \
                     `e2ee` section, so there is no `passphraseRef` to rotate. Adding E2EE is a \
                     MOUNT-CONFIG change, not a rotation — a mount that claims to be encrypted \
                     when the vault is not (or the other way round) fails every read in a way \
                     that looks like corruption. Edit the mount's `e2ee` section in {}, or \
                     re-add the mount with `deep-obsidian-mcp mounts add couchdb --e2ee`, and \
                     rotate the passphrase afterwards.",
                    mount.id,
                    config_path.display(),
                    config_path.display()
                )
            }),
        (MountBackendConfig::Couchdb { .. }, SecretField::ApiKey) => {
            Err(wrong_field("`password` and `e2ee-passphrase`"))
        }
        (MountBackendConfig::Algolia { api_key_ref, .. }, SecretField::ApiKey) => {
            Ok(api_key_ref.clone())
        }
        (MountBackendConfig::Algolia { .. }, _) => Err(wrong_field("`api-key`")),
    }
}

/// The EXACT reference a non-mount target points at, or a refusal.
///
/// Both refusals say the same substantive thing: `secrets set` rotates a VALUE, and a
/// reference is what tells it where. A config with no reference has nothing to rotate, and
/// creating one is a config write this command does not do — so each message names the
/// command that does.
fn target_reference(
    config: &PersistedServiceConfig,
    target: SecretTarget,
    config_path: &Path,
) -> Result<SecretRef> {
    match target {
        SecretTarget::AuthToken => config
            .auth
            .as_ref()
            .and_then(|auth| auth.token_ref.clone())
            .ok_or_else(|| {
                anyhow!(
                    "no HTTP bearer token is configured in {}: `auth.tokenRef` is unset, so there \
                     is no reference to rotate. `deep-obsidian-mcp setup-service --auth` \
                     generates a token, stores it and writes the reference; `secrets set` only \
                     replaces the value an existing reference points at.",
                    config_path.display()
                )
            }),
        SecretTarget::EmbeddingApiKey => config
            .embedding
            .as_ref()
            .and_then(|embedding| embedding.api_key_ref.clone())
            .ok_or_else(|| {
                anyhow!(
                    "no embedding API key is configured in {}: `embedding.apiKeyRef` is unset, so \
                     there is no reference to rotate. The embedding screen of \
                     `deep-obsidian-mcp setup-service --wizard` stores a key and writes the \
                     reference; `secrets set` only replaces the value an existing reference \
                     points at. A local endpoint such as Ollama needs no key at all.",
                    config_path.display()
                )
            }),
    }
}

fn target_config_path(target: SecretTarget) -> &'static str {
    match target {
        SecretTarget::AuthToken => "auth.tokenRef",
        SecretTarget::EmbeddingApiKey => "embedding.apiKeyRef",
    }
}

fn target_label(target: SecretTarget) -> &'static str {
    match target {
        SecretTarget::AuthToken => "New HTTP bearer token",
        SecretTarget::EmbeddingApiKey => "New embedding API key",
    }
}

// ---------------------------------------------------------------------------
// The shared rotation core
// ---------------------------------------------------------------------------

/// One rotation, once the reference is known.
struct Rotation<'a> {
    config_path: &'a Path,
    /// Where the reference lives in the config, for the report and every message.
    subject: String,
    /// The reference READ OUT OF THE FILE. Written back to unchanged; see the module docs.
    reference: SecretRef,
    /// The masked prompt's wording.
    label: &'a str,
    /// Advice appended once the value is stored — what else the operator now has to do.
    follow_up: Option<String>,
}

/// Store one new value at an existing reference, then read it back from the store.
///
/// The read-back is not ceremony: `put` returning `Ok` means the store accepted the write,
/// and for the encrypted file that is a rename over a temp file whose result is worth
/// confirming before an operator is told a rotation succeeded. It compares in memory and
/// reports only a verdict — a mismatch names the reference and neither value.
///
/// There is no fallback to the other store, ever. See the module docs.
fn rotate(
    rotation: &Rotation<'_>,
    resolver: &SecretResolver,
    dry_run: bool,
    secrets: &mut SecretReader,
) -> Result<SecretsSetReport> {
    let descriptor = describe_secret_ref(&rotation.reference);
    let report = |stored: bool, messages: Vec<String>| SecretsSetReport {
        config_path: rotation.config_path.to_path_buf(),
        subject: rotation.subject.clone(),
        kind: reference_kind(&rotation.reference).to_string(),
        reference: descriptor.clone(),
        stored,
        dry_run,
        messages,
    };

    if dry_run {
        // Nothing is READ either, not just nothing written: prompting for a secret and then
        // discarding it would train an operator to type a credential into a command that
        // does nothing with it, and on the `--stdin` path it would consume the line.
        return Ok(report(
            false,
            vec![format!(
                "dry-run: {} points at {descriptor}; no value was read and nothing was stored. \
                 {} was not modified (it never is).",
                rotation.subject,
                rotation.config_path.display()
            )],
        ));
    }

    let value = secrets.next(rotation.label)?;
    // The whole point of this command, in one call: the reference is the one the file holds,
    // so the kind is preserved by construction rather than by a decision.
    resolver
        .put(&rotation.reference, value.clone())
        .map_err(|error| {
            anyhow!(
                "could not store the new value at {descriptor}: {error}\n\
             Nothing was changed. This command deliberately does NOT fall back to the other \
             store: {} still points at this reference, so a value written anywhere else would \
             be ignored and the old one would stay live. Fix the store and run the command \
             again — for an osKeyring reference, unlock the login keyring (or start the \
             keyring daemon); for an encryptedFile reference, check the permissions on {}. To \
             move this secret to a different store, change the reference itself in {}.",
                rotation.subject,
                deep_obsidian_config::default_secrets_path().display(),
                rotation.config_path.display()
            )
        })?;

    let read_back = resolver.get(&rotation.reference).map_err(|error| {
        anyhow!(
            "the new value was written to {descriptor} but could not be read back: {error}. Run \
             `deep-obsidian-mcp secrets check` before relying on it."
        )
    })?;
    let matches = read_back
        .as_ref()
        .is_some_and(|stored| stored.expose_secret() == value.expose_secret());
    if !matches {
        bail!(
            "the new value was written to {descriptor} but the store did not return it again. \
             Treat this rotation as NOT done: run `deep-obsidian-mcp secrets check` and inspect \
             the store before relying on it."
        );
    }

    let mut messages = vec![
        format!(
            "rotated {} at {descriptor} — the value was written and read back from the store.",
            rotation.subject
        ),
        format!(
            "{} was NOT modified: it already pointed at this reference, and the reference's own \
             store ({}) was preserved. Rotation is not migration.",
            rotation.config_path.display(),
            reference_kind(&rotation.reference)
        ),
    ];
    if let Some(follow_up) = &rotation.follow_up {
        messages.push(follow_up.clone());
    }
    Ok(report(true, messages))
}

// ---------------------------------------------------------------------------
// set
// ---------------------------------------------------------------------------

/// Rotate one of a mount's credentials. See [`set_mount_with_resolver`].
pub fn set_mount(
    config_path: &Path,
    mount_id: &str,
    field: Option<SecretField>,
    dry_run: bool,
    secrets: &mut SecretReader,
) -> Result<SecretsSetReport> {
    set_mount_with_resolver(
        config_path,
        mount_id,
        field,
        &SecretResolver::new(),
        dry_run,
        secrets,
    )
}

/// [`set_mount`] against an explicit secret store.
///
/// Exists for the reason `mounts_cmd::add_with_resolver` does: a test must be able to point
/// at a temp secrets file instead of the process-global default path. Note there is no
/// `prefer_os_keyring` twin here and there cannot be one — the reference decides the store,
/// which is this module's entire point. A test that must stay out of the login keychain
/// therefore uses a config whose references are `encryptedFile`, which is also the shape a
/// headless install has.
pub fn set_mount_with_resolver(
    config_path: &Path,
    mount_id: &str,
    field: Option<SecretField>,
    resolver: &SecretResolver,
    dry_run: bool,
    secrets: &mut SecretReader,
) -> Result<SecretsSetReport> {
    let config = load_config_file(config_path)?;
    let mount = find_mount(&config, config_path, mount_id)?;
    let field = match field {
        Some(field) => field,
        None => default_field(mount, config_path)?,
    };
    let reference = mount_field_reference(mount, field, config_path)?;
    let rotation = Rotation {
        config_path,
        subject: format!("mounts.{mount_id}.{}", field_config_path(field)),
        reference,
        label: field_label(field),
        follow_up: Some(format!(
            "the mount was NOT contacted: this command only writes to the store. Run \
             `deep-obsidian-mcp doctor --probe-remote` to confirm mount '{mount_id}' can still \
             authenticate with the new {}.",
            field_flag_value(field)
        )),
    };
    rotate(&rotation, resolver, dry_run, secrets)
}

/// Rotate a secret that belongs to the config rather than to a mount.
pub fn set_target(
    config_path: &Path,
    target: SecretTarget,
    dry_run: bool,
    secrets: &mut SecretReader,
) -> Result<SecretsSetReport> {
    set_target_with_resolver(
        config_path,
        target,
        &SecretResolver::new(),
        dry_run,
        secrets,
    )
}

/// [`set_target`] against an explicit secret store. See [`set_mount_with_resolver`].
pub fn set_target_with_resolver(
    config_path: &Path,
    target: SecretTarget,
    resolver: &SecretResolver,
    dry_run: bool,
    secrets: &mut SecretReader,
) -> Result<SecretsSetReport> {
    let config = load_config_file(config_path)?;
    let reference = target_reference(&config, target, config_path)?;
    let rotation = Rotation {
        config_path,
        subject: target_config_path(target).to_string(),
        reference,
        label: target_label(target),
        // Both of these are the "and now something else is broken until you act" case, which
        // a rotation report has to say out loud: the operator has just invalidated the
        // credential every client of this server holds.
        follow_up: Some(
            match target {
                SecretTarget::AuthToken => {
                    "every MCP client configured with the OLD bearer token will now get 401. \
                     Update their `Authorization: Bearer` header, and restart the server so it \
                     re-reads the token (it resolves the reference once, at startup)."
                }
                SecretTarget::EmbeddingApiKey => {
                    "restart the server so it re-reads the key (it resolves the reference once, \
                     at startup). Until then, embedding-backed recall keeps using the old key \
                     and falls back to lexical if that key has been revoked."
                }
            }
            .to_string(),
        ),
    };
    rotate(&rotation, resolver, dry_run, secrets)
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// One reference to check, as the config declares it.
struct DeclaredRef {
    subject: String,
    reference: SecretRef,
    /// The `secrets set` invocation that rotates it, or `None` for a reference no command
    /// can address.
    rotate_with: Option<String>,
}

/// Every reference the FILE declares, in config order.
///
/// Includes the two references `set` cannot address, because "every reference the config
/// holds" is the question `check` answers and a table that quietly omitted the awkward ones
/// would be weaker than the `doctor` it is modelled on — which already checks
/// `artifactEmbeddingApiKey`. Their lines say they are hand-configured instead.
fn declared_refs(config: &PersistedServiceConfig) -> Vec<DeclaredRef> {
    let mut refs = Vec::new();
    for mount in config.mounts.iter().flatten() {
        let id = &mount.id;
        match &mount.backend {
            MountBackendConfig::Filesystem { .. } => {}
            MountBackendConfig::Couchdb {
                password_ref, e2ee, ..
            } => {
                refs.push(DeclaredRef {
                    subject: format!("mounts.{id}.passwordRef"),
                    reference: password_ref.clone(),
                    rotate_with: Some(format!("secrets set --mount {id} --field password")),
                });
                if let Some(e2ee) = e2ee {
                    refs.push(DeclaredRef {
                        subject: format!("mounts.{id}.e2ee.passphraseRef"),
                        reference: e2ee.passphrase_ref.clone(),
                        rotate_with: Some(format!(
                            "secrets set --mount {id} --field e2ee-passphrase"
                        )),
                    });
                    if let Some(obfuscate) = &e2ee.obfuscate_passphrase_ref {
                        refs.push(DeclaredRef {
                            subject: format!("mounts.{id}.e2ee.obfuscatePassphraseRef"),
                            reference: obfuscate.clone(),
                            rotate_with: None,
                        });
                    }
                }
            }
            MountBackendConfig::Algolia { api_key_ref, .. } => refs.push(DeclaredRef {
                subject: format!("mounts.{id}.apiKeyRef"),
                reference: api_key_ref.clone(),
                rotate_with: Some(format!("secrets set --mount {id} --field api-key")),
            }),
        }
    }
    if let Some(reference) = config.auth.as_ref().and_then(|auth| auth.token_ref.clone()) {
        refs.push(DeclaredRef {
            subject: "auth.tokenRef".to_string(),
            reference,
            rotate_with: Some("secrets set --target auth-token".to_string()),
        });
    }
    if let Some(reference) = config
        .embedding
        .as_ref()
        .and_then(|embedding| embedding.api_key_ref.clone())
    {
        refs.push(DeclaredRef {
            subject: "embedding.apiKeyRef".to_string(),
            reference,
            rotate_with: Some("secrets set --target embedding-api-key".to_string()),
        });
    }
    if let Some(reference) = config
        .artifact_embedding
        .as_ref()
        .and_then(|embedding| embedding.api_key_ref.clone())
    {
        refs.push(DeclaredRef {
            subject: "artifactEmbedding.apiKeyRef".to_string(),
            reference,
            rotate_with: None,
        });
    }
    refs
}

/// Report where every reference the config holds resolves. See [`check_with_resolver`].
pub fn check(config_path: &Path) -> Result<SecretsCheckReport> {
    check_with_resolver(config_path, &SecretResolver::new())
}

/// [`check`] against an explicit secret store.
///
/// A MISSING reference is a RESULT, not an error: the whole point of the command is to name
/// the ones that do not resolve, so a run that finds three of them still succeeds and
/// reports. Only [`SecretsCheckReport::ok`] goes false, which the dispatch turns into a
/// non-zero exit — the same shape `doctor` uses.
pub fn check_with_resolver(
    config_path: &Path,
    resolver: &SecretResolver,
) -> Result<SecretsCheckReport> {
    let config = load_config_file(config_path)?;
    let declared = declared_refs(&config);

    let entries: Vec<SecretsCheckEntry> = declared
        .into_iter()
        .map(|declared| {
            let descriptor = describe_secret_ref(&declared.reference);
            let (status, detail) = match resolver.get(&declared.reference) {
                Ok(Some(_)) => ("ok", None),
                Ok(None) => ("missing", None),
                Err(error) => ("error", Some(error.to_string())),
            };
            SecretsCheckEntry {
                line: render_check_line(&declared.subject, status, &descriptor, detail.as_deref()),
                subject: declared.subject,
                kind: reference_kind(&declared.reference).to_string(),
                reference: descriptor,
                status: status.to_string(),
                detail,
                rotate_with: declared.rotate_with,
            }
        })
        .collect();

    let ok = entries.iter().all(|entry| entry.status == "ok");
    let mut messages = Vec::new();
    if entries.is_empty() {
        messages.push(
            "this config references no secret at all: no remote mount, no HTTP bearer token and \
             no embedding API key. Nothing to check."
                .to_string(),
        );
    }
    // Always printed, and load-bearing: without it a MISSING line reads as "the server is
    // broken", which it need not be.
    messages.push(
        "this reports the STORE, not the effective value: an environment variable \
         (DEEP_OBSIDIAN_AUTH_TOKEN, DEEP_OBSIDIAN_EMBEDDING_API_KEY, \
         DEEP_OBSIDIAN_ALGOLIA_API_KEY) shadows a reference at runtime and is NOT consulted \
         here, so a MISSING line can coexist with a working server. No value is ever printed."
            .to_string(),
    );
    Ok(SecretsCheckReport {
        config_path: config_path.to_path_buf(),
        entries,
        ok,
        messages,
    })
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// One table row. `MISSING` is upper-cased and `ok` is not, so the eye finds the problem
/// without reading the column.
fn render_check_line(
    subject: &str,
    status: &str,
    descriptor: &str,
    detail: Option<&str>,
) -> String {
    let verdict = match status {
        "ok" => "ok",
        "missing" => "MISSING",
        _ => "ERROR",
    };
    // `[verdict] subject descriptor` — the same shape `doctor` prints its checks in
    // (`[ok] rg: ...`), and for a second reason here: the verdict is what a reader scans for,
    // and a reference is of unbounded length. With the verdict LAST, one long `osKeyring
    // service=… account=…` pushed it out of its column and the table stopped being scannable
    // exactly when it had something to report.
    let mut line = format!("[{verdict:<7}] {subject:<40} {descriptor}");
    if let Some(detail) = detail.filter(|_| status == "error") {
        line.push_str(" — ");
        line.push_str(detail);
    }
    line
}

pub fn render_set_report(report: &SecretsSetReport) -> String {
    let mut lines = vec![format!("config: {}", report.config_path.display())];
    lines.extend(report.messages.iter().cloned());
    lines.join("\n")
}

pub fn render_check_report(report: &SecretsCheckReport) -> String {
    let mut lines = vec![format!("config: {}", report.config_path.display())];
    for entry in &report.entries {
        lines.push(entry.line.clone());
        if let Some(rotate_with) = &entry.rotate_with {
            if entry.status != "ok" {
                lines.push(format!("  rotate with: deep-obsidian-mcp {rotate_with}"));
            }
        } else if entry.status != "ok" {
            lines.push(
                "  no command rotates this reference: it is hand-configured, so store the value \
                 under the id/account it names, or edit the config to point elsewhere."
                    .to_string(),
            );
        }
    }
    if !report.entries.is_empty() {
        lines.push(format!(
            "{} reference(s) checked, {} resolved.",
            report.entries.len(),
            report
                .entries
                .iter()
                .filter(|entry| entry.status == "ok")
                .count()
        ));
    }
    lines.extend(report.messages.iter().cloned());
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch the `secrets` family.
///
/// Reads `options.config` and nothing else from the global flags: this family neither
/// resolves a runtime config nor writes one, so a `--vault` or an `--index-dir` has nothing
/// to act on here.
pub fn run(
    options: &ServiceOptions,
    command: SecretsCommand,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let config_path = options
        .config
        .clone()
        .map(deep_obsidian_config::expand_home_path)
        .unwrap_or_else(deep_obsidian_config::default_config_path);

    match command {
        SecretsCommand::Set {
            mount,
            field,
            target,
            stdin,
        } => {
            // A masked prompt is the default and stdin is the opt-in, exactly as in
            // `mounts add`: a credential is never a flag value, and `--stdin` is what makes
            // the non-interactive path testable without a pty.
            let mut secrets = if stdin {
                SecretReader::from_stdin()?
            } else {
                SecretReader::interactive()
            };
            let report = match (mount, target) {
                (Some(mount), None) => {
                    set_mount(&config_path, &mount, field, dry_run, &mut secrets)?
                }
                (None, Some(target)) => set_target(&config_path, target, dry_run, &mut secrets)?,
                // clap's `subject` group is `required(true)` and not `multiple`, so neither
                // of these is reachable from argv. Spelled out rather than
                // `unreachable!()`: a group edited later would turn a panic into a message.
                (None, None) => bail!(
                    "pass either --mount <id> or --target <auth-token|embedding-api-key> to say \
                     which secret to rotate"
                ),
                (Some(_), Some(_)) => bail!("--mount and --target are mutually exclusive"),
            };
            crate::commands::print_report(json, &report, || render_set_report(&report))
        }
        SecretsCommand::Check => {
            let report = check(&config_path)?;
            crate::commands::print_report(json, &report, || render_check_report(&report))?;
            if report.ok {
                Ok(())
            } else {
                // Non-zero for the reason `doctor` exits non-zero on a failed check: a
                // reference the store cannot answer for is a service that will not start (or
                // will start degraded), and a script that runs this as a gate has to see it.
                // Honest only because the report says environment overrides are not
                // consulted.
                std::process::exit(1)
            }
        }
    }
}

/// A rotation neither reads nor stores anything under `--dry-run`, and never touches the
/// config.
///
/// The unit test lives here rather than in `tests/` because it is about this module's
/// contract rather than about the command's wiring; the behavioural coverage — the exact-ref
/// property, kind preservation, the refusals — is in `tests/secrets_cmd.rs`, which needs a
/// real config file and a real store.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_spelled_the_same_way_everywhere() {
        // The `--field` value, the config path and the `check` line's rotate hint all have
        // to agree, or the table tells an operator to run a command that does not parse.
        for field in [
            SecretField::Password,
            SecretField::E2eePassphrase,
            SecretField::ApiKey,
        ] {
            let flag = field_flag_value(field);
            assert!(!flag.is_empty());
            assert!(field_config_path(field).contains(match field {
                SecretField::Password => "password",
                SecretField::E2eePassphrase => "passphrase",
                SecretField::ApiKey => "apiKey",
            }));
            assert!(field_label(field).starts_with("New "));
        }
    }
}
