use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::iter;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use deep_obsidian_config::{
    build_service_endpoints, carry_unknown_fields, default_mount_index_dir,
    default_packaged_index_dir, render_config_text, secrets::SecretResolver, to_persisted_config,
    write_config_file,
};
use deep_obsidian_server::{run_http_service, run_stdio_service};
use deep_obsidian_types::{
    MountBackendConfig, MountConfig, PersistedServiceConfig, ResolvedServiceConfig, SecretRef,
    ServiceEndpoints, TransportMode,
};
use reqwest::Client;
use rusqlite::{Connection, OpenFlags};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde_json::{json, Value};

use crate::cli::{Cli, Command, ServiceOptions};
use crate::config::{ResolvedRuntimeConfig, ResolvedSource, ResolvedSources};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const INDEX_SQLITE_FILENAME: &str = "index.sqlite";
const CONFIG_PRECEDENCE: [&str; 4] = ["cli", "config", "env", "default"];
const HELP_TEXT: &str = "\
Usage:
  deep-obsidian-mcp [serve] [--config <path>] [--vault <path>] [--transport stdio|http] [--packaged]
  deep-obsidian-mcp setup-service --vault <path> [--config <path>] [--mcp] [--skills] [--vault-snippets] [--auth|--no-auth] [--dry-run]
  deep-obsidian-mcp setup-service --wizard [--config <path>] [--dry-run]
  deep-obsidian-mcp doctor [--config <path>] [--json] [--probe-remote]
  deep-obsidian-mcp print-config [--config <path>]
  deep-obsidian-mcp probe [--config <path>] [--json]
  deep-obsidian-mcp couchdb export --mount <id> --out <dir> [--config <path>] [--json]
  deep-obsidian-mcp couchdb restore --mount <id> --from <dir> [--dry-run] [--force] [--json]
  deep-obsidian-mcp algolia seed --mount <id> [--from <folder>] [--dry-run] [--move] [--json]
  deep-obsidian-mcp algolia dump --mount <id> --out <dir> [--json]
  deep-obsidian-mcp algolia restore --mount <id> --from <dir> [--dry-run] [--force] [--json]
  deep-obsidian-mcp algolia status --mount <id> [--json]
  deep-obsidian-mcp algolia retract --mount <id> --path <note> [--yes] [--json]
  deep-obsidian-mcp algolia key --mount <id> [--parent-key-ref <ref>] [--prefix <folder>] [--json]

Commands:
  serve          Start the MCP server using resolved config.
  setup-service  Validate service config and optionally install MCP client entries, skills, or vault snippets.
  doctor         Diagnose config, vault access, dependencies, and health.
  print-config   Print the normalized persisted config.
  probe          Probe the configured HTTP health and MCP endpoints.
  couchdb        Snapshot (export) and restore a CouchDB (Self-hosted LiveSync) mount.
  algolia        Seed, dump, restore, inspect and scope an Algolia-backed shared corpus.
  help           Show this help.
  version        Print the current version.

doctor prints one block of checks per declared mount. The local ones always run and
contact nothing: a filesystem mount's directory, and for a couchdb mount whether the
LiveSync sidecar bundle was located and whether a Node >= 20 is present. --probe-remote
additionally contacts each couchdb and algolia mount READ-ONLY -- one handshake or one
getSettings, no data method -- which is opt-in because it needs credentials and network.
A remote-backed mount that cannot be reached is a warn, never a fail: those mounts are
experimental and non-root, so the vault root keeps serving without them.

setup-service does NOT rewrite a config that declares a mount table, with or without
--overwrite, and refuses an auth change on one. Edit such a file by hand; --mcp,
--skills and --vault-snippets still work. A content-changing write of an ordinary config
leaves the previous file at config.json.bak.

couchdb export writes every entry of one mount to a directory, plus a manifest.json
recording each entry's revision, content hash and storage kind. Two exports of an
unchanged vault are byte-identical, so `diff -r` (or the reported tree hash) verifies
a round trip.

couchdb restore writes such a directory back through the same revision-guarded write
path the MCP tools use. It creates missing entries, skips identical ones, and REFUSES
entries whose remote content differs unless --force is given -- so the default cannot
discard an edit made after the export. --dry-run reports exactly what a real run would
do and works on a read-only mount.

algolia seed imports a local folder into the mount's index ONCE. It defaults to the
folder the mount shadows (<vaultPath>/<mountAt>), creates and updates only, and never
deletes a note from the index to match the source. --move deletes each local original
only after re-reading the index and confirming it holds those exact bytes.

algolia dump writes every note to a directory plus a deterministic manifest.json; two
dumps of an unchanged corpus are byte-identical. algolia restore writes such a tree
back through the guarded write path: creates, skips identical, and REFUSES notes whose
index content differs unless --force -- and even then nothing is destroyed, the current
version moves to history. Non-.md files are refused outright: the corpus stores Markdown
only, and no flag lifts that.

