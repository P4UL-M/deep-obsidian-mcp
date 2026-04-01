use std::fmt::Write as _;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::{anyhow, Context, Result};
use deep_obsidian_config::{
    build_service_endpoints, to_persisted_config, write_config_file,
};
use deep_obsidian_server::run_http_service;
use deep_obsidian_types::{PersistedServiceConfig, ResolvedServiceConfig, ServiceEndpoints, TransportMode};
use reqwest::Client;
use serde::Serialize;

use crate::cli::{Cli, Command};
use crate::config::ResolvedRuntimeConfig;

#[derive(Debug, Serialize)]
pub struct EndpointReport {
    pub mcp: String,
    pub health: String,
}

#[derive(Debug, Serialize)]
pub struct SetupServiceReport {
    pub config_path: PathBuf,
    pub written: bool,
    pub dry_run: bool,
    pub endpoints: EndpointReport,
    pub config: PersistedServiceConfig,
}

#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub name: String,
    pub status: String,
    pub message: String,
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub config_path: PathBuf,
    pub config: PersistedServiceConfig,
    pub endpoints: EndpointReport,
    pub checks: Vec<CheckReport>,
    pub ok: bool,
}

#[derive(Debug, Serialize)]
pub struct PrintConfigReport {
    pub config_path: PathBuf,
    pub config: PersistedServiceConfig,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub endpoints: EndpointReport,
    pub health: serde_json::Value,
    pub mcp: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct ServeReport {
    pub message: String,
    pub endpoints: EndpointReport,
}

pub async fn run() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    let resolved = crate::config::resolve_runtime_config(&cli.options)?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            let report = serve(&resolved).await?;
            println!("{}", report.message);
            Ok(())
        }
        Command::SetupService { dry_run, overwrite } => {
            let report = setup_service(&resolved, dry_run, overwrite)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Command::Doctor {
            probe_timeout_ms,
            json,
        } => {
            let report = doctor(&resolved, probe_timeout_ms).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", render_doctor_report(&report));
            }
            if report.ok {
                Ok(())
            } else {
                Err(anyhow!("doctor found one or more failing checks"))
            }
        }
        Command::PrintConfig { no_redact } => {
            let report = print_config(&resolved, !no_redact)?;
            println!("{}", report.text);
            Ok(())
        }
        Command::Probe { timeout_ms, json } => {
            let report = probe(&resolved, timeout_ms).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", render_probe_report(&report));
            }
            Ok(())
        }
    }
}

pub fn setup_service(
    resolved: &ResolvedRuntimeConfig,
    dry_run: bool,
    overwrite: bool,
) -> Result<SetupServiceReport> {
    let service = ensure_service_transport_http(resolved.service.clone())?;
    let config_path = resolved.config_path.clone();
    validate_vault(&service)?;

    let config = to_persisted_config(&service);
    if !dry_run {
        if config_path.exists() && !overwrite {
            return Err(anyhow!("config file already exists: {}", config_path.display()));
        }
        write_config_file(&config_path, &config)?;
    }

    Ok(SetupServiceReport {
        config_path,
        written: !dry_run,
        dry_run,
        endpoints: endpoint_report(&build_service_endpoints(&service)),
        config,
    })
}

pub async fn doctor(resolved: &ResolvedRuntimeConfig, probe_timeout_ms: u64) -> Result<DoctorReport> {
    let service = resolved.service.clone();
    let persisted = to_persisted_config(&service);
    let endpoints = build_service_endpoints(&service);
    let mut checks = vec![
        check_vault(&service),
        check_index_dir(&service),
        check_rg(),
    ];

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
        config_path: resolved.config_path.clone(),
        config: persisted,
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

    let health = client
        .get(&endpoints.health)
        .send()
        .await
        .context("health probe request failed")?
        .json::<serde_json::Value>()
        .await
        .context("health probe JSON decode failed")?;

    let mcp = client
        .post(&endpoints.mcp)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await
        .context("MCP probe request failed")?
        .json::<serde_json::Value>()
        .await
        .context("MCP probe JSON decode failed")?;

    Ok(ProbeReport {
        endpoints: endpoint_report(&endpoints),
        health,
        mcp,
    })
}

