//! `setup-service --wizard`: the first-init flow, and the per-kind question sequences it
//! shares with `mounts add`.
//!
//! # Why the questions live here and not next to each command
//!
//! There are two ways to describe a mount to this binary — answer the wizard, or type
//! `mounts add couchdb --url ...` — and they ask for the same six things. Written twice they
//! would drift in wording, in ordering, and in what counts as a default, and the drift would
//! show up as two different configs for the same intent. So there is ONE implementation per
//! kind ([`resolve_mount_spec`]), parameterized by what the caller already knows:
//!
//! * The wizard knows nothing, and walks the whole sequence.
//! * `mounts add` knows whatever the flags said. Those are *pre-answers*: a supplied `--url`
//!   is a question already answered, and only the gaps are asked. That is what makes
//!   `mounts add couchdb` on a terminal a guided command rather than a usage error.
//!
//! # The non-interactive contract
//!
//! A missing flag with no terminal to ask on is an ERROR naming every missing flag at once —
//! never a prompt that would hang a script forever waiting on a stdin nobody is typing into.
//! `--yes` means "ask me nothing", so it takes the same path even on a terminal, and so does
//! `--password-stdin` / `--api-key-stdin`, which have already claimed stdin for the
//! credential. See [`resolve_mount_spec_from_flags`].
//!
//! # Why the whole wizard is testable without a tty
//!
//! Every question goes through [`AnswerReader`] and every credential through
//! [`SecretReader`], and both have a `from_lines` constructor. A test drives the entire
//! six-screen flow as a list of strings, which is the only way to get the flow — as opposed
//! to its pieces — under test at all: the previous wizard read `io::stdin()` directly from
//! six different call sites and was therefore untested end to end.
//!
//! # End of input is an abort, not a partial config
//!
//! An EOF at ANY question fails with a message naming the question, and
//! [`run_with_io`] rolls back every credential the run had already stored on its way out.
//! Nothing is written. That matters because a wizard is exactly the command an operator
//! interrupts — a closed pipe, a `^D`, a terminal that went away — and a half-answered
//! config is worse than none.

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use deep_obsidian_config::{
    build_service_endpoints, default_config_path, expand_home_path, render_config_text,
    secrets::SecretResolver,
};
use deep_obsidian_types::{
    AuthConfigInput, EmbeddingConfigInput, EmbeddingProvider, HttpConfigInput,
    PersistedServiceConfig, ResolvedServiceConfig, SecretRef, StdioMode, TransportMode,
};
use secrecy::SecretString;

use crate::cli::{MountsAddKind, ServiceOptions, TransportMode as CliTransport};
use crate::commands::{InstallChoices, SetupServiceReport};
use crate::mounts_cmd::{
    describe_secret_ref, roll_back_stored_secrets, MountCommon, MountProbeReport, MountSpec,
    SecretReader,
};

/// Keyring service name every stored secret in this project shares.
const SECRET_SERVICE: &str = "deep-obsidian-mcp";
/// Account the embedding API key has always been stored under. Unchanged on purpose: a
/// re-run of the wizard must find the key an earlier run stored, not orphan it.
const EMBEDDING_KEY_ACCOUNT: &str = "openai-embedding";

/// The default local endpoint the "Ollama" preset fills in.
const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
/// The model that preset suggests. An Ollama default rather than a hosted one because the
/// wizard's recommended path needs no account and no key.
const OLLAMA_MODEL: &str = "nomic-embed-text";

// ---------------------------------------------------------------------------
// Answer input
// ---------------------------------------------------------------------------

/// Where the wizard's non-secret answers come from.
///
/// The counterpart to [`SecretReader`], and deliberately the same shape: `interactive` reads
/// the terminal, `from_lines` replays a script. Credentials need a separate reader because
/// they must never echo — `rpassword` needs a tty and cannot be fed a line — which is why
/// the two are not one type.
pub struct AnswerReader {
    /// `Some` for the scripted path; `None` means read the terminal.
    lines: Option<VecDeque<String>>,
}

impl AnswerReader {
    /// Read each answer from the terminal.
    pub fn interactive() -> Self {
        Self { lines: None }
    }

    /// An explicit list of answers, consumed in question order.
    ///
    /// The seam the tests use instead of a pty. Blank strings are meaningful: a blank answer
    /// accepts the question's default, which is how a test says "just press Enter".
    pub fn from_lines(lines: Vec<String>) -> Self {
        Self {
            lines: Some(lines.into()),
        }
    }

    /// The raw next line, or an abort naming the question the input ran out at.
    ///
    /// Both exhaustion cases land here — a closed stdin and an exhausted script — so there is
    /// one message and one behaviour for "the operator stopped answering".
    fn read(&mut self, label: &str) -> Result<String> {
        match &mut self.lines {
            Some(lines) => lines
                .pop_front()
                .ok_or_else(|| end_of_input(label))
                .map(|line| line.trim().to_string()),
            None => {
                let mut input = String::new();
                let read = io::stdin()
                    .read_line(&mut input)
                    .context("failed to read prompt input")?;
                if read == 0 {
                    return Err(end_of_input(label));
                }
                Ok(input.trim().to_string())
            }
        }
    }

    fn prompt(&self, text: &str) -> Result<()> {
        print!("{text}");
        io::stdout().flush().context("failed to flush stdout")
    }

    /// A free-text answer, with `default` returned for a blank one.
    pub fn line(&mut self, label: &str, default: Option<&str>) -> Result<String> {
        match default {
            Some(default) => self.prompt(&format!("{label} [{default}]: "))?,
            None => self.prompt(&format!("{label}: "))?,
        }
        let value = self.read(label)?;
        Ok(if value.is_empty() {
            default.unwrap_or_default().to_string()
        } else {
            value
        })
    }

    /// A free-text answer that may not be blank.
    ///
    /// Re-asks rather than aborting: a mistyped Enter is not a decision to stop, and the
    /// re-ask still terminates because [`Self::read`] fails at end of input.
    pub fn required_line(&mut self, label: &str, default: Option<&str>) -> Result<String> {
        loop {
            let value = self.line(label, default)?;
            if !value.trim().is_empty() {
                return Ok(value);
            }
            println!("  This one is required.");
        }
    }

    /// A free-text answer where blank means "not set".
    pub fn optional_line(&mut self, label: &str, default: Option<&str>) -> Result<Option<String>> {
        let value = self.line(label, default)?;
        Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
    }

    /// A yes/no answer. Blank takes `default`; anything unrecognized is re-asked.
    pub fn yes_no(&mut self, label: &str, default: bool) -> Result<bool> {
        let suffix = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            self.prompt(&format!("{label} {suffix} "))?;
            let value = self.read(label)?;
            if value.is_empty() {
                return Ok(default);
            }
            match value.to_ascii_lowercase().as_str() {
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => println!("  Please answer y or n."),
            }
        }
    }

    /// A numbered menu. Returns the zero-based index; blank takes `default`.
    pub fn choice(&mut self, label: &str, options: &[&str], default: usize) -> Result<usize> {
        println!("{label}");
        for (index, option) in options.iter().enumerate() {
            println!("  {}) {option}", index + 1);
        }
        loop {
            self.prompt(&format!("Choice [{}]: ", default + 1))?;
            let value = self.read(label)?;
            if value.is_empty() {
                return Ok(default);
            }
            match value.parse::<usize>() {
                Ok(choice) if choice >= 1 && choice <= options.len() => return Ok(choice - 1),
                _ => println!("  Please answer with a number from 1 to {}.", options.len()),
            }
        }
    }
}

