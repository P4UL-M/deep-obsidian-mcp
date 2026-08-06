//! `mounts add`, `mounts list` and `mounts remove`: the checkable way to build a mount
//! table.
//!
//! # Why this family exists
//!
//! A mount table is the one part of the config a text editor cannot validate. Ids and
//! `mountAt` prefixes must be unique, exactly one mount must sit at the vault root, a
//! remote backend needs an `experimental` flag, and its credential belongs in the secret
//! store rather than in the file. `setup-service` deliberately refuses to rewrite such a
//! config — a mount table is the one thing it cannot reproduce faithfully — which left
//! "edit the JSON by hand and hope" as the only path to a multi-backend vault.
//!
//! # The invariant that makes this safe
//!
//! **Every write goes through the real [`normalize_service_config`].** Nothing here
//! re-implements a uniqueness check, a prefix rule or a root requirement: the candidate
//! table is assembled, handed to the same function the server calls at startup, and only
//! written if that function accepted it. So this command cannot produce a config the
//! server would then refuse to load, and a rule added to the config crate is enforced
//! here for free.
//!
//! # Ordering, and why it is this ordering
//!
//! 1. **Migrate or append.** A legacy `vaultPath`-only config becomes an explicit root
//!    mount first (see [`legacy_root_mount`]), because appending a second mount to a
//!    table that does not exist yet is not a thing the config format can express.
//! 2. **Experimental confirmation.** Before validation, because
//!    `normalize_service_config` REFUSES a couchdb, algolia or multi-mount table whose
//!    flag is unset — asking afterwards would report a decision as a config error.
//! 3. **Full-table validation.** Before the credential is even asked for, so a duplicate
//!    id costs the operator nothing.
//! 4. **Store the credential.** Only the [`SecretRef`] ever reaches the config file.
//! 5. **Probe.** Blocking: a mount that cannot be reached is usually a typo, and finding
//!    out at the next `serve` is worse than finding out now. `--keep-anyway` overrides.
//! 6. **Write**, with a `.bak` of a differing previous file and every unknown key
//!    carried across.
//!
//! # Where the questions live
//!
//! Not here. A missing `--url` is answered by [`crate::wizard::resolve_mount_spec`], which
//! owns the per-kind question sequences and is the SAME code `setup-service --wizard` walks
//! — this module only ever sees a finished [`MountSpec`]. That split is what keeps one
//! wording, one ordering and one set of defaults behind both entry points.
//!
//! # What is deliberately not here
//!
//! Rotation. This module DECIDES where a new credential goes — [`store_mount_secret`] prefers
//! the OS keyring and falls back to the encrypted file — and it may, because it also writes
//! the reference it chose into the config in the same run. Changing the value behind a
//! reference that is already in the file is [`crate::secrets_cmd`], which has the opposite
//! rule: it never modifies the config, so it must write to the reference the file already
//! contains and must never fall back. [`SecretReader`] is the seam both share, and the
//! injection point the wizard and the tests drive instead of a tty.

use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use deep_obsidian_config::{
    carry_unknown_fields, default_mount_index_dir, normalize_service_config, read_config_file,
    secrets::SecretResolver, to_persisted_config,
};
use deep_obsidian_types::{
    CouchdbE2eeConfig, ExperimentalConfig, MountBackendConfig, MountConfig, PersistedServiceConfig,
    ResolvedServiceConfig, SecretRef, ServiceConfigInput, UnknownFields,
};
use secrecy::SecretString;
use serde::Serialize;

use crate::cli::{MountsCommand, ServiceOptions};

// ---------------------------------------------------------------------------
// The resolved mount specification
// ---------------------------------------------------------------------------

/// The settings every mount carries once every question has an answer.
///
/// Structurally [`crate::cli::MountsAddCommon`] with the `Option`s discharged. The
/// duplication is the point: the clap type models *what the operator typed*, where a
/// missing `--id` is normal and means "ask me"; this type models *a mount that can be
/// built*, where a missing id is impossible. Collapsing the two would push
/// `.expect("resolved")` into `build_mount`, i.e. would move a guarantee the type system
/// can hold into a runtime panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountCommon {
    pub id: String,
    pub mount_at: String,
    pub keep_anyway: bool,
    pub yes: bool,
}

/// A mount, fully specified, ready to be validated and written.
///
/// Produced by [`crate::wizard::resolve_mount_spec`] from a
/// [`crate::cli::MountsAddKind`]; consumed by [`add`] and by the wizard. Field names match
/// the clap type's so the two read as one shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSpec {
    Filesystem {
        common: MountCommon,
        vault_path: PathBuf,
    },
    Couchdb {
        common: MountCommon,
        url: String,
        database: String,
        username: Option<String>,
        /// The credential comes from stdin rather than a masked prompt.
        password_stdin: bool,
        writable: bool,
        e2ee: bool,
        sidecar_path: Option<PathBuf>,
    },
    Algolia {
        common: MountCommon,
        app_id: String,
        index_name: String,
        base_url: Option<String>,
        api_key_stdin: bool,
        writable: bool,
        participant_id: Option<String>,
    },
}

/// Id given to the root mount a legacy `vaultPath` is converted into.
///
/// Fixed rather than chosen by the operator so the conversion is predictable and so
/// `mounts list` on a legacy config can show the id the migration WOULD use — the same
/// constant feeds both.
pub const LEGACY_ROOT_MOUNT_ID: &str = "vault";

/// Keyring service name every stored secret in this project shares.
///
/// Matches what `setup-service` uses for the embedding key and the HTTP bearer token, so
/// one keychain entry group holds everything this binary stored.
const SECRET_SERVICE: &str = "deep-obsidian-mcp";

const FLAG_MULTI_VAULT: &str = "multiVault";
const FLAG_COUCHDB_VAULTS: &str = "couchdbVaults";
const FLAG_ALGOLIA_VAULTS: &str = "algoliaVaults";

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// What one probe said about a mount.
///
/// A failed probe is a RESULT, not an error: the verdict is what the operator ran the
/// command to see, and the abort message has to be able to quote it. Only the decision to
/// abort is an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountProbeReport {
    /// The backend kind that was probed, or `skipped`.
    pub kind: String,
    pub ok: bool,
    /// The verdict in the backend's own already-redacted words.
    pub verdict: String,
}

/// What `mounts add` did.
///
/// Carries no credential and no credential-shaped value: `secret_refs` holds REFERENCES
/// (a keyring service/account, or an encrypted-file id), which are not secret and are
/// exactly what an operator needs in order to rotate or delete the stored value later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountsAddReport {
    pub config_path: PathBuf,
    pub mount: String,
    pub mount_at: String,
    pub kind: String,
    /// The mount's secret-free location, as `doctor` renders it.
    pub location: String,
    pub index_dir: PathBuf,
    pub writable: bool,
    /// Set to the root mount's id when a legacy `vaultPath` was converted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migrated_root: Option<String>,
    /// Experimental flags this command turned on, in the order they were confirmed.
    pub experimental_enabled: Vec<String>,
    /// Where each credential was stored. References only — never a value.
    pub secret_refs: Vec<String>,
    pub probe: MountProbeReport,
    pub written: bool,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
    /// The human-readable narration, in order. What the text renderer prints.
    pub messages: Vec<String>,
}

/// One line of `mounts list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountsListEntry {
    pub id: String,
    /// `""` for the root mount, matching what the config stores.
    pub mount_at: String,
    pub kind: String,
    pub location: String,
    /// Where this mount's index lives — `None` when the config does not resolve, because
    /// the default is derived from the RESOLVED root index directory and there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_dir: Option<PathBuf>,
    pub writable: bool,
    pub root: bool,
    /// The text rendering, from the same `render_mount_line` `doctor` uses. Included so
    /// the JSON and the text output cannot drift.
    pub line: String,
}

/// What `mounts list` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountsListReport {
    pub config_path: PathBuf,
    /// True when the config declares NO `mounts` table: the single entry below is the
    /// implicit root derived from the top-level `vaultPath`.
    pub implicit: bool,
    /// `None` when the config does not resolve; see [`Self::unresolved`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_index_dir: Option<PathBuf>,
    pub mounts: Vec<MountsListEntry>,
    /// The experimental flags currently enabled, by their config key.
    pub experimental: Vec<String>,
    /// Why the config does not resolve, in the loader's own words, when it does not.
    ///
    /// `list` still reports the table in that case. Refusing would be exactly backwards:
    /// a config the loader rejects — a duplicate prefix, a removed `experimental` flag, a
    /// table with no root — is precisely when an operator needs to see what is declared,
    /// and the one command whose job is "tell me what is in this file" must not go dark on
    /// the files that need reading most.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<String>,
}

