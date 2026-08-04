//! Supervision of the LiveSync sidecar: locate it, spawn it, speak its protocol,
//! restart it, and kill it.
//!
//! The wire contract is `sidecar/livesync-sidecar/src/protocol.ts`. Everything in
//! this module is a transcription of it, so the invariants that matter are stated
//! where they are enforced rather than trusted:
//!
//! * **Secrets cross the boundary only in `initialize`.** Never argv (world-readable
//!   in `ps`), never the environment (inherited by grandchildren, captured by crash
//!   reporters). [`SidecarLaunch::command_line`] exists so a test can assert the
//!   spawned command line, and [`SidecarCredentials`] holds its password in a
//!   [`SecretString`] so a stray `{:?}` cannot print it.
//! * **A remote problem is a compatibility STATUS, not a protocol error.**
//!   `initialize` succeeds and reports one; data methods then fail `-32003` carrying
//!   it. So a handshake against a locked or unreachable vault must leave the
//!   supervisor *constructed and reporting not-ready*, never failing construction.
//! * **The pinning triple is enforced, not observed.** [`SUPPORTED`] is checked
//!   against the echoed `supported` object on every successful handshake; a mismatch
//!   is a hard failure, because a sidecar built against different upstream
//!   semantics may silently reassemble content differently.
//! * **Cursors are opaque.** They are moved around as `String` and never parsed.
//! * **An empty page does not mean exhausted.** Every paging loop in this crate
//!   drives on `exhausted`; see [`SidecarSupervisor::collect_manifest`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::watch::ChangeEvent;

// ---------------------------------------------------------------------------
// The pinning surface
// ---------------------------------------------------------------------------

/// The wire protocol version this supervisor speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The pinning triple the sidecar echoes from its own `SUPPORTED` constant.
///
/// Enforced exactly (no ranges, no "at least"): `commonlibVersion` is pinned
/// without a caret upstream because the library is pre-1.0 and documents its
/// semantics as "not final", so a minor bump is a potential behaviour change in how
/// note content is reassembled. Being wrong here means silently serving corrupt
/// content, which is worse than refusing to serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedTriple {
    pub protocol_version: u32,
    pub commonlib_version: String,
    pub max_schema_version: u32,
    pub plugin_version_tested: String,
}

/// What this build of the supervisor requires the sidecar to advertise.
pub fn supported_triple() -> SupportedTriple {
    SupportedTriple {
        protocol_version: PROTOCOL_VERSION,
        commonlib_version: "0.1.2".to_string(),
        max_schema_version: 12,
        plugin_version_tested: "1.0.3".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// What the sidecar is allowed to do to the remote.
///
/// Mirrors the protocol's `SidecarMode`. There is deliberately **no `Default`**:
/// [`SidecarConfig`] names it as a required field, so a read-write sidecar cannot
/// come into being by a struct literal that forgot to think about it. The only way
/// to get [`SidecarMode::ReadWrite`] is for a caller to type it, which is what makes
/// "writes are opt-in" structural rather than conventional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarMode {
    ReadOnly,
    ReadWrite,
}

impl SidecarMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SidecarMode::ReadOnly => "read-only",
            SidecarMode::ReadWrite => "read-write",
        }
    }

    pub fn is_writable(self) -> bool {
        matches!(self, SidecarMode::ReadWrite)
    }
}

/// Environment variable naming the built sidecar bundle.
pub const SIDECAR_BUNDLE_ENV: &str = "DEEP_OBSIDIAN_LIVESYNC_SIDECAR";

/// Environment variable naming the `node` executable to run the bundle with.
pub const SIDECAR_NODE_ENV: &str = "DEEP_OBSIDIAN_NODE";

/// Path of the built bundle relative to the repository root, used by the
/// source-checkout fallback.
const BUNDLE_RELATIVE_PATH: &str = "sidecar/livesync-sidecar/dist/sidecar.mjs";

// ---------------------------------------------------------------------------
// Compatibility status
// ---------------------------------------------------------------------------

/// Outcome of the sidecar's pre-serve compatibility gate.
///
/// Only [`CompatibilityStatus::Ok`] unlocks the data methods. Every other value
/// arrives as a *successful* `initialize` result, so the supervisor has exactly one
/// precise reason to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatibilityStatus {
    Ok,
    Incompatible,
    Mismatched,
    Locked,
    Cleaned,
    UnknownSchema,
    AuthFailed,
    Unreachable,
    E2eeRequired,
    E2eeInvalid,
    /// The sidecar's own `unknown`, and also the fallback for a status this build
    /// has never heard of — a newer sidecar must not make the supervisor panic.
    #[serde(other)]
    Unknown,
}

impl CompatibilityStatus {
    pub fn is_ok(self) -> bool {
        matches!(self, CompatibilityStatus::Ok)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CompatibilityStatus::Ok => "ok",
            CompatibilityStatus::Incompatible => "incompatible",
            CompatibilityStatus::Mismatched => "mismatched",
            CompatibilityStatus::Locked => "locked",
            CompatibilityStatus::Cleaned => "cleaned",
            CompatibilityStatus::UnknownSchema => "unknown-schema",
            CompatibilityStatus::AuthFailed => "auth-failed",
            CompatibilityStatus::Unreachable => "unreachable",
            CompatibilityStatus::E2eeRequired => "e2ee-required",
            CompatibilityStatus::E2eeInvalid => "e2ee-invalid",
            CompatibilityStatus::Unknown => "unknown",
        }
    }

    /// The operator-facing remediation for a non-`ok` status.
    ///
    /// Every one of these is a *different* action, which is the whole reason the
    /// sidecar reports a status instead of one generic failure.
    pub fn remediation(self) -> &'static str {
        match self {
            CompatibilityStatus::Ok => "the remote is readable",
            CompatibilityStatus::Incompatible => {
                "the remote's accepted nodes agree on no chunk format this build can read; \
                 upgrade the Self-hosted LiveSync plugin on every device, then restart the service"
            }
            CompatibilityStatus::Mismatched => {
                "the remote's preferred chunking values disagree with this mount's 'options'; \
                 copy the plugin's own chunk settings into the mount's 'options' block"
            }
            CompatibilityStatus::Locked => {
                "the vault is mid-rebuild (LiveSync milestone 'locked'); wait for the rebuild to \
                 finish, then restart the service"
            }
            CompatibilityStatus::Cleaned => {
                "chunks were purged on the remote (LiveSync milestone 'cleaned'); every client \
                 must resync before the vault is readable again"
            }
            CompatibilityStatus::UnknownSchema => {
                "the remote's 'obsydian_livesync_version' is missing, malformed, or newer than \
                 this build supports; this build refuses to guess at an unknown storage format"
            }
            CompatibilityStatus::AuthFailed => {
                "CouchDB rejected the credentials; check the mount's 'username' and the password \
                 stored behind 'passwordRef'"
            }
            CompatibilityStatus::Unreachable => {
                "CouchDB could not be reached (DNS, connection refused, timeout or TLS); check \
                 the mount's 'url' and that the server is running"
            }
            CompatibilityStatus::E2eeRequired => {
                "the vault is end-to-end encrypted but no passphrase was supplied; add an 'e2ee' \
                 block with 'passphraseRef' to the mount"
            }
            CompatibilityStatus::E2eeInvalid => {
                "the supplied end-to-end-encryption passphrase cannot decrypt the remote; check \
                 the secret behind 'e2ee.passphraseRef'"
            }
            CompatibilityStatus::Unknown => {
                "the sidecar could not classify the remote; check the service log for the \
                 sidecar's own diagnostics"
            }
        }
    }
}

/// A compatibility verdict plus the sidecar's already-redacted explanation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Compatibility {
    pub status: CompatibilityStatus,
    #[serde(default)]
    pub detail: Option<String>,
}

impl Compatibility {
    /// One line naming the status, the sidecar's detail, and what to do about it.
    pub fn describe(&self) -> String {
        let detail = self
            .detail
            .as_deref()
            .filter(|detail| !detail.trim().is_empty())
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default();
        format!(
            "{}{}: {}",
            self.status.as_str(),
            detail,
            self.status.remediation()
        )
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The sidecar's `error.data.kind`, the stable discriminator hosts branch on.
///
/// Branching on `kind` rather than on `code` or `message` is what the protocol
/// asks for, so this enum is the only thing the rest of the crate matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidecarErrorKind {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    NotInitialized,
    UnsupportedProtocolVersion,
    AlreadyInitialized,
    IncompatibleRemote,
    NotFound,
    RemoteError,
    DecryptFailed,
    CorruptedDocument,
    /// A guarded write lost: the remote's winning revision is not the one the
    /// caller's precondition named. Carries a [`ConflictDetail`].
    Conflict,
    /// A write method was called on a sidecar initialized `mode: "read-only"`.
    /// A configuration-level refusal, never a remote condition.
    ReadOnly,
    /// A kind this build has never heard of. A newer sidecar must degrade, not panic.
    #[serde(other)]
    Unknown,
}

impl SidecarErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SidecarErrorKind::ParseError => "parse-error",
            SidecarErrorKind::InvalidRequest => "invalid-request",
            SidecarErrorKind::MethodNotFound => "method-not-found",
            SidecarErrorKind::InvalidParams => "invalid-params",
            SidecarErrorKind::InternalError => "internal-error",
            SidecarErrorKind::NotInitialized => "not-initialized",
            SidecarErrorKind::UnsupportedProtocolVersion => "unsupported-protocol-version",
            SidecarErrorKind::AlreadyInitialized => "already-initialized",
            SidecarErrorKind::IncompatibleRemote => "incompatible-remote",
            SidecarErrorKind::NotFound => "not-found",
            SidecarErrorKind::RemoteError => "remote-error",
            SidecarErrorKind::DecryptFailed => "decrypt-failed",
            SidecarErrorKind::CorruptedDocument => "corrupted-document",
            SidecarErrorKind::Conflict => "conflict",
            SidecarErrorKind::ReadOnly => "read-only",
            SidecarErrorKind::Unknown => "unknown",
        }
    }
}

