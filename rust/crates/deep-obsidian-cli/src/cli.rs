use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TransportMode {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StdioMode {
    Auto,
    Newline,
    Framed,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "deep-obsidian-mcp", version, about = "Rust prototype CLI for deep-obsidian-mcp")]
pub struct Cli {
    #[command(flatten)]
    pub options: ServiceOptions,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceOptions {
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[arg(long = "vault", global = true)]
    pub vault_path: Option<PathBuf>,

    #[arg(long = "index-dir", global = true)]
    pub index_dir: Option<PathBuf>,

    #[arg(long, global = true, value_enum)]
    pub transport: Option<TransportMode>,

    #[arg(long = "stdio-mode", global = true, value_enum)]
    pub stdio_mode: Option<StdioMode>,

    #[arg(long, global = true)]
    pub host: Option<String>,

    #[arg(long, global = true)]
    pub port: Option<u16>,

    #[arg(long = "mcp-path", global = true)]
    pub mcp_path: Option<String>,

    #[arg(long = "health-path", global = true)]
    pub health_path: Option<String>,

    #[arg(long = "auto-reindex", global = true)]
    pub auto_reindex: bool,

    #[arg(long = "no-auto-reindex", global = true)]
    pub no_auto_reindex: bool,

    #[arg(long = "reindex-debounce-ms", global = true)]
    pub reindex_debounce_ms: Option<u64>,

    #[arg(long = "reindex-interval-ms", global = true)]
    pub reindex_interval_ms: Option<u64>,

    #[arg(long = "embedding-provider", global = true)]
    pub embedding_provider: Option<String>,

    #[arg(long = "embedding-model", global = true)]
    pub embedding_model: Option<String>,

    #[arg(long = "embedding-base-url", global = true)]
    pub embedding_base_url: Option<String>,

    #[arg(long = "embedding-api-key", global = true)]
    pub embedding_api_key: Option<String>,

    #[arg(long = "embedding-api-key-env", global = true)]
    pub embedding_api_key_env: Option<String>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    Serve,
    SetupService {
        #[arg(long)]
        dry_run: bool,

        #[arg(long)]
        overwrite: bool,
    },
    Doctor {
        #[arg(long = "probe-timeout-ms", default_value_t = 5_000)]
        probe_timeout_ms: u64,

        #[arg(long)]
        json: bool,
    },
    PrintConfig {
        #[arg(long)]
        no_redact: bool,
    },
    Probe {
        #[arg(long = "timeout-ms", default_value_t = 5_000)]
        timeout_ms: u64,

        #[arg(long)]
        json: bool,
    },
}