/// What `mounts remove` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountsRemoveReport {
    pub config_path: PathBuf,
    pub mount: String,
    pub mount_at: String,
    pub kind: String,
    /// `None` when the config did not resolve BEFORE the removal, because a non-root
    /// mount's default index directory is derived from the resolved root's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_dir: Option<PathBuf>,
    pub index_purged: bool,
    /// References the removed mount used. Left in the secret store on purpose; see
    /// [`remove`].
    pub secret_refs: Vec<String>,
    pub written: bool,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
    pub messages: Vec<String>,
}

// ---------------------------------------------------------------------------
// Secret input
// ---------------------------------------------------------------------------

/// Where `mounts add` gets a credential from.
///
/// # Why there is no `--password <value>` flag
///
/// A value-taking credential flag puts the credential into `ps` output, into the shell's
/// history file, and into any process listing a coworker can read. So the only two ways
/// in are a MASKED prompt (`rpassword`, which never echoes and never touches argv) and
/// stdin. That also makes the non-interactive path testable without a pty.
///
/// # Ordering when several secrets are needed
///
/// `--password-stdin` with `--e2ee` reads TWO lines: the CouchDB password first, then the
/// E2EE passphrase, in the order [`store_secrets`] asks for them. Documented rather than
/// inferred, because a script that gets the order wrong would store each secret under the
/// other's reference and the failure would surface as an authentication error.
pub struct SecretReader {
    /// `Some` for the stdin / scripted path; `None` means prompt masked.
    lines: Option<VecDeque<String>>,
}

impl SecretReader {
    /// Prompt each secret masked on the terminal.
    pub fn interactive() -> Self {
        Self { lines: None }
    }

    /// Read every secret from stdin, one per line, consumed in request order.
    ///
    /// Reads stdin to EOF up front rather than lazily: a lazily-locked stdin cannot be
    /// handed to two prompts without threading a lock through every call, and the whole
    /// input here is at most two short lines.
    pub fn from_stdin() -> Result<Self> {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("failed to read secrets from stdin")?;
        Ok(Self::from_lines(
            text.lines().map(|line| line.to_string()).collect(),
        ))
    }

    /// An explicit list of secrets, in request order.
    ///
    /// The seam a test uses instead of a pty, and the seam the future wizard slice will
    /// drive instead of stdin.
    pub fn from_lines(lines: Vec<String>) -> Self {
        Self {
            lines: Some(lines.into()),
        }
    }

    /// The next secret, where ABSENT is a legitimate answer.
    ///
    /// Exists for exactly one caller: the wizard's embedding API key, which a local Ollama
    /// endpoint genuinely does not have. Every mount credential goes through [`Self::next`]
    /// instead, which refuses a blank — a CouchDB password that turned out to be the empty
    /// string would fail later as an authentication error rather than now as a typo.
    pub fn next_optional(&mut self, label: &str) -> Result<Option<SecretString>> {
        let value = match &mut self.lines {
            // An exhausted script means "no key", not an error: a scripted wizard run that
            // wants no embedding key should not have to supply a blank line for one.
            Some(lines) => lines.pop_front().unwrap_or_default(),
            None => crate::commands::prompt_optional_secret(label)?
                .map(|secret| secrecy::ExposeSecret::expose_secret(&secret).to_string())
                .unwrap_or_default(),
        };
        Ok(Some(value)
            .filter(|value| !value.trim().is_empty())
            .map(SecretString::from))
    }

    /// The next secret, prompting masked when this reader is interactive.
    ///
    /// `pub` for [`crate::secrets_cmd`], which reads exactly one secret through the same
    /// seam: the blank-is-an-error rule is the one a rotation wants too, since an empty
    /// CouchDB password stored over a working one fails later as an authentication error
    /// rather than now as a typo.
    pub fn next(&mut self, label: &str) -> Result<SecretString> {
        let value = match &mut self.lines {
            // Worded for BOTH callers: `mounts add` may need two lines, `secrets set` needs
            // exactly one, and a message naming only the first would read as nonsense on an
            // empty stdin from the second.
            Some(lines) => lines.pop_front().ok_or_else(|| {
                anyhow!(
                    "no value left on stdin for {label}: pass one line per secret, in the \
                     documented order (`secrets set --stdin` reads one line; `mounts add \
                     --password-stdin --e2ee` reads the password first, then the E2EE \
                     passphrase)"
                )
            })?,
            None => crate::commands::prompt_optional_secret(label)?
                .map(|secret| secrecy::ExposeSecret::expose_secret(&secret).to_string())
                .unwrap_or_default(),
        };
        if value.trim().is_empty() {
            bail!("{label} is required and was empty");
        }
        Ok(SecretString::from(value))
    }
}

/// A [`SecretRef`] rendered for an operator: enough to find or delete the stored value,
/// and never the value itself.
pub fn describe_secret_ref(reference: &SecretRef) -> String {
    match reference {
        SecretRef::OsKeyring { service, account } => {
            format!("osKeyring service={service} account={account}")
        }
        SecretRef::EncryptedFile { id } => format!("encryptedFile id={id}"),
    }
}

/// The keyring account one of a mount's secrets lives under.
///
/// Keyed by mount id and purpose so two mounts never collide and so a `secrets set`
/// command can address the same entry without guessing. `purpose` is a fixed vocabulary:
/// `password`, `e2ee-passphrase`, `api-key`.
fn mount_secret_account(mount_id: &str, purpose: &str) -> String {
    format!("mount-{mount_id}-{purpose}")
}

/// The reference a mount's secret WOULD get, without storing anything.
///
/// Needed because a [`MountConfig`] cannot be built without its refs, and the candidate
/// mount has to exist before validation — which happens before any secret is stored.
///
/// `prefer_os_keyring` decides the SHAPE, so the placeholder matches what
/// [`store_mount_secret`] will actually produce in the common case and the re-validation
/// after storing is a genuine no-op rather than a coincidence.
fn derived_secret_ref(mount_id: &str, purpose: &str, prefer_os_keyring: bool) -> SecretRef {
    if prefer_os_keyring {
        SecretRef::OsKeyring {
            service: SECRET_SERVICE.to_string(),
            account: mount_secret_account(mount_id, purpose),
        }
    } else {
        SecretRef::EncryptedFile {
            id: mount_secret_account(mount_id, purpose),
        }
    }
}

/// Store one of a mount's secrets, preferring the OS keyring and falling back to the
/// encrypted file. Returns the reference that actually holds it.
///
/// The fallback is automatic and NOT a prompt, unlike the wizard's: this command is the
/// one a script drives with `--yes`, and a headless machine with no keyring daemon must
/// not block on a question. It is reported rather than silent.
///
/// This is the function [`crate::secrets_cmd`] agrees with about ref shapes — but only for
/// the DERIVED case. A rotation reads the ref out of the config rather than deriving it, so a
/// hand-written ref keeps working; see that module's docs.
fn store_mount_secret(
    resolver: &SecretResolver,
    mount_id: &str,
    purpose: &str,
    value: SecretString,
    prefer_os_keyring: bool,
    messages: &mut Vec<String>,
) -> Result<SecretRef> {
    let fallback = SecretRef::EncryptedFile {
        id: mount_secret_account(mount_id, purpose),
    };
    let store_in_file = |value: SecretString| -> Result<SecretRef> {
        resolver.put(&fallback, value).map_err(|error| {
            anyhow!("failed to store the {purpose} for mount {mount_id:?}: {error}")
        })?;
        Ok(fallback.clone())
    };
    if !prefer_os_keyring {
        return store_in_file(value);
    }
    let keyring = derived_secret_ref(mount_id, purpose, true);
    match resolver.put(&keyring, value.clone()) {
        Ok(()) => Ok(keyring),
        Err(error) => {
            messages.push(format!(
                "OS keyring unavailable ({error}); storing the {purpose} in the encrypted \
                 secrets file instead"
            ));
            store_in_file(value)
        }
    }
}

/// The refs a mount kind needs, in the order they are asked for.
#[derive(Debug, Clone, Default)]
struct MountSecretRefs {
    password: Option<SecretRef>,
    e2ee_passphrase: Option<SecretRef>,
    api_key: Option<SecretRef>,
}