/// Why a guarded write lost, straight off `error.data.conflict`.
///
/// `current_rev` is the winning revision at the moment the conflict was detected. It
/// is absent only when no document exists at the path at all, which for a guarded
/// update means the entry was purged between the read and the write.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictDetail {
    #[serde(default)]
    pub current_rev: Option<String>,
    /// What the caller asked for. `Some(None)` is create-only.
    #[serde(default)]
    pub expected: Option<String>,
    /// The remote entry is soft-deleted, not absent.
    #[serde(default)]
    pub deleted: bool,
    /// The remote entry already had sibling conflict revisions.
    #[serde(default)]
    pub conflicted: bool,
    #[serde(default)]
    pub mtime_ms: Option<u64>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// A failure talking to (or about) the sidecar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SidecarError {
    /// A typed JSON-RPC failure from the sidecar.
    ///
    /// `detail` is already redacted sidecar-side (it is masked by secret value, so
    /// which code path produced it does not matter), which is what makes it safe to
    /// surface in an MCP error string.
    #[error("livesync sidecar {kind} error: {detail}", kind = kind.as_str())]
    Rpc {
        kind: SidecarErrorKind,
        detail: String,
        status: Option<CompatibilityStatus>,
        /// Present iff `kind` is [`SidecarErrorKind::Conflict`]. Boxed so the
        /// common (non-conflict) error stays small.
        conflict: Option<Box<ConflictDetail>>,
    },
    /// The bundle could not be located or the child could not be spawned.
    #[error("{0}")]
    Launch(String),
    /// The child died, its stdio closed, or a request timed out.
    #[error("livesync sidecar transport failure: {0}")]
    Transport(String),
    /// The sidecar answered a shape this build cannot read.
    #[error("livesync sidecar protocol violation: {0}")]
    Protocol(String),
    /// The echoed pinning triple did not match [`supported_triple`].
    #[error("{0}")]
    VersionMismatch(String),
    /// The remote is not serveable. Carries the status so callers can report the
    /// precise reason rather than "not ready".
    #[error("livesync mount is not ready: {detail}")]
    NotReady {
        status: CompatibilityStatus,
        detail: String,
    },
}

impl SidecarError {
    /// The compatibility status behind this error, when there is one.
    pub fn status(&self) -> Option<CompatibilityStatus> {
        match self {
            SidecarError::Rpc { status, .. } => *status,
            SidecarError::NotReady { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// True when the failure means the connection is gone and a retry needs a new
    /// child process. Drives the supervisor's restart decision.
    fn is_transport(&self) -> bool {
        matches!(
            self,
            SidecarError::Transport(_) | SidecarError::Launch(_) | SidecarError::Protocol(_)
        )
    }

    /// The typed RPC kind behind this error, when it is one.
    pub fn rpc_kind(&self) -> Option<SidecarErrorKind> {
        match self {
            SidecarError::Rpc { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// The conflict detail a lost compare-and-swap carries.
    pub fn conflict(&self) -> Option<&ConflictDetail> {
        match self {
            SidecarError::Rpc {
                conflict: Some(conflict),
                ..
            } => Some(conflict),
            _ => None,
        }
    }

    /// True when this failure is worth retrying once with the SAME precondition.
    ///
    /// `remote-error` only: a CouchDB/transport hiccup *inside* the sidecar, which
    /// 4a documented as retry-safe (chunks are content-addressed, so re-publishing
    /// is idempotent, and the entry root is written last so an interrupted write
    /// cannot leave a dangling root). Every other kind is a decision the remote
    /// already made — retrying a `conflict`, a `not-found` or a `read-only` would
    /// just ask the same question twice.
    ///
    /// Transport failures are deliberately NOT here: [`SidecarSupervisor::call`]
    /// already restarts the child and retries those one level down.
    pub fn is_retryable_remote(&self) -> bool {
        matches!(
            self,
            SidecarError::Rpc {
                kind: SidecarErrorKind::RemoteError,
                ..
            }
        )
    }
}

// ---------------------------------------------------------------------------
// Locating the bundle
// ---------------------------------------------------------------------------

/// How to start the sidecar: which `node`, and which bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarLaunch {
    pub node: PathBuf,
    pub bundle: PathBuf,
}

impl SidecarLaunch {
    /// The argv this launch will spawn, for assertions.
    ///
    /// Exists so the "no secret in argv" test can check the *actual* command line
    /// rather than a transcription of it. It is total by construction: argv is
    /// exactly `[node, bundle]` and nothing appends to it.
    pub fn command_line(&self) -> Vec<OsString> {
        vec![self.node.clone().into(), self.bundle.clone().into()]
    }
}

/// Resolve the sidecar bundle, in precedence order.
///
/// 1. the mount's explicit `sidecarPath`;
/// 2. [`SIDECAR_BUNDLE_ENV`];
/// 3. a bundled-relative default.
///
/// # The packaging contract (slice 7 must honour this)
///
/// The fallback looks for [`BUNDLE_RELATIVE_PATH`] next to the running executable
/// and then walking up from it, which covers a source checkout
/// (`target/debug/deep-obsidian-mcp` → repository root) and an install layout that
/// keeps `sidecar/livesync-sidecar/dist/sidecar.mjs` under the same prefix as the
/// binary. A packaging slice that puts the bundle anywhere else must either follow
/// that layout or set [`SIDECAR_BUNDLE_ENV`] in the service unit — those are the
/// only two contracts, and they are both checked here rather than guessed at
/// spawn time, so a missing bundle is a clear config error and not an EOF on a
/// pipe.
pub fn locate_sidecar_bundle(explicit: Option<&Path>) -> Result<PathBuf, SidecarError> {
    if let Some(explicit) = explicit {
        if explicit.is_file() {
            return Ok(explicit.to_path_buf());
        }
        return Err(SidecarError::Launch(format!(
            "configured livesync sidecar bundle does not exist: {} (set the mount's 'sidecarPath' \
             to the built dist/sidecar.mjs, or build it with `npm ci && npm run build` in \
             sidecar/livesync-sidecar)",
            explicit.display()
        )));
    }

    if let Some(from_env) = std::env::var_os(SIDECAR_BUNDLE_ENV) {
        let candidate = PathBuf::from(&from_env);
        if candidate.is_file() {
            return Ok(candidate);
        }
        return Err(SidecarError::Launch(format!(
            "{SIDECAR_BUNDLE_ENV} points at {} which is not a file; unset it to fall back to the \
             bundled sidecar, or point it at the built dist/sidecar.mjs",
            candidate.display()
        )));
    }

    for base in bundle_search_roots() {
        let candidate = base.join(BUNDLE_RELATIVE_PATH);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(SidecarError::Launch(format!(
        "could not locate the livesync sidecar bundle ({BUNDLE_RELATIVE_PATH}); build it with \
         `npm ci && npm run build` in sidecar/livesync-sidecar, or set the mount's 'sidecarPath' \
         or {SIDECAR_BUNDLE_ENV} to the built dist/sidecar.mjs"
    )))
}

/// Directories to try [`BUNDLE_RELATIVE_PATH`] under: the executable's directory
/// and each of its ancestors, then the current working directory and its ancestors.
///
/// The cwd chain is what makes `cargo test` work from any crate directory in the
/// workspace; the executable chain is what makes an installed layout work when the
/// service's cwd is unrelated to its install prefix.
fn bundle_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push_chain = |start: Option<PathBuf>| {
        let Some(start) = start else { return };
        let mut current = Some(start.as_path());
        while let Some(directory) = current {
            let owned = directory.to_path_buf();
            if !roots.contains(&owned) {
                roots.push(owned);
            }
            current = directory.parent();
        }
    };
    push_chain(
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf)),
    );
    push_chain(std::env::current_dir().ok());
    roots
}

/// The `node` executable to run the bundle with.
///
/// Not resolved to an absolute path: `Command` performs the `PATH` lookup, and
/// hard-coding a resolved path would break a service whose Node install moves.
fn locate_node() -> PathBuf {
    std::env::var_os(SIDECAR_NODE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node"))
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// CouchDB connection material. **Never** `Debug`-printed as plaintext: the
/// password is a [`SecretString`], whose `Debug` renders a redaction marker.
#[derive(Clone)]
pub struct SidecarCredentials {
    pub url: String,
    pub database: String,
    pub username: String,
    pub password: SecretString,
    pub e2ee_passphrase: Option<SecretString>,
    pub e2ee_obfuscate_passphrase: Option<SecretString>,
}

impl std::fmt::Debug for SidecarCredentials {
    /// Hand-written so adding a field cannot accidentally start printing a secret:
    /// only `url` and `database` are ever rendered, and `url` is validated at config
    /// load to carry no userinfo.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarCredentials")
            .field("url", &self.url)
            .field("database", &self.database)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("e2ee_passphrase", &self.e2ee_passphrase.is_some())
            .field(
                "e2ee_obfuscate_passphrase",
                &self.e2ee_obfuscate_passphrase.is_some(),
            )
            .finish()
    }
}

/// Everything the supervisor needs to run one sidecar.
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub launch: SidecarLaunch,
    pub credentials: SidecarCredentials,
    /// What this sidecar may do to the remote. Required, and required to be typed
    /// out: see [`SidecarMode`] for why there is no default.
    pub mode: SidecarMode,
    /// Forwarded verbatim as `initialize.options`. A `Value::Object`, or `None`.
    pub options: Option<Value>,
    /// Per-request ceiling on the whole round trip.
    pub request_timeout: Duration,
    /// First restart delay. Doubles per consecutive failure, capped at
    /// [`RESTART_BACKOFF_MAX`]. Injectable so the supervision test runs in
    /// milliseconds instead of seconds.
    pub restart_backoff_base: Duration,
}

/// Default per-request timeout. Generous: a cold `manifest` page on a large vault
/// pulls many CouchDB documents.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Default first restart delay.
pub const DEFAULT_RESTART_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Ceiling on the restart backoff.
pub const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How long a graceful shutdown may take before the child is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

impl SidecarConfig {
    /// Build a config, resolving the bundle and the `node` executable.
    pub fn resolve(
        sidecar_path: Option<&Path>,
        credentials: SidecarCredentials,
        mode: SidecarMode,
        options: Option<Value>,
        request_timeout: Option<Duration>,
    ) -> Result<Self, SidecarError> {
        Ok(Self {
            launch: SidecarLaunch {
                node: locate_node(),
                bundle: locate_sidecar_bundle(sidecar_path)?,
            },
            credentials,
            mode,
            options,
            request_timeout: request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            restart_backoff_base: DEFAULT_RESTART_BACKOFF_BASE,
        })
    }
}

/// `base * 2^(attempt-1)`, capped at [`RESTART_BACKOFF_MAX`]; `Duration::ZERO` for
/// `attempt == 0` (the first start is not a retry, so it must not be delayed).
pub fn restart_backoff(base: Duration, attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    let shift = attempt.saturating_sub(1).min(16);
    base.saturating_mul(1u32 << shift).min(RESTART_BACKOFF_MAX)
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// One decoded line of the sidecar's stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarMessage {
    /// A response to a request `id`.
    Response {
        id: u64,
        outcome: Result<Value, SidecarError>,
    },
    /// A notification (no `id`), e.g. `change`.
    Notification { method: String, params: Value },
    /// A line that is not a valid protocol frame. Recorded, never fatal: a broken
    /// line must not desynchronize the rest of the stream.
    Junk(String),
}

/// Decode one NDJSON line.
///
/// A pure function on purpose: the framing rules (blank lines skipped, `id`
/// present ⇒ response, `id` absent or null ⇒ notification, unparseable ⇒ junk) are
/// then testable from canned bytes without a child process.
pub fn decode_line(line: &str) -> Option<SidecarMessage> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return Some(SidecarMessage::Junk(trimmed.to_string()));
    };
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(SidecarMessage::Junk(trimmed.to_string()));
    }