/// The abort every exhausted input produces.
///
/// One wording, and one that names the question, because "unexpected end of input" on its own
/// leaves an operator with no idea how far the wizard got.
fn end_of_input(label: &str) -> anyhow::Error {
    anyhow!(
        "aborted at \"{label}\": the input ended before that question was answered. Nothing was \
         written."
    )
}

// ---------------------------------------------------------------------------
// The per-kind question sequences
// ---------------------------------------------------------------------------

/// How much of a kind's sequence to walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// Only the fields that have no usable default — exactly what `mounts add` left out.
    /// `--writable`, `--username` and friends are NOT asked: they are documented flags with
    /// documented defaults, and asking about them would turn "fill in the two things you
    /// forgot" into a five-question interrogation.
    RequiredOnly,
    /// Every question, including the ones a flag would otherwise have defaulted. The
    /// wizard's depth, because there were no flags for it to default from — and because a
    /// first-time operator has to be *asked* whether an agent may write to their vault
    /// rather than have it silently answered.
    Everything,
}

/// Turn `mounts add`'s flags into a mount, asking for whatever they left out.
///
/// Decides for itself whether asking is possible, which is the whole policy in one place:
///
/// * `--yes` means "ask me nothing", so nothing is asked.
/// * `--password-stdin` / `--api-key-stdin` have given stdin to the credential, so there is
///   no stdin left to read an answer from.
/// * Otherwise a terminal on stdin means ask; no terminal means a script or a pipe, and a
///   prompt there would hang forever waiting for input nobody is going to type.
///
/// In each non-asking case a missing required flag produces [`missing_flags_error`], which
/// names them all at once rather than one per re-run.
pub fn resolve_mount_spec_from_flags(kind: &MountsAddKind) -> Result<MountSpec> {
    let scripted = kind_yes(kind) || kind_reads_stdin(kind);
    let interactive = !scripted && io::stdin().is_terminal();
    let mut answers = AnswerReader::interactive();
    resolve_mount_spec(kind, interactive.then_some(&mut answers))
}

/// [`resolve_mount_spec_from_flags`] with the decision already made.
///
/// `Some(answers)` asks for the gaps; `None` reports them. Public so a test can drive the
/// asking path with a line list instead of a terminal, and the reporting path by passing
/// `None` — which is the same code the no-tty case runs.
pub fn resolve_mount_spec(
    kind: &MountsAddKind,
    answers: Option<&mut AnswerReader>,
) -> Result<MountSpec> {
    resolve(kind, answers, Depth::RequiredOnly)
}

/// The error a non-interactive run gets when a required flag is missing.
///
/// Names every missing flag together, and names the two ways forward: supply them, or run
/// the command where it can ask. Deliberately shaped like clap's own "the following required
/// arguments were not provided", because that is what it replaces.
fn missing_flags_error(kind_name: &str, missing: &[&str]) -> anyhow::Error {
    let flags = missing
        .iter()
        .map(|flag| format!("  {flag}"))
        .collect::<Vec<_>>()
        .join("\n");
    anyhow!(
        "the following required arguments were not provided:\n{flags}\n\n\
         Usage: deep-obsidian-mcp mounts add {kind_name} --id <ID> --mount-at <PREFIX> ...\n\n\
         On a terminal these are asked for instead. There is none here — stdin is not a tty, \
         or --yes / --password-stdin / --api-key-stdin said not to ask — so they have to be \
         flags. `deep-obsidian-mcp setup-service --wizard` walks the same questions for a \
         first-time setup."
    )
}

fn kind_yes(kind: &MountsAddKind) -> bool {
    match kind {
        MountsAddKind::Filesystem { common, .. }
        | MountsAddKind::Couchdb { common, .. }
        | MountsAddKind::Algolia { common, .. } => common.yes,
    }
}

fn kind_reads_stdin(kind: &MountsAddKind) -> bool {
    match kind {
        MountsAddKind::Filesystem { .. } => false,
        MountsAddKind::Couchdb { password_stdin, .. } => *password_stdin,
        MountsAddKind::Algolia { api_key_stdin, .. } => *api_key_stdin,
    }
}

/// A mount id derived from where the mount sits.
///
/// `Team/Alpha` becomes `team-alpha`. Suggested rather than imposed, because the id is what
/// appears in `--mount <id>`, in error messages and in the account its credential is stored
/// under — an operator who wants `alpha` should get `alpha`.
pub fn suggested_mount_id(mount_at: &str) -> String {
    let slug = mount_at
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    // The config requires `[a-z0-9][a-z0-9-]*`, so a prefix of only punctuation — or the
    // root's empty one — needs a fallback rather than an invalid suggestion.
    if slug.is_empty() || !slug.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return "vault".to_string();
    }
    slug
}

