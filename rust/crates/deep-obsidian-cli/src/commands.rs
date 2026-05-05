use std::env;
use std::fmt::Write as _;
use std::fs;
use std::iter;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use deep_obsidian_config::{
    build_service_endpoints, default_packaged_index_dir, to_persisted_config, write_config_file,
};
use deep_obsidian_server::{run_http_service, run_stdio_service};
use deep_obsidian_types::{
    PersistedServiceConfig, ResolvedServiceConfig, ServiceEndpoints, TransportMode,
};
use reqwest::Client;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{json, Value};

use crate::cli::{Cli, Command};
use crate::config::{ResolvedRuntimeConfig, ResolvedSource, ResolvedSources};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const INDEX_SQLITE_FILENAME: &str = "index.sqlite";
const CONFIG_PRECEDENCE: [&str; 4] = ["cli", "config", "env", "default"];
const HELP_TEXT: &str = "\
Usage:
  deep-obsidian-mcp [serve] [--config <path>] [--vault <path>] [--transport stdio|http] [--packaged]
  deep-obsidian-mcp setup-service --vault <path> [--config <path>] [--mcp] [--skills] [--vault-snippets] [--dry-run]
  deep-obsidian-mcp doctor [--config <path>] [--json]
  deep-obsidian-mcp print-config [--config <path>]
  deep-obsidian-mcp probe [--config <path>] [--json]

Commands:
  serve          Start the MCP server using resolved config.
  setup-service  Validate service config and optionally install MCP client entries, skills, or vault snippets.
  doctor         Diagnose config, vault access, dependencies, and health.
  print-config   Print the normalized persisted config.
  probe          Probe the configured HTTP health and MCP endpoints.
  help           Show this help.
  version        Print the current version.";

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
            mcp,
            skills,
            vault_snippets,
        } => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            let report = setup_service(&resolved, dry_run, overwrite, mcp, skills, vault_snippets)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", render_setup_service_report(&report));
            }
            Ok(())
        }
        Command::Doctor { probe_timeout_ms } => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            let report = doctor(&resolved, probe_timeout_ms).await?;
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

fn is_known_command(token: &str) -> bool {
    matches!(
        token,
        "serve" | "setup-service" | "doctor" | "print-config" | "probe" | "help" | "version"
    )
}

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
                | "--embedding-api-key"
                | "--embedding-api-key-env"
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
            || token.starts_with("--embedding-api-key=")
            || token.starts_with("--embedding-api-key-env=")
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
            } else if token.starts_with("--embedding-api-key-env") {
                normalize_value_flag(
                    raw_args,
                    index,
                    "--embedding-api-key-env",
                    "--embedding-api-key-env",
                )
            } else if token.starts_with("--embedding-api-key") {
                normalize_value_flag(
                    raw_args,
                    index,
                    "--embedding-api-key",
                    "--embedding-api-key",
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
) -> Result<SetupServiceReport> {
    let mut service = ensure_service_transport_http(resolved.service.clone())?;
    service.vault_path = absolute_path(&service.vault_path)?;
    if matches!(resolved.sources.index_dir, ResolvedSource::Default) {
        service.index_dir = default_packaged_index_dir(&service.vault_path);
    }
    let config_path = absolute_path(&resolved.config_path)?;
    validate_vault(&service)?;
    let config_dir = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| config_path.clone());

    let config = to_persisted_config(&service);
    let messages = vec![
        format!("vault: {}", service.vault_path.display()),
        format!("index: {}", service.index_dir.display()),
        format!("config: {}", config_path.display()),
    ];
    if dry_run {
        assert_creatable_directory(&service.index_dir)?;
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
            messages: vec![
                messages[0].clone(),
                messages[1].clone(),
                messages[2].clone(),
                "dry-run: config validated but not written".to_string(),
            ],
            mcp,
            skills,
            vault_snippets,
        });
    }

    ensure_writable_directory(&service.index_dir)?;
    ensure_writable_directory(&config_dir)?;
    let mut wrote_config = false;
    let mut final_messages = messages.clone();
    if config_path.exists() && !overwrite {
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
    let config = to_persisted_config(&resolved.service);
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
    let client = http_client(timeout_ms)?;
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
            let mut bootstrap = run_http_service(service).await?;
            eprintln!(
                "deep-obsidian-mcp native server running at {} (health={})",
                report.mcp, report.health
            );
            tokio::select! {
                shutdown = wait_for_shutdown_signal() => {
                    shutdown?;
                }
                server_result = &mut bootstrap.server_handle => {
                    match server_result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => return Err(error.into()),
                        Err(error) => return Err(anyhow!("HTTP server task failed: {error}")),
                    }
                }
            }
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
    if !config.vault_path.exists() || !config.vault_path.is_dir() {
        return Err(anyhow!(
            "vault path does not exist or is not a directory: {}",
            config.vault_path.display()
        ));
    }
    Ok(())
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
        endpoint: endpoint_report(endpoints),
        last_refresh,
        last_error,
        health: health_payload,
        readiness: readiness_payload,
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
                message: error.to_string(),
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
    match ProcessCommand::new("rg").arg("--version").output() {
        Ok(output) if output.status.success() => CheckReport {
            name: "rg".into(),
            status: "ok".into(),
            message: "ripgrep is available".into(),
            details: Some(serde_json::json!({
                "version": String::from_utf8_lossy(&output.stdout).trim(),
            })),
        },
        _ => CheckReport {
            name: "rg".into(),
            status: "fail".into(),
            message: "ripgrep is not available on PATH".into(),
            details: None,
        },
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

fn redact_config(config: &PersistedServiceConfig) -> PersistedServiceConfig {
    let mut cloned = config.clone();
    if let Some(embedding) = &mut cloned.embedding {
        if embedding.api_key.is_some() {
            embedding.api_key = Some("[redacted]".to_string());
        }
    }
    cloned
}

fn render_doctor_report(report: &DoctorReport) -> String {
    let mut output = String::new();
    let vault = report
        .config
        .vault_path
        .as_ref()
        .map(|path| path.display().to_string())
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
        enable_obsidian_snippets, inspect_index, normalize_cli_args, redact_config,
        INDEX_SQLITE_FILENAME,
    };
    use deep_obsidian_types::{
        AutoReindexConfig, EmbeddingConfig, EmbeddingConfigInput, HttpConfig,
        PersistedServiceConfig, ResolvedServiceConfig, StdioMode, TransportMode,
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

    fn resolved_config(vault_path: &Path, index_dir: &Path) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            vault_path: vault_path.to_path_buf(),
            index_dir: index_dir.to_path_buf(),
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
            config_file_path: None,
        }
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
    fn redact_config_removes_inline_embedding_secret() {
        let config = PersistedServiceConfig {
            embedding: Some(EmbeddingConfigInput {
                api_key: Some("super-secret".to_string()),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                ..EmbeddingConfigInput::default()
            }),
            ..PersistedServiceConfig::default()
        };

        let redacted = redact_config(&config);
        let serialized = serde_json::to_string(&redacted).expect("serialize redacted config");

        assert!(!serialized.contains("super-secret"));
        assert!(serialized.contains("[redacted]"));
        assert!(serialized.contains("OPENAI_API_KEY"));
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
}