    match value.get("id") {
        // A notification: no `id`, or an explicit null.
        None | Some(Value::Null) => {
            let Some(method) = value.get("method").and_then(Value::as_str) else {
                return Some(SidecarMessage::Junk(trimmed.to_string()));
            };
            Some(SidecarMessage::Notification {
                method: method.to_string(),
                params: value.get("params").cloned().unwrap_or(Value::Null),
            })
        }
        Some(id) => {
            // Ids are minted by this supervisor as integers, so anything else is a
            // frame we did not send and cannot correlate.
            let Some(id) = id.as_u64() else {
                return Some(SidecarMessage::Junk(trimmed.to_string()));
            };
            let outcome = match value.get("error") {
                Some(error) => Err(decode_error_body(error)),
                None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            };
            Some(SidecarMessage::Response { id, outcome })
        }
    }
}

/// Turn an `error` body into a typed [`SidecarError::Rpc`].
///
/// Branches on `data.kind` (the stable discriminator) and falls back to the
/// numeric `code` only when `data` is absent, which is what a non-sidecar JSON-RPC
/// error (a `parse-error` on a malformed line, say) looks like.
fn decode_error_body(error: &Value) -> SidecarError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unspecified sidecar error");
    let data = error.get("data");
    let kind = data
        .and_then(|data| data.get("kind"))
        .and_then(|kind| serde_json::from_value::<SidecarErrorKind>(kind.clone()).ok())
        .unwrap_or_else(|| kind_from_code(error.get("code").and_then(Value::as_i64).unwrap_or(0)));
    let detail = data
        .and_then(|data| data.get("detail"))
        .and_then(Value::as_str)
        .filter(|detail| !detail.trim().is_empty())
        .unwrap_or(message)
        .to_string();
    let status = data
        .and_then(|data| data.get("status"))
        .and_then(|status| serde_json::from_value::<CompatibilityStatus>(status.clone()).ok());
    // Only read for a `conflict`: the protocol states the field is present iff the
    // kind is `conflict`, so decoding it for any other kind would be inventing
    // structure the sidecar did not send.
    let conflict = (kind == SidecarErrorKind::Conflict)
        .then(|| {
            data.and_then(|data| data.get("conflict"))
                .and_then(|conflict| {
                    serde_json::from_value::<ConflictDetail>(conflict.clone()).ok()
                })
                // A conflict with an unreadable or missing detail is still a
                // conflict: default rather than silently downgrade the kind.
                .unwrap_or_default()
        })
        .map(Box::new);
    SidecarError::Rpc {
        kind,
        detail,
        status,
        conflict,
    }
}

/// The `kind` a bare JSON-RPC `code` implies, for errors carrying no `data`.
fn kind_from_code(code: i64) -> SidecarErrorKind {
    match code {
        -32700 => SidecarErrorKind::ParseError,
        -32600 => SidecarErrorKind::InvalidRequest,
        -32601 => SidecarErrorKind::MethodNotFound,
        -32602 => SidecarErrorKind::InvalidParams,
        -32603 => SidecarErrorKind::InternalError,
        -32000 => SidecarErrorKind::NotInitialized,
        -32001 => SidecarErrorKind::UnsupportedProtocolVersion,
        -32002 => SidecarErrorKind::AlreadyInitialized,
        -32003 => SidecarErrorKind::IncompatibleRemote,
        -32004 => SidecarErrorKind::NotFound,
        -32005 => SidecarErrorKind::RemoteError,
        -32006 => SidecarErrorKind::DecryptFailed,
        -32007 => SidecarErrorKind::CorruptedDocument,
        -32008 => SidecarErrorKind::Conflict,
        -32009 => SidecarErrorKind::ReadOnly,
        _ => SidecarErrorKind::Unknown,
    }
}

// ---------------------------------------------------------------------------
// One live connection
// ---------------------------------------------------------------------------

type PendingMap = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, SidecarError>>>>>;

/// One running child process and the tasks draining it.
struct Connection {
    child: StdMutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicU64,
    /// Set once the reader task sees EOF, so a caller gets a transport error
    /// immediately rather than waiting out the request timeout.
    dead: Arc<AtomicBool>,
    reader: tokio::task::JoinHandle<()>,
    stderr: tokio::task::JoinHandle<()>,
}

impl Connection {
    /// Spawn the child and start draining both of its output streams.
    ///
    /// Notifications are forwarded to `notifications`; stderr is drained to
    /// `debug!`. stderr is *not* logged at `info`: it is pre-redacted sidecar-side,
    /// but it is still a third party's diagnostic stream and a vault's path names
    /// are not the server's to broadcast by default.
    fn spawn(
        launch: &SidecarLaunch,
        notifications: mpsc::UnboundedSender<(String, Value)>,
    ) -> Result<Arc<Self>, SidecarError> {
        let mut command = Command::new(&launch.node);
        // argv is exactly [node, bundle]. No secret is ever appended: they travel
        // in `initialize` only. `SidecarLaunch::command_line` mirrors this.
        command
            .arg(&launch.bundle)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The parent's environment is inherited, but nothing secret is ADDED to
            // it. `kill_on_drop` is what guarantees no zombie survives a panicking
            // test or an aborted task.
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|error| {
            SidecarError::Launch(format!(
                "failed to start the livesync sidecar ({} {}): {error}. Node 20 or newer must be \
                 on PATH, or set {SIDECAR_NODE_ENV} to the node executable.",
                launch.node.display(),
                launch.bundle.display()
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SidecarError::Launch("sidecar stdin was not piped".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SidecarError::Launch("sidecar stdout was not piped".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| SidecarError::Launch("sidecar stderr was not piped".to_string()))?;

        let pending: PendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));

        let reader_pending = pending.clone();
        let reader_dead = dead.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match decode_line(&line) {
                    Some(SidecarMessage::Response { id, outcome }) => {
                        let waiter = reader_pending
                            .lock()
                            .ok()
                            .and_then(|mut pending| pending.remove(&id));
                        match waiter {
                            Some(waiter) => {
                                let _ = waiter.send(outcome);
                            }
                            // A response to an id nobody is waiting for: a timed-out
                            // request. Dropped, not fatal.
                            None => debug!("livesync sidecar: unmatched response id {id}"),
                        }
                    }
                    Some(SidecarMessage::Notification { method, params }) => {
                        let _ = notifications.send((method, params));
                    }
                    Some(SidecarMessage::Junk(text)) => {
                        warn!("livesync sidecar wrote a non-protocol line to stdout: {text}");
                    }
                    None => {}
                }
            }
            // EOF: the child's stdout closed. Fail every waiter now so callers do
            // not sit out their timeouts.
            reader_dead.store(true, Ordering::SeqCst);
            if let Ok(mut pending) = reader_pending.lock() {
                for (_, waiter) in pending.drain() {
                    let _ = waiter.send(Err(SidecarError::Transport(
                        "the sidecar process exited while the request was in flight".to_string(),
                    )));
                }
            }
        });

        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    debug!("livesync sidecar: {line}");
                }
            }
        });

        Ok(Arc::new(Self {
            child: StdMutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            dead,
            reader,
            stderr: stderr_task,
        }))
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// One request/response round trip.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, SidecarError> {
        if self.is_dead() {
            return Err(SidecarError::Transport(
                "the sidecar process is no longer running".to_string(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| SidecarError::Transport("sidecar request table poisoned".to_string()))?
            .insert(id, sender);

        let frame = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|error| SidecarError::Protocol(format!("could not encode {method}: {error}")))?;

        // Written under the stdin lock so two concurrent requests cannot interleave
        // halves of a line.
        {
            let mut guard = self.stdin.lock().await;
            let stdin = guard.as_mut().ok_or_else(|| {
                SidecarError::Transport("the sidecar's stdin is already closed".to_string())
            })?;
            let write = async {
                stdin.write_all(frame.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await
            };
            if let Err(error) = write.await {
                self.pending
                    .lock()
                    .ok()
                    .map(|mut pending| pending.remove(&id));
                return Err(SidecarError::Transport(format!(
                    "could not write {method} to the sidecar: {error}"
                )));
            }
        }

        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(SidecarError::Transport(
                "the sidecar dropped the request without answering".to_string(),
            )),
            Err(_) => {
                self.pending
                    .lock()
                    .ok()
                    .map(|mut pending| pending.remove(&id));
                Err(SidecarError::Transport(format!(
                    "{method} timed out after {}s",
                    timeout.as_secs()
                )))
            }
        }
    }

    /// Close stdin (the documented stop signal), wait out the grace period, then
    /// SIGKILL. Idempotent.
    async fn shutdown(&self) {
        // Best effort: the sidecar answers `shutdown` with `{ok:true}` and exits 0.
        // A failure here is expected when the child is already gone.
        let _ = self.request("shutdown", json!({}), SHUTDOWN_GRACE).await;
        // Dropping stdin closes it, which the sidecar also treats as "stop".
        drop(self.stdin.lock().await.take());

        let child = self.child.lock().ok().and_then(|mut child| child.take());
        if let Some(mut child) = child {
            match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    warn!("livesync sidecar did not exit within the grace period; killing it");
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
        }
        self.reader.abort();
        self.stderr.abort();
    }
}

impl Drop for Connection {
    /// Belt and braces alongside `kill_on_drop(true)`: abort the drain tasks so a
    /// dropped connection leaves nothing running.
    fn drop(&mut self) {
        self.reader.abort();
        self.stderr.abort();
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// What the supervisor knows about its child right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorHealth {
    /// The last handshake's compatibility verdict. `None` before the first one.
    pub compatibility: Option<Compatibility>,
    /// Consecutive failed start-or-handshake attempts. Zero once one succeeds.
    pub consecutive_failures: u32,
    /// Successful child starts over this supervisor's life. `> 1` means a restart.
    pub starts: u32,
    /// Most recent failure, already redacted.
    pub last_error: Option<String>,
    /// Whether a live `watch` subscription is in place.
    pub watching: bool,
}

impl SupervisorHealth {
    fn new() -> Self {
        Self {
            compatibility: None,
            consecutive_failures: 0,
            starts: 0,
            last_error: None,
            watching: false,
        }
    }