algolia retract permanently deletes a note AND its whole history. It is the one
destructive operation here, it prompts unless --yes, and it is deliberately not an MCP
tool. algolia key derives a scoped read-only secured key for a teammate; it REFUSES a
write-capable parent key, because a secured key inherits its parent's ACLs while its
filter restriction constrains search only.";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointReport {
    pub mcp: String,
    pub health: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDiagnostics {
    pub path: PathBuf,
    pub exists: bool,
    pub precedence: Vec<&'static str>,
    pub sources: ResolvedSources,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoReindexDiagnostics {
    pub enabled: bool,
    pub debounce_ms: u64,
    pub interval_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingDiagnostics {
    pub configured: bool,
    pub active: bool,
    pub backend: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexDiagnostics {
    pub path: PathBuf,
    pub exists: bool,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_snapshot_rows: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDiagnostics {
    pub auto_reindex: AutoReindexDiagnostics,
    pub embedding: EmbeddingDiagnostics,
    pub endpoint: EndpointReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupServiceReport {
    pub config_file_path: PathBuf,
    pub written: bool,
    pub dry_run: bool,
    pub endpoints: EndpointReport,
    pub persisted_config: PersistedServiceConfig,
    pub messages: Vec<String>,
    pub mcp: Vec<SetupActionReport>,
    pub skills: Vec<SetupActionReport>,
    pub vault_snippets: Vec<SetupActionReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupActionReport {
    pub target: String,
    pub path: Option<PathBuf>,
    pub changed: bool,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckReport {
    pub name: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub config: PersistedServiceConfig,
    pub config_diagnostics: ConfigDiagnostics,
    pub endpoints: EndpointReport,
    pub index: IndexDiagnostics,
    pub service: ServiceDiagnostics,
    pub checks: Vec<CheckReport>,
    pub ok: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintConfigReport {
    pub config_path: PathBuf,
    pub config: PersistedServiceConfig,
    pub text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthProbeReport {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpProbeReport {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_tool: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_info: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub endpoints: EndpointReport,
    pub health: HealthProbeReport,
    pub mcp: McpProbeReport,
}

#[derive(Debug, Serialize)]
pub struct ServeReport {
    pub message: String,
    pub endpoints: EndpointReport,
}

pub async fn run() -> Result<()> {
    let raw_args = env::args().skip(1).collect::<Vec<_>>();
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP_TEXT}");
        return Ok(());
    }
    if raw_args.len() == 1 && matches!(raw_args[0].as_str(), "--version" | "-v") {
        println!("{VERSION}");
        return Ok(());
    }

    let normalized_args = normalize_cli_args(&raw_args)?;
    let cli = Cli::parse_from(iter::once("deep-obsidian-mcp".to_string()).chain(normalized_args));
    let json = cli.options.json && !cli.options.no_json;
    let dry_run = cli.options.dry_run && !cli.options.no_dry_run;

    if cli.options.insecure_no_auth {
        // The bootstrap reads this env var as the fail-closed escape hatch.
        std::env::set_var("DEEP_OBSIDIAN_ALLOW_INSECURE", "1");
    }

    match cli.command.unwrap_or(Command::Serve) {
        Command::Help => {
            println!("{HELP_TEXT}");
            Ok(())
        }
        Command::Version => {
            println!("{VERSION}");
            Ok(())
        }
        Command::Serve => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            serve(&resolved).await?;
            Ok(())
        }
        Command::SetupService {
            overwrite,
            wizard,
            mcp,
            skills,
            vault_snippets,
            auth,
            no_auth,
        } => {
            // `--no-auth` disables (and deletes the token); `--auth` enables and
            // provisions one; without either, auth is left as configured (off for
            // a fresh config). `--no-auth` wins if both are passed.
            let auth_choice = if no_auth {
                Some(false)
            } else if auth {
                Some(true)
            } else {
                None
            };
            let report = if wizard {
                setup_service_wizard(
                    &cli.options,
                    dry_run,
                    overwrite,
                    mcp,
                    skills,
                    vault_snippets,
                )?
            } else {
                let resolved = crate::config::resolve_runtime_config(&cli.options)?;
                setup_service(
                    &resolved,
                    dry_run,
                    overwrite,
                    mcp,
                    skills,
                    vault_snippets,
                    auth_choice,
                    false,
                )?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", render_setup_service_report(&report));
            }
            Ok(())
        }
        Command::Doctor {
            probe_timeout_ms,
            probe_remote,
        } => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            let report = doctor(&resolved, probe_timeout_ms, probe_remote).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", render_doctor_report(&report));
            }
            if report.ok {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Command::PrintConfig { no_redact } => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            let report = print_config(&resolved, !no_redact)?;
            println!("{}", report.text);
            Ok(())
        }
        Command::Couchdb { command } => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            match command {
                crate::cli::CouchdbCommand::Export { mount, out } => {
                    let report =
                        crate::couchdb_transfer::export(&resolved.service, &mount, &out).await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        println!("{}", crate::couchdb_transfer::render_export_report(&report));
                    }
                    Ok(())
                }
                crate::cli::CouchdbCommand::Restore {
                    mount,
                    from,
                    dry_run: restore_dry_run,
                    force,
                } => {
                    // The global `--dry-run` counts too: a user who has learned that it
                    // makes every command harmless must not be surprised here of all
                    // places.
                    let report = crate::couchdb_transfer::restore(
                        &resolved.service,
                        &mount,
                        &from,
                        restore_dry_run || dry_run,
                        force,
                    )
                    .await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        println!(
                            "{}",
                            crate::couchdb_transfer::render_restore_report(&report)
                        );
                    }
                    if report.ok() {
                        Ok(())
                    } else {
                        // A refusal is the tool working, but the operator asked for a
                        // restore and did not fully get one, so it must not look like
                        // success to a script.
                        std::process::exit(1)
                    }
                }
            }
        }
        Command::Algolia { command } => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            run_algolia(&resolved.service, command, dry_run, json).await
        }
        Command::Probe { timeout_ms } => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            let report = probe(&resolved, timeout_ms).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", render_probe_report(&report));
            }
            if report.health.ok && report.mcp.ok {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
    }
}

/// Dispatch the `algolia` family.
///
/// Its own function rather than another arm inline: six subcommands with their own
/// rendering, and the `retract` arm has an interactive gate that must not be lost in a
/// match ladder.
async fn run_algolia(
    config: &ResolvedServiceConfig,
    command: crate::cli::AlgoliaCommand,
    global_dry_run: bool,
    json: bool,
) -> Result<()> {
    use crate::algolia_cmd as algolia;
    use crate::cli::AlgoliaCommand;

    match command {
        AlgoliaCommand::Seed {
            mount,
            from,
            dry_run,
            move_files,
        } => {
            // The GLOBAL `--dry-run` counts too, here as everywhere: a user who has learned
            // that it makes every command harmless must not be surprised by an import.
            let report = algolia::seed(
                config,
                &mount,
                from.as_deref(),
                dry_run || global_dry_run,
                move_files,
            )
            .await?;
            print_report(json, &report, || algolia::render_seed_report(&report))
        }
        AlgoliaCommand::Dump { mount, out } => {
            if global_dry_run {
                println!(
                    "(dry run: would dump mount '{mount}' to {}; nothing was read or written)",
                    out.display()
                );
                return Ok(());
            }
            let report = algolia::dump(config, &mount, &out).await?;
            print_report(json, &report, || algolia::render_dump_report(&report))?;
            // A hash mismatch means a chunk record is missing or duplicated, so the dumped
            // bytes are not what the index claims they are. That is a corrupt snapshot and
            // a script must not read it as a good backup.
            if report.hash_mismatches.is_empty() {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        AlgoliaCommand::Restore {
            mount,
            from,
            dry_run,
            force,
        } => {
            let report =
                algolia::restore(config, &mount, &from, dry_run || global_dry_run, force).await?;
            print_report(json, &report, || algolia::render_restore_report(&report))?;
            if report.ok() {
                Ok(())
            } else {
                // A refusal is the tool working, but the operator asked for a restore and
                // did not fully get one, so it must not look like success to a script.
                std::process::exit(1)
            }
        }
        AlgoliaCommand::Status { mount } => {
            let report = algolia::status(config, &mount).await?;
            print_report(json, &report, || algolia::render_status_report(&report))?;
            if report.reachable {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        AlgoliaCommand::Retract { mount, path, yes } => {
            // Planned first, unconditionally: the confirmation has to be able to say WHAT
            // is about to be destroyed — the note, its head version, and how many versions
            // go with it — and that is only knowable by looking.
            let planned = algolia::retract(config, &mount, &path, true).await?;
            if global_dry_run {
                return print_report(json, &planned, || algolia::render_retract_report(&planned));
            }
            if !yes && !confirm(&algolia::retract_confirmation(&planned))? {
                println!("aborted; nothing was removed");
                return Ok(());
            }
            let report = algolia::retract(config, &mount, &path, false).await?;
            print_report(json, &report, || algolia::render_retract_report(&report))
        }
        AlgoliaCommand::Key {
            mount,
            parent_key_ref,
            prefix,
        } => {
            let parent = algolia::parse_parent_key_ref(&parent_key_ref)?;
            let report = algolia::derive_key(config, &mount, &parent, prefix.as_deref()).await?;
            print_report(json, &report, || {
                algolia::render_derived_key_report(&report)
            })
        }
    }
}

/// Print a report as JSON or as its rendered text.
fn print_report<T: Serialize>(
    json: bool,
    report: &T,
    render: impl FnOnce() -> String,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        println!("{}", render());
    }
    Ok(())
}

/// Ask a yes/no question on the terminal. Anything but an explicit yes is a no.
fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn parse_boolean_like(value: &str, default_value: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default_value,
    }
}

fn normalize_optional_bool_flag(
    args: &[String],
    index: usize,
    flag: &str,
    positive: &str,
    negative: &str,
) -> Result<(String, usize)> {
    let token = &args[index];
    if let Some(value) = token.strip_prefix(&format!("{flag}=")) {
        return Ok((
            if parse_boolean_like(value, true) {
                positive.to_string()
            } else {
                negative.to_string()
            },
            index + 1,
        ));
    }

    if let Some(value) = args.get(index + 1) {
        if !value.starts_with('-') {
            return Ok((
                if parse_boolean_like(value, true) {
                    positive.to_string()
                } else {
                    negative.to_string()
                },
                index + 2,
            ));
        }
    }

    Ok((positive.to_string(), index + 1))
}

fn normalize_required_bool_flag(
    args: &[String],
    index: usize,
    flag: &str,
    positive: &str,
    negative: &str,
) -> Result<(String, usize)> {
    let token = &args[index];
    if let Some(value) = token.strip_prefix(&format!("{flag}=")) {
        return Ok((
            if parse_boolean_like(value, true) {
                positive.to_string()
            } else {
                negative.to_string()
            },
            index + 1,
        ));
    }

    let Some(value) = args.get(index + 1) else {
        return Err(anyhow!("Missing value for {flag}."));
    };
    if value.starts_with('-') {
        return Err(anyhow!("Missing value for {flag}."));
    }

    Ok((
        if parse_boolean_like(value, true) {
            positive.to_string()
        } else {
            negative.to_string()
        },
        index + 2,
    ))
}

/// Whether a bare positional is a SUBCOMMAND rather than a vault path.
///
/// This list is load-bearing, not documentation. `normalize_cli_args` promotes the first
/// unrecognized positional to `--vault <path>` — that is what makes
/// `deep-obsidian-mcp ~/Vault` work — so a subcommand missing from here is SWALLOWED as a
/// vault path, and its own subcommand then reaches clap at the top level and fails with
/// "unrecognized subcommand". Every command in [`Command`](crate::cli::Command) must appear.
///
/// `couchdb` and `algolia` are the two that take nested subcommands, which is exactly why
/// forgetting them is easy: the failure surfaces as a complaint about `export` or `status`
/// rather than about the word that was actually lost.
fn is_known_command(token: &str) -> bool {
    matches!(
        token,
        "serve"
            | "setup-service"
            | "doctor"
            | "print-config"
            | "probe"
            | "couchdb"
            | "algolia"
            | "help"
            | "version"
    )
}

/// Whether this command is followed by a nested subcommand of its own.
///
/// Needed because the positional-vault-path promotion is otherwise indistinguishable from a
/// nested subcommand: `doctor ~/Vault` and `couchdb export` are both "a known command then a
/// bare word", and the second word must be promoted in the first case and kept in the
/// second. So the FIRST positional after one of these is never a vault path.
fn command_takes_subcommand(token: &str) -> bool {
    matches!(token, "couchdb" | "algolia")
}

/// Value-taking flags that belong to a SUBCOMMAND rather than to `ServiceOptions`.
///
/// They need listing for one reason: `normalize_cli_args` has to consume a flag's value, or
/// the value is left looking like a bare positional and gets promoted to `--vault`. The
/// global value flags are already enumerated below; these are the ones `couchdb` and
/// `algolia` add.
///
/// **Anything added to a subcommand that takes a value must be added here.** The failure is
/// silent and confusing — clap complains that the flag has no value, naming the flag whose
/// value was stolen rather than the promotion that stole it.
const SUBCOMMAND_VALUE_FLAGS: &[&str] = &[
    "--mount",
    "--out",
    "--from",
    "--path",
    "--parent-key-ref",
    "--prefix",
];

fn normalize_value_flag(
    args: &[String],
    index: usize,
    flag: &str,
    replacement_flag: &str,
) -> (Vec<String>, usize) {
    let token = &args[index];
    if let Some(value) = token.strip_prefix(&format!("{flag}=")) {
        return (vec![format!("{replacement_flag}={value}")], index + 1);
    }

    let mut normalized = vec![replacement_flag.to_string()];
    if let Some(value) = args.get(index + 1) {
        if !value.starts_with('-') {
            normalized.push(value.clone());
            return (normalized, index + 2);
        }
    }

    (normalized, index + 1)
}

fn normalize_cli_args(raw_args: &[String]) -> Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(raw_args.len() + 2);
    let mut index = 0;
    let mut pending_vault_path: Option<String> = None;
    let mut saw_vault_flag = false;
    // Set by `couchdb` / `algolia`: the next bare word is their subcommand, not a path.
    let mut awaiting_subcommand = false;

    while index < raw_args.len() {
        let token = &raw_args[index];
        if token == "--vault-path" {
            saw_vault_flag = true;
            let (replacement, next_index) =
                normalize_value_flag(raw_args, index, "--vault-path", "--vault");
            normalized.extend(replacement);
            index = next_index;
            continue;
        }
        if let Some(value) = token.strip_prefix("--vault-path=") {
            saw_vault_flag = true;
            normalized.push(format!("--vault={value}"));
            index += 1;
            continue;
        }
        if token == "--vault" || token.starts_with("--vault=") {
            saw_vault_flag = true;
            let (replacement, next_index) =
                normalize_value_flag(raw_args, index, "--vault", "--vault");
            normalized.extend(replacement);
            index = next_index;
            continue;
        }
        if token == "--json" || token.starts_with("--json=") {
            let (replacement, next_index) =
                normalize_optional_bool_flag(raw_args, index, "--json", "--json", "--no-json")?;
            normalized.push(replacement);
            index = next_index;
            continue;
        }
        if token == "--dry-run" || token.starts_with("--dry-run=") {
            let (replacement, next_index) = normalize_optional_bool_flag(
                raw_args,
                index,
                "--dry-run",
                "--dry-run",
                "--no-dry-run",
            )?;
            normalized.push(replacement);
            index = next_index;
            continue;
        }
        if token == "--auto-reindex" || token.starts_with("--auto-reindex=") {
            let (replacement, next_index) = normalize_required_bool_flag(
                raw_args,
                index,
                "--auto-reindex",
                "--auto-reindex",
                "--no-auto-reindex",
            )?;
            normalized.push(replacement);
            index = next_index;
            continue;
        }
        if token == "--version" || token == "-v" {
            index += 1;
            continue;
        }
        // Subcommand value flags, passed through with their VALUE consumed.
        //
        // Consuming the value is the whole point: `--mount wiki` leaves `wiki` as a bare
        // positional otherwise, and the positional-vault-path promotion turns it into
        // `--vault wiki` — so `algolia status --mount wiki` failed with "a value is required
        // for '--mount'" about the flag whose value had just been stolen. Same class of bug
        // as a subcommand missing from `is_known_command`, one level down.
        if let Some(flag) = SUBCOMMAND_VALUE_FLAGS
            .iter()
            .find(|flag| token.as_str() == **flag || token.starts_with(&format!("{flag}=")))
        {
            let (replacement, next_index) = normalize_value_flag(raw_args, index, flag, flag);
            normalized.extend(replacement);
            index = next_index;
            continue;
        }
        if matches!(
            token.as_str(),
            "--config"
                | "--index-dir"
                | "--transport"
                | "--stdio-mode"
                | "--host"
                | "--port"
                | "--mcp-path"
                | "--health-path"
                | "--reindex-debounce-ms"
                | "--reindex-interval-ms"
                | "--embedding-provider"
                | "--embedding-model"
                | "--embedding-base-url"
                | "--probe-timeout-ms"
                | "--timeout-ms"
        ) || token.starts_with("--config=")
            || token.starts_with("--index-dir=")
            || token.starts_with("--transport=")
            || token.starts_with("--stdio-mode=")
            || token.starts_with("--host=")
            || token.starts_with("--port=")
            || token.starts_with("--mcp-path=")
            || token.starts_with("--health-path=")
            || token.starts_with("--reindex-debounce-ms=")
            || token.starts_with("--reindex-interval-ms=")
            || token.starts_with("--embedding-provider=")
            || token.starts_with("--embedding-model=")
            || token.starts_with("--embedding-base-url=")
            || token.starts_with("--probe-timeout-ms=")
            || token.starts_with("--timeout-ms=")
        {
            let (replacement, next_index) = if token.starts_with("--config") {
                normalize_value_flag(raw_args, index, "--config", "--config")
            } else if token.starts_with("--index-dir") {
                normalize_value_flag(raw_args, index, "--index-dir", "--index-dir")
            } else if token.starts_with("--transport") {
                normalize_value_flag(raw_args, index, "--transport", "--transport")
            } else if token.starts_with("--stdio-mode") {
                normalize_value_flag(raw_args, index, "--stdio-mode", "--stdio-mode")
            } else if token.starts_with("--host") {
                normalize_value_flag(raw_args, index, "--host", "--host")
            } else if token.starts_with("--port") {
                normalize_value_flag(raw_args, index, "--port", "--port")
            } else if token.starts_with("--mcp-path") {
                normalize_value_flag(raw_args, index, "--mcp-path", "--mcp-path")
            } else if token.starts_with("--health-path") {
                normalize_value_flag(raw_args, index, "--health-path", "--health-path")
            } else if token.starts_with("--reindex-debounce-ms") {
                normalize_value_flag(
                    raw_args,
                    index,
                    "--reindex-debounce-ms",
                    "--reindex-debounce-ms",
                )
            } else if token.starts_with("--reindex-interval-ms") {
                normalize_value_flag(
                    raw_args,
                    index,
                    "--reindex-interval-ms",
                    "--reindex-interval-ms",
                )
            } else if token.starts_with("--embedding-provider") {
                normalize_value_flag(
                    raw_args,
                    index,
                    "--embedding-provider",
                    "--embedding-provider",
                )
            } else if token.starts_with("--embedding-model") {
                normalize_value_flag(raw_args, index, "--embedding-model", "--embedding-model")
            } else if token.starts_with("--embedding-base-url") {
                normalize_value_flag(
                    raw_args,
                    index,
                    "--embedding-base-url",
                    "--embedding-base-url",
                )
            } else if token.starts_with("--probe-timeout-ms") {
                normalize_value_flag(raw_args, index, "--probe-timeout-ms", "--probe-timeout-ms")
            } else {
                normalize_value_flag(raw_args, index, "--timeout-ms", "--timeout-ms")
            };
            normalized.extend(replacement);
            index = next_index;
            continue;
        }
        if !token.starts_with('-') {
            if is_known_command(token) {
                awaiting_subcommand = command_takes_subcommand(token);
                normalized.push(token.clone());
            } else if awaiting_subcommand {
                // The nested subcommand of `couchdb` / `algolia`, never a vault path.
                awaiting_subcommand = false;
                normalized.push(token.clone());
            } else if !saw_vault_flag && pending_vault_path.is_none() {
                pending_vault_path = Some(token.clone());
            } else {
                normalized.push(token.clone());
            }
            index += 1;
            continue;
        }
        normalized.push(token.clone());
        index += 1;
    }

    if let Some(vault_path) = pending_vault_path {
        normalized.push("--vault".to_string());
        normalized.push(vault_path);
    }

    Ok(normalized)
}

pub fn setup_service(
    resolved: &ResolvedRuntimeConfig,
    dry_run: bool,
    overwrite: bool,
    install_mcp: bool,
    install_skills: bool,
    install_vault_snippets: bool,
    // `Some(true)` enables auth (generates/stores/prints a token), `Some(false)`
    // disables it, `None` leaves the resolved auth config untouched.
    enable_auth: Option<bool>,
    // When true (wizard), prompt before the encrypted-file fallback; when false
    // (flag-driven), fall back automatically.
    interactive_auth: bool,
) -> Result<SetupServiceReport> {
    let mut service = ensure_service_transport_http(resolved.service.clone())?;
    // A DECLARED mount table is never rewritten — but the command no longer refuses
    // outright either, because refusing took `--mcp`, `--skills` and `--vault-snippets`
    // down with it and left the operator of a multi-mount install with no supported way
    // to register the server with their agent.
    //
    // What is skipped, and why it must be:
    //
    // * The config WRITE. `to_persisted_config` omits `vaultPath` when `mounts` is set,
    //   so the vault-path rewrite below would be silently discarded; worse, a write
    //   would re-render the mount table from this build's understanding of it, which is
    //   exactly the clobber the `.bak` logic exists to make recoverable. Not writing at
    //   all is stronger than writing recoverably.
    // * The vault-path ABSOLUTIZE and the packaged index-dir derivation. Both act on
    //   `service.vault_path`, which for a mount table is only the ROOT mount, and
    //   per-mount index dirs are derived by the runtime rather than persisted here.
    // * The macOS TCC PREFLIGHT, which would prompt for one vault and leave the others
    //   silently unapproved — a worse outcome than saying so.
    //
    // Everything that is NOT config — MCP client entries, agent skills, vault snippets,
    // the endpoint report — proceeds normally.
    let declared_mounts = !service.mounts.is_empty();
    let config_path = absolute_path(&resolved.config_path)?;
    let mut vault_access_messages = Vec::new();
    if declared_mounts {
        vault_access_messages.push(format!(
            "mounts config detected ({} mounts): leaving {} untouched. setup-service does not \
             rewrite a declared mount table — edit the file by hand to change mounts, index \
             directories or per-mount settings.",
            service.mounts.len(),
            config_path.display()
        ));
        vault_access_messages.push(
            "skipped: vault-path absolutization, the packaged index-dir default and the macOS \
             vault-access preflight. Each acts on the root mount only, so applying them to a \
             mount table would be misleading; give every mount an absolute vaultPath in the \
             config, and approve vault access per mount if macOS prompts."
                .to_string(),
        );
        if service
            .mounts
            .iter()
            .any(|mount| matches!(mount.backend, MountBackendConfig::Couchdb { .. }))
        {
            // Stated because the obvious expectation is the opposite: most tools that
            // package a helper runtime need an environment variable pointing at it.
            vault_access_messages.push(format!(
                "a couchdb mount is configured: the LiveSync sidecar bundle is found relative to \
                 the installed binary, so no {} is set in any service unit. Run `deep-obsidian-mcp \
                 doctor` to confirm the bundle and a Node >= 20 are present.",
                deep_obsidian_backend::sidecar::SIDECAR_BUNDLE_ENV
            ));
        }
    } else {
        service.vault_path = absolute_path(&service.vault_path)?;
        if matches!(resolved.sources.index_dir, ResolvedSource::Default) {
            service.index_dir = default_packaged_index_dir(&service.vault_path);
        }
        validate_vault(&service)?;
        vault_access_messages = macos_vault_access_preflight(&service.vault_path, dry_run)?;
    }
    let config_dir = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| config_path.clone());

    // An auth change on a mounts config is REFUSED rather than half-applied. Nothing
    // below writes the config for such a table, so provisioning would store a bearer
    // token in the secret store and leave nothing referencing it: `auth.enabled` and
    // `tokenRef` would stay whatever the file already said. The user would come away
    // believing authentication was just turned on (or off) when it was not — a silent
    // security-relevant lie, which is worse than a refusal even though a message is
    // printed either way. `--no-auth` is refused for the mirror-image reason: it would
    // deprovision the stored token while the config kept requiring one, breaking every
    // client with no explanation in the file.
    if declared_mounts && enable_auth.is_some() {
        anyhow::bail!(
            "setup-service cannot change auth on a config that declares a mount table: it does \
             not rewrite such a file, so the token would be stored with nothing referencing it. \
             Edit the `auth` section of {} by hand — `deep-obsidian-mcp print-config` shows the \
             current shape — and put the token in the OS keyring or the encrypted secrets file \
             at {} under the id the `tokenRef` names.",
            config_path.display(),
            deep_obsidian_config::default_secrets_path().display()
        );
    }

    // Apply the auth choice before building the persisted config so it is
    // reflected in both the dry-run preview and the written file. A change here
    // forces a config write below even without `--overwrite`.
    match enable_auth {
        Some(true) => provision_auth_token(&mut service.auth, dry_run, interactive_auth)?,
        Some(false) => deprovision_auth_token(&mut service.auth, dry_run),
        None => {}
    }
    let auth_changed = enable_auth.is_some();

    let mut config = to_persisted_config(&service);
    // A newer build's config keys survive being rewritten by an older binary. Without
    // this, one `setup-service --overwrite` under a downgraded install silently
    // deletes every setting this build has never heard of.
    carry_unknown_fields(&mut config, resolved.config_file.as_ref());
    let mut messages = vec![
        format!("vault: {}", service.vault_path.display()),
        format!("index: {}", service.index_dir.display()),
        format!("config: {}", config_path.display()),
    ];
    messages.extend(vault_access_messages);
    if dry_run {
        if !declared_mounts {
            assert_creatable_directory(&service.index_dir)?;
        }
        assert_creatable_directory(&config_dir)?;
        let endpoints = endpoint_report(&build_service_endpoints(&service));
        let mcp = if install_mcp {
            setup_mcp_clients(&endpoints, true, overwrite)?
        } else {
            Vec::new()
        };
        let skills = if install_skills {
            setup_agent_skills(true, overwrite)?
        } else {
            Vec::new()
        };
        let vault_snippets = if install_vault_snippets {
            setup_vault_snippets(&service.vault_path, true, overwrite)?
        } else {
            Vec::new()
        };
        return Ok(SetupServiceReport {
            config_file_path: config_path,
            written: false,
            dry_run: true,
            endpoints,
            persisted_config: config,
            messages: {
                let mut messages = messages.clone();
                messages.push("dry-run: config validated but not written".to_string());
                messages
            },
            mcp,
            skills,
            vault_snippets,
        });
    }

    if !declared_mounts {
        ensure_writable_directory(&service.index_dir)?;
    }
    ensure_writable_directory(&config_dir)?;
    let mut wrote_config = false;
    let mut final_messages = messages.clone();
    if declared_mounts {
        // The one path that writes NOTHING to the config, whatever the flags. `--overwrite`
        // is honoured for the MCP/skills/snippets installers below but deliberately not
        // here: an operator asking to overwrite their agent config has not asked to have
        // their mount table regenerated, and a mount table is the one thing in this file
        // that this command cannot reproduce faithfully.
        final_messages.push(format!(
            "config not written: {} declares a mount table, which setup-service does not \
             rewrite (--overwrite does not apply). Edit it by hand; `deep-obsidian-mcp \
             print-config` shows what this build reads from it.",
            config_path.display()
        ));
        // `auth_changed` cannot be true here: an auth change on a mounts config is
        // refused above, precisely so no token is ever stored without a reference.
        debug_assert!(!auth_changed);
    } else if config_path.exists() && !overwrite && !auth_changed {
        if !(install_mcp || install_skills || install_vault_snippets) {
            return Err(anyhow!(
                "config file already exists: {}",
                config_path.display()
            ));
        }
        final_messages.push(format!(
            "config exists, skipped write: {} (use --overwrite to replace it)",
            config_path.display()
        ));
    } else {
        // Never clobber silently: when replacing an existing config with
        // different content, keep the previous file next to the new one so a
        // wrong wizard answer stays recoverable.
        if config_path.exists() {
            let new_text = render_config_text(&config_path, &config)?;
            let old_text = fs::read_to_string(&config_path).unwrap_or_default();
            if old_text != new_text {
                let backup_path = config_path.with_extension("json.bak");
                fs::copy(&config_path, &backup_path).with_context(|| {
                    format!(
                        "failed to back up existing config to {}",
                        backup_path.display()
                    )
                })?;
                final_messages.push(format!(
                    "backed up previous config: {}",
                    backup_path.display()
                ));
            }
        }
        write_config_file(&config_path, &config)?;
        wrote_config = true;
        final_messages.push(format!("wrote config: {}", config_path.display()));
    }

    let endpoints = endpoint_report(&build_service_endpoints(&service));
    let mcp = if install_mcp {
        setup_mcp_clients(&endpoints, false, overwrite)?
    } else {
        Vec::new()
    };
    let skills = if install_skills {
        setup_agent_skills(false, overwrite)?
    } else {
        Vec::new()
    };
    let vault_snippets = if install_vault_snippets {
        setup_vault_snippets(&service.vault_path, false, overwrite)?
    } else {
        Vec::new()
    };

    Ok(SetupServiceReport {
        config_file_path: config_path.clone(),
        written: wrote_config,
        dry_run,
        endpoints,
        persisted_config: config,
        messages: final_messages,
        mcp,
        skills,
        vault_snippets,
    })
}

/// Refuse the wizard on a config that declares a mount table, before the first prompt.
///
/// The wizard exists to write a config, and `setup_service` never writes one that declares
/// a mount table. Left to fall through, the wizard would ask every question and then fail
/// on the auth guard — the wizard passes `Some(...)` for auth unconditionally — reporting
/// an *auth* problem for what is really "this command does not edit this kind of file", and
/// reading as though a different answer could have worked.
///
/// Checked on the FILE rather than on the resolved config, because the file is what the
/// wizard prefills from and what it would overwrite. Split out from the wizard so it is
/// testable without a stdin.
fn refuse_wizard_on_a_mounts_config(
    config_path: &Path,
    existing: Option<&PersistedServiceConfig>,
) -> Result<()> {
    let declares_mounts = existing
        .and_then(|config| config.mounts.as_ref())
        .is_some_and(|mounts| !mounts.is_empty());
    if !declares_mounts {
        return Ok(());
    }
    Err(anyhow!(
        "the setup wizard cannot edit {}: it declares a mount table, and setup-service does not \
         rewrite one (a mount table is the one thing in this file it cannot reproduce \
         faithfully). Edit the file by hand — `deep-obsidian-mcp print-config` shows what this \
         build reads from it, and `deep-obsidian-mcp doctor` checks each mount. The non-config \
         installers still work: `setup-service --mcp --skills --vault-snippets`.",
        config_path.display()
    ))
}

fn setup_service_wizard(
    options: &ServiceOptions,
    dry_run: bool,
    overwrite: bool,
    mcp: bool,
    skills: bool,
    vault_snippets: bool,
) -> Result<SetupServiceReport> {
    let mut options = options.clone();
    // Prefill every prompt from the existing config file so re-running the
    // wizard is an edit, not a from-scratch rewrite: pressing Enter keeps the
    // current value instead of replacing it.
    let config_path = options
        .config
        .clone()
        .unwrap_or_else(deep_obsidian_config::default_config_path);
    let existing = deep_obsidian_config::read_config_file(&config_path)
        .ok()
        .flatten();

    // Refused HERE, before the first prompt, rather than after the last one.
    //
    // The wizard exists to write a config, and `setup_service` never writes one that
    // declares a mount table. Left to fall through, the wizard would ask every question,
    // provision nothing, and then fail on the auth guard — reporting an auth problem for
    // what is really "this command does not edit this kind of file". Worse, it would have
    // read as though answering differently could work.
    //
    // Checked on the FILE rather than on the resolved config because that is what the
    // wizard prefills from and what it would overwrite.
    refuse_wizard_on_a_mounts_config(&config_path, existing.as_ref())?;

    if options.vault_path.is_none() {
        let existing_vault = existing
            .as_ref()
            .and_then(|config| config.vault_path.as_ref())
            .map(|path| path.display().to_string());
        let answer = prompt_string("Vault path", existing_vault.as_deref())?;
        if answer.trim().is_empty() {
            return Err(anyhow!("vault path is required"));
        }
        options.vault_path = Some(PathBuf::from(answer));
    }

    let install_mcp = mcp || prompt_bool("Configure MCP clients?", false)?;
    let install_skills = skills || prompt_bool("Install packaged skills?", false)?;
    let install_vault_snippets = vault_snippets || prompt_bool("Install vault snippets?", false)?;
    let existing_embedding = existing
        .as_ref()
        .and_then(|config| config.embedding.clone());
    let has_embeddings = existing_embedding
        .as_ref()
        .map(|embedding| embedding.provider.is_some() || embedding.model.is_some())
        .unwrap_or(false);
    let enable_embeddings = prompt_bool("Enable embeddings?", has_embeddings)?;

    if enable_embeddings {
        options.embedding_provider = Some("openai-compatible".to_string());
        let model = prompt_string(
            "Embedding model",
            existing_embedding
                .as_ref()
                .and_then(|embedding| embedding.model.as_deref()),
        )?;
        if !model.trim().is_empty() {
            options.embedding_model = Some(model);
        }
        let base_url = prompt_string(
            "Embedding base URL",
            existing_embedding
                .as_ref()
                .and_then(|embedding| embedding.base_url.as_deref()),
        )?;
        if !base_url.trim().is_empty() {
            options.embedding_base_url = Some(base_url);
        }
    }

    let mut resolved = crate::config::resolve_runtime_config(&options)?;

    if enable_embeddings {
        let secret = prompt_optional_secret("Embedding API key (blank for no auth)")?;
        if let Some(secret) = secret {
            let reference = SecretRef::OsKeyring {
                service: "deep-obsidian-mcp".to_string(),
                account: "openai-embedding".to_string(),
            };
            if dry_run {
                resolved.service.embedding.api_key_ref = Some(reference);
            } else {
                let resolver = SecretResolver::new();
                match resolver.put(
                    &reference,
                    SecretString::from(secret.expose_secret().to_string()),
                ) {
                    Ok(()) => {
                        resolved.service.embedding.api_key_ref = Some(reference);
                    }
                    Err(error) => {
                        println!("OS keyring unavailable: {error}");
                        if prompt_bool("Use encrypted local file fallback?", true)? {
                            let fallback = SecretRef::EncryptedFile {
                                id: "openai-embedding".to_string(),
                            };
                            resolver.put(&fallback, secret)?;
                            resolved.service.embedding.api_key_ref = Some(fallback);
                        } else {
                            return Err(anyhow!("embedding API key was not stored"));
                        }
                    }
                }
            }
        } else {
            resolved.service.embedding.api_key_ref = None;
        }
    }

    // This prompt is the one that made the wizard destructive: it is always
    // answered, which sets `auth_changed` and bypasses the existing-config
    // guard. Defaulting it from the current file means Enter keeps auth ENABLED
    // on a vault that already had it, instead of silently deprovisioning the
    // token. (The token itself is still regenerated and printed, so the change
    // is visible rather than silent.)
    let auth_enabled = existing
        .as_ref()
        .and_then(|config| config.auth.as_ref())
        .and_then(|auth| auth.enabled)
        .unwrap_or(false);
    let enable_auth = prompt_bool(
        "Enable HTTP bearer authentication (required for non-loopback exposure)?",
        auth_enabled,
    )?;

    setup_service(
        &resolved,
        dry_run,
        overwrite,
        install_mcp,
        install_skills,
        install_vault_snippets,
        Some(enable_auth),
        true,
    )
}

/// Generate an HTTP bearer token, store it through the shared secret store, and
/// print it to stdout exactly once so the operator can configure their client.
/// Wires the resulting reference into `auth`. In `dry_run` nothing is stored.
/// When `interactive` is false the encrypted-file fallback is used automatically
/// if the OS keyring is unavailable (no prompt), suiting flag-driven automation.
fn provision_auth_token(
    auth: &mut deep_obsidian_types::AuthConfig,
    dry_run: bool,
    interactive: bool,
) -> Result<()> {
    let token = deep_obsidian_server::auth::generate_token();
    let reference = SecretRef::OsKeyring {
        service: "deep-obsidian-mcp".to_string(),
        account: "http-auth-token".to_string(),
    };

    if dry_run {
        auth.enabled = true;
        auth.token_ref = Some(reference);
        println!("dry-run: would generate and store an HTTP bearer token");
        return Ok(());
    }

    let resolver = SecretResolver::new();
    let stored_reference = match resolver.put(&reference, SecretString::from(token.clone())) {
        Ok(()) => reference,
        Err(error) => {
            println!("OS keyring unavailable: {error}");
            let use_file = if interactive {
                prompt_bool("Use encrypted local file fallback?", true)?
            } else {
                println!("Falling back to encrypted local file storage.");
                true
            };
            if use_file {
                let fallback = SecretRef::EncryptedFile {
                    id: "http-auth-token".to_string(),
                };
                resolver.put(&fallback, SecretString::from(token.clone()))?;
                fallback
            } else {
                return Err(anyhow!("HTTP bearer token was not stored"));
            }
        }
    };

    auth.enabled = true;
    auth.token_ref = Some(stored_reference);

    println!();
    println!("HTTP bearer authentication enabled. Save this token now (shown only once):");
    println!();
    println!("    {token}");
    println!();
    println!("Configure your MCP client with header: Authorization: Bearer {token}");
    println!();

    Ok(())
}

/// Disable HTTP bearer auth, deleting any stored token from the secret store so
/// it does not linger orphaned. Deletion is best-effort: a failure (e.g. a
/// locked keyring) is reported but does not block disabling. In `dry_run`
/// nothing is deleted.
fn deprovision_auth_token(auth: &mut deep_obsidian_types::AuthConfig, dry_run: bool) {
    let previous_ref = auth.token_ref.take();
    auth.enabled = false;

    if dry_run {
        if previous_ref.is_some() {
            println!("dry-run: would disable auth and delete the stored HTTP bearer token");
        } else {
            println!("dry-run: would disable HTTP bearer authentication");
        }
        return;
    }

    match previous_ref {
        Some(reference) => match SecretResolver::new().delete(&reference) {
            Ok(()) => {
                println!("HTTP bearer authentication disabled; stored token deleted.")
            }
            Err(error) => println!(
                "HTTP bearer authentication disabled; could not delete stored token: {error}"
            ),
        },
        None => println!("HTTP bearer authentication disabled."),
    }
}

fn prompt_string(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(default) => print!("{label} [{default}]: "),
        None => print!("{label}: "),
    }
    io::stdout().flush().context("failed to flush stdout")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read prompt input")?;
    let value = input.trim().to_string();
    Ok(if value.is_empty() {
        default.unwrap_or_default().to_string()
    } else {
        value
    })
}

fn prompt_bool(label: &str, default: bool) -> Result<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        let value = prompt_string(&format!("{label} {suffix}"), None)?;
        if value.trim().is_empty() {
            return Ok(default);
        }
        match value.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please answer y or n."),
        }
    }
}

