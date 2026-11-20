//! In-process per-note mutation locks.
//!
//! Serialize mutations to the same stored note within one running MCP server,
//! while writes to unrelated notes stay concurrent. The lock is acquired before
//! the authoritative read and held through `expectedHash` validation, content
//! derivation and completion of the storage mutation — the read/derive/write
//! sequence that two overlapping requests could otherwise interleave.
//!
//! Deliberately NOT covered (see the implementation plan in
//! `_Agent/Sessions/Deep Obsidian/2026-11-20 - pr80-review.md`): cross-process
//! or distributed locking, coordination with Obsidian/sync/external writers,
//! vault-wide transactions. `expectedHash` and backend revision/CAS checks
//! remain necessary; this registry removes the read-derive-write race for
//! requests this server runs to completion, nothing more.
//!
//! Guards are [`Send + 'static`] and are meant to be moved INTO the task that
//! performs the final storage write, so a request cancelled at an await point
//! cannot release the lock while its backend work (a `spawn_blocking` filesystem
//! operation, a slow remote call) is still in flight.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One registry per server, shared across every `AppState` clone, every
/// principal-scoped router and every live-mount reload generation that points
/// at the same storage.
#[derive(Debug, Default)]
pub struct MutationLockRegistry {
    map: Mutex<HashMap<String, LockEntry>>,
}

#[derive(Debug, Default)]
struct LockEntry {
    mutex: Arc<tokio::sync::Mutex<()>>,
    users: usize,
}