    /// True when the remote is serveable.
    pub fn is_ready(&self) -> bool {
        self.compatibility
            .as_ref()
            .is_some_and(|compatibility| compatibility.status.is_ok())
    }
}

// ---------------------------------------------------------------------------
// The supervisor
// ---------------------------------------------------------------------------

/// A manifest page as this crate consumes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestPage {
    pub entries: Vec<ManifestEntry>,
    #[serde(default)]
    pub next_cursor: Option<String>,
    pub exhausted: bool,
}

/// One vault entry's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ms: u64,
    #[serde(default)]
    pub ctime_ms: u64,
    pub deleted: bool,
    #[serde(default)]
    pub conflicted: bool,
    pub kind: EntryKind,
}

/// How the sidecar classified an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    /// Stored as text. Upstream's `plain`/`notes`, i.e. anything the plugin judged
    /// textual — not only `.md`.
    Markdown,
    /// Stored as base64 chunks. Upstream's `newnote`.
    Binary,
    /// `i:`-prefixed hidden-file entry. Never listed by `manifest`.
    Internal,
    #[serde(other)]
    Unknown,
}

/// The content of one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadPayload {
    Text(String),
    Bytes(Vec<u8>),
}

/// One entry's content plus the metadata that came with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    pub payload: ReadPayload,
    pub size: u64,
    pub deleted: bool,
    pub conflicted: bool,
    /// The winning revision this content came from.
    ///
    /// This is the token that closes the read-then-write race: a write guarded by
    /// the revision a read returned cannot land on top of an edit that arrived in
    /// between. Empty only if a sidecar omitted it, which is treated as "no
    /// observation" rather than as a revision.
    pub rev: String,
}

/// Metadata for one entry, with no chunk fetch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatResult {
    pub path: String,
    pub size: u64,
    pub mtime_ms: u64,
    #[serde(default)]
    pub ctime_ms: u64,
    pub deleted: bool,
    #[serde(default)]
    pub conflicted: bool,
    pub kind: EntryKind,
    /// The winning revision. See [`ReadResult::rev`].
    #[serde(default)]
    pub rev: String,
}

/// What a `write` landed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResult {
    pub path: String,
    pub rev: String,
    /// A PRE-EXISTING conflict. A rev-guarded write extends the winning branch
    /// only, so it neither creates nor resolves conflict branches.
    #[serde(default)]
    pub conflicted: bool,
    pub size: u64,
    #[serde(default)]
    pub mtime_ms: u64,
    #[serde(default)]
    pub ctime_ms: u64,
    pub created: bool,
    /// The write replaced a soft-deleted entry, bringing it back.
    #[serde(default)]
    pub resurrected: bool,
}

/// One sibling revision of a conflicted entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRevision {
    pub rev: String,
    #[serde(default)]
    pub mtime_ms: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub deleted: bool,
    /// The revision's body could not be fetched (CouchDB compacted it away). The
    /// sidecar reports it rather than dropping it, so a host never silently
    /// under-reports a conflict.
    #[serde(default)]
    pub unavailable: bool,
}

/// The winning revision and every sibling, for one path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictsResult {
    pub path: String,
    pub winning: String,
    pub conflicts: Vec<ConflictRevision>,
}

/// What a write's compare-and-swap precondition is.
///
/// The three variants are the sidecar's three `baseRev` cases, named rather than
/// encoded as a nullable string, so a call site cannot express "create-only" and
/// "no precondition" with the same value by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteGuard {
    /// `baseRev` absent: the caller states no precondition. The sidecar still
    /// guards against the revision it observed a moment earlier, so this can never
    /// fork the revision tree — it only loses a genuinely concurrent race.
    Unguarded,
    /// `baseRev: null`: create-only. Fails if ANY document exists at the path,
    /// including a soft-deleted one.
    CreateOnly,
    /// `baseRev: "<rev>"`: the remote's winning revision must be exactly this.
    Revision(String),
}

impl WriteGuard {
    /// The `baseRev` JSON this guard sends. `None` means "omit the key".
    fn base_rev(&self) -> Option<Value> {
        match self {
            WriteGuard::Unguarded => None,
            WriteGuard::CreateOnly => Some(Value::Null),
            WriteGuard::Revision(rev) => Some(json!(rev)),
        }
    }

    /// How this guard reads in an operator-facing message.
    pub fn describe(&self) -> String {
        match self {
            WriteGuard::Unguarded => "no precondition".to_string(),
            WriteGuard::CreateOnly => "create-only (no document may exist)".to_string(),
            WriteGuard::Revision(rev) => format!("revision {rev}"),
        }
    }
}

/// The outcome of one `write`, plus whether it is ambiguous.
#[derive(Debug)]
pub struct WriteAttempt {
    pub outcome: Result<WriteResult, SidecarError>,
    /// True when at least one attempt was issued whose outcome was never observed —
    /// a transport failure that killed the child mid-request, or a `remote-error`
    /// that could have come from the entry root's own response.
    ///
    /// This is the ONLY circumstance under which a `conflict` may be reporting the
    /// caller's own earlier write rather than a competing one, and therefore the only
    /// circumstance under which a caller may legitimately look at the current content
    /// before deciding what the conflict means. Without it, "the revision moved" and
    /// "my write landed and I never heard" are indistinguishable, and treating the
    /// second as the first would report a spurious failure for a write that succeeded.
    pub outcome_unknown: bool,
}

/// Content for a `write`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritePayload {
    Text(String),
    /// Already base64-encoded, because the caller is the one holding the bytes and
    /// re-encoding them here would mean buffering them twice.
    Base64(String),
}

/// One change from `changesSince` or a `change` notification.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeEntry {
    pub path: String,
    pub deleted: bool,
    pub kind: EntryKind,
}

/// A supervised sidecar: one child process, restarted on demand.
///
/// # Lifecycle
///
/// Construction does no IO. The first data call (or an explicit
/// [`SidecarSupervisor::ensure_started`]) spawns the child, runs `initialize`,
/// enforces [`supported_triple`], and records the compatibility verdict. A
/// non-`ok` verdict leaves the supervisor *alive and not ready*: the child stays up
/// (so `health` still answers) and every data call fails with
/// [`SidecarError::NotReady`], which is what makes the mount degrade rather than
/// take the server down.
///
/// # Restart
///
/// A transport failure marks the connection dead. The next call waits
/// [`restart_backoff`] for the current consecutive-failure count, spawns a fresh
/// child, re-runs the handshake, and — if anything had subscribed to changes —
/// replays `changesSince` from the last cursor before re-arming `watch`. The
/// catch-up is what keeps a restart from silently dropping edits made while the
/// child was down.
pub struct SidecarSupervisor {
    config: SidecarConfig,
    /// Serializes start/restart so a burst of concurrent calls spawns one child.
    connect_lock: Mutex<()>,
    connection: StdMutex<Option<Arc<Connection>>>,
    health: StdMutex<SupervisorHealth>,
    /// Latest opaque cursor seen from `watch`, `changesSince` or a notification.
    /// Never parsed.
    ///
    /// Behind its own `Arc` so the per-connection notification pump can update it
    /// without holding a reference to the supervisor (which would be a cycle, since
    /// the supervisor owns the pump's `JoinHandle`).
    cursor: Arc<StdMutex<Option<String>>>,
    /// Set once anything calls [`SidecarSupervisor::changes`], so a restart
    /// re-arms the live feed.
    watch_requested: AtomicBool,
    /// Same `Arc` reasoning as `cursor`: shared with the pump, not copied to it.
    subscribers: Arc<StdMutex<Vec<mpsc::UnboundedSender<ChangeEvent>>>>,
    notification_pump: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for SidecarSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarSupervisor")
            .field("bundle", &self.config.launch.bundle)
            .field("health", &self.health())
            .finish()
    }
}

