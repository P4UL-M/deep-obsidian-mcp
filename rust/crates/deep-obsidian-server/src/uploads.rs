//! Out-of-band binary upload support: capability tokens and the streaming
//! commit path used by the `PUT /upload/{token}` endpoint.
//!
//! A token is minted by the `request_vault_upload` MCP tool. It is bound at mint
//! time to a validated, vault-relative destination path and carries a short TTL.
//! Bytes travel out-of-band (e.g. via `curl --data-binary`) and the endpoint has
//! no standing write power: it can only land bytes at the bound destination, once,
//! before the token expires.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Maximum bytes a single upload may carry (100 MiB).
pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 104_857_600;
/// Time-to-live for a minted upload token.
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
        // mid-stream process) are swept separately via `sweep_orphan_temp_files`.
        map.retain(|key, pending| {
            key == token || pending.in_flight || !pending.is_expired(now)
        });
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

/// Prefix used for in-progress upload temp files.
const TEMP_PREFIX: &str = ".upload-";

/// Sweep the vault for orphan `.upload-*.tmp` files older than the token TTL and
/// unlink them. These are left behind only if a process is killed mid-stream
/// (the normal failure path always removes its own temp file). Called lazily on
/// mint so a crashed upload does not leak disk indefinitely. Errors are ignored:
/// this is best-effort housekeeping, never a hard failure.
pub fn sweep_orphan_temp_files(vault_path: &Path) {
    sweep_orphan_temp_files_at(vault_path, SystemTime::now());
}

/// Testable core of [`sweep_orphan_temp_files`] with an injectable reference time.
/// A `.upload-*.tmp` file is removed only when `now - mtime > TOKEN_TTL`.
fn sweep_orphan_temp_files_at(vault_path: &Path, now: SystemTime) {
    sweep_dir(vault_path, now, 0);
}

fn sweep_dir(dir: &Path, now: SystemTime, depth: usize) {
    // Bound recursion to avoid pathological deep trees.
    if depth > 24 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            // Skip hidden/system dirs except we still descend normal folders.
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || name == ".obsidian" {
                continue;
            }
            sweep_dir(&path, now, depth + 1);
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(TEMP_PREFIX) || !name.ends_with(".tmp") {
            continue;
        }
        // Only remove temp files older than the TTL, so a concurrent in-flight
        // upload's temp file is never deleted out from under it.
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| {
                now.duration_since(modified)
                    .map(|age| age > TOKEN_TTL)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(&path);
        }
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

/// Result of a successful commit.
#[derive(Debug)]
pub struct CommitOutcome {
    pub created: bool,
    pub bytes_written: usize,
    pub hash: String,
}

/// Compute and return the absolute destination path, ensuring the canonical
/// parent directory stays within the canonical vault root.
///
/// `ensure_inside_vault` is lexical only; this adds the runtime symlink guard.
/// Directories are created within the vault as needed before canonicalization.
fn resolve_guarded_destination(
    vault_path: &Path,
    dest_path: &str,
) -> Result<PathBuf, CommitError> {
    let absolute = deep_obsidian_core::vault::ensure_inside_vault(vault_path, dest_path)
        .map_err(|_| CommitError::EscapesVault)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| CommitError::Io("destination has no parent directory".to_string()))?;
    fs::create_dir_all(parent).map_err(|error| CommitError::Io(error.to_string()))?;

    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| CommitError::Io(error.to_string()))?;
    let canonical_vault = vault_path
        .canonicalize()
        .map_err(|error| CommitError::Io(error.to_string()))?;
    if !canonical_parent.starts_with(&canonical_vault) {
        return Err(CommitError::EscapesVault);
    }
    Ok(absolute)
}