fn prompt_optional_secret(label: &str) -> Result<Option<SecretString>> {
    let value = rpassword::prompt_password(format!("{label}: "))
        .context("failed to read secret prompt input")?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(SecretString::from(value)))
    }
}

fn setup_mcp_clients(
    endpoints: &EndpointReport,
    dry_run: bool,
    overwrite: bool,
) -> Result<Vec<SetupActionReport>> {
    Ok(vec![
        setup_codex_mcp(&endpoints.mcp, dry_run, overwrite)?,
        setup_claude_mcp(&endpoints.mcp, dry_run, overwrite),
    ])
}

fn setup_codex_mcp(mcp_url: &str, dry_run: bool, overwrite: bool) -> Result<SetupActionReport> {
    let config_path = codex_config_path()?;
    if dry_run {
        assert_creatable_directory(config_path.parent().unwrap_or_else(|| Path::new(".")))?;
        return Ok(SetupActionReport {
            target: "codex mcp".into(),
            path: Some(config_path),
            changed: false,
            status: "dry-run".into(),
            message: format!("would configure Codex MCP server `deep_obsidian` -> {mcp_url}"),
        });
    }

    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    let mut config = if existing.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        existing
            .parse::<toml::Value>()
            .with_context(|| format!("failed to parse Codex config: {}", config_path.display()))?
    };
    let root = config.as_table_mut().ok_or_else(|| {
        anyhow!(
            "Codex config root must be a TOML table: {}",
            config_path.display()
        )
    })?;
    let mcp_servers = root
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let mcp_servers = mcp_servers.as_table_mut().ok_or_else(|| {
        anyhow!(
            "Codex config key `mcp_servers` must be a table: {}",
            config_path.display()
        )
    })?;

    if mcp_servers.contains_key("deep_obsidian") && !overwrite {
        return Ok(SetupActionReport {
            target: "codex mcp".into(),
            path: Some(config_path),
            changed: false,
            status: "skipped".into(),
            message:
                "Codex MCP server `deep_obsidian` already exists; use --overwrite to replace it"
                    .into(),
        });
    }

    let mut server = toml::map::Map::new();
    server.insert("url".to_string(), toml::Value::String(mcp_url.to_string()));
    server.insert("enabled".to_string(), toml::Value::Boolean(true));
    mcp_servers.insert("deep_obsidian".to_string(), toml::Value::Table(server));

    if let Some(parent) = config_path.parent() {
        ensure_writable_directory(parent)?;
    }
    fs::write(&config_path, toml::to_string_pretty(&config)?)
        .with_context(|| format!("failed to write Codex config: {}", config_path.display()))?;

    Ok(SetupActionReport {
        target: "codex mcp".into(),
        path: Some(config_path),
        changed: true,
        status: "ok".into(),
        message: format!("configured Codex MCP server `deep_obsidian` -> {mcp_url}"),
    })
}

fn setup_claude_mcp(mcp_url: &str, dry_run: bool, overwrite: bool) -> SetupActionReport {
    let scope = "user";
    if dry_run {
        return SetupActionReport {
            target: "claude mcp".into(),
            path: None,
            changed: false,
            status: "dry-run".into(),
            message: format!(
                "would run: claude mcp add --transport http --scope {scope} deep-obsidian {mcp_url}"
            ),
        };
    }

    if ProcessCommand::new("claude")
        .arg("--version")
        .output()
        .is_err()
    {
        return SetupActionReport {
            target: "claude mcp".into(),
            path: None,
            changed: false,
            status: "skipped".into(),
            message: "Claude Code CLI not found in PATH; run `claude mcp add --transport http --scope user deep-obsidian <mcp-url>` manually".into(),
        };
    }

    if overwrite {
        let _ = ProcessCommand::new("claude")
            .args(["mcp", "remove", "deep-obsidian", "--scope", scope])
            .output();
    }

    let output = ProcessCommand::new("claude")
        .args([
            "mcp",
            "add",
            "--transport",
            "http",
            "--scope",
            scope,
            "deep-obsidian",
            mcp_url,
        ])
        .output();

    match output {
        Ok(output) if output.status.success() => SetupActionReport {
            target: "claude mcp".into(),
            path: None,
            changed: true,
            status: "ok".into(),
            message: format!("configured Claude Code MCP server `deep-obsidian` -> {mcp_url}"),
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            SetupActionReport {
                target: "claude mcp".into(),
                path: None,
                changed: false,
                status: "skipped".into(),
                message: if stderr.is_empty() {
                    "Claude Code MCP configuration command failed".into()
                } else {
                    format!("Claude Code MCP configuration command failed: {stderr}")
                },
            }
        }
        Err(error) => SetupActionReport {
            target: "claude mcp".into(),
            path: None,
            changed: false,
            status: "skipped".into(),
            message: format!("failed to run Claude Code CLI: {error}"),
        },
    }
}

fn setup_agent_skills(dry_run: bool, overwrite: bool) -> Result<Vec<SetupActionReport>> {
    let source_dir = packaged_skills_dir()?;
    Ok(vec![
        install_skills_for_target(
            "codex skills",
            &source_dir,
            &codex_skills_dir()?,
            dry_run,
            overwrite,
        )?,
        install_skills_for_target(
            "claude skills",
            &source_dir,
            &claude_skills_dir()?,
            dry_run,
            overwrite,
        )?,
    ])
}

fn setup_vault_snippets(
    vault_path: &Path,
    dry_run: bool,
    overwrite: bool,
) -> Result<Vec<SetupActionReport>> {
    let source_dir = packaged_obsidian_snippets_dir()?;
    Ok(vec![install_vault_snippets_for_target(
        vault_path,
        &source_dir,
        dry_run,
        overwrite,
    )?])
}

fn install_vault_snippets_for_target(
    vault_path: &Path,
    source_dir: &Path,
    dry_run: bool,
    overwrite: bool,
) -> Result<SetupActionReport> {
    let snippets = packaged_snippet_files(source_dir)?;
    let snippets_dir = vault_path.join(".obsidian").join("snippets");
    let appearance_path = vault_path.join(".obsidian").join("appearance.json");
    let snippet_names = snippets
        .iter()
        .filter_map(|path| path.file_stem().and_then(|stem| stem.to_str()))
        .map(str::to_string)
        .collect::<Vec<_>>();

    if dry_run {
        assert_creatable_directory(&snippets_dir)?;
        return Ok(SetupActionReport {
            target: "vault snippets".into(),
            path: Some(snippets_dir),
            changed: false,
            status: "dry-run".into(),
            message: format!(
                "would install and enable {} Obsidian CSS snippets: {}",
                snippet_names.len(),
                snippet_names.join(", ")
            ),
        });
    }

    ensure_writable_directory(&snippets_dir)?;
    let mut installed = 0usize;
    let mut skipped = 0usize;
    for source in snippets {
        let file_name = source
            .file_name()
            .ok_or_else(|| anyhow!("invalid snippet path: {}", source.display()))?;
        let destination = snippets_dir.join(file_name);
        if destination.exists() && !overwrite {
            skipped += 1;
            continue;
        }
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "failed to copy snippet {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        installed += 1;
    }
    let enabled = enable_obsidian_snippets(&appearance_path, &snippet_names)?;

    Ok(SetupActionReport {
        target: "vault snippets".into(),
        path: Some(snippets_dir),
        changed: installed > 0 || enabled > 0,
        status: "ok".into(),
        message: format!(
            "installed {installed} snippets, skipped {skipped} existing snippets, enabled {enabled} snippets"
        ),
    })
}

fn enable_obsidian_snippets(appearance_path: &Path, snippet_names: &[String]) -> Result<usize> {
    let existing = fs::read_to_string(appearance_path).unwrap_or_default();
    let mut appearance = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<Value>(&existing)
            .with_context(|| format!("failed to parse {}", appearance_path.display()))?
    };
    let object = appearance.as_object_mut().ok_or_else(|| {
        anyhow!(
            "Obsidian appearance config must be a JSON object: {}",
            appearance_path.display()
        )
    })?;

    let snippets = object
        .entry("enabledCssSnippets".to_string())
        .or_insert_with(|| json!([]));
    let snippets = snippets.as_array_mut().ok_or_else(|| {
        anyhow!(
            "Obsidian appearance key `enabledCssSnippets` must be an array: {}",
            appearance_path.display()
        )
    })?;

    let mut enabled = 0usize;
    for name in snippet_names {
        if !snippets
            .iter()
            .any(|value| value.as_str().is_some_and(|existing| existing == name))
        {
            snippets.push(Value::String(name.clone()));
            enabled += 1;
        }
    }

    if enabled == 0 {
        return Ok(0);
    }

    if let Some(parent) = appearance_path.parent() {
        ensure_writable_directory(parent)?;
    }
    fs::write(appearance_path, serde_json::to_string_pretty(&appearance)?)
        .with_context(|| format!("failed to write {}", appearance_path.display()))?;
    Ok(enabled)
}

