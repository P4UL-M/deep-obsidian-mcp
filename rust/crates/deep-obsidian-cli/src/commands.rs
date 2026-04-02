use std::fmt::Write as _;
use std::env;
use std::fs;
use std::iter;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use deep_obsidian_config::{build_service_endpoints, to_persisted_config, write_config_file};
use deep_obsidian_server::{run_http_service, run_stdio_service};
use deep_obsidian_types::{
    PersistedServiceConfig, ResolvedServiceConfig, ServiceEndpoints, TransportMode,
};
use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};

use crate::cli::{Cli, Command};
use crate::config::ResolvedRuntimeConfig;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP_TEXT: &str = "\
Usage:
  deep-obsidian-mcp [serve] [--config <path>] [--vault <path>] [--transport stdio|http]
  deep-obsidian-mcp setup-service --vault <path> [--config <path>] [--dry-run]
  deep-obsidian-mcp doctor [--config <path>] [--json]
  deep-obsidian-mcp print-config [--config <path>]
  deep-obsidian-mcp probe [--config <path>] [--json]

Commands:
  serve          Start the MCP server using resolved config.
  setup-service  Validate and persist HTTP service config.
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
    pub config: ResolvedServiceConfig,
    pub endpoints: EndpointReport,
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
        Command::SetupService { overwrite } => {
            let resolved = crate::config::resolve_runtime_config(&cli.options)?;
            let report = setup_service(&resolved, dry_run, overwrite)?;
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
                normalize_value_flag(
                    raw_args,
                    index,
                    "--probe-timeout-ms",
                    "--probe-timeout-ms",
                )
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
) -> Result<SetupServiceReport> {
    let mut service = ensure_service_transport_http(resolved.service.clone())?;
    service.vault_path = absolute_path(&service.vault_path)?;
    let config_path = absolute_path(&resolved.config_path)?;
    validate_vault(&service)?;
    let config_dir = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| config_path.clone());

    let config = to_persisted_config(&service);
    let messages = vec![
        format!("vault: {}", service.vault_path.display()),
        format!("config: {}", config_path.display()),
    ];
    if dry_run {
        assert_creatable_directory(&service.index_dir)?;
        assert_creatable_directory(&config_dir)?;
        return Ok(SetupServiceReport {
            config_file_path: config_path,
            written: false,
            dry_run: true,
            endpoints: endpoint_report(&build_service_endpoints(&service)),
            persisted_config: config,
            messages: vec![
                messages[0].clone(),
                messages[1].clone(),
                "dry-run: config validated but not written".to_string(),
            ],
        });
    }

    ensure_writable_directory(&service.index_dir)?;
    ensure_writable_directory(&config_dir)?;
    if !dry_run {
        if config_path.exists() && !overwrite {
            return Err(anyhow!(
                "config file already exists: {}",
                config_path.display()
            ));
        }
        write_config_file(&config_path, &config)?;
    }

    Ok(SetupServiceReport {
        config_file_path: config_path.clone(),
        written: !dry_run,
        dry_run,
        endpoints: endpoint_report(&build_service_endpoints(&service)),
        persisted_config: config,
        messages: vec![
            messages[0].clone(),
            messages[1].clone(),
            format!("wrote config: {}", config_path.display()),
        ],
    })
}

pub async fn doctor(
    resolved: &ResolvedRuntimeConfig,
    probe_timeout_ms: u64,
) -> Result<DoctorReport> {
    let service = resolved.service.clone();
    let endpoints = build_service_endpoints(&service);
    let mut checks = vec![check_vault(&service), check_index_dir(&service), check_rg()];

    if matches!(service.transport, TransportMode::Http) {
        let port_check = check_port(&service);
        let should_probe = port_check.status != "ok";
        checks.push(port_check);
        if should_probe {
            checks.push(check_health(&endpoints, probe_timeout_ms).await);
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
    Ok(DoctorReport {
        config: service,
        endpoints: endpoint_report(&endpoints),
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
            let _bootstrap = run_http_service(service).await?;
            eprintln!(
                "deep-obsidian-mcp native server running at {} (health={})",
                report.mcp, report.health
            );
            wait_for_shutdown_signal().await?;
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
    }
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
                serde_json::from_str::<Value>(&body_text).unwrap_or_else(|_| Value::String(body_text))
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

async fn check_health(endpoints: &ServiceEndpoints, timeout_ms: u64) -> CheckReport {
    let client = match http_client(timeout_ms) {
        Ok(client) => client,
        Err(error) => {
            return CheckReport {
                name: "health".into(),
                status: "fail".into(),
                message: error.to_string(),
                details: None,
            };
        }
    };

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
                serde_json::from_str::<Value>(&body_text).unwrap_or_else(|_| Value::String(body_text))
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
    let config_file_path = report
        .config
        .config_file_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let _ = writeln!(&mut output, "config: {}", config_file_path);
    let _ = writeln!(&mut output, "vault: {}", report.config.vault_path.display());
    let _ = writeln!(&mut output, "transport: {}", serde_json::to_string(&report.config.transport).unwrap_or_else(|_| "\"stdio\"".to_string()).trim_matches('"'));
    let _ = writeln!(&mut output, "mcp endpoint: {}", report.endpoints.mcp);
    let _ = writeln!(&mut output, "health endpoint: {}", report.endpoints.health);
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
    use super::normalize_cli_args;

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
            vec![
                "--vault".to_string(),
                "tests/fixtures/vault".to_string(),
            ]
        );
    }
}