/// The one implementation of every per-kind sequence. See the module docs.
fn resolve(
    kind: &MountsAddKind,
    mut answers: Option<&mut AnswerReader>,
    depth: Depth,
) -> Result<MountSpec> {
    let kind_name = match kind {
        MountsAddKind::Filesystem { .. } => "filesystem",
        MountsAddKind::Couchdb { .. } => "couchdb",
        MountsAddKind::Algolia { .. } => "algolia",
    };
    let mut missing: Vec<&str> = Vec::new();

    // `mountAt` before `id`, in both modes, so the id question can suggest a slug of the
    // prefix the operator has just given. Reversing them would make the suggestion
    // impossible and is the reason this ordering is not "as declared".
    let common = match kind {
        MountsAddKind::Filesystem { common, .. }
        | MountsAddKind::Couchdb { common, .. }
        | MountsAddKind::Algolia { common, .. } => common,
    };
    // `required_line`, not `line`: the ROOT prefix is `""`, and it is only ever a
    // PRE-ANSWER — the wizard supplies it on screen 1, and `mounts add` runs against a config
    // that already has a root. So every time this question is actually asked, a blank is a
    // stray Enter, and accepting it would build a second root mount, fail the whole-table
    // validation and take a six-screen wizard run down with it. Re-asking costs one keystroke.
    let mount_at = match (&common.mount_at, answers.as_deref_mut()) {
        (Some(value), _) => value.clone(),
        (None, Some(answers)) => answers.required_line(
            "Mount it under which vault folder? (e.g. Team, or Team/Alpha)",
            None,
        )?,
        (None, None) => {
            missing.push("--mount-at <MOUNT_AT>");
            String::new()
        }
    };
    let id = match (&common.id, answers.as_deref_mut()) {
        (Some(value), _) => value.clone(),
        (None, Some(answers)) => {
            answers.required_line("Mount id", Some(&suggested_mount_id(&mount_at)))?
        }
        (None, None) => {
            missing.push("--id <ID>");
            String::new()
        }
    };
    let resolved_common = MountCommon {
        id,
        mount_at,
        keep_anyway: common.keep_anyway,
        yes: common.yes,
    };

    let spec = match kind {
        MountsAddKind::Filesystem { vault_path, .. } => {
            let vault_path = match (vault_path, answers.as_deref_mut()) {
                (Some(value), _) => value.clone(),
                (None, Some(answers)) => {
                    PathBuf::from(answers.required_line("Vault folder", None)?)
                }
                (None, None) => {
                    missing.push("--vault-path <VAULT_PATH>");
                    PathBuf::new()
                }
            };
            MountSpec::Filesystem {
                common: resolved_common,
                vault_path,
            }
        }
        MountsAddKind::Couchdb {
            url,
            database,
            username,
            password_stdin,
            writable,
            e2ee,
            sidecar_path,
            ..
        } => {
            let url = match (url, answers.as_deref_mut()) {
                (Some(value), _) => value.clone(),
                (None, Some(answers)) => answers.required_line(
                    "CouchDB server URL (the origin only, e.g. https://couch.example)",
                    None,
                )?,
                (None, None) => {
                    missing.push("--url <URL>");
                    String::new()
                }
            };
            let database = match (database, answers.as_deref_mut()) {
                (Some(value), _) => value.clone(),
                (None, Some(answers)) => answers.required_line("LiveSync database name", None)?,
                (None, None) => {
                    missing.push("--database <DATABASE>");
                    String::new()
                }
            };
            // The optional half, asked only at wizard depth. `username` is an identifier
            // rather than a credential, so it is a plain line; the password is never one.
            let (username, e2ee, writable) = match (depth, answers.as_deref_mut()) {
                (Depth::Everything, Some(answers)) => (
                    answers.optional_line("CouchDB user name (blank for none)", None)?,
                    answers.yes_no(
                        "Is this vault end-to-end encrypted? (LiveSync's E2EE passphrase)",
                        false,
                    )?,
                    answers.yes_no("May the agent WRITE to this vault?", false)?,
                ),
                _ => (username.clone(), *e2ee, *writable),
            };
            MountSpec::Couchdb {
                common: resolved_common,
                url,
                database,
                username,
                password_stdin: *password_stdin,
                writable,
                e2ee,
                sidecar_path: sidecar_path.clone(),
            }
        }
        MountsAddKind::Algolia {
            app_id,
            index_name,
            base_url,
            api_key_stdin,
            writable,
            participant_id,
            ..
        } => {
            let app_id = match (app_id, answers.as_deref_mut()) {
                (Some(value), _) => value.clone(),
                (None, Some(answers)) => answers.required_line("Algolia application id", None)?,
                (None, None) => {
                    missing.push("--app-id <APP_ID>");
                    String::new()
                }
            };
            let index_name = match (index_name, answers.as_deref_mut()) {
                (Some(value), _) => value.clone(),
                (None, Some(answers)) => answers.required_line("Index name", None)?,
                (None, None) => {
                    missing.push("--index-name <INDEX_NAME>");
                    String::new()
                }
            };
            // `baseUrl` is asked at wizard depth and `participantId` is not, and the line
            // between them is REACHABILITY: the default endpoint is derived from the app id
            // and is wrong for anything not on Algolia's public one, so leaving it unasked
            // would produce a mount whose probe fails for a reason the wizard could have
            // avoided. `participantId` only labels an audit trail and has a working default.
            let (base_url, writable) = match (depth, &mut answers) {
                (Depth::Everything, Some(answers)) => (
                    answers.optional_line(
                        "REST endpoint override (blank for https://<appId>.algolia.net)",
                        None,
                    )?,
                    answers.yes_no("May the agent WRITE to this corpus?", false)?,
                ),
                _ => (base_url.clone(), *writable),
            };
            MountSpec::Algolia {
                common: resolved_common,
                app_id,
                index_name,
                base_url,
                api_key_stdin: *api_key_stdin,
                writable,
                participant_id: participant_id.clone(),
            }
        }
    };

    if !missing.is_empty() {
        return Err(missing_flags_error(kind_name, &missing));
    }
    Ok(spec)
}

// ---------------------------------------------------------------------------
// Wizard plumbing
// ---------------------------------------------------------------------------

/// Everything the wizard reads from and writes secrets to.
///
/// Owns its readers rather than borrowing them so the whole flow needs one lifetime, and
/// carries the [`SecretResolver`] explicitly for the reason `add_with_resolver` does: a test
/// must be able to point at a temp secrets file, and `prefer_os_keyring = false` is what
/// keeps an `osKeyring`-shaped reference from reaching the developer's login keychain.
pub struct WizardIo {
    pub answers: AnswerReader,
    pub secrets: SecretReader,
    pub resolver: SecretResolver,
    pub prefer_os_keyring: bool,
}

impl WizardIo {
    /// The real thing: terminal answers, masked credentials, the default secret store.
    pub fn interactive() -> Self {
        Self {
            answers: AnswerReader::interactive(),
            secrets: SecretReader::interactive(),
            resolver: SecretResolver::new(),
            prefer_os_keyring: true,
        }
    }
}

/// The flags `setup-service` passes through to the wizard.
///
/// A struct because the wizard's own entry point would otherwise take seven parameters, four
/// of them booleans in a row — the shape in which two get transposed and nobody notices.
pub struct WizardRequest<'a> {
    pub options: &'a ServiceOptions,
    pub dry_run: bool,
    pub overwrite: bool,
    /// `--mcp` / `--skills` / `--vault-snippets`. A flag that is already `true` skips its
    /// offer; the wizard asks about the rest.
    pub installs: InstallChoices,
}

/// What the run has accumulated so far, across every screen.
///
/// Separate from the answers because it is what an ABORT needs: `stored` is the list of
/// credentials to roll back, and `messages` is the narration the failure is prefixed with.
#[derive(Default)]
struct WizardState {
    messages: Vec<String>,
    /// Every secret reference this run created. See [`roll_back_stored_secrets`].
    stored: Vec<SecretRef>,
    secret_refs: Vec<String>,
    experimental_enabled: Vec<String>,
    /// One entry per mount that was probed, for the closing doctor report.
    probes: Vec<(String, MountProbeReport)>,
}

/// Where the vault's root lives, as answered on screen 1.
enum RootChoice {
    /// A local directory, which stays a legacy top-level `vaultPath`: a one-mount table
    /// would resolve identically and be a gratuitously different file for the common case.
    Local(PathBuf),
    /// A remote backend, which can only be expressed as a `mountAt: ""` mount.
    Remote(MountSpec),
}

/// The embedding answers from screen 3.
struct EmbeddingAnswers {
    enabled: bool,
    model: Option<String>,
    base_url: Option<String>,
    api_key_ref: Option<SecretRef>,
}