fn install_skills_for_target(
    target: &str,
    source_dir: &Path,
    destination_dir: &Path,
    dry_run: bool,
    overwrite: bool,
) -> Result<SetupActionReport> {
    let skills = packaged_skill_names(source_dir)?;
    if dry_run {
        assert_creatable_directory(destination_dir)?;
        return Ok(SetupActionReport {
            target: target.into(),
            path: Some(destination_dir.to_path_buf()),
            changed: false,
            status: "dry-run".into(),
            message: format!(
                "would install {} skills from {}",
                skills.len(),
                source_dir.display()
            ),
        });
    }

    ensure_writable_directory(destination_dir)?;
    let mut installed = 0usize;
    let mut skipped = 0usize;
    for skill in skills {
        let source = source_dir.join(&skill);
        let destination = destination_dir.join(&skill);
        if destination.exists() {
            if !overwrite {
                skipped += 1;
                continue;
            }
            fs::remove_dir_all(&destination)
                .with_context(|| format!("failed to replace skill: {}", destination.display()))?;
        }
        copy_dir_recursive(&source, &destination)?;
        installed += 1;
    }

    Ok(SetupActionReport {
        target: target.into(),
        path: Some(destination_dir.to_path_buf()),
        changed: installed > 0,
        status: if skipped > 0 && installed == 0 {
            "skipped".into()
        } else {
            "ok".into()
        },
        message: format!("installed {installed} skills, skipped {skipped} existing skills"),
    })
}

fn packaged_skill_names(source_dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(source_dir)
        .with_context(|| format!("failed to read skills directory: {}", source_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").is_file() {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    if names.is_empty() {
        return Err(anyhow!(
            "no packaged skills found under {}",
            source_dir.display()
        ));
    }
    Ok(names)
}

fn packaged_snippet_files(source_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(source_dir).with_context(|| {
        format!(
            "failed to read snippets directory: {}",
            source_dir.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("css")
        {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(anyhow!(
            "no packaged Obsidian snippets found under {}",
            source_dir.display()
        ));
    }
    Ok(files)
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create directory: {}", destination.display()))?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to read directory: {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn packaged_skills_dir() -> Result<PathBuf> {
    if let Ok(path) = env::var("DEEP_OBSIDIAN_SKILLS_DIR") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Ok(path);
        }
    }

    let mut candidates = Vec::new();
    candidates.push(env::current_dir()?.join("skills"));
    if let Ok(exe) = env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            candidates.push(bin_dir.join("../share/deep-obsidian-mcp/skills"));
            candidates.push(bin_dir.join("../share/skills"));
        }
        if let Some(prefix) = exe.parent().and_then(Path::parent) {
            candidates.push(prefix.join("share/deep-obsidian-mcp/skills"));
        }
    }

    for candidate in candidates {
        let candidate = absolute_path(&candidate)?;
        if candidate.is_dir()
            && !packaged_skill_names(&candidate)
                .unwrap_or_default()
                .is_empty()
        {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "packaged skills directory not found; set DEEP_OBSIDIAN_SKILLS_DIR"
    ))
}

fn packaged_obsidian_snippets_dir() -> Result<PathBuf> {
    if let Ok(path) = env::var("DEEP_OBSIDIAN_SNIPPETS_DIR") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Ok(path);
        }
    }

    let mut candidates = Vec::new();
    candidates.push(env::current_dir()?.join("obsidian-snippets"));
    if let Ok(exe) = env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            candidates.push(bin_dir.join("../share/deep-obsidian-mcp/obsidian-snippets"));
            candidates.push(bin_dir.join("../share/obsidian-snippets"));
        }
        if let Some(prefix) = exe.parent().and_then(Path::parent) {
            candidates.push(prefix.join("share/deep-obsidian-mcp/obsidian-snippets"));
        }
    }

    for candidate in candidates {
        let candidate = absolute_path(&candidate)?;
        if candidate.is_dir()
            && !packaged_snippet_files(&candidate)
                .unwrap_or_default()
                .is_empty()
        {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "packaged Obsidian snippets directory not found; set DEEP_OBSIDIAN_SNIPPETS_DIR"
    ))
}

fn codex_config_path() -> Result<PathBuf> {
    Ok(codex_home_dir()?.join("config.toml"))
}

fn codex_skills_dir() -> Result<PathBuf> {
    Ok(codex_home_dir()?.join("skills"))
}

fn codex_home_dir() -> Result<PathBuf> {
    if let Ok(path) = env::var("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join(".codex"))
}

fn claude_skills_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("skills"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

pub async fn doctor(
    resolved: &ResolvedRuntimeConfig,
    probe_timeout_ms: u64,
    probe_remote: bool,
) -> Result<DoctorReport> {
    let service = resolved.service.clone();
    let endpoints = build_service_endpoints(&service);
    let index = inspect_index(&service);
    let mut checks = vec![
        check_config(resolved),
        check_vault(&service),
        check_index_dir(&service),
        check_index_file(&index),
        check_rg(),
    ];

    // Per-mount checks, and ONLY for a config that declared a mount table — the same
    // gate the mount LINES use. A legacy `vaultPath` install's `doctor` output stays
    // byte-identical: its single implicit root mount is already covered by `vault`,
    // `index-dir` and `index sqlite`, so adding a `mount.vault` duplicate of `vault`
    // would be noise.
    if !service.mounts.is_empty() {
        for mount in &service.mounts {
            checks.extend(check_mount_local(mount));
            if probe_remote {
                checks.push(probe_mount_remote(&service, mount).await);
            }
        }
    } else if probe_remote {
        checks.push(CheckReport {
            name: "mounts.remote".into(),
            status: "skip".into(),
            message: "--probe-remote had nothing to probe: this config declares no mounts, so \
                      its only vault is the local root"
                .into(),
            details: None,
        });
    }
    let mut health_payload = None;
    let mut readiness_payload = None;

    if matches!(service.transport, TransportMode::Http) {
        let port_check = check_port(&service);
        let should_probe = port_check.status != "ok";
        checks.push(port_check);
        if should_probe {
            let client = http_client(probe_timeout_ms).ok();
            let health_check = match &client {
                Some(client) => check_health(client, &endpoints).await,
                None => CheckReport {
                    name: "health".into(),
                    status: "fail".into(),
                    message: "failed to build HTTP client".into(),
                    details: None,
                },
            };
            health_payload = health_payload_from_check(&health_check);
            checks.push(health_check);
            if let (Some(client), Some(readiness_url)) =
                (&client, readiness_endpoint_from_health(&endpoints.health))
            {
                let readiness_check = check_readiness(client, &readiness_url).await;
                readiness_payload = health_payload_from_check(&readiness_check);
                checks.push(readiness_check);
            }
        } else {
            checks.push(CheckReport {
                name: "health".into(),
                status: "skip".into(),
                message: "health endpoint skipped because the service is not running".into(),
                details: None,
            });
        }
    } else {
        checks.push(CheckReport {
            name: "http-port".into(),
            status: "skip".into(),
            message: "transport is stdio; HTTP port checks are skipped".into(),
            details: None,
        });
        checks.push(CheckReport {
            name: "health".into(),
            status: "skip".into(),
            message: "transport is stdio; health probe is skipped".into(),
            details: None,
        });
    }

    if let Some(check) =
        secret_reference_check("embeddingApiKey", service.embedding.api_key_ref.as_ref())
    {
        checks.push(check);
    }
    if let Some(check) = secret_reference_check(
        "artifactEmbeddingApiKey",
        service.artifact_embedding.api_key_ref.as_ref(),
    ) {
        checks.push(check);
    }
    if let Some(check) = secret_reference_check("authToken", service.auth.token_ref.as_ref()) {
        checks.push(check);
    }

    let ok = checks.iter().all(|check| check.status != "fail");
    let config = redact_config(&to_persisted_config(&service));
    let service_diagnostics = service_diagnostics(
        &service,
        &endpoints,
        health_payload,
        readiness_payload,
        &index,
    );
    Ok(DoctorReport {
        config,
        config_diagnostics: ConfigDiagnostics {
            path: resolved.config_path.clone(),
            exists: resolved.config_file.is_some(),
            precedence: CONFIG_PRECEDENCE.to_vec(),
            sources: resolved.sources.clone(),
        },
        endpoints: endpoint_report(&endpoints),
        index,
        service: service_diagnostics,
        checks,
        ok,
    })
}

pub fn print_config(resolved: &ResolvedRuntimeConfig, redact: bool) -> Result<PrintConfigReport> {
    let mut config = to_persisted_config(&resolved.service);
    // `print-config` is what an operator uses to see what a write would produce, so it
    // must show the retained keys too — otherwise it would report a file this build is
    // about to write as if those keys were already gone.
    carry_unknown_fields(&mut config, resolved.config_file.as_ref());
    let printable = if redact {
        redact_config(&config)
    } else {
        config.clone()
    };

    Ok(PrintConfigReport {
        config_path: resolved.config_path.clone(),
        config,
        text: serde_json::to_string_pretty(&printable)?,
    })
}

pub async fn probe(resolved: &ResolvedRuntimeConfig, timeout_ms: u64) -> Result<ProbeReport> {
    let service = ensure_service_transport_http(resolved.service.clone())?;
    let endpoints = build_service_endpoints(&service);
    let client = http_client_with_bearer(timeout_ms, resolve_probe_token(&service))?;
    let health = probe_health(&client, &endpoints.health).await;
    let mcp = probe_mcp(&client, &endpoints.mcp).await;

    Ok(ProbeReport {
        endpoints: endpoint_report(&endpoints),
        health,
        mcp,
    })
}

pub async fn serve(resolved: &ResolvedRuntimeConfig) -> Result<ServeReport> {
    match resolved.service.transport {
        TransportMode::Http => {
            let service = ensure_service_transport_http(resolved.service.clone())?;
            let endpoints = build_service_endpoints(&service);
            let report = endpoint_report(&endpoints);
            // Capture auth state before `service` is moved into the bootstrap so
            // the operator gets a clear startup signal (the CLI does not install
            // a tracing subscriber, so the library's log lines are not shown).
            let auth_enabled = service.auth.enabled
                || std::env::var("DEEP_OBSIDIAN_AUTH_TOKEN")
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false);
            let host = service.http.host.clone();
            let host_is_loopback = deep_obsidian_config::is_loopback_host(&host);
            let mut bootstrap = run_http_service(service).await?;
            eprintln!(
                "deep-obsidian-mcp native server running at {} (health={})",
                report.mcp, report.health
            );
            if auth_enabled {
                eprintln!("auth: bearer token required on {}", report.mcp);
            } else {
                eprintln!("auth: disabled (no client authentication)");
                if !host_is_loopback {
                    eprintln!(
                        "WARNING: serving without authentication on non-loopback host {host}"
                    );
                }
            }
            tokio::select! {
                shutdown = wait_for_shutdown_signal() => {
                    shutdown?;
                }
                server_result = &mut bootstrap.server_handle => {
                    match server_result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            bootstrap.shutdown_sidecars().await;
                            return Err(error.into());
                        }
                        Err(error) => {
                            bootstrap.shutdown_sidecars().await;
                            return Err(anyhow!("HTTP server task failed: {error}"));
                        }
                    }
                }
            }
            // Stop any sidecar children gracefully before the context drops (whose
            // `Drop` would kill them instead).
            bootstrap.shutdown_sidecars().await;
            Ok(ServeReport {
                message: format!(
                    "Rust native server stopped for {} (health={})",
                    report.mcp, report.health
                ),
                endpoints: report,
            })
        }
        TransportMode::Stdio => serve_stdio_native(&resolved.service).await,
    }
}

fn ensure_service_transport_http(config: ResolvedServiceConfig) -> Result<ResolvedServiceConfig> {
    if matches!(config.transport, TransportMode::Http) {
        return Ok(config);
    }

    deep_obsidian_config::ensure_http_service_config(ResolvedServiceConfig {
        federated_rerank: true,
        transport: TransportMode::Http,
        ..config
    })
    .map_err(Into::into)
}

async fn serve_stdio_native(config: &ResolvedServiceConfig) -> Result<ServeReport> {
    run_stdio_service(config.clone())
        .await
        .context("failed to run the native Rust stdio server")?;
    Ok(ServeReport {
        message: "Rust native stdio server exited successfully".to_string(),
        endpoints: EndpointReport {
            mcp: "stdio".to_string(),
            health: "n/a".to_string(),
            readiness: None,
        },
    })
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("failed to register SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("failed to wait for SIGINT")?;
            }
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed to wait for shutdown signal")?;
    }

    Ok(())
}

fn validate_vault(config: &ResolvedServiceConfig) -> Result<()> {
    match fs::metadata(&config.vault_path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(anyhow!(
            "vault path is not a directory: {}",
            config.vault_path.display()
        )),
        Err(error) if is_permission_denied(&error) => {
            let privacy_opened = open_macos_full_disk_access_panel();
            Err(anyhow!(macos_vault_access_guidance(
                &config.vault_path,
                privacy_opened
            )))
        }
        Err(_) => Err(anyhow!(
            "vault path does not exist or is not a directory: {}",
            config.vault_path.display()
        )),
    }
}

fn macos_vault_access_preflight(vault_path: &Path, dry_run: bool) -> Result<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        if dry_run {
            return Ok(vec![format!(
                "macOS vault access preflight: would verify current binary can read {}",
                vault_path.display()
            )]);
        }

        match fs::read_dir(vault_path) {
            Ok(mut entries) => {
                let _ = entries.next().transpose().with_context(|| {
                    format!("failed to inspect vault contents: {}", vault_path.display())
                })?;
                Ok(vec![format!(
                    "macOS vault access preflight: current binary can read {}",
                    vault_path.display()
                )])
            }
            Err(error) if is_permission_denied(&error) => {
                let privacy_opened = open_macos_full_disk_access_panel();
                let guidance = macos_vault_access_guidance(vault_path, privacy_opened);
                Err(anyhow!(guidance))
            }
            Err(error) => Err(anyhow!(
                "failed to read vault directory {}: {}",
                vault_path.display(),
                error
            )),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = vault_path;
        let _ = dry_run;
        Ok(Vec::new())
    }
}

fn is_permission_denied(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::PermissionDenied)
        || error
            .raw_os_error()
            .is_some_and(|code| code == 1 || code == 13)
}

fn macos_vault_access_guidance(vault_path: &Path, privacy_opened: bool) -> String {
    let mut message = String::new();
    let _ = writeln!(
        &mut message,
        "macOS denied access to the vault: {}",
        vault_path.display()
    );
    let _ = writeln!(
        &mut message,
        "If a permission pop-up appeared, approve it and rerun setup-service."
    );
    if privacy_opened {
        let _ = writeln!(
            &mut message,
            "Privacy & Security > Full Disk Access was opened."
        );
    } else {
        let _ = writeln!(
            &mut message,
            "Open Privacy & Security > Full Disk Access manually."
        );
    }
    let _ = writeln!(
        &mut message,
        "Add the Homebrew service binary, then restart the service:"
    );
    for candidate in service_binary_candidates() {
        let _ = writeln!(&mut message, "  {}", candidate.display());
    }
    let _ = writeln!(&mut message, "  brew services restart deep-obsidian-mcp");
    let _ = write!(&mut message, "  deep-obsidian-mcp doctor");
    message
}

fn service_binary_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = env::current_exe() {
        candidates.push(exe);
    }
    if let Ok(prefix) = env::var("HOMEBREW_PREFIX") {
        candidates.push(PathBuf::from(prefix).join("bin/deep-obsidian-mcp"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/deep-obsidian-mcp"));
    candidates.push(PathBuf::from("/usr/local/bin/deep-obsidian-mcp"));

    let mut unique = Vec::new();
    for candidate in candidates {
        if !unique.iter().any(|existing| existing == &candidate) {
            unique.push(candidate);
        }
    }
    unique
}

fn open_macos_full_disk_access_panel() -> bool {
    #[cfg(target_os = "macos")]
    {
        ProcessCommand::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()
            .context("failed to resolve current working directory")?
            .join(path))
    }
}

fn writable_directory_error(path: &Path) -> anyhow::Error {
    anyhow!("Directory is not writable: {}", path.display())
}

fn writable_probe_path(directory: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    directory.join(format!(
        ".deep-obsidian-mcp-write-test-{}-{}",
        std::process::id(),
        nanos
    ))
}

fn probe_directory_writable(directory: &Path, reported_path: &Path) -> Result<()> {
    let metadata = fs::metadata(directory).map_err(|_| writable_directory_error(reported_path))?;
    if !metadata.is_dir() {
        return Err(writable_directory_error(reported_path));
    }

    let probe_path = writable_probe_path(directory);
    fs::write(&probe_path, b"").map_err(|_| writable_directory_error(reported_path))?;
    let _ = fs::remove_file(&probe_path);
    Ok(())
}

fn ensure_writable_directory(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory).map_err(|_| writable_directory_error(directory))?;
    probe_directory_writable(directory, directory)
}

fn assert_creatable_directory(directory: &Path) -> Result<()> {
    let resolved = absolute_path(directory)?;
    let mut current = resolved.clone();
    while !current.exists() {
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    probe_directory_writable(&current, &resolved)
}

fn endpoint_report(endpoints: &ServiceEndpoints) -> EndpointReport {
    EndpointReport {
        mcp: endpoints.mcp.clone(),
        health: endpoints.health.clone(),
        readiness: readiness_endpoint_from_health(&endpoints.health),
    }
}

fn index_sqlite_path(config: &ResolvedServiceConfig) -> PathBuf {
    config.index_dir.join(INDEX_SQLITE_FILENAME)
}

fn readiness_endpoint_from_health(health_url: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(health_url).ok()?;
    url.set_path("/readyz");
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

async fn probe_health(client: &Client, url: &str) -> HealthProbeReport {
    match client.get(url).send().await {
        Ok(response) => {
            let status = response.status();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body_text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    return HealthProbeReport {
                        ok: false,
                        status: Some(status.as_u16()),
                        body: None,
                        error: Some(error.to_string()),
                    };
                }
            };
            let body = if content_type.contains("application/json") {
                serde_json::from_str::<Value>(&body_text)
                    .unwrap_or_else(|_| Value::String(body_text))
            } else {
                Value::String(body_text)
            };
            HealthProbeReport {
                ok: status.is_success(),
                status: Some(status.as_u16()),
                body: Some(body),
                error: None,
            }
        }
        Err(error) => HealthProbeReport {
            ok: false,
            status: None,
            body: None,
            error: Some(error.to_string()),
        },
    }
}

async fn post_json_rpc(client: &Client, url: &str, payload: Value) -> Result<Value> {
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    response
        .json::<Value>()
        .await
        .map_err(|error| anyhow!(error.to_string()))
}