pub async fn serve(resolved: &ResolvedRuntimeConfig) -> Result<ServeReport> {
    let service = ensure_service_transport_http(resolved.service.clone())?;
    let endpoints = build_service_endpoints(&service);
    let report = endpoint_report(&endpoints);
    run_http_service(service).await?;
    tokio::signal::ctrl_c().await?;
    Ok(ServeReport {
        message: format!(
            "Rust prototype service running at {} (health={})",
            report.mcp, report.health
        ),
        endpoints: report,
    })
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

fn validate_vault(config: &ResolvedServiceConfig) -> Result<()> {
    if !config.vault_path.exists() || !config.vault_path.is_dir() {
        return Err(anyhow!(
            "vault path does not exist or is not a directory: {}",
            config.vault_path.display()
        ));
    }
    Ok(())
}

fn endpoint_report(endpoints: &ServiceEndpoints) -> EndpointReport {
    EndpointReport {
        mcp: endpoints.mcp.clone(),
        health: endpoints.health.clone(),
    }
}

fn check_vault(config: &ResolvedServiceConfig) -> CheckReport {
    if config.vault_path.exists() && config.vault_path.is_dir() {
        CheckReport {
            name: "vault".into(),
            status: "ok".into(),
            message: "vault is readable".into(),
            details: Some(serde_json::json!({ "path": config.vault_path })),
        }
    } else {
        CheckReport {
            name: "vault".into(),
            status: "fail".into(),
            message: format!(
                "vault path is not a readable directory: {}",
                config.vault_path.display()
            ),
            details: None,
        }
    }
}

fn check_index_dir(config: &ResolvedServiceConfig) -> CheckReport {
    let parent = config
        .index_dir
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| config.index_dir.clone());
    match fs::create_dir_all(&parent) {
        Ok(_) => CheckReport {
            name: "index-dir".into(),
            status: "ok".into(),
            message: "index directory can be created or is writable".into(),
            details: Some(serde_json::json!({ "path": config.index_dir })),
        },
        Err(error) => CheckReport {
            name: "index-dir".into(),
            status: "fail".into(),
            message: format!("index directory is not writable: {error}"),
            details: Some(serde_json::json!({ "path": config.index_dir })),
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
            status: "warn".into(),
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
            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({ "status": status.as_u16() }));
            CheckReport {
                name: "health".into(),
                status: if status.is_success() { "ok" } else { "fail" }.into(),
                message: if status.is_success() {
                    "health endpoint responded successfully".into()
                } else {
                    format!("health endpoint returned HTTP {status}")
                },
                details: Some(body),
            }
        }
        Err(error) => CheckReport {
            name: "health".into(),
            status: "fail".into(),
            message: format!("health probe failed: {error}"),
            details: None,
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
    let _ = writeln!(&mut output, "config: {}", report.config_path.display());
    let _ = writeln!(&mut output, "mcp endpoint: {}", report.endpoints.mcp);
    let _ = writeln!(&mut output, "health endpoint: {}", report.endpoints.health);
    let _ = writeln!(&mut output);
    for check in &report.checks {
        let _ = writeln!(&mut output, "[{}] {}: {}", check.status, check.name, check.message);
    }
    output.trim_end().to_string()
}

fn render_probe_report(report: &ProbeReport) -> String {
    let mut output = String::new();
    let _ = writeln!(&mut output, "mcp endpoint: {}", report.endpoints.mcp);
    let _ = writeln!(&mut output, "health endpoint: {}", report.endpoints.health);
    let _ = writeln!(&mut output, "health: {}", report.health);
    let _ = writeln!(&mut output, "mcp: {}", report.mcp);
    output.trim_end().to_string()
}