impl SidecarSupervisor {
    pub fn new(config: SidecarConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            connect_lock: Mutex::new(()),
            connection: StdMutex::new(None),
            health: StdMutex::new(SupervisorHealth::new()),
            cursor: Arc::new(StdMutex::new(None)),
            watch_requested: AtomicBool::new(false),
            subscribers: Arc::new(StdMutex::new(Vec::new())),
            notification_pump: StdMutex::new(None),
        })
    }

    /// The bundle this supervisor runs, for diagnostics.
    pub fn bundle(&self) -> &Path {
        &self.config.launch.bundle
    }

    /// The argv the child is spawned with. Used by the "no secret in argv" test.
    pub fn command_line(&self) -> Vec<OsString> {
        self.config.launch.command_line()
    }

    /// What this supervisor's child was allowed to do to the remote. Fixed at
    /// construction: a restart re-hand-shakes with the same mode.
    pub fn mode(&self) -> SidecarMode {
        self.config.mode
    }

    pub fn health(&self) -> SupervisorHealth {
        self.health
            .lock()
            .map(|health| health.clone())
            .unwrap_or_else(|_| SupervisorHealth::new())
    }

    fn update_health(&self, apply: impl FnOnce(&mut SupervisorHealth)) {
        if let Ok(mut health) = self.health.lock() {
            apply(&mut health);
        }
    }

    /// The last opaque cursor observed. Callers persist it and hand it back
    /// verbatim.
    pub fn cursor(&self) -> Option<String> {
        self.cursor.lock().ok().and_then(|cursor| cursor.clone())
    }

    fn remember_cursor(&self, cursor: Option<String>) {
        if let (Some(cursor), Ok(mut slot)) = (cursor, self.cursor.lock()) {
            *slot = Some(cursor);
        }
    }

    /// Ensure a child is running and its handshake reported `ok`.
    ///
    /// A non-`ok` verdict is [`SidecarError::NotReady`] rather than `Ok`, so no
    /// caller can accidentally treat "connected but not serveable" as serveable.
    pub async fn ensure_ready(&self) -> Result<(), SidecarError> {
        self.ready_connection().await.map(|_| ())
    }

    /// Ensure a child is running and has completed a handshake, whatever the
    /// verdict. `health` and `shutdown` are available in this state; data methods
    /// are not.
    pub async fn ensure_started(&self) -> Result<(), SidecarError> {
        self.started_connection().await.map(|_| ())
    }

    /// [`Self::ensure_ready`], keeping the connection handle. Private because
    /// `Connection` is an implementation detail.
    async fn ready_connection(&self) -> Result<Arc<Connection>, SidecarError> {
        let connection = self.started_connection().await?;
        let health = self.health();
        match health.compatibility {
            Some(compatibility) if compatibility.status.is_ok() => Ok(connection),
            Some(compatibility) => Err(SidecarError::NotReady {
                status: compatibility.status,
                detail: compatibility.describe(),
            }),
            None => Err(SidecarError::NotReady {
                status: CompatibilityStatus::Unknown,
                detail: "the sidecar handshake has not completed".to_string(),
            }),
        }
    }

    /// [`Self::ensure_started`], keeping the connection handle.
    async fn started_connection(&self) -> Result<Arc<Connection>, SidecarError> {
        if let Some(connection) = self.live_connection() {
            return Ok(connection);
        }
        let _guard = self.connect_lock.lock().await;
        // Re-check under the lock: a concurrent caller may have started one.
        if let Some(connection) = self.live_connection() {
            return Ok(connection);
        }

        let attempt = self.health().consecutive_failures;
        let delay = restart_backoff(self.config.restart_backoff_base, attempt);
        if !delay.is_zero() {
            debug!(
                "waiting {delay:?} before restarting the livesync sidecar (attempt {})",
                attempt + 1
            );
            tokio::time::sleep(delay).await;
        }

        match self.start_and_handshake().await {
            Ok(connection) => {
                self.update_health(|health| {
                    health.consecutive_failures = 0;
                    health.starts = health.starts.saturating_add(1);
                    health.last_error = None;
                });
                Ok(connection)
            }
            Err(error) => {
                let message = error.to_string();
                self.update_health(|health| {
                    health.consecutive_failures = health.consecutive_failures.saturating_add(1);
                    health.last_error = Some(message);
                    health.watching = false;
                });
                Err(error)
            }
        }
    }

    fn live_connection(&self) -> Option<Arc<Connection>> {
        let mut slot = self.connection.lock().ok()?;
        match slot.as_ref() {
            Some(connection) if !connection.is_dead() => Some(connection.clone()),
            Some(_) => {
                // Reap the dead one here so the restart path is the only place that
                // constructs a connection.
                *slot = None;
                None
            }
            None => None,
        }
    }

    /// Spawn a child, hand it the secrets, and enforce the pinning triple.
    async fn start_and_handshake(&self) -> Result<Arc<Connection>, SidecarError> {
        let (notification_tx, notification_rx) = mpsc::unbounded_channel();
        let connection = Connection::spawn(&self.config.launch, notification_tx)?;

        let initialize = connection
            .request(
                "initialize",
                self.initialize_params(),
                self.config.request_timeout,
            )
            .await;
        let result = match initialize {
            Ok(result) => result,
            Err(error) => {
                // A protocol-version mismatch keeps the child alive on purpose, so
                // shut it down explicitly rather than leaking it.
                connection.shutdown().await;
                return Err(error);
            }
        };

        if let Err(error) = enforce_supported(&result) {
            connection.shutdown().await;
            return Err(error);
        }

        let compatibility: Compatibility = result
            .get("compatibility")
            .cloned()
            .ok_or_else(|| {
                SidecarError::Protocol("initialize returned no 'compatibility' object".to_string())
            })
            .and_then(|value| {
                serde_json::from_value(value).map_err(|error| {
                    SidecarError::Protocol(format!(
                        "initialize returned an unreadable 'compatibility' object: {error}"
                    ))
                })
            })?;

        let ready = compatibility.status.is_ok();
        self.update_health(|health| {
            health.compatibility = Some(compatibility.clone());
            health.watching = false;
        });
        if !ready {
            warn!(
                "livesync mount is not serveable: {}",
                compatibility.describe()
            );
        }

        // Publish the connection BEFORE the catch-up, so the catch-up's own calls go
        // through the normal path.
        if let Ok(mut slot) = self.connection.lock() {
            *slot = Some(connection.clone());
        }
        self.start_notification_pump(notification_rx);

        if ready && self.watch_requested.load(Ordering::SeqCst) {
            self.resume_watch(&connection).await;
        }

        Ok(connection)
    }

    /// The `initialize` params. The ONLY place secrets cross the boundary.
    fn initialize_params(&self) -> Value {
        let credentials = &self.config.credentials;
        let mut params = Map::new();
        params.insert("protocolVersion".to_string(), json!(PROTOCOL_VERSION));
        // Always sent explicitly, including `read-only`. The protocol defaults an
        // omitted `mode` to `read-only`, so sending it changes nothing on the wire —
        // but it means a reader of a captured handshake never has to know the
        // default to know what the process was allowed to do.
        params.insert("mode".to_string(), json!(self.config.mode.as_str()));
        params.insert(
            "couchdb".to_string(),
            json!({
                "url": credentials.url,
                "database": credentials.database,
                "username": credentials.username,
                "password": credentials.password.expose_secret(),
            }),
        );
        if let Some(passphrase) = &credentials.e2ee_passphrase {
            let mut e2ee = Map::new();
            e2ee.insert("passphrase".to_string(), json!(passphrase.expose_secret()));
            if let Some(obfuscate) = &credentials.e2ee_obfuscate_passphrase {
                e2ee.insert(
                    "obfuscatePassphrase".to_string(),
                    json!(obfuscate.expose_secret()),
                );
            }
            params.insert("e2ee".to_string(), Value::Object(e2ee));
        }
        if let Some(options) = &self.config.options {
            if !options.is_null() {
                params.insert("options".to_string(), options.clone());
            }
        }
        Value::Object(params)
    }

    /// Fan notifications out to every `changes()` subscriber.
    ///
    /// One pump per connection; the previous one ends when its channel's sender is
    /// dropped with the old connection.
    fn start_notification_pump(&self, mut receiver: mpsc::UnboundedReceiver<(String, Value)>) {
        let subscribers = self.subscriber_handle();
        let cursor_slot = self.cursor.clone();
        let pump = tokio::spawn(async move {
            while let Some((method, params)) = receiver.recv().await {
                if method != "change" {
                    debug!("livesync sidecar sent an unknown notification: {method}");
                    continue;
                }
                if let Some(cursor) = params.get("cursor").and_then(Value::as_str) {
                    if let Ok(mut slot) = cursor_slot.lock() {
                        *slot = Some(cursor.to_string());
                    }
                }
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                subscribers.broadcast(ChangeEvent::Change(format!("livesync:{path}")));
            }
        });
        if let Ok(mut slot) = self.notification_pump.lock() {
            if let Some(previous) = slot.replace(pump) {
                previous.abort();
            }
        }
    }

    /// Replay `changesSince` from the remembered cursor, then re-arm `watch`.
    ///
    /// The catch-up runs FIRST and drives on `exhausted`, because a change made
    /// while the child was down is otherwise lost: `watch` only delivers from
    /// subscription onward.
    async fn resume_watch(&self, connection: &Arc<Connection>) {
        let mut any_change = false;
        let mut cursor = self.cursor();
        // Bounded so a permanently non-exhausted feed cannot spin forever.
        for _ in 0..1000 {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = connection
                .request("changesSince", params, self.config.request_timeout)
                .await;
            let Ok(page) = page else { break };
            any_change |= !page
                .get("changes")
                .and_then(Value::as_array)
                .map(Vec::is_empty)
                .unwrap_or(true);
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            self.remember_cursor(cursor.clone());
            // Drive on `exhausted`, never on `changes.len()`: a page's budget can be
            // spent entirely on filtered-out chunk documents.
            if page
                .get("exhausted")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                break;
            }
        }

        match connection
            .request("watch", json!({}), self.config.request_timeout)
            .await
        {
            Ok(result) => {
                self.remember_cursor(
                    result
                        .get("cursor")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                );
                self.update_health(|health| health.watching = true);
            }
            Err(error) => {
                warn!("could not re-arm the livesync change feed: {error}");
                self.update_health(|health| health.watching = false);
            }
        }

        // A restart that missed edits must not leave a stale index: report the
        // catch-up as one change so the runtime reindexes.
        if any_change {
            self.subscriber_handle()
                .broadcast(ChangeEvent::Change("livesync:resume-catchup".to_string()));
        }
    }

    /// A handle onto the SHARED subscriber list.
    ///
    /// Cloning the `Arc`, not the `Vec`: a copied list would mean the pump task
    /// broadcast to senders registered before it started and never saw a later
    /// `changes()` call.
    fn subscriber_handle(&self) -> SubscriberHandle {
        SubscriberHandle {
            subscribers: self.subscribers.clone(),
        }
    }

    /// Issue one data-method request, restarting the child if it has died.
    ///
    /// Retries exactly once on a transport failure: the common case is "the child
    /// exited between two calls", and a second failure is a real problem the caller
    /// must see rather than something to keep hammering.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, SidecarError> {
        self.call_tracked(method, params).await.0
    }

    /// [`Self::call`], also reporting whether the first attempt's outcome was never
    /// observed.
    ///
    /// The flag exists for exactly one caller: a write. When a transport failure
    /// kills the child mid-request, the request may or may not have taken effect
    /// remotely, and the retry cannot tell. For a read that does not matter. For a
    /// compare-and-swap write it is the difference between "somebody else changed
    /// this" and "my own first attempt landed and I never heard about it", and only
    /// the caller — which knows what it was trying to write — can tell those apart.
    /// So the fact is reported rather than swallowed here.
    pub async fn call_tracked(
        &self,
        method: &str,
        params: Value,
    ) -> (Result<Value, SidecarError>, bool) {
        let connection = match self.ready_connection().await {
            Ok(connection) => connection,
            // Never reached the child, so nothing can have taken effect.
            Err(error) => return (Err(error), false),
        };
        let outcome = connection
            .request(method, params.clone(), self.config.request_timeout)
            .await;
        let Err(error) = outcome else {
            return (outcome, false);
        };
        if !error.is_transport() {
            return (Err(error), false);
        }
        warn!("livesync sidecar {method} failed ({error}); restarting and retrying once");
        self.mark_connection_dead();
        let connection = match self.ready_connection().await {
            Ok(connection) => connection,
            // The first attempt's outcome is STILL unknown even though the retry
            // never got off the ground.
            Err(error) => return (Err(error), true),
        };
        (
            connection
                .request(method, params, self.config.request_timeout)
                .await,
            true,
        )
    }

    fn mark_connection_dead(&self) {
        let previous = self.connection.lock().ok().and_then(|mut slot| slot.take());
        if let Some(previous) = previous {
            previous.dead.store(true, Ordering::SeqCst);
        }
    }

    /// `health`, which the protocol keeps available even when nothing else is.
    ///
    /// Returns the supervisor's own view merged with the child's, and never fails:
    /// "the sidecar could not be started" is itself the health answer.
    pub async fn probe_health(&self) -> SupervisorHealth {
        match self.started_connection().await {
            Ok(connection) => {
                match connection
                    .request("health", json!({}), self.config.request_timeout)
                    .await
                {
                    Ok(result) => {
                        let watching = result
                            .get("watching")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let last_error = result
                            .get("lastError")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        self.update_health(|health| {
                            health.watching = watching;
                            if let Some(last_error) = last_error {
                                health.last_error = Some(last_error);
                            }
                            if let Some(compatibility) = result
                                .get("compatibility")
                                .cloned()
                                .and_then(|value| serde_json::from_value(value).ok())
                            {
                                health.compatibility = Some(compatibility);
                            }
                        });
                    }
                    Err(error) => {
                        let message = error.to_string();
                        self.update_health(|health| health.last_error = Some(message));
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                self.update_health(|health| health.last_error = Some(message));
            }
        }
        self.health()
    }

    /// Every manifest entry, paged to exhaustion.
    ///
    /// **Drives on `exhausted`, not on `entries.len()`.** A page can legitimately
    /// return zero entries with `exhausted: false` because its budget was spent on
    /// documents that get filtered out, so stopping on an empty page would silently
    /// truncate the vault.
    pub async fn collect_manifest(&self) -> Result<Vec<ManifestEntry>, SidecarError> {
        let mut entries = Vec::new();
        let mut cursor: Option<String> = None;
        // Bounded so a sidecar that never reports `exhausted` fails loudly instead
        // of looping forever. 2000 pages x the 2000-entry page cap is far beyond any
        // real vault.
        for _ in 0..2000 {
            let params = match &cursor {
                Some(cursor) => json!({ "metaOnly": true, "cursor": cursor }),
                None => json!({ "metaOnly": true }),
            };
            let page: ManifestPage = self.call_typed("manifest", params).await?;
            entries.extend(page.entries);
            if page.exhausted {
                return Ok(entries);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Err(SidecarError::Protocol(
                    "manifest reported exhausted=false but returned no nextCursor".to_string(),
                ));
            }
        }
        Err(SidecarError::Protocol(
            "manifest never reported exhausted after 2000 pages".to_string(),
        ))
    }

    /// One entry's content.
    pub async fn read(&self, path: &str) -> Result<ReadResult, SidecarError> {
        let result = self.call("read", json!({ "path": path })).await?;
        let size = result.get("size").and_then(Value::as_u64).unwrap_or(0);
        let deleted = result
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let conflicted = result
            .get("conflicted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let payload = match result.get("kind").and_then(Value::as_str) {
            Some("text") => ReadPayload::Text(
                result
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        SidecarError::Protocol("read returned kind=text with no 'text'".to_string())
                    })?
                    .to_string(),
            ),
            Some("binary") => {
                let base64 = result
                    .get("base64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        SidecarError::Protocol(
                            "read returned kind=binary with no 'base64'".to_string(),
                        )
                    })?;
                ReadPayload::Bytes(decode_base64(base64)?)
            }
            other => {
                return Err(SidecarError::Protocol(format!(
                    "read returned an unknown kind: {}",
                    other.unwrap_or("<missing>")
                )));
            }
        };
        let rev = result
            .get("rev")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(ReadResult {
            payload,
            size,
            deleted,
            conflicted,
            rev,
        })
    }

    /// One entry's metadata.
    pub async fn stat(&self, path: &str) -> Result<StatResult, SidecarError> {
        self.call_typed("stat", json!({ "path": path })).await
    }

    /// Compare-and-swap write of one entry.
    ///
    /// Refused with [`SidecarErrorKind::ReadOnly`] by the sidecar unless it was
    /// initialized `read-write`; that refusal is left to the sidecar rather than
    /// pre-empted here, so the process that owns the mode is the one that enforces
    /// it. `mtime_ms`/`ctime_ms` are left to the sidecar's defaults (now, and the
    /// existing entry's ctime), which is what the plugin itself does.
    ///
    /// Retries ONCE on [`SidecarError::is_retryable_remote`], with the SAME guard.
    /// That is safe by 4a's construction and never double-writes: chunks are
    /// content-addressed so republishing is idempotent, and a retry whose first
    /// attempt actually landed loses the compare-and-swap and comes back as a
    /// conflict carrying the current revision — never as a second write.
    ///
    /// Whether such a retry happened is REPORTED rather than hidden, because a
    /// conflict after one is ambiguous in a way a conflict without one is not. See
    /// [`WriteAttempt::outcome_unknown`].
    pub async fn write(
        &self,
        path: &str,
        payload: &WritePayload,
        guard: &WriteGuard,
    ) -> WriteAttempt {
        let mut params = Map::new();
        params.insert("path".to_string(), json!(path));
        params.insert(
            "content".to_string(),
            match payload {
                WritePayload::Text(text) => json!({ "kind": "text", "text": text }),
                WritePayload::Base64(base64) => json!({ "kind": "binary", "base64": base64 }),
            },
        );
        if let Some(base_rev) = guard.base_rev() {
            params.insert("baseRev".to_string(), base_rev);
        }
        let params = Value::Object(params);

        let (raw, retried) = self.call_tracked("write", params.clone()).await;
        let mut outcome_unknown = retried;
        let raw = match raw {
            // A `remote-error` from a write is itself an unobserved outcome: chunks
            // go out before the entry root, so the failure may have come from the
            // root's own response and the write may already have landed.
            Err(error) if error.is_retryable_remote() => {
                warn!(
                    "livesync write of {path} failed with a retryable remote error ({error}); \
                     retrying once under the same precondition ({})",
                    guard.describe()
                );
                outcome_unknown = true;
                self.call_tracked("write", params).await.0
            }
            other => other,
        };
        WriteAttempt {
            outcome: raw.and_then(|value| {
                serde_json::from_value(value).map_err(|error| {
                    SidecarError::Protocol(format!("could not read the write result: {error}"))
                })
            }),
            outcome_unknown,
        }
    }

    /// The winning revision and every sibling conflict revision for one path.
    ///
    /// Read-only, so it works on a read-only sidecar too — which is the point: a
    /// read-only mount is exactly where a caller most needs to know that the content
    /// it was served has a losing sibling.
    pub async fn conflicts(&self, path: &str) -> Result<ConflictsResult, SidecarError> {
        self.call_typed("conflicts", json!({ "path": path })).await
    }

    /// Subscribe to live changes, resuming from `after` when given.
    ///
    /// Registering a subscriber is what arms `watch`, including across restarts (see
    /// [`SidecarSupervisor::resume_watch`]). The returned stream owns nothing that
    /// stops the child: several subscribers share one feed, and dropping one must
    /// not silence the others.
    pub fn changes(
        self: &Arc<Self>,
        after: Option<String>,
    ) -> mpsc::UnboundedReceiver<ChangeEvent> {
        let (sender, receiver) = mpsc::unbounded_channel();
        if let Ok(mut subscribers) = self.subscribers.lock() {
            // Drop senders whose receiver is gone, so a long-lived supervisor does
            // not accumulate them.
            subscribers.retain(|subscriber| !subscriber.is_closed());
            subscribers.push(sender);
        }
        if let (Some(after), Ok(mut cursor)) = (after, self.cursor.lock()) {
            // Only ever moved around verbatim: an opaque token this crate does not
            // interpret.
            if cursor.is_none() {
                *cursor = Some(after);
            }
        }
        self.watch_requested.store(true, Ordering::SeqCst);

        // Arm the feed. Failures are health, not an error the subscriber can act on:
        // the periodic sync is the fallback either way.
        //
        // Skipped when the feed is ALREADY armed: `start_and_handshake` arms it itself
        // whenever `watch_requested` is set, so a second `changes()` call (or the
        // first one against an already-connected supervisor) would otherwise issue a
        // redundant `watch` and a redundant catch-up.
        let supervisor = self.clone();
        tokio::spawn(async move {
            if supervisor.health().watching {
                return;
            }
            match supervisor.ready_connection().await {
                Ok(connection) => {
                    // Re-check under the connection: the handshake that just ran may
                    // have armed it.
                    if !supervisor.health().watching {
                        supervisor.resume_watch(&connection).await;
                    }
                }
                Err(error) => {
                    debug!("livesync change feed not armed yet: {error}");
                }
            }
        });
        receiver
    }

    /// Stop the child: `shutdown`, close stdin, SIGKILL after the grace period.
    pub async fn shutdown(&self) {
        if let Ok(mut pump) = self.notification_pump.lock() {
            if let Some(pump) = pump.take() {
                pump.abort();
            }
        }
        let connection = self.connection.lock().ok().and_then(|mut slot| slot.take());
        if let Some(connection) = connection {
            connection.shutdown().await;
        }
        self.update_health(|health| health.watching = false);
    }

    async fn call_typed<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, SidecarError> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value).map_err(|error| {
            SidecarError::Protocol(format!("could not read the {method} result: {error}"))
        })
    }
}