/// The transport answers from screen 4.
struct TransportAnswers {
    transport: TransportMode,
    port: Option<u16>,
    /// `None` for stdio: there is no HTTP surface to authenticate, and passing `Some(false)`
    /// would DEPROVISION a token a previous HTTP setup had stored.
    auth: Option<bool>,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Run the first-init wizard against the real terminal and the real secret store.
pub async fn run(request: &WizardRequest<'_>) -> Result<SetupServiceReport> {
    run_with_io(request, &mut WizardIo::interactive()).await
}

/// [`run`] against explicit input streams and an explicit secret store.
///
/// The single place every abort is turned into a rollback: whatever goes wrong — a declined
/// experimental flag, a failed probe, a declined recap, an EOF at question eleven — the
/// credentials this run stored are removed on the way out, because the config that would
/// have referenced them is not being written.
pub async fn run_with_io(
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
) -> Result<SetupServiceReport> {
    let mut state = WizardState::default();
    match drive(request, io, &mut state).await {
        Ok(report) => Ok(report),
        Err(error) => {
            let mut notes = Vec::new();
            roll_back_stored_secrets(&io.resolver, &state.stored, &mut notes);
            if notes.is_empty() {
                Err(error)
            } else {
                Err(anyhow!("{error}\n{}", notes.join("\n")))
            }
        }
    }
}

/// Screens 1 to 6, in order.
async fn drive(
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
    state: &mut WizardState,
) -> Result<SetupServiceReport> {
    let config_path = request
        .options
        .config
        .clone()
        .map(expand_home_path)
        .unwrap_or_else(default_config_path);
    // Prefill every prompt from the existing file so a re-run is an edit rather than a
    // from-scratch rewrite: pressing Enter keeps the current value.
    let existing = deep_obsidian_config::read_config_file(&config_path)
        .ok()
        .flatten();

    // Refused HERE, before the first prompt, rather than after the last one. See
    // `refuse_wizard_on_a_mounts_config`: the wizard CREATES mount tables but does not edit
    // one, and finding that out after twelve answers would be the worst possible ordering.
    crate::commands::refuse_wizard_on_a_mounts_config(&config_path, existing.as_ref())?;

    println!("deep-obsidian-mcp setup");
    println!("config: {}", config_path.display());
    if request.dry_run {
        println!("--dry-run: nothing will be written, stored or contacted.");
    }
    println!();

    // ---- Screen 1: the root vault.
    let root = ask_root(request, io, existing.as_ref())?;

    // The config the run builds up. Starts from the existing file so unknown keys, the
    // auto-reindex section and anything else the wizard does not ask about survive.
    let mut working = existing.clone().unwrap_or_default();
    let mut local_root: Option<PathBuf> = None;
    match root {
        RootChoice::Local(path) => {
            working.vault_path = Some(path.clone());
            local_root = Some(path);
        }
        RootChoice::Remote(spec) => {
            append_and_gate(&mut working, &config_path, &spec, true, request, io, state).await?;
        }
    }

    // ---- Screen 2: additional mounts, under subfolders.
    println!();
    while io.answers.yes_no(
        "Add another vault, mounted under a subfolder of this one?",
        false,
    )? {
        let spec = ask_mount_kind(io, false)?;
        append_and_gate(&mut working, &config_path, &spec, false, request, io, state).await?;
        println!();
    }

    // ---- Screen 3: embeddings.
    println!();
    let embeddings = ask_embeddings(request, io, existing.as_ref(), state)?;

    // ---- Screen 4: transport.
    println!();
    let transport = ask_transport(io, existing.as_ref())?;

    // ---- Screen 5: the extras.
    println!();
    let installs = ask_extras(request, io, &transport, local_root.as_deref(), state)?;

    // ---- Screen 6: recap, confirm, write, report.
    finish(
        request,
        io,
        state,
        FinishInputs {
            config_path,
            existing,
            working,
            local_root,
            embeddings,
            transport,
            installs,
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Screen 1: the root vault
// ---------------------------------------------------------------------------

/// Ask where the vault's root lives.
///
/// The remote options are offered HERE rather than only in screen 2 because a fully-remote
/// vault is a supported configuration — the config validator accepts any backend at
/// `mountAt: ""` — and a wizard that only ever offered a folder would make the one path a
/// LiveSync-only user needs look unsupported.
fn ask_root(
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
    existing: Option<&PersistedServiceConfig>,
) -> Result<RootChoice> {
    let choice = io.answers.choice(
        "Where do your notes live?",
        &[
            "A local folder                          (recommended)",
            "A remote LiveSync vault in CouchDB      (experimental)",
            "A shared Algolia index                  (experimental)",
        ],
        0,
    )?;
    if choice == 0 {
        // The detected default, in the same precedence the rest of the CLI uses: an explicit
        // `--vault`, then the current config, then the vault environment variables.
        let detected = request
            .options
            .vault_path
            .clone()
            .or_else(|| existing.and_then(|config| config.vault_path.clone()))
            .or_else(|| {
                ["DEEP_OBSIDIAN_VAULT_PATH", "OBSIDIAN_VAULT_PATH"]
                    .iter()
                    .find_map(|key| std::env::var(key).ok())
                    .map(PathBuf::from)
            })
            .map(|path| path.display().to_string());
        let answer = io
            .answers
            .required_line("Vault folder", detected.as_deref())?;
        let path = expand_home_path(PathBuf::from(answer));
        if !path.exists() {
            // Offered rather than done silently, and offered rather than refused: a typo and
            // a genuinely new vault look identical from here, so the operator is the only one
            // who can tell them apart.
            if !io.answers.yes_no(
                &format!("{} does not exist. Create it?", path.display()),
                true,
            )? {
                bail!(
                    "aborted: {} does not exist and was not created, so there is no vault to \
                     configure. Nothing was written.",
                    path.display()
                );
            }
            if !request.dry_run {
                std::fs::create_dir_all(&path).with_context(|| {
                    format!("failed to create the vault folder {}", path.display())
                })?;
                println!("  created {}", path.display());
            }
        }
        return Ok(RootChoice::Local(path));
    }

    println!();
    println!(
        "A remote root means this vault has no local folder at all. `vaultPath` is reported as \
         the backend's own location, vault snippets cannot be installed (Obsidian reads those \
         from a device's own vault folder), and an outage starts the service DEGRADED rather \
         than killing it."
    );
    // `mountAt` and the id are pre-answered: the root sits at "" by definition, and `vault`
    // is the same id the legacy migration uses, so a table written here and a table migrated
    // by `mounts add` name their root the same thing.
    let spec = ask_kind_questions(
        io,
        choice_to_kind(choice),
        Some(String::new()),
        Some("vault"),
    )?;
    Ok(RootChoice::Remote(spec))
}

/// Which backend a menu index means. Index 0 is the local folder, handled by the caller.
fn choice_to_kind(choice: usize) -> MountKind {
    match choice {
        1 => MountKind::Couchdb,
        2 => MountKind::Algolia,
        _ => MountKind::Filesystem,
    }
}

/// The three backends, as a menu answer.
#[derive(Debug, Clone, Copy)]
enum MountKind {
    Filesystem,
    Couchdb,
    Algolia,
}

/// Ask which kind an additional mount is, then where it goes and what it needs.
fn ask_mount_kind(io: &mut WizardIo, _root: bool) -> Result<MountSpec> {
    let choice = io.answers.choice(
        "What kind of vault is it?",
        &[
            "A local folder",
            "A remote LiveSync vault in CouchDB      (experimental)",
            "A shared Algolia index                  (experimental)",
        ],
        0,
    )?;
    ask_kind_questions(io, choice_to_kind(choice), None, None)
}

/// Walk one kind's full question sequence, with `mount_at` / `id` optionally pre-answered.
///
/// The wizard's only door into [`resolve`]: it builds the same `MountsAddKind` pre-answers
/// value `mounts add` would have parsed, with everything it does not already know left
/// `None`, and asks at [`Depth::Everything`]. That is what makes "the wizard and `mounts
/// add` ask the same questions" a fact about one function rather than a claim about two.
fn ask_kind_questions(
    io: &mut WizardIo,
    kind: MountKind,
    mount_at: Option<String>,
    id: Option<&str>,
) -> Result<MountSpec> {
    let common = crate::cli::MountsAddCommon {
        id: id.map(str::to_string),
        mount_at,
        keep_anyway: false,
        // Never `true`: the wizard asks its own experimental confirmation through the same
        // hook `mounts add` uses, and `--yes` would skip it.
        yes: false,
    };
    let pre_answers = match kind {
        MountKind::Filesystem => MountsAddKind::Filesystem {
            common,
            vault_path: None,
        },
        MountKind::Couchdb => MountsAddKind::Couchdb {
            common,
            url: None,
            database: None,
            username: None,
            password_stdin: false,
            writable: false,
            e2ee: false,
            sidecar_path: None,
        },
        MountKind::Algolia => MountsAddKind::Algolia {
            common,
            app_id: None,
            index_name: None,
            base_url: None,
            api_key_stdin: false,
            writable: false,
            participant_id: None,
        },
    };
    resolve(&pre_answers, Some(&mut io.answers), Depth::Everything)
}

// ---------------------------------------------------------------------------
// Adding one mount
// ---------------------------------------------------------------------------

/// Append one mount to the working config: the `mounts add` sequence, then the verdict.
///
/// Reuses [`crate::mounts_cmd::append_mount`] rather than restating it, so the legacy
/// migration, the experimental confirmation, the full-table revalidation, the credential
/// storage and the probe are the same code with the same ordering in both commands. What
/// this adds is the wizard's answer to a failed probe: `mounts add` was told by
/// `--keep-anyway` before it started, while the wizard can ask now that there is a verdict
/// to show.
async fn append_and_gate(
    working: &mut PersistedServiceConfig,
    config_path: &Path,
    spec: &MountSpec,
    allow_empty_base: bool,
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
    state: &mut WizardState,
) -> Result<()> {
    let existing = working.clone();
    let WizardIo {
        answers,
        secrets,
        resolver,
        prefer_os_keyring,
    } = io;
    let mut confirm = |question: &str| answers.yes_no(question, false);
    let mut mount_io = crate::mounts_cmd::MountIo {
        resolver,
        prefer_os_keyring: *prefer_os_keyring,
        secrets,
        confirm: &mut confirm,
    };
    let appended = crate::mounts_cmd::append_mount(
        &crate::mounts_cmd::AppendRequest {
            existing: &existing,
            config_path,
            // Deliberately NOT `options.index_dir`. On `mounts add` that global flag sets
            // the NEW MOUNT's own `indexDir`; here it keeps the meaning it has for
            // `setup-service`, the config's TOP-LEVEL one, which is applied once in
            // `finish_with_mount_table`. Forwarding it per mount would give every mount the
            // same index directory and make them overwrite each other's index.
            index_dir: None,
            spec,
            allow_empty_base,
            dry_run: request.dry_run,
        },
        &mut mount_io,
    )
    .await?;

    // Recorded BEFORE the probe is judged, so a rollback covers what was stored even when
    // the very next line aborts.
    state.stored.extend(appended.stored.iter().cloned());
    state.secret_refs.extend(appended.secret_refs.clone());
    state
        .experimental_enabled
        .extend(appended.experimental_enabled.clone());
    state.messages.extend(appended.messages.clone());

    let probe = appended.probe.clone();
    if !probe.ok && !request.dry_run {
        println!(
            "  probe FAILED for mount '{}' ({}): {}",
            appended.mount.id, probe.kind, probe.verdict
        );
        if !answers.yes_no(
            "That usually means a typo in the settings above. Add the mount anyway? (`doctor \
             --probe-remote` will report it degraded until it can be reached)",
            false,
        )? {
            bail!(
                "aborted: mount '{}' did not pass its probe ({}: {}), and it was not kept. \
                 Nothing was written.",
                appended.mount.id,
                probe.kind,
                probe.verdict
            );
        }
        state.messages.push(format!(
            "kept mount '{}' despite a failed probe ({}: {}); `doctor --probe-remote` will \
             report it degraded until it can be reached",
            appended.mount.id, probe.kind, probe.verdict
        ));
    } else if !request.dry_run {
        println!(
            "  probe ok for mount '{}' ({}): {}",
            appended.mount.id, probe.kind, probe.verdict
        );
        state
            .messages
            .push(format!("probe {}: ok — {}", probe.kind, probe.verdict));
    }
    state.probes.push((appended.mount.id.clone(), probe));

    // Fold the validated table back in, so the NEXT addition validates against it — the
    // "full-table revalidation per addition" the mount table's invariants depend on.
    *working =
        crate::mounts_cmd::candidate_config(&existing, &appended.mounts, &appended.experimental);
    Ok(())
}

// ---------------------------------------------------------------------------
// Screen 3: embeddings
// ---------------------------------------------------------------------------

/// Ask whether to enable embedding-backed semantic search, and where.
fn ask_embeddings(
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
    existing: Option<&PersistedServiceConfig>,
    state: &mut WizardState,
) -> Result<EmbeddingAnswers> {
    let configured = existing.and_then(|config| config.embedding.clone());
    let already_on = configured
        .as_ref()
        .is_some_and(|embedding| embedding.provider.is_some() || embedding.model.is_some());

    // One honest line, and only one. Semantic search is the feature people most often
    // believe they have already; saying what happens WITHOUT it is the difference between an
    // informed "no" and a surprise six weeks later.
    println!(
        "Semantic search needs an embedding endpoint. Without one, search stays lexical \
         (results report recallMode: lexical) — an Algolia mount keeps its own native recall \
         either way."
    );
    if !io.answers.yes_no("Enable embeddings?", already_on)? {
        return Ok(EmbeddingAnswers {
            enabled: false,
            model: None,
            base_url: None,
            api_key_ref: None,
        });
    }

    let preset = io.answers.choice(
        "Which endpoint?",
        &[
            "Ollama, running on this machine         (recommended)",
            "Another OpenAI-compatible endpoint",
        ],
        0,
    )?;
    let (default_model, default_base_url) = if preset == 0 {
        (Some(OLLAMA_MODEL), Some(OLLAMA_BASE_URL))
    } else {
        (None, None)
    };
    let model = io.answers.optional_line(
        "Embedding model",
        configured
            .as_ref()
            .and_then(|embedding| embedding.model.as_deref())
            .or(default_model),
    )?;
    let base_url = io.answers.optional_line(
        "Embedding base URL",
        configured
            .as_ref()
            .and_then(|embedding| embedding.base_url.as_deref())
            .or(default_base_url),
    )?;

    // Blank is a real answer, and the common one: a local Ollama needs no key at all.
    let api_key = io
        .secrets
        .next_optional("Embedding API key (blank for none)")?;
    let api_key_ref = match api_key {
        None => None,
        Some(_) if request.dry_run => Some(SecretRef::OsKeyring {
            service: SECRET_SERVICE.to_string(),
            account: EMBEDDING_KEY_ACCOUNT.to_string(),
        }),
        Some(key) => Some(store_embedding_key(io, key, state)?),
    };

    Ok(EmbeddingAnswers {
        enabled: true,
        model,
        base_url,
        api_key_ref,
    })
}

/// Store the embedding API key, preferring the OS keyring and falling back to the encrypted
/// file with a reported message.
///
/// The fallback is automatic rather than a prompt, unlike the wizard's previous behaviour. An
/// unavailable keyring is not a decision the operator can usefully make in the middle of a
/// setup — the only alternative to the encrypted file is "no semantic search" — and it is the
/// same rule `mounts add` already applies to a mount's credential, so one keyring outage now
/// produces one behaviour rather than two.
fn store_embedding_key(
    io: &mut WizardIo,
    key: SecretString,
    state: &mut WizardState,
) -> Result<SecretRef> {
    let fallback = SecretRef::EncryptedFile {
        id: EMBEDDING_KEY_ACCOUNT.to_string(),
    };
    let keyring = SecretRef::OsKeyring {
        service: SECRET_SERVICE.to_string(),
        account: EMBEDDING_KEY_ACCOUNT.to_string(),
    };
    let reference = if io.prefer_os_keyring {
        match io.resolver.put(&keyring, key.clone()) {
            Ok(()) => keyring,
            Err(error) => {
                state.messages.push(format!(
                    "OS keyring unavailable ({error}); storing the embedding API key in the \
                     encrypted secrets file instead"
                ));
                io.resolver
                    .put(&fallback, key)
                    .map_err(|error| anyhow!("failed to store the embedding API key: {error}"))?;
                fallback
            }
        }
    } else {
        io.resolver
            .put(&fallback, key)
            .map_err(|error| anyhow!("failed to store the embedding API key: {error}"))?;
        fallback
    };
    state.stored.push(reference.clone());
    state.secret_refs.push(describe_secret_ref(&reference));
    state.messages.push(format!(
        "stored credential at {} (the config holds this reference only)",
        describe_secret_ref(&reference)
    ));
    Ok(reference)
}

// ---------------------------------------------------------------------------
// Screen 4: transport
// ---------------------------------------------------------------------------

/// Ask how the agent reaches the server.
fn ask_transport(
    io: &mut WizardIo,
    existing: Option<&PersistedServiceConfig>,
) -> Result<TransportAnswers> {
    let configured = existing.and_then(|config| config.transport);
    let default = usize::from(matches!(configured, Some(TransportMode::Http)));
    let choice = io.answers.choice(
        "How will your agent reach the server?",
        &[
            "stdio — your MCP client launches it on demand   (simplest)",
            "HTTP  — a long-lived local service",
        ],
        default,
    )?;
    if choice == 0 {
        return Ok(TransportAnswers {
            transport: TransportMode::Stdio,
            port: None,
            // Not `Some(false)`: see the field's own comment. Leaving auth alone is not the
            // same as turning it off, and only one of the two deletes a stored token.
            auth: None,
        });
    }

    let configured_port = existing
        .and_then(|config| config.http.as_ref().and_then(|http| http.port))
        .unwrap_or(4100);
    let port = io
        .answers
        .line("Port", Some(&configured_port.to_string()))?
        .parse::<u16>()
        .map_err(|error| anyhow!("that is not a port number: {error}. Nothing was written."))?;
    let auth_on = existing
        .and_then(|config| config.auth.as_ref().and_then(|auth| auth.enabled))
        .unwrap_or(false);
    let auth = io.answers.yes_no(
        "Enable HTTP bearer authentication? (not needed on loopback; REQUIRED before you \
         expose the port beyond this machine)",
        auth_on,
    )?;
    Ok(TransportAnswers {
        transport: TransportMode::Http,
        port: Some(port),
        auth: Some(auth),
    })
}

// ---------------------------------------------------------------------------
// Screen 5: the extras
// ---------------------------------------------------------------------------

/// Offer the three non-config installs, skipping the ones that cannot work here.
fn ask_extras(
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
    transport: &TransportAnswers,
    local_root: Option<&Path>,
    state: &mut WizardState,
) -> Result<InstallChoices> {
    // The MCP installers write an HTTP URL — a `url` entry for Codex, `claude mcp add
    // --transport http` for Claude Code. There is no stdio shape for either, so on stdio the
    // offer is SKIPPED with the reason rather than answered: writing a URL nothing will ever
    // serve is worse than saying it cannot be done from here.
    let mcp = if matches!(transport.transport, TransportMode::Stdio) {
        let note = "MCP client registration skipped: the installers register an HTTP URL, and \
                    a stdio server is launched by the client itself. Add it to your client as \
                    a command instead — USAGE.md has the Codex snippet.";
        if request.installs.mcp {
            println!("{note}");
        }
        state.messages.push(note.to_string());
        false
    } else {
        request.installs.mcp
            || io.answers.yes_no(
                "Register this server with your local agents? (Codex, Claude Code)",
                false,
            )?
    };
    let skills = request.installs.skills
        || io
            .answers
            .yes_no("Install the packaged agent skills?", false)?;
    // Offered only for a local root: the snippets are files Obsidian reads out of
    // `<vault>/.obsidian/snippets`, and a remote vault has no such folder on this machine.
    let vault_snippets = if local_root.is_some() {
        request.installs.vault_snippets
            || io
                .answers
                .yes_no("Install the Obsidian CSS snippets into the vault?", false)?
    } else {
        let note = "Obsidian snippets skipped: this vault has no local folder, and the \
                    snippets live in `<vault>/.obsidian/snippets` on each syncing device. \
                    Install them into the local vault of each device instead.";
        if request.installs.vault_snippets {
            println!("{note}");
        }
        state.messages.push(note.to_string());
        false
    };
    Ok(InstallChoices {
        mcp,
        skills,
        vault_snippets,
    })
}

// ---------------------------------------------------------------------------
// Screen 6: recap, write, report
// ---------------------------------------------------------------------------

/// Everything screen 6 needs, bundled so [`finish`] takes four parameters instead of ten.
struct FinishInputs {
    config_path: PathBuf,
    existing: Option<PersistedServiceConfig>,
    /// The config as screens 1 and 2 left it: root vault and mount table, nothing else.
    working: PersistedServiceConfig,
    local_root: Option<PathBuf>,
    embeddings: EmbeddingAnswers,
    transport: TransportAnswers,
    installs: InstallChoices,
}

/// Show the config, confirm it, write it, then report on it.
async fn finish(
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
    state: &mut WizardState,
    inputs: FinishInputs,
) -> Result<SetupServiceReport> {
    // A declared mount table is the one thing `setup_service` will not write, so the two
    // shapes take two paths. The local-root-only shape goes through `setup_service` exactly
    // as it always has, which is what keeps a plain first install byte-identical to what
    // the flag-driven command produces — index-dir default, vault validation, macOS
    // access preflight, existing-file backup and all.
    let declares_mounts = inputs
        .working
        .mounts
        .as_ref()
        .is_some_and(|mounts| !mounts.is_empty());

    if declares_mounts {
        finish_with_mount_table(request, io, state, inputs).await
    } else {
        finish_through_setup_service(request, io, state, inputs).await
    }
}

/// Print the recap and ask for the one confirmation that authorizes a write.
///
/// Rendered with the same `print-config` machinery `deep-obsidian-mcp print-config` uses, and
/// REDACTED — which on this config model is the identity, because a config stores secret
/// references and never secrets. That is the property worth showing an operator: the file
/// they are about to accept has their credentials' addresses in it, not their credentials.
fn recap_and_confirm(
    io: &mut WizardIo,
    config_path: &Path,
    persisted: &PersistedServiceConfig,
    state: &WizardState,
) -> Result<()> {
    println!();
    println!(
        "This is the configuration that will be written to {}:",
        config_path.display()
    );
    println!();
    let text = render_config_text(config_path, &crate::commands::redact_config(persisted))?;
    for line in text.lines() {
        println!("  {line}");
    }
    println!();
    if !state.secret_refs.is_empty() {
        println!("Credentials are stored outside this file, at:");
        for reference in &state.secret_refs {
            println!("  {reference}");
        }
        println!();
    }
    if !io.answers.yes_no("Write it?", true)? {
        bail!("aborted at the recap: nothing was written.");
    }
    Ok(())
}

/// Close out a LOCAL-root, single-vault install through `setup_service`.
async fn finish_through_setup_service(
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
    state: &mut WizardState,
    inputs: FinishInputs,
) -> Result<SetupServiceReport> {
    let FinishInputs {
        config_path,
        local_root,
        embeddings,
        transport,
        installs,
        ..
    } = inputs;
    let vault_path = local_root.expect("a config with no mount table has a local root");
    let mut options = request.options.clone();
    options.config = Some(config_path.clone());
    // Absolutized here so the recap names the path `setup_service` will persist; its own
    // `absolute_path` call then has nothing left to change.
    options.vault_path = Some(crate::commands::absolute_path(&vault_path)?);
    apply_embedding_options(&mut options, &embeddings);
    options.transport = Some(match transport.transport {
        TransportMode::Stdio => CliTransport::Stdio,
        TransportMode::Http => CliTransport::Http,
    });
    if let Some(port) = transport.port {
        options.port = Some(port);
    }

    let mut resolved = crate::config::resolve_runtime_config(&options)?;
    resolved.service.embedding.api_key_ref = embeddings.api_key_ref.clone();

    // The recap is built from the SAME resolved config `setup_service` is about to be handed,
    // with the two rewrites that command applies on a write mirrored onto the preview: the
    // index-dir default (shared helper, so it cannot drift) and the auth flag. Anything else
    // it changes would be a recap that lied.
    let mut preview = resolved.service.clone();
    crate::commands::apply_packaged_index_default(
        &mut preview,
        options.vault_path.as_deref().expect("just set"),
        resolved.sources.index_dir,
    );
    if let Some(enabled) = transport.auth {
        preview.auth.enabled = enabled;
    }
    let mut persisted = deep_obsidian_config::to_persisted_config(&preview);
    deep_obsidian_config::carry_unknown_fields(&mut persisted, resolved.config_file.as_ref());
    recap_and_confirm(io, &config_path, &persisted, state)?;

    // Two things `setup_service` reads off one `overwrite` flag, and the wizard needs
    // different answers for them:
    //
    // * The CONFIG write must proceed. The recap confirmation is a specific, informed "write
    //   this file", so refusing because the file already exists would read as a bug — and the
    //   previous file still goes to `.bak`, so the answer stays recoverable.
    // * An existing MCP entry, skill or snippet must NOT be silently replaced. Nobody agreed
    //   to that; `--overwrite` is how they would.
    //
    // So the installers are run HERE, with `request.overwrite`, and `setup_service` is asked
    // for none of them — which also makes this path structurally the same as the mount-table
    // one below, where the installers were always the wizard's own to run.
    let report = crate::commands::setup_service(&crate::commands::SetupServiceRequest {
        resolved: &resolved,
        dry_run: request.dry_run,
        overwrite: true,
        installs: InstallChoices::default(),
        enable_auth: transport.auth,
        interactive_auth: true,
        // The wizard's OWN store, not the process default. This path used to be the one
        // place a wizard run reached the real login keychain — the mount-table path below
        // has always provisioned through `io.resolver` — so a test of "accept HTTP auth"
        // could only be written against a remote root. See `WizardIo::prefer_os_keyring`.
        auth_store: if io.prefer_os_keyring {
            crate::commands::AuthStore::Default
        } else {
            crate::commands::AuthStore::Injected(&io.resolver)
        },
    })?;
    let (mcp, skills, vault_snippets) = crate::commands::run_installers(
        &report.endpoints,
        resolved.service.vault_path.as_deref(),
        installs,
        request.dry_run,
        request.overwrite,
    )?;
    let report = SetupServiceReport {
        messages: state
            .messages
            .iter()
            .cloned()
            .chain(report.messages.iter().cloned())
            .collect(),
        mcp,
        skills,
        vault_snippets,
        ..report
    };
    close_out(&report, state, &resolved.service, request.dry_run).await;
    Ok(report)
}

/// Close out an install whose config declares a mount table.
///
/// `setup_service` will not write such a file — a mount table is the one thing it cannot
/// reproduce faithfully — so the write happens here, through the same
/// `write_config_with_backup` the `mounts` family uses, and the non-config installers are
/// reached through the same `run_installers` helper `setup_service` calls.
async fn finish_with_mount_table(
    request: &WizardRequest<'_>,
    io: &mut WizardIo,
    state: &mut WizardState,
    inputs: FinishInputs,
) -> Result<SetupServiceReport> {
    let FinishInputs {
        config_path,
        existing,
        working,
        local_root,
        embeddings,
        transport,
        installs,
    } = inputs;
    let mut candidate = working;
    candidate.transport = Some(transport.transport);
    candidate.stdio_mode = candidate.stdio_mode.or(Some(StdioMode::Auto));
    if let Some(port) = transport.port {
        let mut http = candidate.http.clone().unwrap_or(HttpConfigInput {
            host: None,
            port: None,
            mcp_path: None,
            health_path: None,
        });
        http.port = Some(port);
        candidate.http = Some(http);
    }
    if embeddings.enabled {
        let previous = candidate.embedding.clone().unwrap_or_default();
        candidate.embedding = Some(EmbeddingConfigInput {
            provider: Some(EmbeddingProvider::OpenAiCompatible),
            model: embeddings.model.clone(),
            base_url: embeddings.base_url.clone(),
            api_key_ref: embeddings.api_key_ref.clone(),
            ..previous
        });
    }
    if let Some(index_dir) = request.options.index_dir.clone() {
        candidate.index_dir = Some(index_dir);
    }
    if let Some(enabled) = transport.auth {
        let mut auth = candidate.auth.clone().unwrap_or(AuthConfigInput {
            enabled: None,
            token_ref: None,
            allowed_origins: None,
        });
        auth.enabled = Some(enabled);
        candidate.auth = Some(auth);
    }

    // Re-validated as a whole, immediately before the recap, through the SAME loader the
    // server runs at startup. Screens 1 and 2 validated the mount table; the transport,
    // embeddings and auth answers landed afterwards, so this is the first moment the whole
    // document exists — and the object it returns is the one that gets persisted, so the
    // recap and the bytes cannot disagree.
    let mut resolved = crate::mounts_cmd::validate(candidate, &config_path)?;

    // Auth is provisioned BEFORE the recap only to the extent of its flag; the token itself
    // is generated below, after the confirmation, so a declined recap never prints one.
    let mut preview = resolved.clone();
    if let Some(enabled) = transport.auth {
        preview.auth.enabled = enabled;
    }
    let persisted_preview = crate::mounts_cmd::persist(
        &preview,
        existing
            .as_ref()
            .unwrap_or(&PersistedServiceConfig::default()),
    );
    recap_and_confirm(io, &config_path, &persisted_preview, state)?;

    match transport.auth {
        Some(true) => crate::commands::provision_auth_token(
            &mut resolved.auth,
            request.dry_run,
            true,
            &io.resolver,
            io.prefer_os_keyring,
        )?,
        Some(false) => crate::commands::deprovision_auth_token(
            &mut resolved.auth,
            request.dry_run,
            &io.resolver,
        ),
        None => {}
    }

    let persisted = crate::mounts_cmd::persist(
        &resolved,
        existing
            .as_ref()
            .unwrap_or(&PersistedServiceConfig::default()),
    );
    let endpoints = crate::commands::endpoint_report(&build_service_endpoints(&resolved));
    let mut messages = state.messages.clone();
    messages.insert(0, format!("vault: {}", resolved.root_location()));
    messages.insert(1, format!("index: {}", resolved.index_dir.display()));
    messages.insert(2, format!("config: {}", config_path.display()));

    let written = if request.dry_run {
        crate::commands::assert_creatable_directory(
            config_path.parent().unwrap_or(Path::new(".")),
        )?;
        messages.push("dry-run: config validated but not written".to_string());
        false
    } else {
        let backup_path = crate::commands::write_config_with_backup(&config_path, &persisted)?;
        if let Some(backup_path) = backup_path {
            messages.push(format!(
                "backed up previous config: {}",
                backup_path.display()
            ));
        }
        messages.push(format!("wrote config: {}", config_path.display()));
        true
    };

    let (mcp, skills, vault_snippets) = crate::commands::run_installers(
        &endpoints,
        local_root.as_deref(),
        installs,
        request.dry_run,
        // `--overwrite` reaches the installers only when it was asked for: the recap
        // confirmation authorized the CONFIG write, not the replacement of an agent's
        // existing MCP entry.
        request.overwrite,
    )?;

    let report = SetupServiceReport {
        config_file_path: config_path,
        written,
        dry_run: request.dry_run,
        endpoints,
        persisted_config: persisted,
        messages,
        mcp,
        skills,
        vault_snippets,
    };
    close_out(&report, state, &resolved, request.dry_run).await;
    Ok(report)
}

/// Apply the embedding answers to the flags `resolve_runtime_config` reads.
fn apply_embedding_options(options: &mut ServiceOptions, embeddings: &EmbeddingAnswers) {
    if !embeddings.enabled {
        return;
    }
    // The only provider this build has. Named rather than left to be inferred so the written
    // config says what it is instead of relying on the model field to imply it.
    options.embedding_provider = Some("openai-compatible".to_string());
    if let Some(model) = &embeddings.model {
        options.embedding_model = Some(model.clone());
    }
    if let Some(base_url) = &embeddings.base_url {
        options.embedding_base_url = Some(base_url.clone());
    }
}

/// The closing report: the local `doctor` checks, the probe verdicts already in hand, and
/// what to do next.
///
/// The remote mounts are deliberately NOT re-probed. They were contacted a few questions ago
/// and the verdicts are recorded in [`WizardState::probes`]; contacting them again would
/// double the setup's side effects on the operator's servers to tell them something they
/// have already been told.
async fn close_out(
    report: &SetupServiceReport,
    state: &WizardState,
    service: &ResolvedServiceConfig,
    dry_run: bool,
) {
    println!();
    println!("{}", crate::commands::render_setup_service_report(report));

    let runtime = crate::config::ResolvedRuntimeConfig {
        config_path: report.config_file_path.clone(),
        config_file: Some(report.persisted_config.clone()),
        service: service.clone(),
        sources: Default::default(),
    };
    println!();
    println!("Checks:");
    match crate::commands::doctor(&runtime, 2_000, false).await {
        Ok(doctor) => {
            for check in &doctor.checks {
                println!("  [{}] {} — {}", check.status, check.name, check.message);
            }
        }
        // A failed doctor is not a failed setup: the config is written and the operator can
        // run `doctor` themselves. Saying so beats propagating an error that would read as
        // "the setup failed".
        Err(error) => {
            println!("  could not run the local checks ({error}); run `deep-obsidian-mcp doctor`")
        }
    }
    for (mount, probe) in &state.probes {
        // A dry run reports `skipped`: there is no verdict to carry, and printing one would
        // claim a mount had been contacted when nothing was.
        if probe.kind == "skipped" {
            continue;
        }
        println!(
            "  [{}] mount.{mount}.remote — {} (probed during setup; not re-contacted)",
            if probe.ok { "ok" } else { "warn" },
            probe.verdict
        );
    }

    println!();
    println!("Next steps:");
    if dry_run {
        println!("  * this was a --dry-run: re-run without it to write the config");
    }
    match service.transport {
        TransportMode::Stdio => println!(
            "  * your MCP client launches the server; point it at this binary (USAGE.md \
             \"Connect your agent\")"
        ),
        TransportMode::Http => {
            println!("  * start the service: `brew services start deep-obsidian-mcp` (macOS) or");
            println!("    `systemctl --user enable --now deep-obsidian-mcp` (Linux)");
        }
    }
    println!("  * check it: `deep-obsidian-mcp doctor`");
    println!(
        "  * try a search: ask your agent to search the vault, or run `deep-obsidian-mcp probe`"
    );
    if !state.probes.is_empty() {
        println!("  * inspect the mount table: `deep-obsidian-mcp mounts list`");
    }
}