async fn post_json_rpc_notification(client: &Client, url: &str, payload: Value) -> Result<()> {
    client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(())
}

fn json_rpc_result(value: Value, label: &str) -> Result<Value> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown JSON-RPC error");
        return Err(anyhow!("{label} failed: {message}"));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("{label} response missing result"))
}

async fn probe_mcp(client: &Client, url: &str) -> McpProbeReport {
    let initialize_response = match post_json_rpc(
        client,
        url,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "deep-obsidian-mcp-probe",
                    "version": "1.0.0"
                }
            }
        }),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            return McpProbeReport {
                ok: false,
                tool_count: None,
                first_tool: None,
                vault_info: None,
                error: Some(error.to_string()),
            };
        }
    };
    if let Err(error) = json_rpc_result(initialize_response, "initialize") {
        return McpProbeReport {
            ok: false,
            tool_count: None,
            first_tool: None,
            vault_info: None,
            error: Some(error.to_string()),
        };
    }
    if let Err(error) = post_json_rpc_notification(
        client,
        url,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }),
    )
    .await
    {
        return McpProbeReport {
            ok: false,
            tool_count: None,
            first_tool: None,
            vault_info: None,
            error: Some(error.to_string()),
        };
    }

    let tools_value = match post_json_rpc(
        client,
        url,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return McpProbeReport {
                ok: false,
                tool_count: None,
                first_tool: None,
                vault_info: None,
                error: Some(error.to_string()),
            };
        }
    };
    let tools_result = match json_rpc_result(tools_value, "tools/list") {
        Ok(value) => value,
        Err(error) => {
            return McpProbeReport {
                ok: false,
                tool_count: None,
                first_tool: None,
                vault_info: None,
                error: Some(error.to_string()),
            };
        }
    };
    let tools = tools_result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let vault_info_value = match post_json_rpc(
        client,
        url,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "vault_info",
                "arguments": {}
            }
        }),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            return McpProbeReport {
                ok: false,
                tool_count: None,
                first_tool: None,
                vault_info: None,
                error: Some(error.to_string()),
            };
        }
    };
    let vault_info = match json_rpc_result(vault_info_value, "tools/call vault_info") {
        Ok(value) => value,
        Err(error) => {
            return McpProbeReport {
                ok: false,
                tool_count: None,
                first_tool: None,
                vault_info: None,
                error: Some(error.to_string()),
            };
        }
    };

    McpProbeReport {
        ok: true,
        tool_count: Some(tools.len()),
        first_tool: Some(
            tools
                .first()
                .and_then(|tool| tool.get("name"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        ),
        vault_info: Some(vault_info),
        error: None,
    }
}

fn check_config(resolved: &ResolvedRuntimeConfig) -> CheckReport {
    let source_details = serde_json::to_value(&resolved.sources).unwrap_or(Value::Null);
    CheckReport {
        name: "config".into(),
        status: "ok".into(),
        message: if resolved.config_file.is_some() {
            "config file loaded; resolution precedence is cli > config > env > default".into()
        } else {
            "config file not found; using cli, environment, and defaults".into()
        },
        details: Some(serde_json::json!({
            "path": &resolved.config_path,
            "exists": resolved.config_file.is_some(),
            "precedence": CONFIG_PRECEDENCE,
            "sources": source_details,
        })),
    }
}

fn inspect_index(config: &ResolvedServiceConfig) -> IndexDiagnostics {
    let path = index_sqlite_path(config);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return IndexDiagnostics {
                path,
                exists: false,
                status: "warn".to_string(),
                message: "index sqlite file does not exist yet".to_string(),
                size_bytes: None,
                schema_version: None,
                user_version: None,
                metadata: None,
                note_rows: None,
                chunk_rows: None,
                file_snapshot_rows: None,
            };
        }
        Err(error) => {
            return IndexDiagnostics {
                path,
                exists: false,
                status: "fail".to_string(),
                message: format!("failed to read index sqlite metadata: {error}"),
                size_bytes: None,
                schema_version: None,
                user_version: None,
                metadata: None,
                note_rows: None,
                chunk_rows: None,
                file_snapshot_rows: None,
            };
        }
    };

    let size_bytes = Some(metadata.len());
    let connection = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => connection,
        Err(error) => {
            return IndexDiagnostics {
                path,
                exists: true,
                status: "fail".to_string(),
                message: format!("failed to open index sqlite read-only: {error}"),
                size_bytes,
                schema_version: None,
                user_version: None,
                metadata: None,
                note_rows: None,
                chunk_rows: None,
                file_snapshot_rows: None,
            };
        }
    };

    let schema_version = pragma_i64(&connection, "schema_version");
    let user_version = pragma_i64(&connection, "user_version");
    let index_metadata = read_index_metadata(&connection);
    let note_rows = count_table_rows(&connection, "notes");
    let chunk_rows = count_table_rows(&connection, "chunks");
    let file_snapshot_rows = count_table_rows(&connection, "file_snapshots");

    IndexDiagnostics {
        path,
        exists: true,
        status: "ok".to_string(),
        message: "index sqlite file is readable".to_string(),
        size_bytes,
        schema_version,
        user_version,
        metadata: index_metadata,
        note_rows,
        chunk_rows,
        file_snapshot_rows,
    }
}

fn check_index_file(index: &IndexDiagnostics) -> CheckReport {
    CheckReport {
        name: "index-sqlite".into(),
        status: index.status.clone(),
        message: index.message.clone(),
        details: Some(serde_json::json!({
            "path": &index.path,
            "exists": index.exists,
            "sizeBytes": index.size_bytes,
            "schemaVersion": index.schema_version,
            "userVersion": index.user_version,
            "metadata": &index.metadata,
            "noteRows": index.note_rows,
            "chunkRows": index.chunk_rows,
            "fileSnapshotRows": index.file_snapshot_rows,
        })),
    }
}

fn pragma_i64(connection: &Connection, name: &str) -> Option<i64> {
    connection
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .ok()
}

fn table_exists(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
}

fn count_table_rows(connection: &Connection, table: &str) -> Option<u64> {
    if !table_exists(connection, table) {
        return None;
    }
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .ok()
        .and_then(|count| u64::try_from(count).ok())
}

fn read_index_metadata(connection: &Connection) -> Option<Value> {
    if !table_exists(connection, "metadata") {
        return None;
    }

    let mut statement = connection
        .prepare("SELECT key, value FROM metadata ORDER BY key")
        .ok()?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;
    let mut metadata = serde_json::Map::new();
    for row in rows.flatten() {
        let (key, value) = row;
        if key.to_ascii_lowercase().contains("apikey") {
            metadata.insert(key, Value::String("[redacted]".to_string()));
        } else {
            metadata.insert(key, Value::String(value));
        }
    }
    Some(Value::Object(metadata))
}

fn health_payload_from_check(check: &CheckReport) -> Option<Value> {
    check
        .details
        .as_ref()
        .and_then(|details| details.get("body"))
        .cloned()
}