impl Drop for SidecarSupervisor {
    /// `Connection`'s `kill_on_drop(true)` does the actual killing; this only stops
    /// the pump task, which holds no child.
    fn drop(&mut self) {
        if let Ok(mut pump) = self.notification_pump.lock() {
            if let Some(pump) = pump.take() {
                pump.abort();
            }
        }
    }
}

/// Fan-out helper the notification pump owns, so it does not borrow the supervisor.
struct SubscriberHandle {
    subscribers: Arc<StdMutex<Vec<mpsc::UnboundedSender<ChangeEvent>>>>,
}

impl SubscriberHandle {
    fn broadcast(&self, event: ChangeEvent) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
        }
    }
}

/// Check the echoed pinning triple against [`supported_triple`].
///
/// A mismatch is fatal rather than a warning: the sidecar is what reassembles note
/// content out of chunks, and a build wired to different upstream semantics may do
/// it differently without any error. Refusing to serve is the safe answer.
fn enforce_supported(initialize_result: &Value) -> Result<(), SidecarError> {
    let echoed = initialize_result.get("supported").ok_or_else(|| {
        SidecarError::VersionMismatch(
            "the livesync sidecar's initialize result carries no 'supported' object, so its \
             version pinning cannot be verified; rebuild the sidecar from this checkout \
             (`npm ci && npm run build` in sidecar/livesync-sidecar)"
                .to_string(),
        )
    })?;
    let echoed: SupportedTriple = serde_json::from_value(echoed.clone()).map_err(|error| {
        SidecarError::VersionMismatch(format!(
            "the livesync sidecar's 'supported' object is unreadable ({error}); rebuild the \
             sidecar from this checkout"
        ))
    })?;
    let expected = supported_triple();
    if echoed == expected {
        return Ok(());
    }
    Err(SidecarError::VersionMismatch(format!(
        "livesync sidecar version pinning mismatch: this build requires protocolVersion={} \
         commonlibVersion={} maxSchemaVersion={} pluginVersionTested={}, but the sidecar reported \
         protocolVersion={} commonlibVersion={} maxSchemaVersion={} pluginVersionTested={}. \
         Rebuild the sidecar from this checkout (`npm ci && npm run build` in \
         sidecar/livesync-sidecar) rather than pointing at another one — the pinning covers how \
         note content is reassembled from chunks.",
        expected.protocol_version,
        expected.commonlib_version,
        expected.max_schema_version,
        expected.plugin_version_tested,
        echoed.protocol_version,
        echoed.commonlib_version,
        echoed.max_schema_version,
        echoed.plugin_version_tested,
    )))
}

