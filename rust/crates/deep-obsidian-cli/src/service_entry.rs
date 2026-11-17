//! Entry point for the server executable, separate from the management client.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::{env, path::PathBuf};

use crate::cli::{ServiceOptions, StdioMode, TransportMode};

#[derive(Parser)]
#[command(
    name = "deep-obsidian-mcp",
    version,
    about = "Deep Obsidian MCP server. Manage it with deep-obsidian-cli."
)]
struct ServerCli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long = "vault", global = true)]
    vault_path: Option<PathBuf>,
    #[arg(long, global = true)]
    index_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    packaged: bool,
    #[arg(long, global = true)]
    insecure_no_auth: bool,
    #[arg(long, global = true, value_enum)]
    transport: Option<TransportMode>,
    #[arg(long, global = true, value_enum)]
    stdio_mode: Option<StdioMode>,
    #[arg(long, global = true)]
    host: Option<String>,
    #[arg(long, global = true)]
    port: Option<u16>,
    #[arg(long, global = true)]
    mcp_path: Option<String>,
    #[arg(long, global = true)]
    health_path: Option<String>,
    #[arg(long, global = true, overrides_with = "no_auto_reindex")]
    auto_reindex: bool,
    #[arg(long, global = true, overrides_with = "auto_reindex")]
    no_auto_reindex: bool,
    #[arg(long, global = true)]
    reindex_debounce_ms: Option<u64>,
    #[arg(long, global = true)]
    reindex_interval_ms: Option<u64>,
    #[arg(long, global = true)]
    embedding_provider: Option<String>,
    #[arg(long, global = true)]
    embedding_model: Option<String>,
    #[arg(long, global = true)]
    embedding_base_url: Option<String>,
    #[command(subcommand)]
    command: Option<ServerCommand>,
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Start the server (also the default when no command is given).
    Serve,
    /// Print the server version.
    Version,
}

pub async fn run() -> Result<()> {
    let raw = env::args().skip(1).collect::<Vec<_>>();
    if raw.first().is_some_and(|command| {
        matches!(
            command.as_str(),
            "setup-service"
                | "doctor"
                | "print-config"
                | "probe"
                | "couchdb"
                | "algolia"
                | "mounts"
                | "secrets"
                | "users"
                | "link"
                | "unlink"
        )
    }) {
        bail!("management commands belong to deep-obsidian-cli; deep-obsidian-mcp is the server");
    }
    // Keep legacy positional vault paths and startup aliases without exposing the
    // management parser to the server executable.
    let (normalized, _) = crate::commands::normalize_cli_args_detailed(&raw)?;
    let args = if raw.len() == 1 && matches!(raw[0].as_str(), "--version" | "-v") {
        raw
    } else {
        normalized
    };
    let cli = ServerCli::parse_from(std::iter::once("deep-obsidian-mcp".to_owned()).chain(args));
    if matches!(cli.command, Some(ServerCommand::Version)) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let options = ServiceOptions {
        config: cli.config,
        vault_path: cli.vault_path,
        index_dir: cli.index_dir,
        packaged: cli.packaged,
        insecure_no_auth: cli.insecure_no_auth,
        transport: cli.transport,
        stdio_mode: cli.stdio_mode,
        host: cli.host,
        port: cli.port,
        mcp_path: cli.mcp_path,
        health_path: cli.health_path,
        auto_reindex: cli.auto_reindex,
        no_auto_reindex: cli.no_auto_reindex,
        reindex_debounce_ms: cli.reindex_debounce_ms,
        reindex_interval_ms: cli.reindex_interval_ms,
        embedding_provider: cli.embedding_provider,
        embedding_model: cli.embedding_model,
        embedding_base_url: cli.embedding_base_url,
        ..Default::default()
    };
    if options.insecure_no_auth {
        std::env::set_var("DEEP_OBSIDIAN_ALLOW_INSECURE", "1");
    }
    let resolved = crate::config::resolve_runtime_config(&options)?;
    crate::commands::serve(&resolved).await?;
    Ok(())
}
