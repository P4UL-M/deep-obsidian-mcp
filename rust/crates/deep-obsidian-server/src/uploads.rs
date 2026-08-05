//! Out-of-band binary upload support: capability tokens and the streaming
//! commit path used by the `PUT /upload/{token}` endpoint.
//!
//! The bytes themselves are landed by the vault backend; what lives here is the
//! capability-token store and the error taxonomy the HTTP endpoint maps to status
//! codes.
//!
//! A token is minted by the `request_vault_upload` MCP tool. It is bound at mint
//! time to a validated, vault-relative destination path and carries a short TTL.
//! Bytes travel out-of-band (e.g. via `curl --data-binary`) and the endpoint has
//! no standing write power: it can only land bytes at the bound destination, once,
//! before the token expires.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deep_obsidian_backend::{
    BackendError, BackendRequest, MutationRequest, UploadChunks, VaultBackend,
};

/// Maximum bytes a single upload may carry (100 MiB).
pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 104_857_600;
/// Time-to-live for a minted upload token.
///
/// Must stay >= the backend's staging-file TTL (`filesystem::STAGING_TTL`), which
/// decides when an abandoned staging file is old enough to delete. If this grew
/// past it, the sweep could unlink a staging file belonging to an upload that is
/// still legitimately in flight.
pub const TOKEN_TTL: Duration = Duration::from_secs(300);
/// Maximum number of outstanding (unconsumed) tokens.
pub const MAX_OUTSTANDING_TOKENS: usize = 64;

/// A pending, capability-bound upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpload {
    /// Validated vault-relative destination path (traversal rejected at mint).
    pub dest_path: String,
    /// Optional expected hash for optimistic-concurrency at commit.
    pub expected_hash: Option<String>,
    /// Maximum bytes the upload may carry.
    pub max_bytes: usize,
    /// Absolute expiry instant.
    pub expires_at: SystemTime,
    /// True while a PUT is actively streaming for this token.
    pub in_flight: bool,
}

impl PendingUpload {
    fn is_expired(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }
}

/// Outcome of attempting to claim a token for an in-flight upload.
#[derive(Debug, PartialEq, Eq)]
pub enum ClaimError {
    /// Token does not exist (or was already consumed).
    Unknown,
    /// Token exists but has expired.
    Expired,
    /// Token exists but another PUT is already streaming for it.
    InFlight,
}

/// Shared store of pending uploads, cloneable via the inner `Arc`.
#[derive(Clone, Default)]
pub struct UploadStore {
    inner: Arc<Mutex<HashMap<String, PendingUpload>>>,
}

impl UploadStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lazily purge expired entries. Caller holds the lock.
    fn purge_expired(map: &mut HashMap<String, PendingUpload>, now: SystemTime) {
        map.retain(|_, pending| pending.in_flight || !pending.is_expired(now));
    }

    /// Mint a new token bound to `pending`. Returns the token string.
    ///
    /// Errors (with a generic message) when the outstanding-token cap is reached.
    pub fn mint(&self, pending: PendingUpload) -> Result<String, String> {
        let token = random_token();
        let now = SystemTime::now();
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::purge_expired(&mut map, now);
        if map.len() >= MAX_OUTSTANDING_TOKENS {
            return Err("too many outstanding upload tokens; retry later".to_string());
        }
        map.insert(token.clone(), pending);
        Ok(token)
    }

    /// Atomically claim a token for an in-flight upload.
    ///
    /// On success the token is marked in-flight (so a concurrent PUT with the same
    /// token is rejected) and a snapshot of its binding is returned. The token is
    /// NOT removed yet — only a successful commit consumes it.
    pub fn claim(&self, token: &str) -> Result<PendingUpload, ClaimError> {
        let now = SystemTime::now();
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Lazily purge OTHER expired entries. The requested token is handled
        // explicitly so an expired-but-present token reports `Expired` (410)
        // rather than `Unknown` (403). Orphan temp files (from a crashed
        // mid-stream process) are swept separately by the backend's staging sweep.
        map.retain(|key, pending| key == token || pending.in_flight || !pending.is_expired(now));
        let pending = map.get_mut(token).ok_or(ClaimError::Unknown)?;
        if pending.is_expired(now) {
            map.remove(token);
            return Err(ClaimError::Expired);
        }
        if pending.in_flight {
            return Err(ClaimError::InFlight);
        }
        pending.in_flight = true;
        Ok(pending.clone())
    }

    /// Consume a token after a successful commit (remove it permanently).
    pub fn consume(&self, token: &str) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.remove(token);
    }

    /// Release an in-flight claim without consuming the token, so a transient
    /// failure can be retried until the TTL expires.
    pub fn release(&self, token: &str) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(pending) = map.get_mut(token) {
            pending.in_flight = false;
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

/// Generate a 256-bit random token rendered as lowercase hex.
fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Produce an RFC3339-ish display for `expires_at` (epoch seconds), used in the
/// minted JSON. We avoid pulling in a date crate and emit epoch seconds.
pub fn expires_at_epoch(expires_at: SystemTime) -> u64 {
    expires_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Error returned by the streaming commit path.
#[derive(Debug)]
pub enum CommitError {
    /// The streamed body exceeded `max_bytes`.
    TooLarge,
    /// The destination's canonical parent escaped the vault root (symlink, etc).
    EscapesVault,
    /// Optimistic-concurrency check failed: destination changed since mint.
    HashConflict { expected: String, found: String },
    /// An I/O error occurred.
    Io(String),
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitError::TooLarge => write!(f, "upload exceeds maximum allowed size"),
            CommitError::EscapesVault => write!(f, "destination escapes the vault root"),
            CommitError::HashConflict { expected, found } => {
                write!(f, "hash conflict: expected {expected}, found {found}")
            }
            CommitError::Io(message) => write!(f, "{message}"),
        }
    }
}