/// Decode standard base64 without pulling a dependency into this crate.
///
/// The backend crate has no `base64` dependency and this is the only place that
/// needs one, for attachment reads. Strict: rejects a bad alphabet or a bad length
/// rather than producing truncated bytes, because the caller is about to hand these
/// bytes out as file content.
fn decode_base64(input: &str) -> Result<Vec<u8>, SidecarError> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let body = cleaned.strip_suffix(b"==").map_or_else(
        || {
            cleaned
                .strip_suffix(b"=")
                .map_or((&cleaned[..], 0), |body| (body, 1))
        },
        |body| (body, 2),
    );
    let (body, padding) = body;
    if (body.len() + padding) % 4 != 0 {
        return Err(SidecarError::Protocol(
            "read returned base64 of an invalid length".to_string(),
        ));
    }

    let mut output = Vec::with_capacity(body.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    for byte in body {
        let value = sextet(*byte).ok_or_else(|| {
            SidecarError::Protocol("read returned base64 with an invalid character".to_string())
        })?;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> SidecarCredentials {
        SidecarCredentials {
            url: "http://couch.example".to_string(),
            database: "vault".to_string(),
            username: "vaultuser".to_string(),
            password: SecretString::new("s3cr3t-password-value".to_string()),
            e2ee_passphrase: Some(SecretString::new("passphrase-value".to_string())),
            e2ee_obfuscate_passphrase: Some(SecretString::new("obfuscate-value".to_string())),
        }
    }

    fn config(bundle: &str) -> SidecarConfig {
        SidecarConfig {
            launch: SidecarLaunch {
                node: PathBuf::from("node"),
                bundle: PathBuf::from(bundle),
            },
            credentials: credentials(),
            // The unit tests never write; a read-write literal here would make the
            // supervision tests silently exercise a mode nothing configures.
            mode: SidecarMode::ReadOnly,
            options: None,
            request_timeout: Duration::from_secs(5),
            restart_backoff_base: Duration::from_millis(1),
        }
    }

    // -----------------------------------------------------------------------
    // Framing
    // -----------------------------------------------------------------------

    #[test]
    fn decodes_a_response_a_notification_and_junk() {
        assert_eq!(
            decode_line(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#),
            Some(SidecarMessage::Response {
                id: 7,
                outcome: Ok(json!({"ok": true})),
            })
        );
        assert_eq!(
            decode_line(
                r#"{"jsonrpc":"2.0","method":"change","params":{"path":"A.md","deleted":false,"kind":"markdown","cursor":"c1"}}"#
            ),
            Some(SidecarMessage::Notification {
                method: "change".to_string(),
                params: json!({"path":"A.md","deleted":false,"kind":"markdown","cursor":"c1"}),
            })
        );
        // Blank lines are skipped, not junk.
        assert_eq!(decode_line("   "), None);
        // Non-JSON, and JSON that is not a protocol frame, are both junk -- and
        // neither may desynchronize the stream.
        assert!(matches!(
            decode_line("startup banner"),
            Some(SidecarMessage::Junk(_))
        ));
        assert!(matches!(
            decode_line(r#"{"hello":"world"}"#),
            Some(SidecarMessage::Junk(_))
        ));
    }

    /// A whole canned NDJSON stream decodes frame-by-frame, including an
    /// interleaved notification between two responses.
    #[test]
    fn decodes_a_canned_ndjson_stream_in_order() {
        let stream = concat!(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#,
            "\n",
            "\n",
            r#"{"jsonrpc":"2.0","method":"change","params":{"path":"B.md","cursor":"c2"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32004,"message":"missing","data":{"kind":"not-found","detail":"no entry at that path"}}}"#,
            "\n",
        );
        let decoded: Vec<SidecarMessage> = stream.lines().filter_map(decode_line).collect();
        assert_eq!(decoded.len(), 3);
        assert!(matches!(
            decoded[0],
            SidecarMessage::Response {
                id: 1,
                outcome: Ok(_)
            }
        ));
        assert!(
            matches!(&decoded[1], SidecarMessage::Notification { method, .. } if method == "change")
        );
        assert!(matches!(
            &decoded[2],
            SidecarMessage::Response {
                id: 2,
                outcome: Err(SidecarError::Rpc {
                    kind: SidecarErrorKind::NotFound,
                    ..
                })
            }
        ));
    }

    /// `error.data.kind` is the discriminator, and `detail` is preferred over
    /// `message` because it is the field the sidecar redacts.
    #[test]
    fn error_mapping_table_branches_on_kind() {
        let cases = [
            ("not-initialized", SidecarErrorKind::NotInitialized),
            (
                "protocol-version-mismatch",
                // Not a kind the sidecar defines (it uses
                // `unsupported-protocol-version`), so it must degrade to Unknown
                // rather than panic.
                SidecarErrorKind::Unknown,
            ),
            (
                "unsupported-protocol-version",
                SidecarErrorKind::UnsupportedProtocolVersion,
            ),
            ("already-initialized", SidecarErrorKind::AlreadyInitialized),
            ("incompatible-remote", SidecarErrorKind::IncompatibleRemote),
            ("not-found", SidecarErrorKind::NotFound),
            ("remote-error", SidecarErrorKind::RemoteError),
            ("decrypt-failed", SidecarErrorKind::DecryptFailed),
            ("corrupted-document", SidecarErrorKind::CorruptedDocument),
        ];
        for (wire, expected) in cases {
            let error = decode_error_body(&json!({
                "code": -32003,
                "message": "generic",
                "data": {"kind": wire, "detail": "specific detail"}
            }));
            match error {
                SidecarError::Rpc { kind, detail, .. } => {
                    assert_eq!(kind, expected, "kind for {wire}");
                    assert_eq!(detail, "specific detail", "detail for {wire}");
                }
                other => panic!("expected an Rpc error for {wire}, got {other:?}"),
            }
        }
    }

    /// An error with no `data` still gets a kind, from its numeric code.
    #[test]
    fn error_without_data_falls_back_to_the_code() {
        let error = decode_error_body(&json!({"code": -32001, "message": "bad version"}));
        assert!(matches!(
            error,
            SidecarError::Rpc {
                kind: SidecarErrorKind::UnsupportedProtocolVersion,
                ..
            }
        ));
    }

    /// `-32003` carries the compatibility status, which is what lets a data-method
    /// refusal name the same reason `initialize` reported.
    #[test]
    fn incompatible_remote_carries_its_status() {
        let error = decode_error_body(&json!({
            "code": -32003,
            "message": "locked",
            "data": {"kind": "incompatible-remote", "status": "locked"}
        }));
        assert_eq!(error.status(), Some(CompatibilityStatus::Locked));
    }

    // -----------------------------------------------------------------------
    // Version pinning
    // -----------------------------------------------------------------------

    #[test]
    fn accepts_the_exact_supported_triple() {
        let result = json!({"supported": {
            "protocolVersion": 1,
            "commonlibVersion": "0.1.2",
            "maxSchemaVersion": 12,
            "pluginVersionTested": "1.0.3",
        }});
        assert!(enforce_supported(&result).is_ok());
    }

    /// Every single field is load-bearing: a drift in any one of them changes how
    /// content may be reassembled, so none may be tolerated.
    #[test]
    fn rejects_any_drift_in_the_supported_triple() {
        let drifts = [
            json!({"protocolVersion": 2, "commonlibVersion": "0.1.2", "maxSchemaVersion": 12, "pluginVersionTested": "1.0.3"}),
            json!({"protocolVersion": 1, "commonlibVersion": "0.1.3", "maxSchemaVersion": 12, "pluginVersionTested": "1.0.3"}),
            json!({"protocolVersion": 1, "commonlibVersion": "0.1.2", "maxSchemaVersion": 13, "pluginVersionTested": "1.0.3"}),
            json!({"protocolVersion": 1, "commonlibVersion": "0.1.2", "maxSchemaVersion": 12, "pluginVersionTested": "1.0.4"}),
        ];
        for drift in drifts {
            let error = enforce_supported(&json!({"supported": drift.clone()}))
                .expect_err("drift must be refused");
            assert!(
                matches!(error, SidecarError::VersionMismatch(_)),
                "expected a version mismatch for {drift}"
            );
        }
        // A missing object is a mismatch too: pinning that cannot be verified is not
        // pinning.
        assert!(matches!(
            enforce_supported(&json!({})),
            Err(SidecarError::VersionMismatch(_))
        ));
    }

    // -----------------------------------------------------------------------
    // Secrets
    // -----------------------------------------------------------------------

    /// The whole security posture in one assertion: the credentials appear in
    /// `initialize` and nowhere else -- not in argv, and not in any `Debug`.
    #[test]
    fn secrets_reach_initialize_and_nothing_else() {
        let supervisor = SidecarSupervisor::new(config("/nonexistent/sidecar.mjs"));

        let argv = format!("{:?}", supervisor.command_line());
        for secret in [
            "s3cr3t-password-value",
            "passphrase-value",
            "obfuscate-value",
        ] {
            assert!(!argv.contains(secret), "{secret} leaked into argv: {argv}");
        }
        assert_eq!(supervisor.command_line().len(), 2);

        // Debug of the config, the credentials and the supervisor: none may render a
        // secret, so a stray `{:?}` in a log line cannot leak one.
        let debug = format!(
            "{:?} {:?} {:?}",
            supervisor.config.credentials, supervisor.config, supervisor
        );
        for secret in [
            "s3cr3t-password-value",
            "passphrase-value",
            "obfuscate-value",
        ] {
            assert!(
                !debug.contains(secret),
                "{secret} leaked into Debug: {debug}"
            );
        }

        // ...and they DO reach `initialize`, so the test above is not vacuous.
        let params = supervisor.initialize_params();
        assert_eq!(
            params["couchdb"]["password"],
            json!("s3cr3t-password-value")
        );
        assert_eq!(params["e2ee"]["passphrase"], json!("passphrase-value"));
        assert_eq!(
            params["e2ee"]["obfuscatePassphrase"],
            json!("obfuscate-value")
        );
        assert_eq!(params["protocolVersion"], json!(1));
    }

    /// No `e2ee` key at all when no passphrase is configured — an empty object
    /// would make the sidecar think a passphrase was supplied.
    #[test]
    fn omits_e2ee_when_no_passphrase_is_configured() {
        let mut config = config("/nonexistent/sidecar.mjs");
        config.credentials.e2ee_passphrase = None;
        config.credentials.e2ee_obfuscate_passphrase = None;
        let params = SidecarSupervisor::new(config).initialize_params();
        assert!(params.get("e2ee").is_none());
        assert!(params.get("options").is_none());
    }

    #[test]
    fn forwards_options_verbatim() {
        let mut config = config("/nonexistent/sidecar.mjs");
        config.options = Some(json!({"requestTimeoutMs": 1234, "useEden": true}));
        let params = SidecarSupervisor::new(config).initialize_params();
        assert_eq!(
            params["options"],
            json!({"requestTimeoutMs": 1234, "useEden": true})
        );
    }

    // -----------------------------------------------------------------------
    // Backoff
    // -----------------------------------------------------------------------

    #[test]
    fn restart_backoff_grows_and_caps() {
        let base = Duration::from_secs(1);
        // The first start is not a retry.
        assert_eq!(restart_backoff(base, 0), Duration::ZERO);
        assert_eq!(restart_backoff(base, 1), base);
        assert_eq!(restart_backoff(base, 2), Duration::from_secs(2));
        assert_eq!(restart_backoff(base, 3), Duration::from_secs(4));
        assert_eq!(restart_backoff(base, 30), RESTART_BACKOFF_MAX);
        // Never exceeds the cap, whatever the base.
        assert_eq!(
            restart_backoff(Duration::from_secs(120), 1),
            RESTART_BACKOFF_MAX
        );
    }

    // -----------------------------------------------------------------------
    // Locating the bundle
    // -----------------------------------------------------------------------

    #[test]
    fn an_explicit_missing_bundle_is_a_clear_launch_error() {
        let error = locate_sidecar_bundle(Some(Path::new("/nonexistent/sidecar.mjs")))
            .expect_err("a missing bundle must be refused");
        let message = error.to_string();
        assert!(message.contains("/nonexistent/sidecar.mjs"), "{message}");
        assert!(message.contains("sidecarPath"), "{message}");
        assert!(matches!(error, SidecarError::Launch(_)));
    }

    #[test]
    fn an_explicit_bundle_that_exists_is_used_verbatim() {
        let path = std::env::temp_dir().join(format!(
            "deep-obsidian-bundle-{}-{}.mjs",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&path, "// stub\n").expect("write stub bundle");
        assert_eq!(
            locate_sidecar_bundle(Some(&path)).expect("locate the stub"),
            path
        );
        let _ = std::fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // base64
    // -----------------------------------------------------------------------

    #[test]
    fn decodes_base64_including_both_padding_lengths() {
        assert_eq!(decode_base64("").expect("empty"), Vec::<u8>::new());
        assert_eq!(decode_base64("Zg==").expect("1 byte"), b"f");
        assert_eq!(decode_base64("Zm8=").expect("2 bytes"), b"fo");
        assert_eq!(decode_base64("Zm9v").expect("3 bytes"), b"foo");
        assert_eq!(
            decode_base64("iVBORw0KGgoAAQID").expect("png header"),
            vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01, 0x02, 0x03]
        );
        // Strict: a bad alphabet or a bad length is refused rather than silently
        // truncated, because these bytes are about to be served as file content.
        assert!(decode_base64("Zm9v!!").is_err());
        assert!(decode_base64("Zm9").is_err());
    }

    // -----------------------------------------------------------------------
    // Supervision, driven by a stub child
    // -----------------------------------------------------------------------

    /// Writes a tiny node script that speaks the protocol, so the supervision state
    /// machine can be exercised without a CouchDB anywhere.
    struct StubBundle {
        path: PathBuf,
    }

    impl StubBundle {
        fn new(name: &str, body: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "deep-obsidian-stub-{name}-{}-{}.mjs",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::write(&path, body).expect("write stub bundle");
            Self { path }
        }
    }

    impl Drop for StubBundle {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// True when `node` can be started. These tests are hermetic but do need a Node
    /// runtime, exactly like `rg_available` gates the ripgrep tests.
    fn node_available() -> bool {
        std::process::Command::new(locate_node())
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// A stub that answers `initialize` with the exact supported triple and the
    /// given compatibility status, answers `manifest` with one empty exhausted page,
    /// and exits after `exit_after` requests (0 = never).
    fn stub_source(status: &str, exit_after: usize) -> String {
        format!(
            r#"
import {{ createInterface }} from "node:readline";
let seen = 0;
const rl = createInterface({{ input: process.stdin }});
rl.on("line", (line) => {{
    const text = line.trim();
    if (!text) return;
    const message = JSON.parse(text);
    seen += 1;
    const reply = (result) =>
        process.stdout.write(JSON.stringify({{ jsonrpc: "2.0", id: message.id, result }}) + "\n");
    if (message.method === "initialize") {{
        reply({{
            protocolVersion: 1,
            sidecarVersion: "0.1.0",
            commonlibVersion: "0.1.2",
            supportedSchemaVersion: 12,
            supported: {{
                protocolVersion: 1,
                commonlibVersion: "0.1.2",
                maxSchemaVersion: 12,
                pluginVersionTested: "1.0.3",
            }},
            compatibility: {{ status: "{status}", detail: "stub" }},
            remote: {{ schemaVersion: 12, encrypted: false, pathObfuscation: false }},
        }});
    }} else if (message.method === "manifest") {{
        reply({{ entries: [], exhausted: true }});
    }} else if (message.method === "health") {{
        reply({{ status: "ok", compatibility: {{ status: "{status}" }}, watching: false, uptimeMs: 1 }});
    }} else if (message.method === "shutdown") {{
        reply({{ ok: true }});
        process.exit(0);
    }} else {{
        reply({{}});
    }}
    if ({exit_after} > 0 && seen >= {exit_after}) {{
        // Die abruptly, mid-conversation, without answering anything further.
        process.exit(1);
    }}
}});
"#
        )
    }

    /// The happy path: handshake succeeds, the triple is accepted, health reports
    /// ready, and shutdown leaves no child behind.
    #[tokio::test]
    async fn handshake_succeeds_and_reports_ready() {
        if !node_available() {
            eprintln!("skipping: `node` is not available on PATH");
            return;
        }
        let stub = StubBundle::new("ready", &stub_source("ok", 0));
        let mut config = config("unused");
        config.launch.bundle = stub.path.clone();
        let supervisor = SidecarSupervisor::new(config);

        supervisor.ensure_ready().await.expect("handshake");
        let health = supervisor.health();
        assert!(health.is_ready());
        assert_eq!(health.starts, 1);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(
            health.compatibility.as_ref().map(|c| c.status),
            Some(CompatibilityStatus::Ok)
        );
        // A data method works.
        assert!(supervisor
            .collect_manifest()
            .await
            .expect("manifest")
            .is_empty());

        supervisor.shutdown().await;
    }

    /// A non-`ok` compatibility status is NOT a construction failure: the child is
    /// up, `health` answers, and data methods refuse with the status. That is what
    /// makes the mount degrade while the vault root keeps serving.
    #[tokio::test]
    async fn a_locked_remote_is_not_ready_but_does_not_fail_construction() {
        if !node_available() {
            eprintln!("skipping: `node` is not available on PATH");
            return;
        }
        let stub = StubBundle::new("locked", &stub_source("locked", 0));
        let mut config = config("unused");
        config.launch.bundle = stub.path.clone();
        let supervisor = SidecarSupervisor::new(config);

        // Starting works...
        supervisor.ensure_started().await.expect("child starts");
        // ...but the mount is not serveable, and the error names the reason.
        let error = supervisor
            .ensure_ready()
            .await
            .expect_err("a locked remote must not be serveable");
        assert_eq!(error.status(), Some(CompatibilityStatus::Locked));
        let message = error.to_string();
        assert!(message.contains("locked"), "{message}");
        assert!(message.contains("mid-rebuild"), "{message}");

        let health = supervisor.health();
        assert!(!health.is_ready());
        assert_eq!(health.starts, 1);

        supervisor.shutdown().await;
    }

    /// A stub that speaks one exchange then dies is restarted, and the restart is
    /// observable in health (`starts` climbs) rather than in timing.
    #[tokio::test]
    async fn a_child_that_exits_is_restarted_and_the_restart_is_observable() {
        if !node_available() {
            eprintln!("skipping: `node` is not available on PATH");
            return;
        }
        // Exits after its FIRST request, i.e. right after answering `initialize`.
        let stub = StubBundle::new("restart", &stub_source("ok", 1));
        let mut config = config("unused");
        config.launch.bundle = stub.path.clone();
        // Milliseconds, so the test does not sit out a real backoff.
        config.restart_backoff_base = Duration::from_millis(5);
        let supervisor = SidecarSupervisor::new(config);

        supervisor.ensure_ready().await.expect("first handshake");
        assert_eq!(supervisor.health().starts, 1);

        // The child answered `initialize` and exited. A data call must therefore
        // notice, restart, and succeed against the fresh child's handshake -- which
        // in turn dies after ITS initialize, so the call itself still fails. What is
        // asserted is the RESTART, not the call.
        let _ = supervisor.collect_manifest().await;
        let health = supervisor.health();
        assert!(
            health.starts >= 2,
            "expected a restart, health was {health:?}"
        );

        supervisor.shutdown().await;
    }

    /// A bundle that cannot be started is a clear launch error, and the failure
    /// count climbs so health can report it.
    #[tokio::test]
    async fn a_missing_bundle_fails_closed_and_counts_the_failure() {
        let supervisor = SidecarSupervisor::new(config("/nonexistent/definitely-not-here.mjs"));
        let error = supervisor
            .ensure_started()
            .await
            .expect_err("a missing bundle cannot start");
        assert!(matches!(
            error,
            SidecarError::Launch(_) | SidecarError::Transport(_)
        ));
        let health = supervisor.health();
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.starts, 0);
        assert!(health.last_error.is_some());
        assert!(!health.is_ready());
    }

    /// A sidecar advertising a different pinning triple is refused, and the child is
    /// shut down rather than left running.
    #[tokio::test]
    async fn a_version_mismatch_refuses_to_serve() {
        if !node_available() {
            eprintln!("skipping: `node` is not available on PATH");
            return;
        }
        let drifted = stub_source("ok", 0).replace(
            r#"commonlibVersion: "0.1.2""#,
            r#"commonlibVersion: "9.9.9""#,
        );
        let stub = StubBundle::new("drift", &drifted);
        let mut config = config("unused");
        config.launch.bundle = stub.path.clone();
        let supervisor = SidecarSupervisor::new(config);

        let error = supervisor
            .ensure_started()
            .await
            .expect_err("a drifted triple must be refused");
        let message = error.to_string();
        assert!(matches!(error, SidecarError::VersionMismatch(_)));
        assert!(message.contains("9.9.9"), "{message}");
        assert!(message.contains("0.1.2"), "{message}");
        assert!(!supervisor.health().is_ready());
    }
}