impl MountSecretRefs {
    /// Every ref this mount carries, rendered for an operator.
    fn describe(&self) -> Vec<String> {
        [&self.password, &self.e2ee_passphrase, &self.api_key]
            .into_iter()
            .flatten()
            .map(describe_secret_ref)
            .collect()
    }
}

/// The refs the candidate mount is built with before anything is stored.
fn derived_secret_refs(kind: &MountSpec, prefer_os_keyring: bool) -> MountSecretRefs {
    match kind {
        MountSpec::Filesystem { .. } => MountSecretRefs::default(),
        MountSpec::Couchdb { common, e2ee, .. } => MountSecretRefs {
            password: Some(derived_secret_ref(
                &common.id,
                "password",
                prefer_os_keyring,
            )),
            e2ee_passphrase: e2ee
                .then(|| derived_secret_ref(&common.id, "e2ee-passphrase", prefer_os_keyring)),
            api_key: None,
        },
        MountSpec::Algolia { common, .. } => MountSecretRefs {
            password: None,
            e2ee_passphrase: None,
            api_key: Some(derived_secret_ref(&common.id, "api-key", prefer_os_keyring)),
        },
    }
}

/// Read and store every credential this mount kind needs.
///
/// Records what it stored in `stored` as it goes, so an abort can remove exactly the
/// entries this run created and nothing else.
fn store_secrets(
    kind: &MountSpec,
    resolver: &SecretResolver,
    prefer_os_keyring: bool,
    secrets: &mut SecretReader,
    stored: &mut Vec<SecretRef>,
    messages: &mut Vec<String>,
) -> Result<MountSecretRefs> {
    match kind {
        MountSpec::Filesystem { .. } => Ok(MountSecretRefs::default()),
        MountSpec::Couchdb { common, e2ee, .. } => {
            let password = secrets.next("CouchDB password")?;
            let password_ref = store_mount_secret(
                resolver,
                &common.id,
                "password",
                password,
                prefer_os_keyring,
                messages,
            )?;
            stored.push(password_ref.clone());
            let e2ee_passphrase_ref = if *e2ee {
                let passphrase = secrets.next("CouchDB vault E2EE passphrase")?;
                let reference = store_mount_secret(
                    resolver,
                    &common.id,
                    "e2ee-passphrase",
                    passphrase,
                    prefer_os_keyring,
                    messages,
                )?;
                stored.push(reference.clone());
                Some(reference)
            } else {
                None
            };
            Ok(MountSecretRefs {
                password: Some(password_ref),
                e2ee_passphrase: e2ee_passphrase_ref,
                api_key: None,
            })
        }
        MountSpec::Algolia { common, .. } => {
            let key = secrets.next("Algolia API key")?;
            let api_key_ref = store_mount_secret(
                resolver,
                &common.id,
                "api-key",
                key,
                prefer_os_keyring,
                messages,
            )?;
            stored.push(api_key_ref.clone());
            Ok(MountSecretRefs {
                password: None,
                e2ee_passphrase: None,
                api_key: Some(api_key_ref),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Mount construction
// ---------------------------------------------------------------------------

/// The settings every mount shares, whichever kind was chosen.
fn common_of(kind: &MountSpec) -> &MountCommon {
    match kind {
        MountSpec::Filesystem { common, .. }
        | MountSpec::Couchdb { common, .. }
        | MountSpec::Algolia { common, .. } => common,
    }
}

/// Read the config file, flattening the loader's own message into ours.
///
/// Same reason [`validate`] inlines its cause: `main` prints `{error}` only, so a parse
/// error attached as an anyhow cause would reach the operator as "failed to load config
/// file" with the line number and the reason stripped off.
fn load_config_file(config_path: &Path) -> Result<Option<PersistedServiceConfig>> {
    read_config_file(config_path).map_err(|error| {
        anyhow!(
            "failed to load config file {}: {error}",
            config_path.display()
        )
    })
}

/// The explicit root mount a legacy `vaultPath` becomes.
///
/// # Why this is byte-for-byte equivalent to the `vaultPath` it replaces
///
/// `normalize_service_config` resolves a filesystem ROOT mount by taking its `vaultPath`
/// as the top-level one and its `indexDir` as the FALLBACK for the top-level `indexDir`.
/// So a root mount carrying the same path and no `indexDir` resolves to the same
/// `vault_path` and the same `index_dir` as the legacy config did — which is why
/// `index_dir` is left `None` here rather than filled in with the resolved value. Filling
/// it in would be harmless today and wrong the moment the top-level `indexDir` changes.
pub fn legacy_root_mount(vault_path: &Path) -> MountConfig {
    MountConfig {
        id: LEGACY_ROOT_MOUNT_ID.to_string(),
        mount_at: String::new(),
        backend: MountBackendConfig::Filesystem {
            vault_path: vault_path.to_path_buf(),
            index_dir: None,
        },
        recall_weight: None,
        unknown: UnknownFields::new(),
    }
}

/// The mount table to append to, plus the root mount a legacy config was converted into.
///
/// `allow_empty_base` decides what an empty config means. `mounts add` passes `false`: it
/// EDITS a vault, and a file declaring neither `vaultPath` nor `mounts` has no vault to add
/// a mount beside — refusing names the setup command that creates one. The first-init wizard
/// passes `true`, because there the empty base is the whole point: its first mount IS the
/// root, and the resulting one-entry table is exactly what a fully-remote vault looks like.
/// A parameter rather than a behaviour change so `mounts add`'s refusal stays intact.
fn base_mount_table(
    existing: &PersistedServiceConfig,
    config_path: &Path,
    allow_empty_base: bool,
) -> Result<(Vec<MountConfig>, Option<MountConfig>)> {
    if let Some(mounts) = existing.mounts.as_ref().filter(|mounts| !mounts.is_empty()) {
        return Ok((mounts.clone(), None));
    }
    let Some(vault_path) = existing.vault_path.as_deref() else {
        if allow_empty_base {
            return Ok((Vec::new(), None));
        }
        bail!(
            "{} declares neither `vaultPath` nor `mounts`, so there is no vault to add a mount \
             beside. Run `deep-obsidian-mcp setup-service --vault <path>` first.",
            config_path.display()
        );
    };
    let root = legacy_root_mount(vault_path);
    Ok((vec![root.clone()], Some(root)))
}

/// Build the new mount from its flags and its (already decided) secret references.
fn build_mount(
    kind: &MountSpec,
    index_dir: Option<&Path>,
    refs: &MountSecretRefs,
) -> Result<MountConfig> {
    let common = common_of(kind);
    let index_dir = index_dir.map(Path::to_path_buf);
    let backend = match kind {
        MountSpec::Filesystem { vault_path, .. } => MountBackendConfig::Filesystem {
            vault_path: vault_path.clone(),
            index_dir,
        },
        MountSpec::Couchdb {
            url,
            database,
            username,
            writable,
            sidecar_path,
            ..
        } => MountBackendConfig::Couchdb {
            url: url.clone(),
            database: database.clone(),
            username: username.clone(),
            password_ref: refs
                .password
                .clone()
                .ok_or_else(|| anyhow!("a couchdb mount needs a password reference"))?,
            e2ee: refs
                .e2ee_passphrase
                .clone()
                .map(|passphrase_ref| CouchdbE2eeConfig {
                    passphrase_ref,
                    // Left unset: path obfuscation is a second, independent passphrase, and
                    // guessing that a vault has it enabled would make every read fail in a
                    // way that looks like corruption. So it stays a hand-written config key;
                    // `secrets check` reports such a ref, and `secrets set` deliberately has
                    // no `--field` for it, because creating one is a config change and that
                    // command never writes the config.
                    obfuscate_passphrase_ref: None,
                }),
            sidecar_path: sidecar_path.clone(),
            index_dir,
            // Chunking knobs are deliberately not CLI flags: they must MATCH how the vault
            // was written, so a wrong value silently reassembles content incorrectly. An
            // operator who needs them edits the config, where they are documented.
            options: None,
            writable: *writable,
        },
        MountSpec::Algolia {
            app_id,
            index_name,
            base_url,
            writable,
            participant_id,
            ..
        } => MountBackendConfig::Algolia {
            app_id: app_id.clone(),
            index_name: index_name.clone(),
            api_key_ref: refs
                .api_key
                .clone()
                .ok_or_else(|| anyhow!("an algolia mount needs an API key reference"))?,
            base_url: base_url.clone(),
            writable: *writable,
            participant_id: participant_id.clone(),
            cache: None,
            retention: None,
            index_dir,
        },
    };
    Ok(MountConfig {
        id: common.id.clone(),
        mount_at: common.mount_at.clone(),
        backend,
        recall_weight: None,
        unknown: UnknownFields::new(),
    })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Rebuild a [`ServiceConfigInput`] from a persisted config.
///
/// Exhaustively DESTRUCTURED on purpose: a field added to [`PersistedServiceConfig`] later
/// breaks this function at compile time instead of being silently dropped from every
/// config `mounts add` rewrites. That is the only reason this is spelled out rather than
/// hidden behind a `..`.
fn input_from_persisted(config: PersistedServiceConfig, config_path: &Path) -> ServiceConfigInput {
    let PersistedServiceConfig {
        vault_path,
        mounts,
        experimental,
        index_dir,
        transport,
        stdio_mode,
        http,
        auto_reindex,
        embedding,
        artifact_embedding,
        auth,
        federated_rerank,
        // Not part of the input type: unknown keys are restored on the way OUT, by
        // `carry_unknown_fields`, because the resolved config cannot carry them.
        unknown: _,
    } = config;
    ServiceConfigInput {
        vault_path,
        mounts,
        experimental,
        index_dir,
        transport,
        stdio_mode,
        http,
        auto_reindex,
        embedding,
        artifact_embedding,
        auth,
        federated_rerank,
        config_file_path: Some(config_path.to_path_buf()),
    }
}

/// The candidate config: the file, with `vaultPath` dropped and this mount table and
/// experimental section in its place.
///
/// `vaultPath` is cleared unconditionally because a declared `mounts` table and a
/// top-level `vaultPath` are mutually exclusive on input
/// (`ConfigError::VaultPathAndMountsBothSet`) — the legacy path's information has already
/// moved into the root mount by the time this is called.
pub(crate) fn candidate_config(
    existing: &PersistedServiceConfig,
    mounts: &[MountConfig],
    experimental: &ExperimentalConfig,
) -> PersistedServiceConfig {
    let mut candidate = existing.clone();
    candidate.vault_path = None;
    candidate.mounts = Some(mounts.to_vec());
    candidate.experimental = Some(experimental.clone());
    candidate
}

/// Run a candidate config through the REAL loader, or fail naming the file.
///
/// The single validation in this module. Every rule — id shape, id uniqueness, `mountAt`
/// canonicalization and uniqueness, the root requirement, per-backend field checks, the
/// experimental gates, recall weights — lives in `normalize_service_config` and is
/// enforced here by calling it, never by restating it.
/// The loader's own message is INLINED rather than attached as an anyhow cause, because
/// `main` prints only `{error}` — a `Caused by` chain never reaches the terminal, so a
/// `with_context` here would replace "duplicate mount id 'team'" with "the mount table is
/// not valid" and lose the only actionable half.
pub(crate) fn validate(
    candidate: PersistedServiceConfig,
    config_path: &Path,
) -> Result<ResolvedServiceConfig> {
    let input = input_from_persisted(candidate, config_path);
    normalize_service_config(input).map_err(|error| {
        anyhow!(
            "the resulting mount table would not be a valid config for {}: {error}",
            config_path.display()
        )
    })
}

/// [`validate`] on a path that is about to WRITE: the narration so far is carried, and the
/// refusal says the file was left alone.
///
/// Split from `validate` because `mounts list` also validates, and there "Nothing was
/// written" would be a non-sequitur — it never intended to write.
fn validate_for_write(
    candidate: PersistedServiceConfig,
    config_path: &Path,
    messages: &[String],
) -> Result<ResolvedServiceConfig> {
    with_narration(validate(candidate, config_path), messages)
        .map_err(|error| anyhow!("{error}\nNothing was written."))
}

/// Prefix an error with the narration so far.
///
/// The legacy migration INVENTS a root mount with id `vault`, so `mounts add filesystem
/// --id vault ...` on a legacy config fails with "duplicate mount id 'vault'" about a mount
/// the operator never wrote. Discarding `messages` on the error path would leave that
/// unexplainable; carried, the reader sees the conversion that created it first.
fn with_narration<T>(result: Result<T>, messages: &[String]) -> Result<T> {
    result.map_err(|error| {
        if messages.is_empty() {
            error
        } else {
            anyhow!("{}\n{error}", messages.join("\n"))
        }
    })
}

/// The persisted form of a validated config, with the previous file's unknown keys
/// restored.
pub(crate) fn persist(
    resolved: &ResolvedServiceConfig,
    previous: &PersistedServiceConfig,
) -> PersistedServiceConfig {
    let mut persisted = to_persisted_config(resolved);
    // Mandatory for every config writer: `to_persisted_config` rebuilds the file from the
    // server's interpreted view, which by design holds no keys this build cannot read.
    carry_unknown_fields(&mut persisted, Some(previous));
    persisted
}

/// Where a mount's index lives, by the same rule `render_mount_line` and the server use:
/// the root's is the resolved top-level one, a non-root mount's is its explicit `indexDir`
/// or the id-keyed default beneath the root's.
pub(crate) fn resolved_mount_index_dir(
    resolved: &ResolvedServiceConfig,
    mount: &MountConfig,
) -> PathBuf {
    if mount.mount_at.is_empty() {
        return resolved.index_dir.clone();
    }
    let declared = match &mount.backend {
        MountBackendConfig::Filesystem { index_dir, .. }
        | MountBackendConfig::Couchdb { index_dir, .. }
        | MountBackendConfig::Algolia { index_dir, .. } => index_dir.clone(),
    };
    declared.unwrap_or_else(|| default_mount_index_dir(&resolved.index_dir, &mount.id))
}

/// Whether a mount accepts writes. A filesystem mount always does; a remote one carries
/// the flag.
pub(crate) fn mount_is_writable(mount: &MountConfig) -> bool {
    match &mount.backend {
        MountBackendConfig::Filesystem { .. } => true,
        MountBackendConfig::Couchdb { writable, .. }
        | MountBackendConfig::Algolia { writable, .. } => *writable,
    }
}

// ---------------------------------------------------------------------------
// Experimental flags
// ---------------------------------------------------------------------------

/// Which experimental flags this addition needs, in the order they are confirmed.
///
/// Derived from the same two facts `normalize_service_config` checks: the backend kind,
/// and whether the resulting table has more than one mount. `mount_count` is the size of
/// the table AFTER the append, which is why a first explicit root mount needs no flag —
/// a single root mount is the legacy shape spelled out longhand.
fn required_experimental_flags(kind: &MountSpec, mount_count: usize) -> Vec<&'static str> {
    let mut flags = Vec::new();
    match kind {
        MountSpec::Filesystem { .. } => {}
        MountSpec::Couchdb { .. } => flags.push(FLAG_COUCHDB_VAULTS),
        MountSpec::Algolia { .. } => flags.push(FLAG_ALGOLIA_VAULTS),
    }
    if mount_count > 1 {
        flags.push(FLAG_MULTI_VAULT);
    }
    flags
}

fn experimental_flag_is_set(experimental: &ExperimentalConfig, flag: &str) -> bool {
    match flag {
        FLAG_MULTI_VAULT => experimental.multi_vault,
        FLAG_COUCHDB_VAULTS => experimental.couchdb_vaults,
        FLAG_ALGOLIA_VAULTS => experimental.algolia_vaults,
        _ => false,
    }
}

fn set_experimental_flag(experimental: &mut ExperimentalConfig, flag: &str) {
    match flag {
        FLAG_MULTI_VAULT => experimental.multi_vault = true,
        FLAG_COUCHDB_VAULTS => experimental.couchdb_vaults = true,
        FLAG_ALGOLIA_VAULTS => experimental.algolia_vaults = true,
        _ => {}
    }
}

/// The experimental flags currently enabled, by their config key.
pub(crate) fn enabled_experimental_flags(experimental: &ExperimentalConfig) -> Vec<String> {
    [
        (FLAG_MULTI_VAULT, experimental.multi_vault),
        (FLAG_COUCHDB_VAULTS, experimental.couchdb_vaults),
        (FLAG_ALGOLIA_VAULTS, experimental.algolia_vaults),
    ]
    .into_iter()
    .filter(|(_, enabled)| *enabled)
    .map(|(flag, _)| flag.to_string())
    .collect()
}

/// The question asked before a flag is turned on.
///
/// Names the flag, says the word "experimental", and says what experimental MEANS here —
/// all three, because "enable multiVault? [y/N]" tells an operator nothing about what
/// they are agreeing to. A flag is never enabled without this being answered, or `--yes`
/// standing in for the answer.
fn experimental_confirmation(flag: &str, config_path: &Path) -> String {
    let what = match flag {
        FLAG_MULTI_VAULT => {
            "routing one logical vault across several mounts, and federating search across them"
        }
        FLAG_COUCHDB_VAULTS => {
            "reading a Self-hosted LiveSync vault out of CouchDB through a supervised Node \
             sidecar, in a format owned by a community plugin"
        }
        FLAG_ALGOLIA_VAULTS => {
            "storing notes as records in a hosted Algolia index that several participants may \
             write at once"
        }
        _ => "behaviour that is not yet stable",
    };
    format!(
        "This needs experimental.{flag} in {}, which is not enabled.\n  \
         The flag turns on {what}.\n  \
         It is EXPERIMENTAL: its behaviour may change, and it may be removed, in any release.\n\
         Enable experimental.{flag}?",
        config_path.display()
    )
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

/// Contact the new mount once, read-only, and report what it said.
///
/// # Why a failed probe is not an `Err`
///
/// The verdict is the whole point: `--keep-anyway` has to be able to write the mount and
/// still report the verdict, and the abort path has to be able to quote it. So this
/// always returns a report and the caller decides.
///
/// # What each kind actually does
///
/// * **filesystem** — one `read_dir`. Nothing is created; a vault directory that does not
///   exist yet is a typo far more often than it is intent.
/// * **couchdb** — the same `initialize`-only handshake `doctor --probe-remote` uses
///   ([`crate::couchdb_transfer::probe_compatibility_with_resolver`]): no data method is
///   ever issued, so it cannot mutate a vault even on a writable mount, and the sidecar
///   child does not outlive the probe.
/// * **algolia** — the same reachability check `algolia status` uses
///   ([`crate::algolia_cmd::status_with_resolver`]), which is a `getSettings`-class read.
async fn probe_mount(
    resolved: &ResolvedServiceConfig,
    mount: &MountConfig,
    resolver: &SecretResolver,
) -> MountProbeReport {
    let kind = mount.backend.kind_name().to_string();
    match &mount.backend {
        MountBackendConfig::Filesystem { vault_path, .. } => {
            let verdict = match fs::metadata(vault_path) {
                Ok(metadata) if !metadata.is_dir() => {
                    return MountProbeReport {
                        kind,
                        ok: false,
                        verdict: format!("{} is not a directory", vault_path.display()),
                    };
                }
                Ok(_) => match fs::read_dir(vault_path) {
                    Ok(_) => {
                        return MountProbeReport {
                            kind,
                            ok: true,
                            verdict: format!("{} exists and is readable", vault_path.display()),
                        }
                    }
                    Err(error) => format!("{} is not readable: {error}", vault_path.display()),
                },
                Err(error) => format!("{} cannot be read: {error}", vault_path.display()),
            };
            MountProbeReport {
                kind,
                ok: false,
                verdict,
            }
        }
        MountBackendConfig::Couchdb { .. } => {
            match crate::couchdb_transfer::probe_compatibility_with_resolver(
                resolved, &mount.id, resolver,
            )
            .await
            {
                Ok(status) => MountProbeReport {
                    ok: status == "ok",
                    verdict: format!("sidecar compatibility: {status}"),
                    kind,
                },
                Err(error) => MountProbeReport {
                    kind,
                    ok: false,
                    verdict: format!("handshake failed: {error}"),
                },
            }
        }
        MountBackendConfig::Algolia { .. } => {
            match crate::algolia_cmd::status_with_resolver(resolved, &mount.id, resolver).await {
                Ok(status) => MountProbeReport {
                    ok: status.reachable,
                    verdict: if status.reachable {
                        format!(
                            "index reachable (provisioned: {}, notes: {})",
                            status.main_provisioned, status.notes
                        )
                    } else {
                        "index unreachable".to_string()
                    },
                    kind,
                },
                Err(error) => MountProbeReport {
                    kind,
                    ok: false,
                    verdict: format!("unreachable: {error}"),
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The shared append core
// ---------------------------------------------------------------------------

/// The stores, streams and decisions one mount addition needs from its caller.
///
/// Bundled rather than passed as five parameters for two reasons. It keeps
/// [`append_mount`] inside clippy's argument budget, and — the substantive one — it names
/// the set of things that differ between `mounts add` and the wizard: where secrets go,
/// where they come from, and who answers a yes/no question. Everything else about an
/// addition is identical, which is exactly the claim this type makes checkable.
pub(crate) struct MountIo<'a> {
    pub resolver: &'a SecretResolver,
    /// See [`add_with_resolver`]: `false` keeps a test out of the login keychain.
    pub prefer_os_keyring: bool,
    pub secrets: &'a mut SecretReader,
    /// Answers the experimental-flag confirmation. `mounts add` reads the terminal; the
    /// wizard reads its own answer stream, which is what makes the whole flow testable
    /// without a tty.
    pub confirm: &'a mut dyn FnMut(&str) -> Result<bool>,
}

/// One addition's inputs: the file it applies to, and the mount to add.
pub(crate) struct AppendRequest<'a> {
    pub existing: &'a PersistedServiceConfig,
    /// Named in every message and in the validation error. Not read from.
    pub config_path: &'a Path,
    /// The NEW mount's own `indexDir`; the config's top-level one is never changed here.
    pub index_dir: Option<&'a Path>,
    pub spec: &'a MountSpec,
    /// See [`base_mount_table`].
    pub allow_empty_base: bool,
    /// Stop after validation: store nothing, probe nothing.
    pub dry_run: bool,
}

/// What [`append_mount`] produced. Nothing has been written.
pub(crate) struct AppendedMount {
    /// The table with the new mount in it, validated.
    pub mounts: Vec<MountConfig>,
    pub experimental: ExperimentalConfig,
    /// The new mount, carrying the secret references that actually hold its credentials.
    pub mount: MountConfig,
    /// The whole config as the loader resolved it — the object to persist.
    pub resolved: ResolvedServiceConfig,
    pub migrated_root: Option<String>,
    pub experimental_enabled: Vec<String>,
    /// Every reference THIS call stored, for a caller that has to roll back.
    pub stored: Vec<SecretRef>,
    /// The same references rendered for an operator.
    pub secret_refs: Vec<String>,
    pub probe: MountProbeReport,
    pub messages: Vec<String>,
}

/// Steps 1–5 of the module's ordering: migrate, confirm, validate, store, probe.
///
/// Deliberately stops short of both the write and the decision about a failed probe. The
/// write is the caller's because the wizard writes ONCE at the end of six screens rather
/// than once per mount. The probe decision is the caller's because the two entry points
/// answer it differently — `mounts add` has already been told by `--keep-anyway`, while the
/// wizard has to ask now that there is a verdict to show — and because a shared hook for it
/// would hide which of the two wordings an operator is reading.
///
/// A caller that abandons the addition after this returns MUST call
/// [`roll_back_stored_secrets`] with [`AppendedMount::stored`].
pub(crate) async fn append_mount(
    request: &AppendRequest<'_>,
    io: &mut MountIo<'_>,
) -> Result<AppendedMount> {
    let AppendRequest {
        existing,
        config_path,
        index_dir,
        spec,
        allow_empty_base,
        dry_run,
    } = *request;
    let common = common_of(spec);
    let mut messages = Vec::new();

    // 1. Migrate or append.
    let (mut mounts, migrated) = base_mount_table(existing, config_path, allow_empty_base)?;
    if let Some(root) = &migrated {
        messages.push(format!(
            "converted the legacy `vaultPath` into an explicit root mount: id '{}', mountAt \"\" \
             (the vault root), filesystem at {}. It resolves to exactly the same vault path and \
             index directory, and `vaultPath` is dropped from the file because a declared mount \
             table and a top-level `vaultPath` are mutually exclusive.",
            root.id,
            root.backend.location()
        ));
    }

    // 2. Experimental gates, BEFORE validation: the loader refuses a table whose flag is
    //    unset, so asking afterwards would report a decision as a config error.
    let mut experimental = existing.experimental.clone().unwrap_or_default();
    let mut experimental_enabled = Vec::new();
    for flag in required_experimental_flags(spec, mounts.len() + 1) {
        if experimental_flag_is_set(&experimental, flag) {
            continue;
        }
        if !common.yes && !(io.confirm)(&experimental_confirmation(flag, config_path))? {
            bail!(
                "aborted: experimental.{flag} was not enabled, so mount '{}' was not added. \
                 Nothing was written and no secret was stored.",
                common.id
            );
        }
        set_experimental_flag(&mut experimental, flag);
        experimental_enabled.push(flag.to_string());
        messages.push(format!("enabled experimental.{flag}"));
    }

    // 3. Full-table validation, before the credential is even asked for. A duplicate id or
    //    a colliding `mountAt` therefore costs the operator nothing, and — the reason the
    //    abort cleanup in step 5 is safe — guarantees the mount id is NEW, so the
    //    id-keyed secret references below cannot belong to any other mount.
    let candidate_mount = build_mount(
        spec,
        index_dir,
        &derived_secret_refs(spec, io.prefer_os_keyring),
    )?;
    mounts.push(candidate_mount);
    let validated = validate_for_write(
        candidate_config(existing, &mounts, &experimental),
        config_path,
        &messages,
    )?;

    if dry_run {
        let mount = mounts.last().expect("the mount just pushed").clone();
        messages.push(format!(
            "dry-run: the mount table is valid; no credential was stored, nothing was probed, \
             and {} was not written",
            config_path.display()
        ));
        return Ok(AppendedMount {
            mounts,
            experimental,
            mount,
            resolved: validated,
            migrated_root: migrated.map(|root| root.id),
            experimental_enabled,
            stored: Vec::new(),
            secret_refs: Vec::new(),
            probe: MountProbeReport {
                kind: "skipped".to_string(),
                ok: true,
                verdict: "dry-run".to_string(),
            },
            messages,
        });
    }

    // 4. Credentials. Stored under references derived from the (now validated) mount id;
    //    only the reference ever reaches the config file.
    let mut stored = Vec::new();
    let refs = store_secrets(
        spec,
        io.resolver,
        io.prefer_os_keyring,
        io.secrets,
        &mut stored,
        &mut messages,
    )?;
    for descriptor in refs.describe() {
        messages.push(format!(
            "stored credential at {descriptor} (the config holds this reference only)"
        ));
    }

    // The final ref shapes may differ from the derived ones when the keyring was
    // unavailable, so the table is rebuilt and re-validated rather than assumed. Cheap,
    // and it keeps "what was validated" and "what is written" the same object.
    let mount = build_mount(spec, index_dir, &refs)?;
    let mounts = {
        let mut mounts = mounts;
        mounts.pop();
        mounts.push(mount.clone());
        mounts
    };
    // A failure here has already stored a credential, so the narration is carried and the
    // caller's rollback still applies — `stored` travels out inside the error's sibling
    // path only because this validation cannot fail in practice (the shapes are the ones
    // just validated); if it ever does, the refusal names the file and writes nothing.
    let resolved = validate_for_write(
        candidate_config(existing, &mounts, &experimental),
        config_path,
        &messages,
    )?;

    // 5. Blocking probe. The VERDICT is the result; whether it blocks is the caller's.
    let probe = probe_mount(&resolved, &mount, io.resolver).await;

    Ok(AppendedMount {
        mounts,
        experimental,
        mount,
        resolved,
        migrated_root: migrated.map(|root| root.id),
        experimental_enabled,
        stored,
        secret_refs: refs.describe(),
        probe,
        messages,
    })
}

/// Delete exactly the credentials one addition stored, narrating each outcome.
///
/// Safe because the full-table validation in [`append_mount`] step 3 proved the mount id is
/// not already in the table, so an id-keyed reference cannot be a live mount's — the worst
/// case is deleting a leftover from an earlier aborted attempt, which is the desired
/// outcome. The alternative, leaving it, orphans a credential in the operator's keychain
/// that nothing references and nothing will ever clean up.
///
/// Shared by every path that abandons an addition after step 4: a failed probe, a wizard
/// recap the operator declined, and an end-of-input at any later question.
pub(crate) fn roll_back_stored_secrets(
    resolver: &SecretResolver,
    stored: &[SecretRef],
    messages: &mut Vec<String>,
) {
    for reference in stored {
        let descriptor = describe_secret_ref(reference);
        match resolver.delete(reference) {
            Ok(()) => messages.push(format!("removed the credential stored at {descriptor}")),
            Err(error) => messages.push(format!(
                "could not remove the credential stored at {descriptor}: {error} — delete it by \
                 hand"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

/// Append a mount to the config's table. See the module docs for the ordering.
pub async fn add(
    config_path: &Path,
    index_dir: Option<&Path>,
    kind: &MountSpec,
    dry_run: bool,
    secrets: &mut SecretReader,
) -> Result<MountsAddReport> {
    add_with_resolver(
        config_path,
        index_dir,
        kind,
        dry_run,
        &SecretResolver::new(),
        true,
        secrets,
    )
    .await
}

/// [`add`] against an explicit secret store.
///
/// Exists for the reason `export_with_resolver` does: a test must be able to point at a
/// temp secrets file instead of mutating the process-global default path, which would
/// race every other test that reads it.
///
/// `prefer_os_keyring` is the second half of that isolation and is NOT cosmetic: a
/// [`SecretResolver`] routes by ref SHAPE, so an `osKeyring` reference reaches the real
/// login keychain however the resolver was built. `false` keeps a test — and a CI runner
/// with no keyring daemon — in the temp encrypted file. The CLI passes `true`.
pub async fn add_with_resolver(
    config_path: &Path,
    index_dir: Option<&Path>,
    kind: &MountSpec,
    dry_run: bool,
    resolver: &SecretResolver,
    prefer_os_keyring: bool,
    secrets: &mut SecretReader,
) -> Result<MountsAddReport> {
    let common = common_of(kind);
    let existing = load_config_file(config_path)?.ok_or_else(|| {
        anyhow!(
            "no config file at {}: `mounts add` edits an existing config rather than inventing \
             one. Run `deep-obsidian-mcp setup-service --vault <path>` first, then add mounts \
             to it.",
            config_path.display()
        )
    })?;

    let mut confirm = |question: &str| crate::commands::confirm(question);
    let mut io = MountIo {
        resolver,
        prefer_os_keyring,
        secrets,
        confirm: &mut confirm,
    };
    let appended = append_mount(
        &AppendRequest {
            existing: &existing,
            config_path,
            index_dir,
            spec: kind,
            // `mounts add` edits an existing vault; an empty config has none to add beside.
            allow_empty_base: false,
            dry_run,
        },
        &mut io,
    )
    .await?;

    let AppendedMount {
        mount,
        resolved,
        migrated_root,
        experimental_enabled,
        stored,
        secret_refs,
        probe,
        mut messages,
        ..
    } = appended;

    let report =
        |written: bool, backup_path: Option<PathBuf>, messages: Vec<String>| MountsAddReport {
            config_path: config_path.to_path_buf(),
            mount: mount.id.clone(),
            mount_at: mount.mount_at.clone(),
            kind: mount.backend.kind_name().to_string(),
            location: mount.backend.location(),
            index_dir: resolved_mount_index_dir(&resolved, &mount),
            writable: mount_is_writable(&mount),
            migrated_root: migrated_root.clone(),
            experimental_enabled: experimental_enabled.clone(),
            secret_refs: secret_refs.clone(),
            probe: probe.clone(),
            written,
            dry_run,
            backup_path,
            messages,
        };

    if dry_run {
        return Ok(report(false, None, messages));
    }

    if !probe.ok && !common.keep_anyway {
        roll_back_stored_secrets(io.resolver, &stored, &mut messages);
        let narration = if messages.is_empty() {
            String::new()
        } else {
            format!("{}\n", messages.join("\n"))
        };
        bail!(
            "mount '{}' did not pass its probe ({}: {}), so {} was NOT written.\n{narration}\
             Fix the mount's settings and run the command again, or pass --keep-anyway to add it \
             regardless (`doctor --probe-remote` will then report it degraded).",
            common.id,
            probe.kind,
            probe.verdict,
            config_path.display(),
        );
    }
    messages.push(format!(
        "probe {}: {} — {}",
        probe.kind,
        if probe.ok { "ok" } else { "FAILED" },
        probe.verdict
    ));
    if !probe.ok {
        messages.push(
            "--keep-anyway: the mount was added despite the failed probe. `deep-obsidian-mcp \
             doctor --probe-remote` will report it degraded until it can be reached."
                .to_string(),
        );
    }

    // 6. Write, with the previous file kept and every unknown key carried across.
    let persisted = persist(&resolved, &existing);
    let backup_path = crate::commands::write_config_with_backup(config_path, &persisted)?;
    if let Some(backup_path) = &backup_path {
        messages.push(format!(
            "backed up previous config: {}",
            backup_path.display()
        ));
    }
    messages.push(format!("wrote config: {}", config_path.display()));

    Ok(report(true, backup_path, messages))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// One line per mount, plus the experimental flags currently enabled.
///
/// Works on a legacy config: the implicit root is rendered as the mount the migration
/// WOULD create, from the same [`legacy_root_mount`] `mounts add` uses, and flagged
/// `implicit` so the reader knows the file declares no table.
///
/// Works on a config that does NOT resolve, too — see [`MountsListReport::unresolved`].
/// The only thing lost is the index directories, which are derived from the resolved root
/// index dir; everything the file actually declares is still reported.
pub fn list(config_path: &Path) -> Result<MountsListReport> {
    let existing = load_config_file(config_path)?.ok_or_else(|| {
        anyhow!(
            "no config file at {}. Run `deep-obsidian-mcp setup-service --vault <path>` to \
             create one.",
            config_path.display()
        )
    })?;

    let declared = existing
        .mounts
        .as_ref()
        .is_some_and(|mounts| !mounts.is_empty());
    let (mounts, _) = base_mount_table(&existing, config_path, false)?;
    let experimental = existing.experimental.clone().unwrap_or_default();
    // Resolved through the loader rather than read off the file, so the reported index
    // directories are the ones the server will actually use — including the defaults a
    // config that never wrote `indexDir` gets. A REFUSAL is downgraded to a note rather
    // than propagated: see `MountsListReport::unresolved`.
    let resolved = validate(
        candidate_config(&existing, &mounts, &experimental),
        config_path,
    );
    let (root_index_dir, unresolved) = match &resolved {
        Ok(resolved) => (Some(resolved.index_dir.clone()), None),
        Err(error) => (None, Some(error.to_string())),
    };

    let entries = mounts
        .iter()
        .map(|mount| MountsListEntry {
            id: mount.id.clone(),
            mount_at: mount.mount_at.clone(),
            kind: mount.backend.kind_name().to_string(),
            location: mount.backend.location(),
            writable: mount_is_writable(mount),
            root: mount.mount_at.is_empty(),
            // `render_mount_line` takes the root index dir as an `Option` and simply omits
            // the `[index: ...]` note when it is absent, which is exactly the degraded
            // rendering an unresolvable config should get.
            line: crate::commands::render_mount_line(mount, root_index_dir.as_deref()),
            index_dir: resolved
                .as_ref()
                .ok()
                .map(|resolved| resolved_mount_index_dir(resolved, mount)),
        })
        .collect();

    Ok(MountsListReport {
        config_path: config_path.to_path_buf(),
        implicit: !declared,
        root_index_dir,
        mounts: entries,
        experimental: enabled_experimental_flags(&experimental),
        unresolved,
    })
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

/// Every secret reference a mount carries, rendered for an operator.
fn mount_secret_refs(mount: &MountConfig) -> Vec<String> {
    match &mount.backend {
        MountBackendConfig::Filesystem { .. } => Vec::new(),
        MountBackendConfig::Couchdb {
            password_ref, e2ee, ..
        } => {
            let mut refs = vec![describe_secret_ref(password_ref)];
            if let Some(e2ee) = e2ee {
                refs.push(describe_secret_ref(&e2ee.passphrase_ref));
                if let Some(obfuscate) = &e2ee.obfuscate_passphrase_ref {
                    refs.push(describe_secret_ref(obfuscate));
                }
            }
            refs
        }
        MountBackendConfig::Algolia { api_key_ref, .. } => vec![describe_secret_ref(api_key_ref)],
    }
}

/// Unmount a mount, leaving its content and its credentials alone.
///
/// # What is deliberately NOT destroyed
///
/// * **The remote data.** Removing a couchdb or algolia mount stops this build reading
///   the vault; it does not delete a document, a record or a version. `couchdb export` /
///   `algolia dump` remain the ways to take a copy, and `algolia retract` remains the only
///   destructive command in this binary.
/// * **The local index**, unless `--purge-index`. It is a rebuildable cache, but a large
///   one, and re-adding a mount removed by mistake should not cost a full reindex.
/// * **The stored credential**, always. A [`SecretRef`] can be SHARED — two mounts against
///   the same CouchDB server may legitimately point at one keyring entry — and silently
///   deleting a reference this command cannot prove is unshared would break the other
///   mount with no message. So the reference is NAMED in the output instead, and cleaning
///   the keyring stays the operator's explicit act.
///
/// # The two refusals
///
/// * **The last mount.** A config needs a root mount; a table with none is not a shape
///   the loader accepts, so there is nothing valid to write. Refused with a named
///   message rather than silently converted back to a legacy `vaultPath` — that
///   conversion is only possible for a filesystem root, and making it happen implicitly
///   would mean `mounts remove` sometimes rewrites the config into a different FORMAT.
/// * **The root, while others remain.** The table needs its floor: every other mount
///   resolves by longest prefix beneath it. Remove the others first, or edit the file to
///   promote another mount to `mountAt: ""`.
///
/// Takes no [`SecretResolver`]: it is the one command in this family that touches no
/// secret store at all, which is the point of the third bullet above.
pub fn remove(
    config_path: &Path,
    id: &str,
    purge_index: bool,
    yes: bool,
    dry_run: bool,
) -> Result<MountsRemoveReport> {
    let existing = load_config_file(config_path)?
        .ok_or_else(|| anyhow!("no config file at {}", config_path.display()))?;

    let Some(declared) = existing
        .mounts
        .as_ref()
        .filter(|mounts| !mounts.is_empty())
        .cloned()
    else {
        bail!(
            "{} declares no mount table, so there is no mount to remove: its vault is the \
             top-level `vaultPath`. `mounts add` converts such a config to an explicit table.",
            config_path.display()
        );
    };

    let Some(position) = declared.iter().position(|mount| mount.id == id) else {
        bail!(
            "no mount with id {id:?} in {}. Declared ids: {}.",
            config_path.display(),
            declared
                .iter()
                .map(|mount| format!("'{}'", mount.id))
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let mount = declared[position].clone();

    if declared.len() == 1 {
        bail!(
            "refusing to remove '{id}': it is the only mount in {}, and a config needs a root \
             mount — removing it would leave a table the server cannot load. Edit the file \
             directly to start over.",
            config_path.display()
        );
    }
    if mount.mount_at.is_empty() {
        bail!(
            "refusing to remove '{id}': it is the ROOT mount (mountAt \"\") and {} other mount(s) \
             are declared, which resolve by longest prefix beneath it — the table needs its \
             floor. Remove the others first, or edit {} to promote another mount to the root.",
            declared.len() - 1,
            config_path.display()
        );
    }

    let mut messages = Vec::new();
    // Resolved BEFORE the removal so the index directory reported (and possibly purged) is
    // the one this mount was actually using.
    //
    // A failure here is NOT fatal, and that is deliberate: removing a mount is one of the
    // ways to REPAIR a table the loader refuses — dropping one of two mounts from a config
    // whose `experimental.multiVault` was edited out leaves a single valid root. Refusing
    // to remove from a broken config would make the repair impossible with this command.
    // What is lost is the index directory, which is derived from the resolved root.
    let before = validate(
        candidate_config(
            &existing,
            &declared,
            &existing.experimental.clone().unwrap_or_default(),
        ),
        config_path,
    );
    if let Err(error) = &before {
        messages.push(format!(
            "note: this config does not resolve as it stands ({error}), so this mount's index              directory cannot be derived and --purge-index has nothing it can safely delete.              The removal itself is still validated below."
        ));
    }
    let index_dir = before
        .as_ref()
        .ok()
        .map(|before| resolved_mount_index_dir(before, &mount));
    let secret_refs = mount_secret_refs(&mount);

    if !yes
        && !dry_run
        && !crate::commands::confirm(&format!(
            "Remove mount '{id}' ({} at /{}) from {}?\n  \
             Nothing is deleted from the mount's backing store, and its stored credential is \
             kept.\nProceed?",
            mount.backend.kind_name(),
            mount.mount_at,
            config_path.display()
        ))?
    {
        bail!("aborted; nothing was removed");
    }

    let mut remaining = declared;
    remaining.remove(position);
    // The backstop: the explicit refusals above make the actionable cases actionable, and
    // this proves the table that is about to be written is one the server will load.
    // Experimental flags are deliberately left as they are — turning one off because the
    // last mount that needed it went away would be a second, unasked-for change.
    let experimental = existing.experimental.clone().unwrap_or_default();
    let resolved = validate_for_write(
        candidate_config(&existing, &remaining, &experimental),
        config_path,
        &messages,
    )?;

    messages.push(match &mount.backend {
        MountBackendConfig::Filesystem { vault_path, .. } => format!(
            "unmounted '{id}' from /{}. {} was not touched.",
            mount.mount_at,
            vault_path.display()
        ),
        MountBackendConfig::Couchdb { .. } | MountBackendConfig::Algolia { .. } => format!(
            "unmounted '{id}' from /{}. NOTHING was deleted from {} — the remote data is \
             untouched.",
            mount.mount_at,
            mount.backend.location()
        ),
    });

    if dry_run {
        messages.push(format!(
            "dry-run: {} was not written and no index directory was deleted",
            config_path.display()
        ));
        return Ok(MountsRemoveReport {
            config_path: config_path.to_path_buf(),
            mount: mount.id.clone(),
            mount_at: mount.mount_at.clone(),
            kind: mount.backend.kind_name().to_string(),
            index_dir,
            index_purged: false,
            secret_refs,
            written: false,
            dry_run: true,
            backup_path: None,
            messages,
        });
    }

    let persisted = persist(&resolved, &existing);
    let backup_path = crate::commands::write_config_with_backup(config_path, &persisted)?;
    if let Some(backup_path) = &backup_path {
        messages.push(format!(
            "backed up previous config: {}",
            backup_path.display()
        ));
    }
    messages.push(format!("wrote config: {}", config_path.display()));

    // Purged AFTER a successful write: a failed write must not leave the operator with a
    // config that still declares the mount and an index directory that is gone.
    let mut index_purged = false;
    match (&index_dir, purge_index) {
        (None, true) => messages.push(
            "--purge-index: skipped — this mount's index directory could not be derived (see \
             the note above). Delete it by hand if you know where it is."
                .to_string(),
        ),
        (None, false) => {}
        // Belt and braces. A non-root mount's index dir can never be the root's (the
        // default is a per-id subdirectory of it), but an explicit `indexDir` could have
        // been pointed at it by hand, and `remove_dir_all` on the root index would destroy
        // every other mount's index too.
        (Some(index_dir), true)
            if before
                .as_ref()
                .is_ok_and(|before| *index_dir == before.index_dir) =>
        {
            messages.push(format!(
                "--purge-index: refusing to delete {} — it is the ROOT index directory, shared by \
                 every mount. Delete this mount's index by hand if that is really what you want.",
                index_dir.display()
            ));
        }
        (Some(index_dir), true) if index_dir.exists() => {
            fs::remove_dir_all(index_dir)
                .with_context(|| format!("could not delete {}", index_dir.display()))?;
            index_purged = true;
            messages.push(format!("--purge-index: deleted {}", index_dir.display()));
        }
        (Some(index_dir), true) => messages.push(format!(
            "--purge-index: nothing to delete at {} (it does not exist)",
            index_dir.display()
        )),
        (Some(index_dir), false) => messages.push(format!(
            "left this mount's index in place at {} (pass --purge-index to delete it)",
            index_dir.display()
        )),
    }

    if secret_refs.is_empty() {
        messages.push("this mount stored no credential.".to_string());
    } else {
        messages.push(format!(
            "the stored credential(s) were KEPT, because a reference can be shared by more than \
             one mount and deleting one silently would break the other. Delete them yourself if \
             this mount was the only user: {}",
            secret_refs.join("; ")
        ));
    }

    Ok(MountsRemoveReport {
        config_path: config_path.to_path_buf(),
        mount: mount.id.clone(),
        mount_at: mount.mount_at.clone(),
        kind: mount.backend.kind_name().to_string(),
        index_dir,
        index_purged,
        secret_refs,
        written: true,
        dry_run: false,
        backup_path,
        messages,
    })
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_add_report(report: &MountsAddReport) -> String {
    let mut lines = vec![format!("config: {}", report.config_path.display())];
    lines.extend(report.messages.iter().cloned());
    lines.push(format!(
        "mount {} at {} ({}): {} [index: {}] — {}",
        report.mount,
        if report.mount_at.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", report.mount_at)
        },
        report.kind,
        report.location,
        report.index_dir.display(),
        if report.writable {
            "writable"
        } else {
            "read-only"
        }
    ));
    lines.join("\n")
}

pub fn render_list_report(report: &MountsListReport) -> String {
    let mut lines = vec![format!("config: {}", report.config_path.display())];
    // Printed BEFORE the table: an operator reading a listing of a config the server will
    // not load has to know that before they act on what follows.
    if let Some(reason) = &report.unresolved {
        lines.push(format!(
            "WARNING: this config does NOT resolve, so the server would refuse to start on \
             it. The table below is what the file declares; index directories are omitted \
             because they are derived from the resolved root. Reason: {reason}"
        ));
    }
    if report.implicit {
        lines.push(
            "mount table: IMPLICIT — this config declares no `mounts`, so its `vaultPath` is the \
             single root mount below. `mounts add` converts it to an explicit table."
                .to_string(),
        );
    }
    for entry in &report.mounts {
        lines.push(format!(
            "{} — {}{}",
            entry.line,
            if entry.writable {
                "writable"
            } else {
                "read-only"
            },
            if report.implicit { " (implicit)" } else { "" }
        ));
    }
    lines.push(format!(
        "experimental: {}",
        if report.experimental.is_empty() {
            "(none enabled)".to_string()
        } else {
            report.experimental.join(", ")
        }
    ));
    lines.join("\n")
}

pub fn render_remove_report(report: &MountsRemoveReport) -> String {
    let mut lines = vec![format!("config: {}", report.config_path.display())];
    lines.extend(report.messages.iter().cloned());
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch the `mounts` family.
///
/// `options.config` and `options.index_dir` are the GLOBAL flags: the first names the file
/// to edit, the second sets the NEW mount's own `indexDir` (the config's top-level
/// `indexDir` is never changed by this family). Reusing the global `--index-dir` rather
/// than declaring a second one is not just economy — a subcommand redeclaring a global
/// clap argument is a duplicate-argument panic.
pub async fn run(
    options: &ServiceOptions,
    command: MountsCommand,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    // Read the FILE, not `resolve_runtime_config`. Three reasons, each load-bearing:
    // a stray `--vault` or `DEEP_OBSIDIAN_VAULT_PATH` must not decide which path the
    // legacy migration writes into the root mount; `mounts list` needs to know whether the
    // file DECLARED a table, which the resolved view erases; and both commands must work
    // on a config the resolver would reject.
    let config_path = options
        .config
        .clone()
        .map(deep_obsidian_config::expand_home_path)
        .unwrap_or_else(deep_obsidian_config::default_config_path);

    match command {
        MountsCommand::Add { kind } => {
            // Guided mode FIRST: the flags are pre-answers, and anything they left out is
            // asked here (or reported as missing, together, when there is no terminal to
            // ask on). Everything below therefore works on a fully specified mount.
            let spec = crate::wizard::resolve_mount_spec_from_flags(&kind)?;
            let mut secrets = if uses_stdin_secret(&spec) {
                SecretReader::from_stdin()?
            } else {
                SecretReader::interactive()
            };
            let report = add(
                &config_path,
                options.index_dir.as_deref(),
                &spec,
                dry_run,
                &mut secrets,
            )
            .await?;
            crate::commands::print_report(json, &report, || render_add_report(&report))
        }
        MountsCommand::List => {
            let report = list(&config_path)?;
            crate::commands::print_report(json, &report, || render_list_report(&report))
        }
        MountsCommand::Remove {
            id,
            purge_index,
            yes,
        } => {
            let report = remove(&config_path, &id, purge_index, yes, dry_run)?;
            crate::commands::print_report(json, &report, || render_remove_report(&report))
        }
    }
}

/// Whether this add reads its credential from stdin rather than prompting.
pub(crate) fn uses_stdin_secret(kind: &MountSpec) -> bool {
    match kind {
        MountSpec::Filesystem { .. } => false,
        MountSpec::Couchdb { password_stdin, .. } => *password_stdin,
        MountSpec::Algolia { api_key_stdin, .. } => *api_key_stdin,
    }
}