/// Normalize a backend failure into the endpoint's error taxonomy.
///
/// Every arm is a 1:1 mapping, so the HTTP status codes and the strings the endpoint
/// returns are unchanged: a byte-budget overrun is still `TooLarge`, an escape is
/// still `EscapesVault`, and anything else renders through `BackendError`'s
/// `Display` — which for a bare IO error is the raw `io::Error` string, exactly what
/// `CommitError::Io` used to carry.
impl From<BackendError> for CommitError {
    fn from(error: BackendError) -> Self {
        match error {
            BackendError::PayloadTooLarge => CommitError::TooLarge,
            BackendError::PathEscapesVault => CommitError::EscapesVault,
            BackendError::HashConflict { expected, found } => {
                CommitError::HashConflict { expected, found }
            }
            other => CommitError::Io(other.to_string()),
        }
    }
}

/// Result of a successful commit.
#[derive(Debug)]
pub struct CommitOutcome {
    pub created: bool,
    pub bytes_written: usize,
    pub hash: String,
}

/// Land an upload's bytes at its bound destination, through the backend.
///
/// The staging file, the incremental hash, the byte budget, the optimistic-concurrency
/// re-read and the atomic swap all live inside the backend: "write a sibling temp file
/// then rename" is a filesystem mechanic, not a vault contract. This function is the
/// thin adapter that keeps [`CommitError`] — and therefore every HTTP status code and
/// message the upload endpoint returns — exactly as it was.
///
/// Takes owned arguments so the caller can drive it as a concurrent task while it
/// pumps the request body into `chunks`.
pub async fn commit_stream_via_backend(
    backend: Arc<dyn VaultBackend>,
    dest_path: String,
    expected_hash: Option<String>,
    max_bytes: usize,
    chunks: UploadChunks,
) -> Result<CommitOutcome, CommitError> {
    let outcome = backend
        .execute(BackendRequest::Mutation(
            MutationRequest::CommitUploadStream {
                path: dest_path,
                expected_hash,
                max_bytes,
                chunks,
            },
        ))
        .await
        .and_then(|response| response.into_upload_outcome())?;
    Ok(CommitOutcome {
        created: outcome.created,
        bytes_written: outcome.bytes_written,
        hash: outcome.hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(dest: &str, ttl: Duration) -> PendingUpload {
        PendingUpload {
            dest_path: dest.to_string(),
            expected_hash: None,
            max_bytes: DEFAULT_MAX_UPLOAD_BYTES,
            expires_at: SystemTime::now() + ttl,
            in_flight: false,
        }
    }

    #[test]
    fn claim_unknown_token_is_rejected() {
        let store = UploadStore::new();
        assert_eq!(store.claim("nope"), Err(ClaimError::Unknown));
    }

    #[test]
    fn claim_expired_token_is_rejected() {
        let store = UploadStore::new();
        // Already-expired token (negative TTL via past expiry).
        let mut p = pending("a/b.bin", Duration::from_secs(300));
        p.expires_at = SystemTime::now() - Duration::from_secs(1);
        let token = {
            // Insert directly to bypass purge-at-mint dropping it.
            let mut map = store.inner.lock().unwrap();
            let token = random_token();
            map.insert(token.clone(), p);
            token
        };
        assert_eq!(store.claim(&token), Err(ClaimError::Expired));
    }

    #[test]
    fn concurrent_double_claim_only_one_succeeds() {
        let store = UploadStore::new();
        let token = store.mint(pending("a/b.bin", TOKEN_TTL)).unwrap();
        let first = store.claim(&token);
        let second = store.claim(&token);
        assert!(first.is_ok());
        assert_eq!(second, Err(ClaimError::InFlight));
    }

    #[test]
    fn consume_makes_token_unknown() {
        let store = UploadStore::new();
        let token = store.mint(pending("a/b.bin", TOKEN_TTL)).unwrap();
        store.claim(&token).unwrap();
        store.consume(&token);
        assert_eq!(store.claim(&token), Err(ClaimError::Unknown));
    }

    #[test]
    fn release_allows_retry() {
        let store = UploadStore::new();
        let token = store.mint(pending("a/b.bin", TOKEN_TTL)).unwrap();
        store.claim(&token).unwrap();
        store.release(&token);
        assert!(store.claim(&token).is_ok());
    }

    #[test]
    fn store_recovers_after_mutex_poison() {
        let store = UploadStore::new();
        let token = store.mint(pending("a/b.bin", TOKEN_TTL)).unwrap();
        // Poison the mutex by panicking while holding the lock.
        let poison_target = store.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poison_target.inner.lock().unwrap();
            panic!("intentional poison");
        }));
        // Despite poisoning, all lock sites recover the inner data and keep working.
        assert_eq!(store.len(), 1);
        assert!(store.claim(&token).is_ok());
        store.release(&token);
        store.consume(&token);
        assert_eq!(store.claim(&token), Err(ClaimError::Unknown));
        assert!(store.mint(pending("c/d.bin", TOKEN_TTL)).is_ok());
    }

    #[test]
    fn mint_rejects_beyond_outstanding_cap() {
        let store = UploadStore::new();
        for _ in 0..MAX_OUTSTANDING_TOKENS {
            store.mint(pending("a/b.bin", TOKEN_TTL)).unwrap();
        }
        assert!(store.mint(pending("a/b.bin", TOKEN_TTL)).is_err());
    }

    /// The adapter between the backend and [`CommitError`] is what decides the
    /// upload endpoint's HTTP status codes, and for a conflict it also decides the
    /// 409 body. These go through `commit_stream_via_backend` — the exact path
    /// `upload_handler` drives — so a dropped or reordered field would fail here.
    #[tokio::test]
    async fn backend_failures_map_to_the_commit_error_taxonomy() {
        use deep_obsidian_backend::FilesystemVaultBackend;

        let dir = std::env::temp_dir().join(format!(
            "upload-adapter-{}-{}",
            std::process::id(),
            random_token()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("doc.bin"), b"current").unwrap();
        let backend = || -> Arc<dyn VaultBackend> { Arc::new(FilesystemVaultBackend::new(&dir)) };

        // Stale expected hash -> 409, carrying both hashes verbatim.
        let error = commit_stream_via_backend(
            backend(),
            "doc.bin".to_string(),
            Some("fnv1a64:0000000000000000".to_string()),
            1024,
            UploadChunks::new(std::iter::once(Ok(b"replacement".to_vec()))),
        )
        .await
        .expect_err("a stale expected hash must conflict");
        assert!(matches!(error, CommitError::HashConflict { .. }));
        assert_eq!(
            error.to_string(),
            format!(
                "hash conflict: expected fnv1a64:0000000000000000, found {}",
                crate::tools::content_hash(b"current")
            )
        );
        // The destination is untouched by a rejected commit.
        assert_eq!(std::fs::read(dir.join("doc.bin")).unwrap(), b"current");

        // Over the byte budget -> 413.
        let error = commit_stream_via_backend(
            backend(),
            "big.bin".to_string(),
            None,
            4,
            UploadChunks::new(std::iter::once(Ok(b"12345".to_vec()))),
        )
        .await
        .expect_err("an oversize body must be rejected");
        assert!(matches!(error, CommitError::TooLarge));
        assert_eq!(error.to_string(), "upload exceeds maximum allowed size");
        assert!(!dir.join("big.bin").exists());

        // Traversal -> the escape arm, which the endpoint reports as 403.
        let error = commit_stream_via_backend(
            backend(),
            "../escaped.bin".to_string(),
            None,
            1024,
            UploadChunks::new(std::iter::once(Ok(b"x".to_vec()))),
        )
        .await
        .expect_err("a traversing destination must be rejected");
        assert!(matches!(error, CommitError::EscapesVault));
        assert_eq!(error.to_string(), "destination escapes the vault root");

        // A successful commit still reports the canonical hash and byte count.
        let outcome = commit_stream_via_backend(
            backend(),
            "fresh.bin".to_string(),
            None,
            1024,
            UploadChunks::new(std::iter::once(Ok(b"landed".to_vec()))),
        )
        .await
        .expect("a clean commit should succeed");
        assert!(outcome.created);
        assert_eq!(outcome.bytes_written, 6);
        assert_eq!(outcome.hash, crate::tools::content_hash(b"landed"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
