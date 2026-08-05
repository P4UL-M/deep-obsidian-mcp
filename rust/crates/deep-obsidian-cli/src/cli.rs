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
    /// Snapshot and restore a CouchDB (Self-hosted LiveSync) mount.
    Couchdb {
        #[command(subcommand)]
        command: CouchdbCommand,
    },
    /// Seed, dump, restore, inspect and scope an Algolia-backed shared corpus.
    Algolia {
        #[command(subcommand)]
        command: AlgoliaCommand,
    },
    Help,
    Version,
}

/// Operations that only make sense against an Algolia mount.
///
/// Grouped under `algolia` for the reason `couchdb` is: `seed` and `retract` have no
/// meaning for a filesystem mount, and `dump`/`restore` mean something *different* from
/// `couchdb export`/`couchdb restore` (a version is appended, not a revision replaced), so
/// sharing one top-level verb between the two would be the wrong kind of uniformity.
///
/// This family is PR #40's `share` renamed. The semantics are unchanged; the addressing is
/// `--mount <id>` instead of `--index <name>`, because a mount is now the unit of
/// configuration.
#[derive(Debug, Clone, Subcommand)]
pub enum AlgoliaCommand {
    /// Import a local folder into the mount's index, once.
    ///
    /// Creates and updates only; nothing is ever deleted from the index to match the
    /// source folder.
    Seed {
        /// The mount id, as it appears in the config's `mounts` table.
        #[arg(long)]
        mount: String,
        /// The local folder to import. Defaults to the folder this mount SHADOWS in the
        /// root vault, i.e. `<vaultPath>/<mountAt>` — which is what a migration wants.
        #[arg(long = "from")]
        from: Option<PathBuf>,
        /// Report what would be imported and write nothing. Works on a read-only mount.
        #[arg(long = "dry-run", action = clap::ArgAction::SetTrue)]
        dry_run: bool,
        /// After verifying each note reached the index, delete the local original.
        ///
        /// Per-file and re-verified: a file whose content no longer matches what the index
        /// holds is KEPT and named, never dropped.
        #[arg(long = "move", action = clap::ArgAction::SetTrue)]
        move_files: bool,
    },
    /// Write every note of the mount to a directory, with a manifest.
    Dump {
        #[arg(long)]
        mount: String,
        /// Destination directory. Created if absent.
        #[arg(long = "out")]
        out: PathBuf,
    },
    /// Write a previously dumped directory back into the mount.
    Restore {
        #[arg(long)]
        mount: String,
        /// A directory produced by `algolia dump`.
        #[arg(long = "from")]
        from: PathBuf,
        /// Report what would happen and write nothing. Works on a read-only mount.
        #[arg(long = "dry-run", action = clap::ArgAction::SetTrue)]
        dry_run: bool,
        /// Supersede notes whose index content differs from the snapshot.
        ///
        /// Without this, a differing note is REFUSED and named. Even with it nothing is
        /// destroyed: the current version moves to history and stays readable.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        force: bool,
    },
    /// Report reachability, provisioning, note and version counts, and divergence.
    Status {
        #[arg(long)]
        mount: String,
    },
    /// Permanently delete a note AND its entire version history.
    ///
    /// The one destructive operation in this family, and deliberately not an MCP tool.
    Retract {
        #[arg(long)]
        mount: String,
        /// The note, with or without the mount's own prefix.
        #[arg(long)]
        path: String,
        /// Skip the interactive confirmation.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        yes: bool,
    },
    /// Derive a scoped, read-only secured API key for a teammate.
    ///
    /// Refuses a parent key that can write: a secured key inherits its parent's ACLs and
    /// its filter restriction constrains SEARCH only.
    Key {
        #[arg(long)]
        mount: String,
        /// Where the parent key comes from: `mount` (the default — the mount's own
        /// `apiKeyRef`, which is refused when it can write), `keyring:<service>/<account>`,
        /// `file:<id>`, or `env:<VAR>`.
        #[arg(long = "parent-key-ref", default_value = "mount")]
        parent_key_ref: String,
        /// Restrict the key to one folder, e.g. `_Wiki`. Omitted means the whole index.
        #[arg(long)]
        prefix: Option<String>,
    },
}

/// Operations that only make sense against a CouchDB mount.
///
/// Grouped under their own `couchdb` subcommand rather than added to the top level: a
/// filesystem vault is already a directory tree, so `export`/`restore` at the top level
/// would read as vault-wide operations that mean something for every mount, which they
/// do not.
#[derive(Debug, Clone, Subcommand)]
pub enum CouchdbCommand {
    /// Write every entry of a couchdb mount to a directory, with a manifest.
    Export {
        /// The mount id, as it appears in the config's `mounts` table.
        #[arg(long)]
        mount: String,
        /// Destination directory. Created if absent.
        #[arg(long = "out")]
        out: PathBuf,
    },
    /// Write a previously exported directory back into a couchdb mount.
    Restore {
        #[arg(long)]
        mount: String,
        /// A directory produced by `couchdb export`.
        #[arg(long = "from")]
        from: PathBuf,
        /// Report what would happen and write nothing. Works on a read-only mount.
        #[arg(long = "dry-run", action = clap::ArgAction::SetTrue)]
        dry_run: bool,
        /// Overwrite entries whose remote content differs from the snapshot.
        ///
        /// Without this, a differing entry is REFUSED and named, so the default can
        /// never discard an edit made after the export.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        force: bool,
    },
}