/// Stream `chunks` to a temp file in the destination's parent directory,
/// enforcing `max_bytes` during streaming, then atomically rename over the
/// destination. The hash and create/update decision are computed at commit.
///
/// `expected_hash`, when set, triggers an optimistic-concurrency re-read of the
/// destination at commit; a mismatch aborts with `HashConflict`.
///
/// On any failure the temp file is removed and the destination is left untouched.
pub fn commit_stream<I>(
    vault_path: &Path,
    dest_path: &str,
    expected_hash: Option<&str>,
    max_bytes: usize,
    mut chunks: I,
) -> Result<CommitOutcome, CommitError>
where
    I: Iterator<Item = Result<Vec<u8>, String>>,
{
    let absolute = resolve_guarded_destination(vault_path, dest_path)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| CommitError::Io("destination has no parent directory".to_string()))?;

    let created = !absolute.exists();

    let temp_path = parent.join(format!("{TEMP_PREFIX}{}.tmp", random_token()));
    let mut temp_file = match fs::File::create(&temp_path) {
        Ok(file) => file,
        Err(error) => return Err(CommitError::Io(error.to_string())),
    };

    let mut total: usize = 0;
    // FNV-1a, computed incrementally so we never buffer the whole file in RAM.
    // Must stay byte-for-byte equivalent to `crate::tools::content_hash`.
    let mut hash_state: u64 = 0xcbf2_9ce4_8422_2325;
    let result = (|| -> Result<(), CommitError> {
        while let Some(chunk) = chunks.next() {
            let chunk = chunk.map_err(CommitError::Io)?;
            total = total.saturating_add(chunk.len());
            if total > max_bytes {
                return Err(CommitError::TooLarge);
            }
            temp_file
                .write_all(&chunk)
                .map_err(|error| CommitError::Io(error.to_string()))?;
            for byte in &chunk {
                hash_state ^= u64::from(*byte);
                hash_state = hash_state.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        temp_file
            .flush()
            .map_err(|error| CommitError::Io(error.to_string()))?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    drop(temp_file);

    // Optimistic concurrency: re-read the destination at commit if requested.
    if let Some(expected) = expected_hash {
        let current_hash = match fs::read(&absolute) {
            Ok(bytes) => Some(crate::tools::content_hash(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                return Err(CommitError::Io(error.to_string()));
            }
        };
        if current_hash.as_deref() != Some(expected) {
            let _ = fs::remove_file(&temp_path);
            return Err(CommitError::HashConflict {
                expected: expected.to_string(),
                found: current_hash.unwrap_or_else(|| "null".to_string()),
            });
        }
    }

    let hash = format!("fnv1a64:{hash_state:016x}");
    if let Err(error) = fs::rename(&temp_path, &absolute) {
        let _ = fs::remove_file(&temp_path);
        return Err(CommitError::Io(error.to_string()));
    }

    Ok(CommitOutcome {
        created,
        bytes_written: total,
        hash,
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

    #[test]
    fn commit_stream_writes_file_and_reports_created() {
        let dir = std::env::temp_dir().join(format!(
            "upload-test-{}-{}",
            std::process::id(),
            random_token()
        ));
        fs::create_dir_all(&dir).unwrap();
        let chunks = vec![Ok(b"hello ".to_vec()), Ok(b"world".to_vec())];
        let outcome = commit_stream(
            &dir,
            "sub/out.bin",
            None,
            DEFAULT_MAX_UPLOAD_BYTES,
            chunks.into_iter(),
        )
        .unwrap();
        assert!(outcome.created);
        assert_eq!(outcome.bytes_written, 11);
        assert_eq!(outcome.hash, crate::tools::content_hash(b"hello world"));
        let written = fs::read(dir.join("sub/out.bin")).unwrap();
        assert_eq!(written, b"hello world");

        // Second write is an update, not a create.
        let chunks = vec![Ok(b"again".to_vec())];
        let outcome = commit_stream(
            &dir,
            "sub/out.bin",
            None,
            DEFAULT_MAX_UPLOAD_BYTES,
            chunks.into_iter(),
        )
        .unwrap();
        assert!(!outcome.created);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_stream_aborts_on_oversize() {
        let dir = std::env::temp_dir().join(format!(
            "upload-test-{}-{}",
            std::process::id(),
            random_token()
        ));
        fs::create_dir_all(&dir).unwrap();
        // Cap of 4 bytes, stream 10.
        let chunks = vec![Ok(b"12345".to_vec()), Ok(b"67890".to_vec())];
        let err = commit_stream(&dir, "big.bin", None, 4, chunks.into_iter()).unwrap_err();
        assert!(matches!(err, CommitError::TooLarge));
        // Destination must not exist and no temp file left behind.
        assert!(!dir.join("big.bin").exists());
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".upload-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file should be cleaned up");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_stream_rejects_hash_conflict() {
        let dir = std::env::temp_dir().join(format!(
            "upload-test-{}-{}",
            std::process::id(),
            random_token()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("doc.bin"), b"current").unwrap();
        let chunks = vec![Ok(b"new".to_vec())];
        let err = commit_stream(
            &dir,
            "doc.bin",
            Some("fnv1a64:0000000000000000"),
            DEFAULT_MAX_UPLOAD_BYTES,
            chunks.into_iter(),
        )
        .unwrap_err();
        assert!(matches!(err, CommitError::HashConflict { .. }));
        // Original is untouched.
        assert_eq!(fs::read(dir.join("doc.bin")).unwrap(), b"current");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_removes_only_stale_orphan_temp_files() {
        let vault = std::env::temp_dir().join(format!(
            "upload-sweep-{}-{}",
            std::process::id(),
            random_token()
        ));
        fs::create_dir_all(vault.join("sub")).unwrap();
        let orphan = vault.join("sub/.upload-old.tmp");
        fs::write(&orphan, b"junk").unwrap();
        let keep = vault.join("sub/real.bin");
        fs::write(&keep, b"data").unwrap();
        let temp_named_but_not_prefix = vault.join("sub/notes.tmp");
        fs::write(&temp_named_but_not_prefix, b"keep").unwrap();

        // With `now` at the file's creation time, nothing is stale yet.
        sweep_orphan_temp_files_at(&vault, SystemTime::now());
        assert!(orphan.exists(), "fresh orphan must be preserved");

        // Advance `now` well past the TTL: the orphan is now stale and removed,
        // while non-`.upload-` files are always preserved regardless of age.
        let future = SystemTime::now() + TOKEN_TTL + Duration::from_secs(60);
        sweep_orphan_temp_files_at(&vault, future);
        assert!(!orphan.exists(), "stale orphan should be removed");
        assert!(keep.exists(), "unrelated file should be preserved");
        assert!(
            temp_named_but_not_prefix.exists(),
            "non-upload .tmp file should be preserved"
        );
        let _ = fs::remove_dir_all(&vault);
    }

    #[test]
    fn commit_stream_rejects_symlink_escape() {
        let outside = std::env::temp_dir().join(format!(
            "upload-outside-{}-{}",
            std::process::id(),
            random_token()
        ));
        let vault = std::env::temp_dir().join(format!(
            "upload-vault-{}-{}",
            std::process::id(),
            random_token()
        ));
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&vault).unwrap();
        // Create a symlink inside the vault pointing outside it.
        let link = vault.join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(not(unix))]
        {
            let _ = link;
            let _ = fs::remove_dir_all(&outside);
            let _ = fs::remove_dir_all(&vault);
            return;
        }
        let chunks = vec![Ok(b"x".to_vec())];
        let err = commit_stream(
            &vault,
            "escape/evil.bin",
            None,
            DEFAULT_MAX_UPLOAD_BYTES,
            chunks.into_iter(),
        )
        .unwrap_err();
        assert!(matches!(err, CommitError::EscapesVault));
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&vault);
    }
}