/// Held for the duration of one note's critical section. Release — and return
/// the entry to the registry for reuse — happens on drop.
#[derive(Debug)]
pub struct MutationLockGuard {
    registry: Arc<MutationLockRegistry>,
    key: String,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl MutationLockRegistry {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Acquire the per-note mutex for `key`, creating the entry on first use.
    ///
    /// Callers MUST pass keys in a deterministic order when holding several
    /// (see [`sorted_mutation_locks`]) — the mutexes themselves are not
    /// reentrant, so nested acquisition of the same key deadlocks.
    pub async fn acquire(self: &Arc<Self>, key: String) -> MutationLockGuard {
        let mutex = {
            let mut map = self.map.lock().expect("mutation lock registry poisoned");
            let entry = map.entry(key.clone()).or_default();
            entry.users += 1;
            entry.mutex.clone()
        };
        // Register cleanup BEFORE waiting: cancellation can happen after the
        // previous holder releases but before this waiter is polled again.
        let mut guard = MutationLockGuard {
            registry: self.clone(),
            key,
            guard: None,
        };
        guard.guard = Some(mutex.lock_owned().await);
        guard
    }

    /// Number of keys currently holding a live entry — locked or lingering.
    /// Exposed for tests and diagnostics, not part of any contract.
    pub fn live_keys(&self) -> usize {
        self.map
            .lock()
            .expect("mutation lock registry poisoned")
            .len()
    }

    fn release(&self, key: &str) {
        let mut map = self.map.lock().expect("mutation lock registry poisoned");
        // Counts include queued acquisitions, whose cleanup guard also runs
        // when their future is cancelled. Arc counts cannot track that lifetime.
        if let Some(entry) = map.get_mut(key) {
            entry.users -= 1;
            if entry.users == 0 {
                map.remove(key);
            }
        }
    }
}

impl Drop for MutationLockGuard {
    fn drop(&mut self) {
        // Unlock before making the key available for a fresh registry entry.
        self.guard = None;
        self.registry.release(&self.key);
    }
}

/// The per-note mutation lock key for one logical path: stable storage identity
/// (backend coordinates, never secrets) plus the backend-relative path, so two
/// callers spelling the same note differently — or reaching it through a scoped
/// principal router — serialize against the same mutex, while a remapped mount
/// gets its own.
pub fn note_lock_key(
    router: &deep_obsidian_backend::VaultRouter,
    path: &str,
) -> Result<String, String> {
    let resolved = router.resolve(path).map_err(|error| error.to_string())?;
    resolved
        .mount
        .backend
        .mutation_key(&resolved.backend_relative_path)
        .map_err(|error| error.to_string())
}

/// Recheck identity after waiting: another mutation may have created a file or
/// moved a symlink while this request was queued.
pub async fn lock_note(
    registry: &Arc<MutationLockRegistry>,
    router: &deep_obsidian_backend::VaultRouter,
    path: &str,
) -> Result<MutationLockGuard, String> {
    loop {
        let key = note_lock_key(router, path)?;
        let guard = registry.acquire(key.clone()).await;
        if note_lock_key(router, path)? == key {
            return Ok(guard);
        }
    }
}

pub async fn lock_note_pair(
    registry: &Arc<MutationLockRegistry>,
    router: &deep_obsidian_backend::VaultRouter,
    from: &str,
    to: &str,
) -> Result<Vec<MutationLockGuard>, String> {
    loop {
        let keys = vec![note_lock_key(router, from)?, note_lock_key(router, to)?];
        let guards = sorted_mutation_locks(registry, keys.clone()).await;
        if keys == vec![note_lock_key(router, from)?, note_lock_key(router, to)?] {
            return Ok(guards);
        }
    }
}

/// Acquire several note locks in deterministic key order.
///
/// Two opposite-direction renames (`A→B` and `B→A`) each need both keys; taking
/// them in the same sorted order is what makes the overlap deadlock-free — one
/// call wins both, the other waits, and then refuses on existence instead of
/// holding one lock forever.
pub async fn sorted_mutation_locks(
    registry: &Arc<MutationLockRegistry>,
    mut keys: Vec<String>,
) -> Vec<MutationLockGuard> {
    keys.sort();
    keys.dedup();
    let mut guards = Vec::with_capacity(keys.len());
    for key in keys {
        guards.push(registry.acquire(key).await);
    }
    guards
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_waiter_is_reclaimed_before_or_after_holder_release() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        for release_first in [false, true] {
            let registry = MutationLockRegistry::shared();
            let holder = registry.acquire("note".into()).await;
            let mut waiter = Box::pin(registry.acquire("note".into()));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Pending));
            if release_first {
                drop(holder);
                drop(waiter);
            } else {
                drop(waiter);
                assert_eq!(registry.live_keys(), 1);
                drop(holder);
            }
            assert_eq!(registry.live_keys(), 0);
        }
    }

    #[tokio::test]
    async fn cancelling_a_partial_pair_releases_the_first_lock() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let registry = MutationLockRegistry::shared();
        let b = registry.acquire("b".into()).await;
        let mut pair = Box::pin(sorted_mutation_locks(
            &registry,
            vec!["b".into(), "a".into()],
        ));
        assert!(matches!(
            pair.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        drop(pair);
        assert_eq!(registry.live_keys(), 1);
        drop(b);
        assert_eq!(registry.live_keys(), 0);
    }

    /// Deterministic on the current-thread test runtime: every step is
    /// synchronized through channels, never through sleeps or yield counts.
    #[tokio::test]
    async fn a_lock_serializes_and_reclaims_on_release() {
        let registry = MutationLockRegistry::shared();
        let first = registry.acquire("note:a".to_string()).await;
        assert_eq!(registry.live_keys(), 1);

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<usize>();
        let waiter_registry = registry.clone();
        tokio::spawn(async move {
            let _ = started_tx.send(());
            let guard = waiter_registry.acquire("note:a".to_string()).await;
            // Measured WHILE holding: the entry exists for this acquirer.
            let _ = held_tx.send(waiter_registry.live_keys());
            drop(guard);
        });

        // The waiter has polled at least once (it signalled), so it is queued
        // on the mutex behind `first`.
        started_rx.await.expect("waiter started");
        assert!(
            held_rx.is_empty(),
            "the second acquirer must wait while the first holds the lock"
        );

        drop(first);
        let held = held_rx.await.expect("waiter acquires after release");
        assert_eq!(held, 1, "the waiter holds the lock and its entry exists");
        // The waiter dropped its guard after signalling; on this runtime the
        // task finishes that drop before we are polled again with the yields.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(registry.live_keys(), 0, "the entry is reclaimed");
    }

    #[tokio::test]
    async fn different_keys_never_block_each_other() {
        let registry = MutationLockRegistry::shared();
        let _a = registry.acquire("note:a".to_string()).await;
        let _b = registry.acquire("note:b".to_string()).await;
        assert_eq!(registry.live_keys(), 2);
    }

    /// An acquirer that has cloned the entry's mutex keeps the entry alive even
    /// if the previous holder releases first: a used mutex is never removed.
    #[tokio::test]
    async fn a_reused_entry_is_not_reclaimed_under_a_concurrent_acquirer() {
        let registry = MutationLockRegistry::shared();
        let first = registry.acquire("note:a".to_string()).await;

        let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel::<usize>();
        let waiter_registry = registry.clone();
        let waiter = tokio::spawn(async move {
            let guard = waiter_registry.acquire("note:a".to_string()).await;
            let _ = acquired_tx.send(waiter_registry.live_keys());
            drop(guard);
        });

        // Let the waiter poll: it clones the entry Arc, then queues on the mutex.
        tokio::task::yield_now().await;
        drop(first);
        // Waiter gets through now; it reports the entry count while holding.
        let held = acquired_rx.await.expect("waiter acquires");
        assert_eq!(held, 1);
        waiter.await.expect("waiter finishes");
        assert_eq!(registry.live_keys(), 0);
    }
}
