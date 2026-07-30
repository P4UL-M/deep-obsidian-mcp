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
#[command(
    name = "deep-obsidian-mcp",
    version,
    about = "Rust prototype CLI for deep-obsidian-mcp",
    disable_help_subcommand = true,
    disable_version_flag = true
)]
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

    #[arg(long = "dry-run", global = true, action = clap::ArgAction::SetTrue, overrides_with = "no_dry_run")]
    pub dry_run: bool,

    #[arg(long = "no-dry-run", global = true, action = clap::ArgAction::SetTrue)]
    pub no_dry_run: bool,

    #[arg(long, global = true, action = clap::ArgAction::SetTrue, overrides_with = "no_json")]
    pub json: bool,

    #[arg(long = "no-json", global = true, action = clap::ArgAction::SetTrue)]
    pub no_json: bool,

    #[arg(long = "vault", global = true)]
    pub vault_path: Option<PathBuf>,

    #[arg(long = "index-dir", global = true)]
    pub index_dir: Option<PathBuf>,

    #[arg(long = "packaged", global = true, action = clap::ArgAction::SetTrue)]
    pub packaged: bool,

    /// Allow binding a non-loopback host without authentication (escape hatch).
    #[arg(long = "insecure-no-auth", global = true, action = clap::ArgAction::SetTrue)]
    pub insecure_no_auth: bool,

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
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    Serve,
    SetupService {
        #[arg(long)]
        overwrite: bool,
        #[arg(long, action = clap::ArgAction::SetTrue)]
        wizard: bool,
        #[arg(long, action = clap::ArgAction::SetTrue)]
        mcp: bool,
        #[arg(long, action = clap::ArgAction::SetTrue)]
        skills: bool,
        #[arg(long = "vault-snippets", action = clap::ArgAction::SetTrue)]
        vault_snippets: bool,
        /// Enable HTTP bearer auth: generate a token, store it, and print it once.
        /// Without this flag, auth is left as configured (off for a new config).
        #[arg(long, action = clap::ArgAction::SetTrue)]
        auth: bool,
        /// Disable HTTP bearer auth and delete the stored token. Takes
        /// precedence over `--auth`.
        #[arg(long = "no-auth", action = clap::ArgAction::SetTrue)]
        no_auth: bool,
    },
    Doctor {
        #[arg(long = "probe-timeout-ms", default_value_t = 5_000)]
        probe_timeout_ms: u64,
    },
    PrintConfig {
        #[arg(long)]
        no_redact: bool,
    },
    Probe {
        #[arg(long = "timeout-ms", default_value_t = 5_000)]
        timeout_ms: u64,
    },
    /// Shared Algolia wiki operations (seed, dump, status, retract, keys).
    Share {
        #[command(subcommand)]
        action: ShareAction,
    },
    Help,
    Version,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ShareAction {
    /// One-shot import of local notes into the shared index (model C: the
    /// wiki lives in the index and is authored through the mount). Only
    /// creates or updates — never removes anything from the index.
    Seed {
        /// Local folder prefix(es) to import, e.g. `--prefix _Wiki/`.
        /// Repeatable.
        #[arg(long = "prefix", required = true)]
        prefixes: Vec<String>,
        /// Delete the local copies after a verified import, so the index
        /// holds the only copy (asks for confirmation unless --yes).
        #[arg(long = "move", action = clap::ArgAction::SetTrue)]
        move_files: bool,
        /// Index name of the mount to seed into (default: the only mount).
        #[arg(long)]
        index: Option<String>,
        /// Skip confirmations (first import, --move deletion).
        #[arg(long, action = clap::ArgAction::SetTrue)]
        yes: bool,
    },
    /// Materialize every note of the shared index (head versions) into a
    /// local directory — backup / exit strategy / human-browsable snapshot.
    Dump {
        /// Target directory (created if missing). Avoid a directory inside
        /// the vault unless you want the dump indexed locally.
        #[arg(long)]
        to: PathBuf,
        /// Index name of the mount to dump (default: the only mount).
        #[arg(long)]
        index: Option<String>,
    },
    /// Show what the shared mounts hold (note count, cache, recall stage).
    Status,
    /// Permanently remove a note from the shared index, INCLUDING its whole
    /// version history. This is the one destructive operation on the wiki —
    /// it is what makes a mistaken publication withdrawable.
    Retract {
        /// Mounted or index-relative note path, e.g.
        /// `_Shared/Team/_Wiki/Foo.md` or `_Wiki/Foo.md`.
        #[arg(long)]
        path: String,
        /// Index name of the mount (default: the only mount).
        #[arg(long)]
        index: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        yes: bool,
    },
    /// Store (or replace) the Algolia API key for a configured mount in the
    /// OS keyring (encrypted-file fallback), and record the keyRef in the
    /// config file if missing.
    SetKey {
        /// Index name of the mount to store the key for (default: the only
        /// configured mount).
        #[arg(long)]
        index: Option<String>,
    },
    /// Generate a secured (read-only, filter-scoped) API key for a teammate.
    Key {
        /// Index name of the configured mount to derive the key for.
        #[arg(long)]
        index: Option<String>,
        /// Algolia `filters` restriction to embed, e.g. `folders.lvl0:_Wiki`.
        #[arg(long)]
        filters: Option<String>,
        /// SEARCH-ONLY parent key to derive from. Required: a secured key
        /// inherits its parent's ACLs, so deriving from the mount's write key
        /// would hand out full write access to the whole index.
        #[arg(long = "parent-key")]
        parent_key: Option<String>,
    },
}
