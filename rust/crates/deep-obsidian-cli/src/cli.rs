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
        /// Also contact each remote-backed mount (couchdb, algolia) read-only.
        ///
        /// Off by default because it is the one part of `doctor` that needs
        /// credentials and network: it resolves each mount's secret, opens a
        /// READ-ONLY connection, and reports what the remote said. Without it
        /// `doctor` still checks everything local — including the sidecar bundle
        /// and the Node runtime a couchdb mount needs.
        #[arg(long = "probe-remote")]
        probe_remote: bool,
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
    /// Build and inspect the config's mount table.
    Mounts {
        #[command(subcommand)]
        command: MountsCommand,
    },
    Help,
    Version,
}

/// Editing operations on the config's `mounts` table.
///
/// # Why a mount is added by a COMMAND and not by hand
///
/// A mount table is the one part of the config that has invariants a text editor cannot
/// check: ids and `mountAt` prefixes must be unique, exactly one mount must sit at the
/// root, a remote backend needs an `experimental` flag, and its credential must be in the
/// secret store rather than in the file. `setup-service` deliberately refuses to rewrite
/// such a file (see `refuse_wizard_on_a_mounts_config`), which left "edit it by hand" as
/// the only path. This family is that path made checkable: every write goes through the
/// real `normalize_service_config`, so the CLI cannot produce a config the server would
/// then refuse to load.
#[derive(Debug, Clone, Subcommand)]
pub enum MountsCommand {
    /// Append a mount to the table, validating the whole table before writing.
    ///
    /// On a legacy `vaultPath`-only config the existing vault is first converted to an
    /// explicit root mount (id `vault`, `mountAt` ""), which resolves identically to the
    /// `vaultPath` it replaces.
    Add {
        #[command(subcommand)]
        kind: MountsAddKind,
    },
    /// One line per declared mount, plus the experimental flags currently enabled.
    ///
    /// Works on a legacy config too, where it reports the implicit root as such.
    List,
    /// Unmount a mount. Nothing is ever deleted from the mount's backing store.
    Remove {
        /// The mount id, as it appears in the config's `mounts` table.
        #[arg(long)]
        id: String,
        /// Also delete this mount's local index directory.
        ///
        /// Off by default: the index is a rebuildable cache, but it can be large and
        /// slow to rebuild, so re-adding a mount that was removed by mistake should not
        /// have to pay for a full reindex.
        #[arg(long = "purge-index", action = clap::ArgAction::SetTrue)]
        purge_index: bool,
        /// Skip the interactive confirmation.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        yes: bool,
    },
}

/// Flags every `mounts add <kind>` shares.
///
/// Flattened into each kind rather than declared on `Add` itself so they can be typed
/// AFTER the kind (`mounts add filesystem --id team --yes`), which is the order the
/// documented surface uses. A flag on `Add` would have to precede the kind subcommand.
#[derive(Debug, Clone, Args)]
pub struct MountsAddCommon {
    /// Stable identifier for the mount: `[a-z0-9][a-z0-9-]*`.
    ///
    /// Also names this mount's default index directory and, for a couchdb or algolia
    /// mount, the account its credential is stored under.
    #[arg(long)]
    pub id: String,
    /// Logical vault-relative prefix the mount appears at, e.g. `Team` or `Team/Alpha`.
    ///
    /// `""` or `/` means the vault ROOT. Forward slashes only; no `.`, `..` or `~`.
    #[arg(long = "mount-at")]
    pub mount_at: String,
    /// Write the mount even though its probe failed.
    ///
    /// The mount lands in the config as configured and `doctor --probe-remote` will
    /// report it degraded. Without this flag a failed probe aborts and writes nothing.
    #[arg(long = "keep-anyway", action = clap::ArgAction::SetTrue)]
    pub keep_anyway: bool,
    /// Skip every interactive prompt, including the experimental-flag confirmation.
    ///
    /// Answers each of them "yes", so it is the flag a script uses. It does NOT skip the
    /// probe, and it does not imply `--keep-anyway`.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub yes: bool,
}

/// The backend kinds `mounts add` can append, one subcommand each.
///
/// A subcommand rather than a `--kind` value because the flags differ per kind: a
/// `--kind` enum would have made every backend's flags optional on every other backend,
/// and "you passed `--url` to a filesystem mount" would have become a runtime check
/// instead of a clap one.
///
/// `--index-dir` is deliberately NOT redeclared here: it is a GLOBAL flag (see
/// [`ServiceOptions::index_dir`]) and clap propagates it into every subcommand, so
/// `mounts add filesystem --index-dir <dir>` already reaches this command — declaring it
/// twice would be a duplicate-argument panic.
#[derive(Debug, Clone, Subcommand)]
pub enum MountsAddKind {
    /// A vault rooted at a local directory.
    Filesystem {
        #[command(flatten)]
        common: MountsAddCommon,
        /// The directory holding the vault. `~` is expanded.
        #[arg(long = "vault-path")]
        vault_path: PathBuf,
    },
    /// A Self-hosted LiveSync vault in CouchDB, reached through the Node sidecar.
    ///
    /// Requires `experimental.couchdbVaults`; the command offers to enable it.
    ///
    /// The password is never a flag VALUE: it is prompted masked, or read from stdin with
    /// `--password-stdin`. A `--password <value>` flag would put the credential into `ps`
    /// output and shell history.
    Couchdb {
        #[command(flatten)]
        common: MountsAddCommon,
        /// CouchDB server origin WITHOUT the database path, e.g. `https://couch.example`.
        /// Must not carry `user:password@` userinfo.
        #[arg(long)]
        url: String,
        /// The LiveSync database name.
        #[arg(long)]
        database: String,
        /// CouchDB user name. An identifier, not a credential.
        #[arg(long)]
        username: Option<String>,
        /// Read the password from stdin instead of prompting (first line, newline
        /// stripped). For scripts and for tests.
        #[arg(long = "password-stdin", action = clap::ArgAction::SetTrue)]
        password_stdin: bool,
        /// Allow the agent to write to this vault. Off by default.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        writable: bool,
        /// The vault is end-to-end encrypted: also prompt for its E2EE passphrase.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        e2ee: bool,
        /// Explicit path to the built sidecar bundle. Omit to use the packaged one.
        #[arg(long = "sidecar-path")]
        sidecar_path: Option<PathBuf>,
    },
    /// A shared, Markdown-only corpus stored as records in an Algolia index.
    ///
    /// Requires `experimental.algoliaVaults`; the command offers to enable it.
    ///
    /// The API key is never a flag value, for the reason the couchdb password is not:
    /// it is prompted masked, or read from stdin with `--api-key-stdin`.
    Algolia {
        #[command(flatten)]
        common: MountsAddCommon,
        /// Algolia application id, e.g. `ABC1234XYZ`. Not a secret.
        #[arg(long = "app-id")]
        app_id: String,
        /// The index holding the shared corpus.
        #[arg(long = "index-name")]
        index_name: String,
        /// Override for the REST endpoint (`https://{appId}.algolia.net` by default).
        /// Must not carry userinfo.
        #[arg(long = "base-url")]
        base_url: Option<String>,
        /// Read the API key from stdin instead of prompting (first line, newline
        /// stripped).
        #[arg(long = "api-key-stdin", action = clap::ArgAction::SetTrue)]
        api_key_stdin: bool,
        /// Allow the agent to write to this corpus. Off by default.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        writable: bool,
        /// Who this participant is in the corpus's audit trail. Defaults to
        /// `<user>@unknown`.
        #[arg(long = "participant-id")]
        participant_id: Option<String>,
    },
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