fn service_diagnostics(
    config: &ResolvedServiceConfig,
    endpoints: &ServiceEndpoints,
    health_payload: Option<Value>,
    readiness_payload: Option<Value>,
    index: &IndexDiagnostics,
) -> ServiceDiagnostics {
    let last_refresh = health_payload
        .as_ref()
        .and_then(|payload| payload.get("generatedAt"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            index
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("generatedAt"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
    let last_error = health_payload
        .as_ref()
        .and_then(|payload| payload.get("error"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    ServiceDiagnostics {
        auto_reindex: AutoReindexDiagnostics {
            enabled: config.auto_reindex.enabled,
            debounce_ms: config.auto_reindex.debounce_ms,
            interval_ms: config.auto_reindex.interval_ms,
        },
        embedding: embedding_diagnostics(config, &health_payload, &readiness_payload, index),
        endpoint: endpoint_report(endpoints),
        last_refresh,
        last_error,
        health: health_payload,
        readiness: readiness_payload,
    }
}

fn embedding_diagnostics(
    config: &ResolvedServiceConfig,
    health_payload: &Option<Value>,
    readiness_payload: &Option<Value>,
    index: &IndexDiagnostics,
) -> EmbeddingDiagnostics {
    let configured = config.embedding.provider.is_some()
        && config
            .embedding
            .model
            .as_ref()
            .map(|model| !model.trim().is_empty())
            .unwrap_or(false);
    let backend = readiness_payload
        .as_ref()
        .and_then(|payload| payload.get("semanticBackend"))
        .or_else(|| {
            health_payload
                .as_ref()
                .and_then(|payload| payload.get("semanticBackend"))
        })
        .or_else(|| {
            index
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("semanticBackend"))
        })
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let active = backend == "embedding";
    let provider = config
        .embedding
        .provider
        .as_ref()
        .and_then(|provider| serde_json::to_string(provider).ok())
        .map(|provider| provider.trim_matches('"').to_string());
    let message = match (configured, active, backend.as_str()) {
        (true, true, _) => "embedding backend is active".to_string(),
        (true, false, "unknown") => {
            "embedding is configured, but active backend is not known yet".to_string()
        }
        (true, false, backend) => {
            format!("embedding is configured, but current backend is {backend}")
        }
        (false, _, _) => "embedding is not configured".to_string(),
    };

    EmbeddingDiagnostics {
        configured,
        active,
        backend,
        message,
        provider,
        model: config.embedding.model.clone(),
        base_url: config.embedding.base_url.clone(),
    }
}

fn check_vault(config: &ResolvedServiceConfig) -> CheckReport {
    let resolved = match absolute_path(&config.vault_path) {
        Ok(path) => path,
        Err(error) => {
            return CheckReport {
                name: "vault".into(),
                status: "fail".into(),
                message: error.to_string(),
                details: None,
            }
        }
    };
    match fs::metadata(&resolved) {
        Ok(metadata) if metadata.is_dir() => match fs::read_dir(&resolved) {
            Ok(_) => CheckReport {
                name: "vault".into(),
                status: "ok".into(),
                message: "vault is readable".into(),
                details: Some(serde_json::json!({ "path": resolved })),
            },
            Err(error) => CheckReport {
                name: "vault".into(),
                status: "fail".into(),
                message: if is_permission_denied(&error) {
                    macos_vault_access_guidance(&resolved, false)
                } else {
                    error.to_string()
                },
                details: None,
            },
        },
        _ => CheckReport {
            name: "vault".into(),
            status: "fail".into(),
            message: format!(
                "Vault path does not exist or is not a directory: {}",
                resolved.display()
            ),
            details: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Per-mount doctor checks
// ---------------------------------------------------------------------------

/// The minimum Node major version the LiveSync sidecar runs on.
///
/// Mirrors `sidecar/livesync-sidecar/package.json`'s `engines.node`, and the esbuild
/// `target: "node20"` the bundle is compiled against — so this is a real floor, not a
/// conservative guess: the bundle can contain syntax an older Node cannot parse.
const SIDECAR_NODE_MIN_MAJOR: u64 = 20;

/// Every check for one mount, in a fixed order so `doctor` output is stable.
///
/// # What runs when
///
/// The LOCAL checks always run: they need no credentials, no network, and no child
/// process, so there is no reason to make an operator ask for them. For a couchdb mount
/// that means "is the sidecar bundle present" and "is there a Node that can run it" —
/// the two failures that make the mount unstartable and that a `.deb` or Homebrew
/// install can plausibly get wrong.
///
/// The REMOTE probe runs only under `--probe-remote`; see [`probe_mount_remote`].
///
/// # Redaction
///
/// Unchanged from the rest of `doctor`: nothing here resolves a secret except the
/// remote probe (which needs one to connect and never reports it), and a
/// [`SecretRef`] is reported by its identifier only, exactly as
/// [`secret_reference_check`] does for the top-level refs.
fn check_mount_local(mount: &MountConfig) -> Vec<CheckReport> {
    let scope = mount_check_scope(mount);
    match &mount.backend {
        // Nothing to add: the root filesystem mount is already covered by `vault`, and
        // a non-root one is a plain directory whose reachability the same code answers.
        MountBackendConfig::Filesystem { vault_path, .. } => {
            vec![check_mount_directory(&scope, vault_path)]
        }
        MountBackendConfig::Couchdb { sidecar_path, .. } => {
            vec![
                // The mount's OWN `sidecarPath` is honoured, not ignored: a mount that
                // names a hand-built bundle starts fine, and a doctor that probed only
                // the default locations would report it as missing — the exact
                // disagreement between doctor and startup this check exists to prevent.
                check_sidecar_bundle(&scope, sidecar_path.as_deref()),
                check_sidecar_node(&scope),
            ]
        }
        // An algolia mount has no local runtime at all — no child process, no bundle,
        // no index. Everything about it that can be wrong is either in the config
        // (validated at load) or on the remote (`--probe-remote`). Saying so is better
        // than emitting no line, which reads as "not checked".
        MountBackendConfig::Algolia { .. } => vec![CheckReport {
            name: format!("{scope}.local"),
            status: "ok".into(),
            message: "an algolia mount has no local runtime to check; use --probe-remote to \
                      contact the index"
                .into(),
            details: None,
        }],
    }
}

/// The check-name prefix for one mount: `mount.<id>`. Keyed by id rather than by
/// `mountAt` because the id is what every other diagnostic and error message names,
/// and because the root mount's `mountAt` is the empty string.
fn mount_check_scope(mount: &MountConfig) -> String {
    format!("mount.{}", mount.id)
}

fn check_mount_directory(scope: &str, vault_path: &Path) -> CheckReport {
    let name = format!("{scope}.vault");
    match fs::metadata(vault_path) {
        Ok(metadata) if metadata.is_dir() => match fs::read_dir(vault_path) {
            Ok(_) => CheckReport {
                name,
                status: "ok".into(),
                message: "mount directory is readable".into(),
                details: Some(serde_json::json!({ "path": vault_path })),
            },
            Err(error) => CheckReport {
                name,
                status: "fail".into(),
                message: if is_permission_denied(&error) {
                    macos_vault_access_guidance(vault_path, false)
                } else {
                    error.to_string()
                },
                details: None,
            },
        },
        _ => CheckReport {
            name,
            status: "fail".into(),
            message: format!(
                "mount directory does not exist or is not a directory: {}",
                vault_path.display()
            ),
            details: None,
        },
    }
}

/// Can the LiveSync sidecar bundle be found at all?
///
/// Reuses the SERVER's own probe rather than re-deriving the paths, so a `doctor` that
/// says "located" cannot disagree with a startup that says "could not locate". This is
/// the check the Linux package smoke test asserts on: it proves the packaged bundle
/// landed where the binary looks for it, with no CouchDB anywhere in sight.
///
/// A `warn` rather than a `fail`, for the same reason [`check_sidecar_node`] is: a
/// couchdb mount is experimental and non-root, so an absent bundle degrades that mount
/// while the vault root keeps serving. Making it a `fail` would send `doctor` to exit 1
/// for every Homebrew install with a couchdb mount, because the formula deliberately does
/// not ship the bundle — see `docs/release-checklist.md`.
fn check_sidecar_bundle(scope: &str, sidecar_path: Option<&Path>) -> CheckReport {
    let name = format!("{scope}.sidecar-bundle");
    match deep_obsidian_backend::sidecar::locate_sidecar_bundle(sidecar_path) {
        Ok(path) => CheckReport {
            name,
            status: "ok".into(),
            message: "livesync sidecar bundle located".into(),
            details: Some(serde_json::json!({ "path": path })),
        },
        Err(error) => CheckReport {
            name,
            status: "warn".into(),
            message: error.to_string(),
            details: None,
        },
    }
}

/// Is there a Node the sidecar can run on?
///
/// A `warn`, never a `fail`, when Node is absent or too old: the mount cannot start,
/// but `doctor`'s exit code gates on `fail` and a couchdb mount is EXPERIMENTAL and
/// non-root — the vault root keeps serving without it. Exiting non-zero would make
/// `doctor` unusable as a health gate on a host that deliberately has no Node, which is
/// the default the `.deb` ships (Node is a Recommends).
fn check_sidecar_node(scope: &str) -> CheckReport {
    let name = format!("{scope}.sidecar-node");
    let node = deep_obsidian_backend::sidecar::sidecar_node_command();
    let output = ProcessCommand::new(&node).arg("--version").output();
    let version = match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        Ok(output) => {
            return CheckReport {
                name,
                status: "warn".into(),
                message: format!(
                    "`{} --version` failed with status {}; a couchdb mount cannot start without \
                     Node >= {SIDECAR_NODE_MIN_MAJOR}",
                    node.to_string_lossy(),
                    output.status
                ),
                details: None,
            }
        }
        Err(_) => {
            return CheckReport {
                name,
                status: "warn".into(),
                message: format!(
                    "node was not found (looked for `{}`); a couchdb mount needs Node >= \
                     {SIDECAR_NODE_MIN_MAJOR}. Install it, or set {} to the executable. Every \
                     other mount kind works without Node.",
                    node.to_string_lossy(),
                    deep_obsidian_backend::sidecar::SIDECAR_NODE_ENV
                ),
                details: None,
            }
        }
    };
    match node_major_version(&version) {
        Some(major) if major >= SIDECAR_NODE_MIN_MAJOR => CheckReport {
            name,
            status: "ok".into(),
            message: format!("node {version} satisfies the sidecar's >= {SIDECAR_NODE_MIN_MAJOR}"),
            details: Some(serde_json::json!({ "version": version })),
        },
        Some(major) => CheckReport {
            name,
            status: "warn".into(),
            message: format!(
                "node {version} is below the sidecar's floor: major {major} < \
                 {SIDECAR_NODE_MIN_MAJOR}. The bundle is compiled for node20 and may not even \
                 parse on an older runtime."
            ),
            details: Some(serde_json::json!({ "version": version })),
        },
        None => CheckReport {
            name,
            status: "warn".into(),
            message: format!("could not parse a major version from node's output: {version:?}"),
            details: Some(serde_json::json!({ "version": version })),
        },
    }
}

/// The leading integer of a `v20.11.1`-style version string.
///
/// Tolerates a missing `v` and any suffix, and refuses anything with no leading digits
/// rather than guessing — a mis-parse here would report a too-old Node as acceptable.
fn node_major_version(version: &str) -> Option<u64> {
    let digits: String = version
        .trim()
        .trim_start_matches('v')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Contact one remote-backed mount, READ-ONLY, and report what it said.
///
/// # Why this is opt-in
///
/// It is the only part of `doctor` that needs credentials and network. It resolves the
/// mount's secret through the shared store (which on macOS can prompt for keychain
/// access), opens a connection, and — for couchdb — starts the sidecar child process.
/// A diagnostic command must not do any of that unless asked.
///
/// # Why read-only, structurally
///
/// A couchdb mount is probed through a supervisor built in [`SidecarMode::ReadOnly`]
/// regardless of the mount's configured mode, and an algolia mount through its
/// `status()`, which issues `getSettings` and reads counts. Neither path has a write in
/// it, so `--probe-remote` cannot mutate a shared corpus even when the mount is
/// writable.
///
/// # Why a failure is a `warn`
///
/// An unreachable remote-backed mount does not make the INSTALL unhealthy: these mounts
/// are experimental and non-root, and the server starts degraded rather than failing
/// when one cannot be served. `doctor` exits non-zero on `fail` only, so a laptop off
/// the VPN must not make it report a broken install.
async fn probe_mount_remote(config: &ResolvedServiceConfig, mount: &MountConfig) -> CheckReport {
    let scope = mount_check_scope(mount);
    let name = format!("{scope}.remote");
    match &mount.backend {
        MountBackendConfig::Filesystem { .. } => CheckReport {
            name,
            status: "skip".into(),
            message: "a filesystem mount has no remote to probe".into(),
            details: None,
        },
        MountBackendConfig::Couchdb { .. } => {
            match crate::couchdb_transfer::probe_compatibility(config, &mount.id).await {
                Ok(status) => {
                    // A non-`ok` compatibility status arrives from a SUCCESSFUL
                    // handshake — it is the sidecar's diagnosis, not a transport
                    // failure — so it is reported as the answer it is.
                    let ok = status == "ok";
                    CheckReport {
                        name,
                        status: if ok { "ok".into() } else { "warn".into() },
                        message: format!("couchdb handshake reported compatibility: {status}"),
                        details: Some(serde_json::json!({ "compatibility": status })),
                    }
                }
                Err(error) => CheckReport {
                    name,
                    status: "warn".into(),
                    message: format!("couchdb probe failed: {error}"),
                    details: None,
                },
            }
        }
        MountBackendConfig::Algolia { .. } => {
            match crate::algolia_cmd::status(config, &mount.id).await {
                Ok(status) => CheckReport {
                    name,
                    status: if status.reachable {
                        "ok".into()
                    } else {
                        "warn".into()
                    },
                    message: format!(
                        "algolia index {}: reachable={} notes={}",
                        if status.main_provisioned {
                            "provisioned"
                        } else {
                            "not provisioned"
                        },
                        status.reachable,
                        status.notes
                    ),
                    // The same fields `algolia status` prints, and for the same reason
                    // they are safe there: counts and provisioning state, never the key
                    // and never the resolved credential.
                    details: Some(serde_json::json!({
                        "reachable": status.reachable,
                        "mainProvisioned": status.main_provisioned,
                        "historyProvisioned": status.history_provisioned,
                        "notes": status.notes,
                        "writable": status.writable,
                    })),
                },
                Err(error) => CheckReport {
                    name,
                    status: "warn".into(),
                    message: format!("algolia probe failed: {error}"),
                    details: None,
                },
            }
        }
    }
}

fn check_index_dir(config: &ResolvedServiceConfig) -> CheckReport {
    match assert_creatable_directory(&config.index_dir) {
        Ok(_) => CheckReport {
            name: "index-dir".into(),
            status: "ok".into(),
            message: "index directory can be created or is writable".into(),
            details: Some(serde_json::json!({ "path": config.index_dir })),
        },
        Err(error) => CheckReport {
            name: "index-dir".into(),
            status: "fail".into(),
            message: error.to_string(),
            details: None,
        },
    }
}

fn check_rg() -> CheckReport {
    let rg = deep_obsidian_backend::resolve_ripgrep();
    match ProcessCommand::new(&rg).arg("--version").output() {
        Ok(output) if output.status.success() => CheckReport {
            name: "rg".into(),
            status: "ok".into(),
            message: format!("ripgrep is available ({})", rg.display()),
            details: Some(serde_json::json!({
                "version": String::from_utf8_lossy(&output.stdout).trim(),
                "path": rg.display().to_string(),
            })),
        },
        _ => CheckReport {
            name: "rg".into(),
            status: "fail".into(),
            message: "ripgrep (rg) not found. Install it (e.g. `brew install ripgrep`) or set DEEP_OBSIDIAN_RIPGREP to its absolute path.".into(),
            details: None,
        },
    }
}

fn secret_reference_check(name: &str, reference: Option<&SecretRef>) -> Option<CheckReport> {
    let reference = reference?;
    let resolver = SecretResolver::new();
    let kind = match reference {
        SecretRef::OsKeyring { .. } => "osKeyring",
        SecretRef::EncryptedFile { .. } => "encryptedFile",
    };
    match resolver.get(reference) {
        Ok(Some(_)) => Some(CheckReport {
            name: name.into(),
            status: "ok".into(),
            message: "secret reference resolved".into(),
            details: Some(serde_json::json!({
                "kind": kind,
                "configured": true,
                "resolved": true,
            })),
        }),
        Ok(None) => Some(CheckReport {
            name: name.into(),
            status: "fail".into(),
            message: "secret reference is configured but missing".into(),
            details: Some(serde_json::json!({
                "kind": kind,
                "configured": true,
                "resolved": false,
            })),
        }),
        Err(error) => Some(CheckReport {
            name: name.into(),
            status: "fail".into(),
            message: error.to_string(),
            details: Some(serde_json::json!({
                "kind": kind,
                "configured": true,
                "resolved": false,
            })),
        }),
    }
}

fn check_port(config: &ResolvedServiceConfig) -> CheckReport {
    match TcpListener::bind((config.http.host.as_str(), config.http.port)) {
        Ok(listener) => {
            drop(listener);
            CheckReport {
                name: "http-port".into(),
                status: "ok".into(),
                message: "port is free; service is not running".into(),
                details: Some(serde_json::json!({
                    "host": config.http.host,
                    "port": config.http.port,
                })),
            }
        }
        Err(_) => CheckReport {
            name: "http-port".into(),
            status: "warn".into(),
            message: "port is in use".into(),
            details: Some(serde_json::json!({
                "host": config.http.host,
                "port": config.http.port,
            })),
        },
    }
}

async fn check_health(client: &Client, endpoints: &ServiceEndpoints) -> CheckReport {
    match client.get(&endpoints.health).send().await {
        Ok(response) => {
            let status = response.status();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body_text = response.text().await.unwrap_or_default();
            let body = if content_type.contains("application/json") {
                serde_json::from_str::<Value>(&body_text)
                    .unwrap_or_else(|_| Value::String(body_text))
            } else {
                Value::String(body_text)
            };
            CheckReport {
                name: "health".into(),
                status: if status.is_success() { "ok" } else { "fail" }.into(),
                message: if status.is_success() {
                    "health endpoint responded successfully".into()
                } else {
                    format!("health endpoint returned status {}", status.as_u16())
                },
                details: if status.is_success() {
                    Some(serde_json::json!({
                        "status": status.as_u16(),
                        "body": body,
                    }))
                } else {
                    Some(serde_json::json!({
                        "status": status.as_u16(),
                    }))
                },
            }
        }
        Err(error) => CheckReport {
            name: "health".into(),
            status: "fail".into(),
            message: error.to_string(),
            details: Some(serde_json::json!({
                "error": error.to_string(),
            })),
        },
    }
}

async fn check_readiness(client: &Client, url: &str) -> CheckReport {
    match client.get(url).send().await {
        Ok(response) => {
            let status = response.status();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body_text = response.text().await.unwrap_or_default();
            let body = if content_type.contains("application/json") {
                serde_json::from_str::<Value>(&body_text)
                    .unwrap_or_else(|_| Value::String(body_text))
            } else {
                Value::String(body_text)
            };
            CheckReport {
                name: "readiness".into(),
                status: if status.is_success() { "ok" } else { "warn" }.into(),
                message: if status.is_success() {
                    "readiness endpoint responded successfully".into()
                } else {
                    format!("readiness endpoint returned status {}", status.as_u16())
                },
                details: Some(serde_json::json!({
                    "url": url,
                    "status": status.as_u16(),
                    "body": body,
                })),
            }
        }
        Err(error) => CheckReport {
            name: "readiness".into(),
            status: "warn".into(),
            message: error.to_string(),
            details: Some(serde_json::json!({
                "url": url,
                "error": error.to_string(),
            })),
        },
    }
}

fn http_client(timeout_ms: u64) -> Result<Client> {
    Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .context("failed to build HTTP client")
}

/// Build an HTTP client that sends `Authorization: Bearer <token>` on every
/// request when a token is available, so `probe` works against an authenticated
/// server.
fn http_client_with_bearer(timeout_ms: u64, token: Option<String>) -> Result<Client> {
    let mut builder = Client::builder().timeout(std::time::Duration::from_millis(timeout_ms));
    if let Some(token) = token {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(mut value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) {
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
    }
    builder.build().context("failed to build HTTP client")
}

/// Resolve the bearer token for probing: the env override first, then the
/// configured secret reference. Returns `None` when auth is not configured.
fn resolve_probe_token(service: &ResolvedServiceConfig) -> Option<String> {
    if let Ok(token) = std::env::var("DEEP_OBSIDIAN_AUTH_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }
    SecretResolver::new()
        .resolve_auth_token(&service.auth)
        .ok()
        .flatten()
        .map(|secret| secret.expose_secret().to_string())
}

fn redact_config(config: &PersistedServiceConfig) -> PersistedServiceConfig {
    config.clone()
}

/// One `doctor` line describing a mount: where its content lives, and where its
/// own search index lives.
///
/// The index directory is the additive half. Each mount indexes independently now,
/// so an operator diagnosing a stale or oversized index needs to know which
/// directory belongs to which mount. The ROOT mount's is the resolved top-level one
/// (also printed as `index sqlite`); a non-root mount's is its explicit `indexDir`
/// or the id-keyed default beneath the root's.
///
/// Only reached for a config that DECLARED a mount table, so a legacy `vaultPath`
/// install's `doctor` output is unchanged.
fn render_mount_line(mount: &MountConfig, root_index_dir: Option<&Path>) -> String {
    let mount_at = if mount.mount_at.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", mount.mount_at)
    };
    let (location, declared_index_dir) = match &mount.backend {
        MountBackendConfig::Filesystem {
            vault_path,
            index_dir,
        } => (vault_path.display().to_string(), index_dir.clone()),
        // `url` and `database` only: no credential, and no `passwordRef`
        // identifier either. The url is validated at config load to carry no
        // userinfo (`ConfigError::CouchdbUrlHasUserinfo`), which is what makes
        // printing it verbatim safe.
        MountBackendConfig::Couchdb {
            url,
            database,
            index_dir,
            ..
        } => (format!("{url}/{database} (read-only)"), index_dir.clone()),
        // `appId`, `indexName` and — when set — `baseUrl`. No key, and no `apiKeyRef`
        // identifier either. `baseUrl` is validated at config load to carry no userinfo
        // (`ConfigError::AlgoliaBaseUrlHasUserinfo`), which is what makes printing it
        // verbatim safe; the default endpoint is derived from `appId` and named as such
        // rather than invented here, so this line never implies a url nobody configured.
        MountBackendConfig::Algolia {
            app_id,
            index_name,
            base_url,
            writable,
            index_dir,
            ..
        } => (
            format!(
                "{app_id}/{index_name}{}{}",
                match base_url {
                    Some(base_url) => format!(" via {base_url}"),
                    None => String::new(),
                },
                if *writable { "" } else { " (read-only)" }
            ),
            index_dir.clone(),
        ),
    };
    let index_dir = if mount.mount_at.is_empty() {
        root_index_dir.map(Path::to_path_buf)
    } else {
        declared_index_dir
            .or_else(|| root_index_dir.map(|root| default_mount_index_dir(root, &mount.id)))
    };
    let index_note = index_dir
        .map(|dir| format!(" [index: {}]", dir.display()))
        .unwrap_or_default();
    format!(
        "mount {} at {} ({}): {}{}",
        mount.id,
        mount_at,
        mount.backend.kind_name(),
        location,
        index_note
    )
}

fn render_doctor_report(report: &DoctorReport) -> String {
    let mut output = String::new();
    // A mounts config writes no top-level `vaultPath`, so fall back to the root
    // mount's path. Legacy configs always have `vaultPath` and so render exactly
    // the same line as before.
    let vault = report
        .config
        .vault_path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| {
            report
                .config
                .mounts
                .as_ref()?
                .iter()
                .find(|mount| mount.mount_at.is_empty())
                .map(|mount| match &mount.backend {
                    MountBackendConfig::Filesystem { vault_path, .. } => {
                        vault_path.display().to_string()
                    }
                    // Unreachable: neither a couchdb nor an algolia mount can be the
                    // root mount (`ConfigError::CouchdbRootMountUnsupported`,
                    // `ConfigError::AlgoliaRootMountUnsupported`), which is precisely
                    // what keeps this line able to name a directory.
                    MountBackendConfig::Couchdb { .. } | MountBackendConfig::Algolia { .. } => {
                        "(missing)".to_string()
                    }
                })
        })
        .unwrap_or_else(|| "(missing)".to_string());
    let transport = report
        .config
        .transport
        .map(|transport| {
            serde_json::to_string(&transport)
                .unwrap_or_else(|_| "\"stdio\"".to_string())
                .trim_matches('"')
                .to_string()
        })
        .unwrap_or_else(|| "(missing)".to_string());
    let index_size = report
        .index
        .size_bytes
        .map(|bytes| bytes.to_string())
        .unwrap_or_else(|| "n/a".to_string());
    let _ = writeln!(
        &mut output,
        "config: {} ({})",
        report.config_diagnostics.path.display(),
        if report.config_diagnostics.exists {
            "found"
        } else {
            "not found"
        }
    );
    let _ = writeln!(
        &mut output,
        "config precedence: cli > config > env > default"
    );
    let _ = writeln!(&mut output, "vault: {}", vault);
    // One line per mount, and ONLY for a config that declared a mount table.
    // A legacy config has `mounts: None` and so gains no line at all, keeping
    // `doctor` output for existing installs unchanged.
    if let Some(mounts) = &report.config.mounts {
        // The RESOLVED root index directory, which is what a non-root mount's
        // default is derived from. `report.index.path` is `<root index dir>/
        // index.sqlite`, so its parent is that directory however it was resolved
        // (explicit `indexDir`, the root mount's own, or the packaged default).
        let root_index_dir = report.index.path.parent();
        for mount in mounts {
            let _ = writeln!(&mut output, "{}", render_mount_line(mount, root_index_dir));
        }
    }
    let _ = writeln!(&mut output, "index sqlite: {}", report.index.path.display());
    let _ = writeln!(&mut output, "index size bytes: {}", index_size);
    let _ = writeln!(&mut output, "transport: {}", transport);
    let _ = writeln!(&mut output, "mcp endpoint: {}", report.endpoints.mcp);
    let _ = writeln!(&mut output, "health endpoint: {}", report.endpoints.health);
    if let Some(readiness) = &report.endpoints.readiness {
        let _ = writeln!(&mut output, "readiness endpoint: {}", readiness);
    }
    let _ = writeln!(
        &mut output,
        "auto reindex: {} (debounce={}ms interval={}ms)",
        report.service.auto_reindex.enabled,
        report.service.auto_reindex.debounce_ms,
        report.service.auto_reindex.interval_ms
    );
    let embedding_state = if report.service.embedding.active {
        "active"
    } else if report.service.embedding.configured {
        "inactive"
    } else {
        "not configured"
    };
    let _ = writeln!(
        &mut output,
        "embedding: {} (backend={})",
        embedding_state, report.service.embedding.backend
    );
    if let Some(model) = &report.service.embedding.model {
        let _ = writeln!(&mut output, "embedding model: {}", model);
    }
    if let Some(base_url) = &report.service.embedding.base_url {
        let _ = writeln!(&mut output, "embedding base URL: {}", base_url);
    }
    if let Some(last_refresh) = &report.service.last_refresh {
        let _ = writeln!(&mut output, "last refresh: {}", last_refresh);
    }
    if let Some(last_error) = &report.service.last_error {
        let _ = writeln!(&mut output, "last error: {}", last_error);
    }
    let _ = writeln!(&mut output);
    for check in &report.checks {
        let _ = writeln!(
            &mut output,
            "[{}] {}: {}",
            check.status, check.name, check.message
        );
    }
    output.trim_end().to_string()
}

fn render_setup_service_report(report: &SetupServiceReport) -> String {
    let mut output = String::new();
    for message in &report.messages {
        let _ = writeln!(&mut output, "{message}");
    }
    let _ = writeln!(&mut output, "mcp endpoint: {}", report.endpoints.mcp);
    let _ = writeln!(&mut output, "health endpoint: {}", report.endpoints.health);
    if let Some(readiness) = &report.endpoints.readiness {
        let _ = writeln!(&mut output, "readiness endpoint: {}", readiness);
    }
    for action in report
        .mcp
        .iter()
        .chain(report.skills.iter())
        .chain(report.vault_snippets.iter())
    {
        let path = action
            .path
            .as_ref()
            .map(|path| format!(" ({})", path.display()))
            .unwrap_or_default();
        let _ = writeln!(
            &mut output,
            "{} [{}]{}: {}",
            action.target, action.status, path, action.message
        );
    }
    output.trim_end().to_string()
}

fn render_probe_report(report: &ProbeReport) -> String {
    let mut output = String::new();
    let _ = writeln!(&mut output, "mcp endpoint: {}", report.endpoints.mcp);
    let _ = writeln!(&mut output, "health endpoint: {}", report.endpoints.health);
    let _ = writeln!(&mut output, "health ok: {}", report.health.ok);
    let _ = writeln!(&mut output, "mcp ok: {}", report.mcp.ok);
    if !report.health.ok {
        if let Some(error) = &report.health.error {
            let _ = writeln!(&mut output, "health error: {}", error);
        }
    }
    if !report.mcp.ok {
        if let Some(error) = &report.mcp.error {
            let _ = writeln!(&mut output, "mcp error: {}", error);
        }
    }
    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        embedding_diagnostics, enable_obsidian_snippets, inspect_index, normalize_cli_args,
        redact_config, render_mount_line, setup_service, IndexDiagnostics, MountConfig,
        INDEX_SQLITE_FILENAME, SUBCOMMAND_VALUE_FLAGS,
    };
    use crate::config::{ResolvedRuntimeConfig, ResolvedSource, ResolvedSources};
    use deep_obsidian_types::{
        AutoReindexConfig, EmbeddingConfig, EmbeddingConfigInput, EmbeddingProvider, HttpConfig,
        MountBackendConfig, PersistedServiceConfig, ResolvedServiceConfig, SecretRef, StdioMode,
        TransportMode,
    };
    use rusqlite::Connection;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "deep-obsidian-commands-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    /// A filesystem mount over a REAL directory, for the checks that touch the disk.
    /// [`filesystem_mount`] points at a fixed `/vaults/<id>` that does not exist, which
    /// is what makes it suitable for the pure string-rendering tests and unsuitable here.
    fn filesystem_mount_at(id: &str, mount_at: &str, vault_path: &Path) -> MountConfig {
        MountConfig {
            id: id.to_string(),
            mount_at: mount_at.to_string(),
            backend: MountBackendConfig::Filesystem {
                vault_path: vault_path.to_path_buf(),
                index_dir: None,
            },
            recall_weight: None,
            unknown: Default::default(),
        }
    }

    fn resolved_config(vault_path: &Path, index_dir: &Path) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            federated_rerank: true,
            vault_path: vault_path.to_path_buf(),
            index_dir: index_dir.to_path_buf(),
            mounts: Vec::new(),
            experimental: Default::default(),
            transport: TransportMode::Http,
            stdio_mode: StdioMode::Auto,
            http: HttpConfig {
                host: "127.0.0.1".to_string(),
                port: 4100,
                mcp_path: "/mcp".to_string(),
                health_path: "/healthz".to_string(),
            },
            auto_reindex: AutoReindexConfig {
                enabled: true,
                debounce_ms: 1500,
                interval_ms: 30000,
            },
            embedding: EmbeddingConfig::default(),
            artifact_embedding: EmbeddingConfig::default(),
            auth: deep_obsidian_types::AuthConfig::default(),
            config_file_path: None,
        }
    }

    fn minimal_index_diagnostics(index_dir: &Path, backend: &str) -> IndexDiagnostics {
        IndexDiagnostics {
            path: index_dir.join(INDEX_SQLITE_FILENAME),
            exists: true,
            status: "ok".to_string(),
            message: "test index".to_string(),
            size_bytes: None,
            schema_version: None,
            user_version: None,
            metadata: Some(serde_json::json!({ "semanticBackend": backend })),
            note_rows: None,
            chunk_rows: None,
            file_snapshot_rows: None,
        }
    }

    #[test]
    fn provision_auth_token_dry_run_sets_ref_without_storing() {
        let mut auth = deep_obsidian_types::AuthConfig::default();
        super::provision_auth_token(&mut auth, true, false).expect("dry-run provision");
        assert!(auth.enabled);
        assert!(auth.token_ref.is_some());
    }

    #[test]
    fn deprovision_auth_token_clears_enabled_and_ref() {
        let mut auth = deep_obsidian_types::AuthConfig {
            enabled: true,
            token_ref: Some(deep_obsidian_types::SecretRef::EncryptedFile {
                id: "http-auth-token".to_string(),
            }),
            allowed_origins: Vec::new(),
        };
        // dry-run must not touch the secret store yet still clear the config.
        super::deprovision_auth_token(&mut auth, true);
        assert!(!auth.enabled);
        assert!(auth.token_ref.is_none());
    }

    #[test]
    fn deprovision_auth_token_on_already_disabled_is_noop() {
        let mut auth = deep_obsidian_types::AuthConfig::default();
        super::deprovision_auth_token(&mut auth, false);
        assert!(!auth.enabled);
        assert!(auth.token_ref.is_none());
    }

    #[test]
    fn normalize_cli_args_maps_boolean_assignment_flags() {
        let args = vec![
            "doctor".to_string(),
            "--json=false".to_string(),
            "--dry-run".to_string(),
            "false".to_string(),
        ];
        let normalized = normalize_cli_args(&args).expect("normalize args");
        assert_eq!(
            normalized,
            vec![
                "doctor".to_string(),
                "--no-json".to_string(),
                "--no-dry-run".to_string()
            ]
        );
    }

    #[test]
    fn normalize_cli_args_maps_vault_path_alias_and_auto_reindex_values() {
        let args = vec![
            "serve".to_string(),
            "--vault-path=tests/fixtures/vault".to_string(),
            "--auto-reindex".to_string(),
            "false".to_string(),
        ];
        let normalized = normalize_cli_args(&args).expect("normalize args");
        assert_eq!(
            normalized,
            vec![
                "serve".to_string(),
                "--vault=tests/fixtures/vault".to_string(),
                "--no-auto-reindex".to_string(),
            ]
        );
    }

    #[test]
    fn normalize_cli_args_ignores_non_standalone_version_flags() {
        let args = vec!["doctor".to_string(), "-v".to_string()];
        let normalized = normalize_cli_args(&args).expect("normalize args");
        assert_eq!(normalized, vec!["doctor".to_string()]);
    }

    #[test]
    fn normalize_cli_args_promotes_positional_vault_path_for_subcommands() {
        let args = vec!["doctor".to_string(), "tests/fixtures/vault".to_string()];
        let normalized = normalize_cli_args(&args).expect("normalize args");
        assert_eq!(
            normalized,
            vec![
                "doctor".to_string(),
                "--vault".to_string(),
                "tests/fixtures/vault".to_string(),
            ]
        );
    }

    /// Every subcommand survives normalization, INCLUDING the two that take nested
    /// subcommands of their own.
    ///
    /// A regression test with teeth: `couchdb` and `algolia` were absent from
    /// `is_known_command`, so `couchdb export --mount x --out y` had its `couchdb`
    /// promoted to `--vault couchdb` and clap then rejected `export` as an unrecognized
    /// TOP-LEVEL subcommand. The whole family was unreachable from the command line while
    /// every library-level test kept passing, because the tests call the functions
    /// directly and never go through argv.
    #[test]
    fn normalize_cli_args_keeps_every_subcommand_including_the_nested_ones() {
        for command in [
            "serve",
            "setup-service",
            "doctor",
            "print-config",
            "probe",
            "couchdb",
            "algolia",
            "help",
            "version",
        ] {
            let normalized = normalize_cli_args(&[command.to_string()]).expect("normalize args");
            assert_eq!(
                normalized,
                vec![command.to_string()],
                "{command} was swallowed as a positional vault path"
            );
        }

        // The shape that actually broke: a nested subcommand behind a global flag.
        for (command, sub) in [("couchdb", "export"), ("algolia", "status")] {
            let args = vec![
                "--config".to_string(),
                "/tmp/config.json".to_string(),
                command.to_string(),
                sub.to_string(),
                "--mount".to_string(),
                "wiki".to_string(),
            ];
            let normalized = normalize_cli_args(&args).expect("normalize args");
            assert_eq!(
                normalized, args,
                "{command} {sub} must pass through untouched"
            );
            assert!(
                !normalized.iter().any(|token| token == "--vault"),
                "{command} must not be mistaken for a vault path: {normalized:?}"
            );
        }
    }

    /// Every subcommand value flag keeps its VALUE.
    ///
    /// The second half of the same bug: with `--mount` unknown to the normalizer, `wiki` was
    /// left looking like a bare positional and promoted to `--vault wiki`, so clap reported
    /// "a value is required for '--mount'" — naming the flag whose value had been stolen
    /// rather than the theft.
    #[test]
    fn normalize_cli_args_consumes_every_subcommand_flags_value() {
        for flag in SUBCOMMAND_VALUE_FLAGS {
            for args in [
                vec![
                    "algolia".to_string(),
                    "seed".to_string(),
                    (*flag).to_string(),
                    "a-value".to_string(),
                ],
                vec![
                    "algolia".to_string(),
                    "seed".to_string(),
                    format!("{flag}=a-value"),
                ],
            ] {
                let normalized = normalize_cli_args(&args).expect("normalize args");
                assert!(
                    !normalized.iter().any(|token| token == "--vault"),
                    "{flag}'s value was promoted to a vault path: {normalized:?}"
                );
                assert!(
                    normalized.iter().any(|token| token.contains("a-value")),
                    "{flag}'s value was lost: {normalized:?}"
                );
            }
        }

        // A path-shaped value is not special-cased into a vault path either.
        let normalized = normalize_cli_args(&[
            "algolia".to_string(),
            "dump".to_string(),
            "--mount".to_string(),
            "wiki".to_string(),
            "--out".to_string(),
            "/tmp/backup".to_string(),
        ])
        .expect("normalize args");
        assert!(
            !normalized.iter().any(|token| token == "--vault"),
            "{normalized:?}"
        );
    }

    #[test]
    fn normalize_cli_args_promotes_positional_vault_path_for_default_serve() {
        let args = vec!["tests/fixtures/vault".to_string()];
        let normalized = normalize_cli_args(&args).expect("normalize args");
        assert_eq!(
            normalized,
            vec!["--vault".to_string(), "tests/fixtures/vault".to_string(),]
        );
    }

    #[test]
    fn persisted_config_uses_secret_reference_without_plaintext() {
        let config = PersistedServiceConfig {
            embedding: Some(EmbeddingConfigInput {
                api_key_ref: Some(SecretRef::EncryptedFile {
                    id: "openai-embedding".to_string(),
                }),
                ..EmbeddingConfigInput::default()
            }),
            ..PersistedServiceConfig::default()
        };

        let redacted = redact_config(&config);
        let serialized = serde_json::to_string(&redacted).expect("serialize redacted config");

        assert!(!serialized.contains("super-secret"));
        assert!(serialized.contains("apiKeyRef"));
        assert!(serialized.contains("encryptedFile"));
    }

    #[test]
    fn embedding_diagnostics_reports_active_backend() {
        let root = unique_temp_dir("embedding-diagnostics");
        let vault = root.join("vault");
        let index_dir = root.join("index");
        fs::create_dir_all(&vault).expect("create vault");
        fs::create_dir_all(&index_dir).expect("create index");
        let mut config = resolved_config(&vault, &index_dir);
        config.embedding.provider = Some(EmbeddingProvider::OpenAiCompatible);
        config.embedding.model = Some("qwen3-embedding:0.6b".to_string());
        config.embedding.base_url = Some("http://localhost:11434/v1".to_string());
        let index = minimal_index_diagnostics(&index_dir, "sparse");
        let readiness = Some(serde_json::json!({ "semanticBackend": "embedding" }));

        let diagnostics = embedding_diagnostics(&config, &None, &readiness, &index);

        assert!(diagnostics.configured);
        assert!(diagnostics.active);
        assert_eq!(diagnostics.backend, "embedding");
        assert_eq!(diagnostics.model.as_deref(), Some("qwen3-embedding:0.6b"));
        assert_eq!(
            diagnostics.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn embedding_diagnostics_reports_configured_but_inactive_backend() {
        let root = unique_temp_dir("embedding-diagnostics-inactive");
        let vault = root.join("vault");
        let index_dir = root.join("index");
        fs::create_dir_all(&vault).expect("create vault");
        fs::create_dir_all(&index_dir).expect("create index");
        let mut config = resolved_config(&vault, &index_dir);
        config.embedding.provider = Some(EmbeddingProvider::OpenAiCompatible);
        config.embedding.model = Some("qwen3-embedding:0.6b".to_string());
        let index = minimal_index_diagnostics(&index_dir, "sparse");

        let diagnostics = embedding_diagnostics(&config, &None, &None, &index);

        assert!(diagnostics.configured);
        assert!(!diagnostics.active);
        assert_eq!(diagnostics.backend, "sparse");
        assert!(diagnostics.message.contains("current backend is sparse"));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn inspect_index_reports_sqlite_metadata_without_loading_vault() {
        let root = unique_temp_dir("index-diagnostics");
        let vault = root.join("vault");
        let index_dir = root.join("index");
        fs::create_dir_all(&vault).expect("create vault");
        fs::create_dir_all(&index_dir).expect("create index dir");
        let index_path = index_dir.join(INDEX_SQLITE_FILENAME);
        let connection = Connection::open(&index_path).expect("open sqlite");
        connection
            .execute_batch(
                r#"
                CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE notes (id INTEGER PRIMARY KEY, path TEXT NOT NULL);
                CREATE TABLE chunks (id INTEGER PRIMARY KEY, path TEXT NOT NULL);
                CREATE TABLE file_snapshots (path TEXT PRIMARY KEY, mtime_ms INTEGER NOT NULL, size INTEGER NOT NULL);
                INSERT INTO metadata (key, value) VALUES ('version', '2');
                INSERT INTO metadata (key, value) VALUES ('generatedAt', '2026-05-05T00:00:00Z');
                INSERT INTO notes (id, path) VALUES (1, 'A.md');
                INSERT INTO chunks (id, path) VALUES (1, 'A.md');
                INSERT INTO file_snapshots (path, mtime_ms, size) VALUES ('A.md', 1, 10);
                "#,
            )
            .expect("seed sqlite");

        let diagnostics = inspect_index(&resolved_config(&vault, &index_dir));

        assert!(diagnostics.exists);
        assert_eq!(diagnostics.status, "ok");
        assert_eq!(diagnostics.note_rows, Some(1));
        assert_eq!(diagnostics.chunk_rows, Some(1));
        assert_eq!(diagnostics.file_snapshot_rows, Some(1));
        assert_eq!(
            diagnostics
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("version"))
                .and_then(serde_json::Value::as_str),
            Some("2")
        );
    }

    #[test]
    fn enable_obsidian_snippets_preserves_existing_and_adds_missing_names() {
        let root = unique_temp_dir("appearance-snippets");
        let appearance_path = root.join(".obsidian").join("appearance.json");
        fs::create_dir_all(appearance_path.parent().expect("appearance parent"))
            .expect("create appearance dir");
        fs::write(
            &appearance_path,
            r#"{"theme":"obsidian","enabledCssSnippets":["templates"]}"#,
        )
        .expect("write appearance");

        let enabled = enable_obsidian_snippets(
            &appearance_path,
            &[
                "templates".to_string(),
                "hide-agent-wiki-folders".to_string(),
            ],
        )
        .expect("enable snippets");

        assert_eq!(enabled, 1);
        let appearance: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&appearance_path).expect("read appearance"))
                .expect("parse appearance");
        assert_eq!(
            appearance["enabledCssSnippets"],
            serde_json::json!(["templates", "hide-agent-wiki-folders"])
        );
        assert_eq!(appearance["theme"], "obsidian");
    }

    fn resolved_runtime(
        config_path: PathBuf,
        service: ResolvedServiceConfig,
    ) -> ResolvedRuntimeConfig {
        let default_source = || ResolvedSource::Default;
        ResolvedRuntimeConfig {
            config_path,
            config_file: None,
            service,
            sources: ResolvedSources {
                vault_path: ResolvedSource::Cli,
                index_dir: ResolvedSource::Cli,
                transport: default_source(),
                stdio_mode: default_source(),
                http_host: default_source(),
                http_port: default_source(),
                http_mcp_path: default_source(),
                http_health_path: default_source(),
                auto_reindex_enabled: default_source(),
                auto_reindex_debounce_ms: default_source(),
                auto_reindex_interval_ms: default_source(),
                embedding_provider: default_source(),
                embedding_model: default_source(),
                embedding_base_url: default_source(),
                embedding_api_key_ref: default_source(),
            },
        }
    }

    fn filesystem_mount(id: &str, mount_at: &str, index_dir: Option<&str>) -> MountConfig {
        MountConfig {
            unknown: Default::default(),
            recall_weight: None,
            id: id.to_string(),
            mount_at: mount_at.to_string(),
            backend: MountBackendConfig::Filesystem {
                vault_path: PathBuf::from(format!("/vaults/{id}")),
                index_dir: index_dir.map(PathBuf::from),
            },
        }
    }

    /// Overwriting an existing config must leave a faithful `.bak` copy of the
    /// previous content — the failure mode of the wizard-overwrite accident,
    /// where one wrong answer replaced the whole file with no recovery path.
    /// A no-op rewrite must not create a backup.
    #[test]
    fn setup_service_backs_up_previous_config_on_overwrite() {
        let root = unique_temp_dir("setup-backup");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("vault dir");
        fs::write(vault.join("Home.md"), "# Home\n").expect("seed note");
        let index_dir = root.join("index");
        let config_path = root.join("config.json");
        let backup_path = config_path.with_extension("json.bak");

        let mut service = resolved_config(&vault, &index_dir);

        // First write: nothing to back up.
        let report = setup_service(
            &resolved_runtime(config_path.clone(), service.clone()),
            false,
            false,
            false,
            false,
            false,
            None,
            false,
        )
        .expect("first setup");
        assert!(report.written);
        assert!(!backup_path.exists(), "first write must not leave a backup");
        let first_text = fs::read_to_string(&config_path).expect("config written");

        // Rewriting identical content is not a clobber, so no backup either.
        let report = setup_service(
            &resolved_runtime(config_path.clone(), service.clone()),
            false,
            true,
            false,
            false,
            false,
            None,
            false,
        )
        .expect("idempotent setup");
        assert!(report.written);
        assert!(
            !backup_path.exists(),
            "an unchanged rewrite must not leave a backup"
        );

        // Overwrite with changed content -> .bak holds the previous file.
        service.http.port = 4200;
        let report = setup_service(
            &resolved_runtime(config_path.clone(), service),
            false,
            true,
            false,
            false,
            false,
            None,
            false,
        )
        .expect("overwrite setup");
        assert!(report.written);
        assert!(report
            .messages
            .iter()
            .any(|message| message.starts_with("backed up previous config:")));
        assert_eq!(
            fs::read_to_string(&backup_path).expect("backup exists"),
            first_text,
            "backup must hold the pre-overwrite content"
        );
        let new_text = fs::read_to_string(&config_path).expect("new config");
        assert!(new_text.contains("4200"), "new config written: {new_text}");
    }

    /// `doctor` names each mount's own index directory, so an operator can tell
    /// which directory belongs to which mount.
    #[test]
    fn doctor_mount_lines_name_each_mounts_index_directory() {
        let root_index = Path::new("/data/index");

        // The root mount reports the resolved top-level index dir verbatim.
        assert_eq!(
            render_mount_line(&filesystem_mount("vault", "", None), Some(root_index)),
            "mount vault at / (filesystem): /vaults/vault [index: /data/index]"
        );
        // A non-root mount reports the id-keyed default beneath it...
        assert_eq!(
            render_mount_line(&filesystem_mount("team", "Team", None), Some(root_index)),
            "mount team at /Team (filesystem): /vaults/team [index: /data/index/mounts/team]"
        );
        // ...or its own explicit indexDir when it has one.
        assert_eq!(
            render_mount_line(
                &filesystem_mount("team", "Team", Some("/elsewhere/team-index")),
                Some(root_index)
            ),
            "mount team at /Team (filesystem): /vaults/team [index: /elsewhere/team-index]"
        );
        // With no resolvable root index dir the line degrades to its previous shape
        // rather than printing a guess.
        assert_eq!(
            render_mount_line(&filesystem_mount("team", "Team", None), None),
            "mount team at /Team (filesystem): /vaults/team"
        );
    }

    // -----------------------------------------------------------------------
    // setup-service against a declared mount table
    // -----------------------------------------------------------------------

    /// A mounts config is never rewritten — not even with `--overwrite`, which is the
    /// footgun this pins. `--overwrite` means "replace my agent config"; it does not
    /// mean "regenerate my mount table", and a mount table is the one thing in the file
    /// this command cannot reproduce faithfully.
    #[test]
    fn setup_service_never_rewrites_a_declared_mount_table() {
        let root = unique_temp_dir("setup-mounts");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("vault dir");
        let config_path = root.join("config.json");
        let backup_path = config_path.with_extension("json.bak");

        // A hand-written mounts config, byte-for-byte what must survive.
        let handwritten = r#"{
  "experimental": { "multiVault": true },
  "mounts": [
    { "id": "vault", "mountAt": "", "backend": { "kind": "filesystem", "vaultPath": "VAULT" } },
    { "id": "team", "mountAt": "Team", "backend": { "kind": "filesystem", "vaultPath": "VAULT" } }
  ]
}
"#
        .replace("VAULT", vault.to_str().expect("utf-8 vault path"));
        fs::write(&config_path, &handwritten).expect("seed config");

        let mut service = resolved_config(&vault, &root.join("index"));
        service.mounts = vec![
            filesystem_mount("vault", "", None),
            filesystem_mount("team", "Team", None),
        ];

        let report = setup_service(
            &resolved_runtime(config_path.clone(), service),
            false,
            // --overwrite, deliberately: the point is that it does NOT apply here.
            true,
            false,
            false,
            false,
            None,
            false,
        )
        .expect("setup-service must succeed on a mounts config, not refuse it");

        assert!(!report.written, "a mount table must never be rewritten");
        assert_eq!(
            fs::read_to_string(&config_path).expect("config still there"),
            handwritten,
            "the hand-written config must be untouched, byte for byte"
        );
        assert!(
            !backup_path.exists(),
            "nothing was written, so there is nothing to back up"
        );
        assert!(
            report.messages.iter().any(|message| {
                message.contains("config not written") && message.contains("mount table")
            }),
            "the operator must be told why, and pointed at manual editing: {:?}",
            report.messages
        );
        assert!(
            report
                .messages
                .iter()
                .any(|message| message.contains("skipped: vault-path absolutization")),
            "the skipped rewrites must be named rather than silently omitted: {:?}",
            report.messages
        );
        // Still useful: the endpoint report is produced, which is what `--mcp` writes
        // into an agent's client config.
        assert!(report.endpoints.mcp.contains("/mcp"));

        let _ = fs::remove_dir_all(&root);
    }

    /// Changing auth on a mounts config is REFUSED, and nothing is provisioned.
    ///
    /// The failure this prevents: the config is not rewritten for a mount table, so a
    /// provisioned token would sit in the secret store with nothing referencing it, and
    /// the operator would believe auth was on when the file still said otherwise.
    #[test]
    fn setup_service_refuses_an_auth_change_on_a_mounts_config() {
        let root = unique_temp_dir("setup-mounts-auth");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("vault dir");
        let config_path = root.join("config.json");
        fs::write(&config_path, "{}\n").expect("seed config");

        let mut service = resolved_config(&vault, &root.join("index"));
        service.mounts = vec![filesystem_mount_at("vault", "", &vault)];

        for choice in [Some(true), Some(false)] {
            let error = setup_service(
                &resolved_runtime(config_path.clone(), service.clone()),
                false,
                true,
                false,
                false,
                false,
                choice,
                false,
            )
            .expect_err("an auth change on a mount table must be refused");
            let message = error.to_string();
            assert!(message.contains("cannot change auth"), "{message}");
            // The remedy is named, not just the refusal.
            assert!(message.contains("by hand"), "{message}");
        }
        assert_eq!(
            fs::read_to_string(&config_path).expect("config"),
            "{}\n",
            "a refused run must not have touched the file"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// The wizard refuses a mounts config UP FRONT, and the refusal names the mount table
    /// rather than auth.
    ///
    /// The wizard passes `Some(...)` for auth unconditionally, so without this guard its
    /// first error on such a file would be the auth refusal — after every prompt had been
    /// answered, and blaming the wrong thing.
    #[test]
    fn the_wizard_refuses_a_mounts_config_before_prompting() {
        let path = Path::new("/config/config.json");

        // No file, and a file with no mount table: nothing to refuse.
        super::refuse_wizard_on_a_mounts_config(path, None).expect("no config is fine");
        let legacy = PersistedServiceConfig {
            vault_path: Some(PathBuf::from("/vault")),
            ..PersistedServiceConfig::default()
        };
        super::refuse_wizard_on_a_mounts_config(path, Some(&legacy))
            .expect("a legacy config is fine");
        // An EMPTY mounts array carries no table, matching the loader's own reading of it.
        let empty = PersistedServiceConfig {
            mounts: Some(Vec::new()),
            ..PersistedServiceConfig::default()
        };
        super::refuse_wizard_on_a_mounts_config(path, Some(&empty))
            .expect("an empty mounts array is not a table");

        let with_mounts = PersistedServiceConfig {
            mounts: Some(vec![filesystem_mount("vault", "", None)]),
            ..PersistedServiceConfig::default()
        };
        let error = super::refuse_wizard_on_a_mounts_config(path, Some(&with_mounts))
            .expect_err("a mount table must be refused");
        let message = error.to_string();
        assert!(message.contains("mount table"), "{message}");
        assert!(message.contains("/config/config.json"), "{message}");
        // Blames the right thing, and points at the remedies.
        assert!(!message.contains("auth"), "must not blame auth: {message}");
        assert!(message.contains("print-config"), "{message}");
        assert!(message.contains("--mcp"), "{message}");
    }

    /// A couchdb mount makes `setup-service` say where the sidecar comes from — and
    /// specifically that NO environment variable is involved, because the opposite is
    /// what a reader expects from a packaged helper runtime.
    #[test]
    fn setup_service_explains_the_sidecar_contract_for_a_couchdb_mount() {
        let root = unique_temp_dir("setup-mounts-couchdb");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("vault dir");
        let config_path = root.join("config.json");
        fs::write(&config_path, "{}\n").expect("seed config");

        let mut service = resolved_config(&vault, &root.join("index"));
        service.mounts = vec![
            filesystem_mount("vault", "", None),
            MountConfig {
                id: "live".to_string(),
                mount_at: "Live".to_string(),
                backend: MountBackendConfig::Couchdb {
                    url: "http://couch.invalid:5984".to_string(),
                    database: "vault".to_string(),
                    username: Some("user".to_string()),
                    password_ref: SecretRef::EncryptedFile {
                        id: "live-password".to_string(),
                    },
                    index_dir: None,
                    options: Default::default(),
                    e2ee: None,
                    sidecar_path: None,
                    writable: false,
                },
                recall_weight: None,
                unknown: Default::default(),
            },
        ];

        let report = setup_service(
            &resolved_runtime(config_path.clone(), service),
            false,
            true,
            false,
            false,
            false,
            None,
            false,
        )
        .expect("setup");

        assert!(
            report.messages.iter().any(|message| {
                message.contains("DEEP_OBSIDIAN_LIVESYNC_SIDECAR")
                    && message.contains("relative to the installed binary")
            }),
            "the sidecar location contract must be stated: {:?}",
            report.messages
        );

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // doctor: per-mount checks
    // -----------------------------------------------------------------------

    /// `doctor` reports the sidecar bundle and the Node runtime for a couchdb mount
    /// WITHOUT `--probe-remote`, and never contacts the remote — the config below points
    /// at a host that does not resolve, so a probe would show up as a timeout.
    ///
    /// It also pins that neither missing piece is a `fail`: a couchdb mount is
    /// experimental and non-root, so `doctor`'s exit code must not depend on it.
    #[tokio::test]
    async fn doctor_checks_the_sidecar_locally_and_does_not_contact_the_remote() {
        let root = unique_temp_dir("doctor-mounts");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("vault dir");
        fs::write(vault.join("Home.md"), "# Home\n").expect("seed note");

        let mut service = resolved_config(&vault, &root.join("index"));
        service.mounts = vec![
            filesystem_mount_at("vault", "", &vault),
            MountConfig {
                id: "live".to_string(),
                mount_at: "Live".to_string(),
                backend: MountBackendConfig::Couchdb {
                    url: "http://couchdb.invalid:5984".to_string(),
                    database: "vault".to_string(),
                    username: Some("user".to_string()),
                    password_ref: SecretRef::EncryptedFile {
                        id: "live-password".to_string(),
                    },
                    index_dir: None,
                    options: Default::default(),
                    e2ee: None,
                    sidecar_path: None,
                    writable: false,
                },
                recall_weight: None,
                unknown: Default::default(),
            },
        ];
        // stdio so the HTTP port/health probes are skipped and the test needs no socket.
        service.transport = TransportMode::Stdio;

        let report = super::doctor(
            &resolved_runtime(root.join("config.json"), service),
            50,
            false,
        )
        .await
        .expect("doctor");

        let named = |name: &str| {
            report
                .checks
                .iter()
                .find(|check| check.name == name)
                .unwrap_or_else(|| panic!("missing check {name}; got {:?}", report.checks))
        };

        // The root filesystem mount gets a readable-directory line.
        assert_eq!(named("mount.vault.vault").status, "ok");
        // The couchdb mount gets both local checks, always — and NEITHER may be a
        // `fail`, because `doctor`'s exit code gates on `fail` and an experimental,
        // non-root mount must not decide whether the install is healthy.
        for name in ["mount.live.sidecar-bundle", "mount.live.sidecar-node"] {
            let check = named(name);
            assert!(
                check.status == "ok" || check.status == "warn",
                "{name} must be ok or warn, never fail: {check:?}"
            );
        }
        assert!(
            report.ok,
            "a couchdb mount with no bundle and no Node must not make doctor report a \
             broken install: {:?}",
            report.checks
        );

        // And nothing probed the remote.
        assert!(
            !report
                .checks
                .iter()
                .any(|check| check.name == "mount.live.remote"),
            "no remote check may appear without --probe-remote: {:?}",
            report.checks
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// A legacy `vaultPath` config gains no mount checks at all, so `doctor`'s output for
    /// every existing install is unchanged.
    #[tokio::test]
    async fn doctor_adds_no_mount_checks_to_a_legacy_config() {
        let root = unique_temp_dir("doctor-legacy");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("vault dir");
        let mut service = resolved_config(&vault, &root.join("index"));
        service.transport = TransportMode::Stdio;

        let report = super::doctor(
            &resolved_runtime(root.join("config.json"), service),
            50,
            // Even asked for: there is nothing to probe, and saying so beats silence.
            true,
        )
        .await
        .expect("doctor");

        assert!(
            !report
                .checks
                .iter()
                .any(|check| check.name.starts_with("mount.")),
            "a legacy config must gain no per-mount check: {:?}",
            report.checks
        );
        let skipped = report
            .checks
            .iter()
            .find(|check| check.name == "mounts.remote")
            .expect("--probe-remote with no mounts must report why");
        assert_eq!(skipped.status, "skip");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn node_major_version_parses_and_refuses() {
        assert_eq!(super::node_major_version("v20.11.1"), Some(20));
        assert_eq!(super::node_major_version("22.0.0"), Some(22));
        assert_eq!(super::node_major_version(" v18.19.0\n"), Some(18));
        // Refused rather than guessed: reading a too-old Node as acceptable would be
        // worse than reporting that the version could not be parsed.
        assert_eq!(super::node_major_version("node v20"), None);
        assert_eq!(super::node_major_version(""), None);
    }

    // -----------------------------------------------------------------------
    // print-config
    // -----------------------------------------------------------------------

    /// `print-config` on a full three-kind mounts config prints no resolved secret.
    ///
    /// Redaction here is expected to be the IDENTITY: a config stores secret
    /// REFERENCES, never secrets, so there is nothing to redact and the redacted and
    /// unredacted renderings are the same document. That is the property worth pinning —
    /// if it ever stops holding, a plaintext credential has appeared in the config model.
    #[test]
    fn print_config_round_trips_every_mount_kind_and_leaks_no_secret() {
        let root = unique_temp_dir("print-config-mounts");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("vault dir");

        let mut service = resolved_config(&vault, &root.join("index"));
        service.experimental = deep_obsidian_types::ExperimentalConfig {
            multi_vault: true,
            couchdb_vaults: true,
            algolia_vaults: true,
        };
        service.mounts = vec![
            filesystem_mount("vault", "", None),
            MountConfig {
                id: "live".to_string(),
                mount_at: "Live".to_string(),
                backend: MountBackendConfig::Couchdb {
                    url: "http://couch.invalid:5984".to_string(),
                    database: "vault".to_string(),
                    username: Some("couch-user".to_string()),
                    password_ref: SecretRef::OsKeyring {
                        service: "deep-obsidian-mcp".to_string(),
                        account: "live-password".to_string(),
                    },
                    index_dir: None,
                    options: Default::default(),
                    e2ee: None,
                    sidecar_path: None,
                    writable: false,
                },
                recall_weight: Some(1.5),
                unknown: Default::default(),
            },
            MountConfig {
                id: "wiki".to_string(),
                mount_at: "Wiki".to_string(),
                backend: MountBackendConfig::Algolia {
                    app_id: "APPID".to_string(),
                    index_name: "wiki".to_string(),
                    api_key_ref: SecretRef::EncryptedFile {
                        id: "wiki-key".to_string(),
                    },
                    base_url: None,
                    writable: false,
                    participant_id: None,
                    cache: None,
                    retention: None,
                    index_dir: None,
                },
                recall_weight: None,
                unknown: Default::default(),
            },
        ];

        let runtime = resolved_runtime(root.join("config.json"), service);
        let redacted = super::print_config(&runtime, true).expect("print-config redacted");
        let plain = super::print_config(&runtime, false).expect("print-config plain");

        // Identity: a config carries refs, so redaction has nothing to remove.
        assert_eq!(
            redacted.text, plain.text,
            "redaction must be the identity on a config that stores only secret refs"
        );

        // Every mount round-tripped, keyed by id, with its kind intact.
        let parsed: serde_json::Value =
            serde_json::from_str(&redacted.text).expect("print-config emits valid json");
        let mounts = parsed["mounts"]
            .as_array()
            .expect("mounts array")
            .iter()
            .map(|mount| {
                (
                    mount["id"].as_str().expect("id").to_string(),
                    mount["backend"]["kind"].as_str().expect("kind").to_string(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            mounts,
            vec![
                ("vault".to_string(), "filesystem".to_string()),
                ("live".to_string(), "couchdb".to_string()),
                ("wiki".to_string(), "algolia".to_string()),
            ]
        );
        // The refs are printed as refs — the identifier, never a resolved value.
        assert!(redacted.text.contains("passwordRef"), "{}", redacted.text);
        assert!(redacted.text.contains("apiKeyRef"), "{}", redacted.text);
        // And no plaintext credential field exists to print in the first place.
        for forbidden in ["\"password\"", "\"apiKey\"", "\"token\"", "\"passphrase\""] {
            assert!(
                !redacted.text.contains(forbidden),
                "{forbidden} must not appear: {}",
                redacted.text
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// An unknown top-level key in the config file survives what `print-config` shows,
    /// so an operator diagnosing a version skew sees the key that is actually in their
    /// file rather than a rendering that has already dropped it.
    #[test]
    fn print_config_shows_retained_unknown_fields() {
        let root = unique_temp_dir("print-config-unknown");
        let vault = root.join("vault");
        fs::create_dir_all(&vault).expect("vault dir");

        let mut runtime = resolved_runtime(
            root.join("config.json"),
            resolved_config(&vault, &root.join("index")),
        );
        let mut loaded = PersistedServiceConfig {
            vault_path: Some(vault.clone()),
            ..PersistedServiceConfig::default()
        };
        loaded
            .unknown
            .insert("futureKnob".to_string(), serde_json::json!(["a", "b"]));
        runtime.config_file = Some(loaded);

        let report = super::print_config(&runtime, true).expect("print-config");
        let parsed: serde_json::Value = serde_json::from_str(&report.text).expect("valid json");
        assert_eq!(parsed["futureKnob"], serde_json::json!(["a", "b"]));

        let _ = fs::remove_dir_all(&root);
    }
}
