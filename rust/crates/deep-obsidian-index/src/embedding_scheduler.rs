//! Provider-independent admission before each real embedding HTTP request.
//! Cache hits never reach this module. A permit covers one request, not a rebuild.
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use deep_obsidian_types::EmbeddingSchedulingConfig;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::embeddings::EmbeddingConfig;

const POLL: Duration = Duration::from_millis(20);
const MAX_GROUPS: usize = 128;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Interactive,
    Background,
}
#[derive(Debug, Error)]
pub enum SchedulingError {
    #[error("invalid embedding scheduling policy: {0}")]
    Configuration(String),
    #[error("embedding queue wait timed out")]
    QueueTimeout,
    #[error("interactive embedding queue is full")]
    QueueFull,
    #[error("embedding request abandoned before execution")]
    Cancelled,
}
pub type CancelCheck = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn check(&self) -> CancelCheck {
        let token = self.clone();
        Arc::new(move || token.is_cancelled())
    }
}
thread_local! { static CANCELLATION: RefCell<Option<CancelCheck>> = const { RefCell::new(None) }; }
pub fn current_cancellation() -> Option<CancelCheck> {
    CANCELLATION.with(|c| c.borrow().clone())
}
pub fn with_cancellation<T>(check: Option<CancelCheck>, operation: impl FnOnce() -> T) -> T {
    struct Reset(Option<CancelCheck>);
    impl Drop for Reset {
        fn drop(&mut self) {
            CANCELLATION.with(|c| {
                c.replace(self.0.take());
            });
        }
    }
    let _reset = Reset(CANCELLATION.with(|c| c.replace(check)));
    operation()
}
fn cancelled(check: &Option<CancelCheck>) -> bool {
    check.as_ref().is_some_and(|check| check())
}

impl Priority {
    fn index(self) -> usize {
        match self {
            Self::Interactive => 0,
            Self::Background => 1,
        }
    }
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassMetrics {
    pub started: u64,
    pub completed: u64,
    pub failed: u64,
    pub inputs: u64,
    pub estimated_input_tokens: u64,
    pub cancelled: u64,
    pub timed_out: u64,
    pub rejected: u64,
    pub queue_wait_ms: u64,
    pub backend_ms: u64,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerDiagnostics {
    pub backend_id: String,
    pub max_in_flight: usize,
    pub active: usize,
    pub queued_interactive: usize,
    pub queued_background: usize,
    pub interactive: ClassMetrics,
    pub background: ClassMetrics,
}
#[derive(Default)]
struct State {
    active: usize,
    next: u64,
    interactive_streak: usize,
    queues: [VecDeque<u64>; 2],
    metrics: [ClassMetrics; 2],
}
struct Group {
    id: String,
    policy: EmbeddingSchedulingConfig,
    state: Mutex<State>,
    changed: Condvar,
}
fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}
impl Group {
    fn new(id: String, policy: EmbeddingSchedulingConfig) -> Self {
        Self {
            id,
            policy,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }
    }
    fn acquire(self: &Arc<Self>, priority: Priority) -> Result<Permit, SchedulingError> {
        let began = Instant::now();
        let check = current_cancellation();
        let deadline = (priority == Priority::Interactive)
            .then(|| Duration::from_millis(self.policy.interactive_wait_timeout_ms));
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let class = priority.index();
        // Background producers wait here rather than filling an unbounded queue.
        // Interactive callers have their own capacity, independent of background work.
        while state.queues[class].len() >= self.policy.queue_capacity {
            if cancelled(&check) {
                return Err(SchedulingError::Cancelled);
            }
            if priority == Priority::Interactive {
                state.metrics[class].rejected += 1;
                return Err(SchedulingError::QueueFull);
            }
            state = self
                .changed
                .wait_timeout(state, POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let ticket = state.next;
        state.next = state.next.wrapping_add(1);
        state.queues[class].push_back(ticket);
        loop {
            let failure = if cancelled(&check) {
                Some(SchedulingError::Cancelled)
            } else if deadline.is_some_and(|limit| began.elapsed() >= limit) {
                Some(SchedulingError::QueueTimeout)
            } else {
                None
            };
            if let Some(error) = failure {
                state.queues[class].retain(|&entry| entry != ticket);
                match error {
                    SchedulingError::Cancelled => state.metrics[class].cancelled += 1,
                    SchedulingError::QueueTimeout => state.metrics[class].timed_out += 1,
                    _ => {}
                }
                state.metrics[class].queue_wait_ms += millis(began.elapsed());
                self.changed.notify_all();
                return Err(error);
            }
            let background_due = !state.queues[1].is_empty()
                && (state.queues[0].is_empty()
                    || state.interactive_streak >= self.policy.interactive_burst);
            let selected = if background_due { 1 } else { 0 };
            if state.active < self.policy.max_in_flight.expect("finite registered policy")
                && class == selected
                && state.queues[class].front() == Some(&ticket)
            {
                state.queues[class].pop_front();
                state.active += 1;
                if class == 0 && !state.queues[1].is_empty() {
                    state.interactive_streak = state.interactive_streak.saturating_add(1);
                } else {
                    state.interactive_streak = 0;
                }
                let elapsed = began.elapsed();
                state.metrics[class].started += 1;
                state.metrics[class].queue_wait_ms += millis(elapsed);
                self.changed.notify_all();
                tracing::debug!(priority = ?priority, queue_wait_ms = millis(elapsed), "embedding admitted");
                return Ok(Permit {
                    group: Some(Arc::clone(self)),
                    priority,
                    successful: false,
                    started: Instant::now(),
                });
            }
            state = self
                .changed
                .wait_timeout(state, POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    fn diagnostics(&self) -> SchedulerDiagnostics {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        SchedulerDiagnostics {
            backend_id: self.id.clone(),
            max_in_flight: self.policy.max_in_flight.unwrap(),
            active: s.active,
            queued_interactive: s.queues[0].len(),
            queued_background: s.queues[1].len(),
            interactive: s.metrics[0].clone(),
            background: s.metrics[1].clone(),
        }
    }
}

pub struct Permit {
    group: Option<Arc<Group>>,
    priority: Priority,
    successful: bool,
    started: Instant,
}
impl Permit {
    pub(crate) fn record_inputs(&self, inputs: usize, estimated_tokens: usize) {
        if let Some(group) = &self.group {
            let mut state = group.state.lock().unwrap_or_else(|e| e.into_inner());
            let metrics = &mut state.metrics[self.priority.index()];
            metrics.inputs = metrics.inputs.saturating_add(inputs as u64);
            metrics.estimated_input_tokens = metrics
                .estimated_input_tokens
                .saturating_add(estimated_tokens as u64);
        }
    }
    pub(crate) fn success(&mut self) {
        self.successful = true;
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let elapsed = millis(self.started.elapsed());
        if let Some(group) = &self.group {
            let mut state = group.state.lock().unwrap_or_else(|e| e.into_inner());
            state.active -= 1;
            let metrics = &mut state.metrics[self.priority.index()];
            metrics.completed += 1;
            if !self.successful {
                metrics.failed += 1;
            }
            metrics.backend_ms += elapsed;
            group.changed.notify_all();
        }
        tracing::debug!(priority = ?self.priority, backend_ms = elapsed, "embedding request finished");
    }
}
#[derive(Default)]
struct Registry {
    groups: HashMap<String, Arc<Group>>,
    // A configured alias also covers callers using that same URL without an alias.
    endpoints: HashMap<String, String>,
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}
fn endpoint(base_url: &str) -> String {
    reqwest::Url::parse(base_url)
        .map(|mut url| {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|_| base_url.to_string())
        .trim_end_matches('/')
        .to_string()
}
fn fingerprint(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn policy_key(base_url: &str, policy: Option<&EmbeddingSchedulingConfig>) -> String {
    match policy.and_then(|p| p.group.as_deref()) {
        Some(group) => format!("group:{}", group.trim()),
        None => format!("endpoint:{}", endpoint(base_url)),
    }
}
fn validate(policy: &EmbeddingSchedulingConfig) -> Result<(), SchedulingError> {
    if policy.max_in_flight == Some(0)
        || policy.interactive_burst == 0
        || policy.queue_capacity == 0
        || policy.interactive_wait_timeout_ms == 0
        || policy.background_batch_size == 0
        || policy
            .group
            .as_ref()
            .is_some_and(|group| group.trim().is_empty())
    {
        return Err(SchedulingError::Configuration(
            "limits must be positive; omit maxInFlight for unlimited".into(),
        ));
    }
    Ok(())
}
/// Register finite policies before starting index/search work. Conflicting policies for
/// the same shared resource fail explicitly; changing them requires a process restart.
pub fn register_backend(
    base_url: &str,
    policy: Option<&EmbeddingSchedulingConfig>,
) -> Result<(), SchedulingError> {
    let Some(policy) = policy else {
        return Ok(());
    };
    validate(policy)?;
    if policy.max_in_flight.is_none() {
        return Ok(());
    }
    let endpoint = endpoint(base_url);
    let key = policy_key(base_url, Some(policy));
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    if registry
        .endpoints
        .get(&endpoint)
        .is_some_and(|existing| existing != &key)
    {
        return Err(SchedulingError::Configuration(
            "one endpoint declares conflicting resource groups; restart after changing policies"
                .into(),
        ));
    }
    if let Some(group) = registry.groups.get(&key) {
        let mut old = group.policy.clone();
        old.group = None;
        let mut new = policy.clone();
        new.group = None;
        if old != new {
            return Err(SchedulingError::Configuration(
                "shared resource policies disagree; restart after changing policies".into(),
            ));
        }
    } else {
        if registry.groups.len() >= MAX_GROUPS || registry.endpoints.len() >= MAX_GROUPS {
            return Err(SchedulingError::Configuration(
                "too many configured embedding resource groups".into(),
            ));
        }
        registry.groups.insert(
            key.clone(),
            Arc::new(Group::new(fingerprint(&key), policy.clone())),
        );
    }
    if !registry.endpoints.contains_key(&endpoint) && registry.endpoints.len() >= MAX_GROUPS {
        return Err(SchedulingError::Configuration(
            "too many embedding endpoint aliases".into(),
        ));
    }
    registry.endpoints.insert(endpoint, key);
    Ok(())
}
fn lookup(base_url: &str, policy: Option<&EmbeddingSchedulingConfig>) -> Option<Arc<Group>> {
    let registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    let key = if policy.and_then(|p| p.group.as_ref()).is_some() {
        policy_key(base_url, policy)
    } else {
        registry
            .endpoints
            .get(&endpoint(base_url))
            .cloned()
            .unwrap_or_else(|| policy_key(base_url, policy))
    };
    registry.groups.get(&key).cloned()
}
pub fn diagnostics_for_backend(
    base_url: &str,
    policy: Option<&EmbeddingSchedulingConfig>,
) -> Option<SchedulerDiagnostics> {
    lookup(base_url, policy).map(|group| group.diagnostics())
}
pub(crate) fn background_batch_size(config: &EmbeddingConfig) -> Option<usize> {
    lookup(config.base_url()?, config.scheduling.as_ref())
        .map(|group| group.policy.background_batch_size)
}
pub(crate) fn acquire(
    config: &EmbeddingConfig,
    priority: Priority,
) -> Result<Permit, SchedulingError> {
    let check = current_cancellation();
    if cancelled(&check) {
        return Err(SchedulingError::Cancelled);
    }
    let base = config.base_url().unwrap_or_default();
    register_backend(base, config.scheduling.as_ref())?;
    match lookup(base, config.scheduling.as_ref()) {
        Some(group) => group.acquire(priority),
        None => Ok(Permit {
            group: None,
            priority,
            successful: false,
            started: Instant::now(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    fn policy() -> EmbeddingSchedulingConfig {
        EmbeddingSchedulingConfig {
            max_in_flight: Some(1),
            interactive_wait_timeout_ms: 2000,
            ..Default::default()
        }
    }
    fn group(policy: EmbeddingSchedulingConfig) -> Arc<Group> {
        Arc::new(Group::new("test".into(), policy))
    }
    fn until(mut predicate: impl FnMut() -> bool) {
        let start = Instant::now();
        while !predicate() {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "condition did not become true"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn interactive_overtakes_background_with_eight_to_one_fairness() {
        let g = group(policy());
        let initial = g.acquire(Priority::Background).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for class in [Priority::Background, Priority::Interactive] {
            let count = if class == Priority::Interactive {
                17
            } else {
                2
            };
            for _ in 0..count {
                let (g, tx) = (g.clone(), tx.clone());
                handles.push(thread::spawn(move || {
                    let _permit = g.acquire(class).unwrap();
                    tx.send(class).unwrap();
                }));
            }
        }
        until(|| {
            let s = g.state.lock().unwrap();
            s.queues[0].len() == 17 && s.queues[1].len() == 2
        });
        drop(initial);
        let order: Vec<_> = (0..19)
            .map(|_| rx.recv_timeout(Duration::from_secs(3)).unwrap())
            .collect();
        assert_eq!(order[8], Priority::Background);
        assert_eq!(order[17], Priority::Background);
        assert!(order[..8].iter().all(|p| *p == Priority::Interactive));
        assert!(order[9..17].iter().all(|p| *p == Priority::Interactive));
        assert_eq!(order[18], Priority::Interactive);
        for handle in handles {
            handle.join().unwrap();
        }
        let d = g.diagnostics();
        assert_eq!(d.interactive.started, 17);
        assert_eq!(d.background.completed, 3);
        assert_eq!(d.active, 0);
    }
    #[test]
    fn finite_capacity_and_raii_release_on_panic() {
        let g = group(EmbeddingSchedulingConfig {
            max_in_flight: Some(2),
            ..policy()
        });
        let first = g.acquire(Priority::Background).unwrap();
        let second = g.acquire(Priority::Interactive).unwrap();
        let other = g.clone();
        let handle = thread::spawn(move || {
            let _permit = other.acquire(Priority::Interactive).unwrap();
            panic!("release during unwind");
        });
        until(|| g.diagnostics().queued_interactive == 1);
        assert_eq!(g.diagnostics().active, 2);
        drop(first);
        assert!(handle.join().is_err());
        assert_eq!(g.diagnostics().active, 1);
        drop(second);
        assert_eq!(g.diagnostics().active, 0);
    }
    #[test]
    fn deadline_full_queue_and_cancellation_remove_pending_jobs() {
        let g = group(EmbeddingSchedulingConfig {
            queue_capacity: 1,
            interactive_wait_timeout_ms: 80,
            ..policy()
        });
        let held = g.acquire(Priority::Background).unwrap();
        let other = g.clone();
        let timeout = thread::spawn(move || other.acquire(Priority::Interactive));
        until(|| g.diagnostics().queued_interactive == 1);
        assert!(matches!(
            g.acquire(Priority::Interactive),
            Err(SchedulingError::QueueFull)
        ));
        assert!(matches!(
            timeout.join().unwrap(),
            Err(SchedulingError::QueueTimeout)
        ));
        let token = CancellationToken::default();
        let (other, check) = (g.clone(), token.check());
        let cancelled = thread::spawn(move || {
            with_cancellation(Some(check), || other.acquire(Priority::Interactive))
        });
        until(|| g.diagnostics().queued_interactive == 1);
        token.cancel();
        assert!(matches!(
            cancelled.join().unwrap(),
            Err(SchedulingError::Cancelled)
        ));
        let d = g.diagnostics();
        assert_eq!(d.queued_interactive, 0);
        assert_eq!(d.interactive.started, 0);
        assert_eq!(d.interactive.timed_out, 1);
        assert_eq!(d.interactive.cancelled, 1);
        drop(held);
    }
    #[test]
    fn background_backpressure_keeps_queue_bounded_and_resumes() {
        let g = group(EmbeddingSchedulingConfig {
            queue_capacity: 1,
            ..policy()
        });
        let held = g.acquire(Priority::Background).unwrap();
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let g = g.clone();
                thread::spawn(move || {
                    let _permit = g.acquire(Priority::Background).unwrap();
                })
            })
            .collect();
        until(|| g.diagnostics().queued_background == 1);
        assert_eq!(g.diagnostics().queued_background, 1);
        drop(held);
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(g.diagnostics().background.completed, 4);
    }
    #[test]
    fn backend_groups_share_aliases_ignore_credentials_and_reject_conflicts() {
        assert_eq!(
            endpoint("https://u:password@SCHEDULER-A.invalid:443/v1/#ignored"),
            endpoint("https://scheduler-a.invalid/v1")
        );
        let p = EmbeddingSchedulingConfig {
            group: Some("scheduler-test-aliases".into()),
            ..policy()
        };
        register_backend("https://scheduler-a.invalid/v1/", Some(&p)).unwrap();
        register_backend("https://scheduler-b.invalid/v1", Some(&p)).unwrap();
        let a = lookup("https://scheduler-a.invalid/v1", None).unwrap();
        let b = lookup("https://scheduler-b.invalid/v1", Some(&p)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let mut config = EmbeddingConfig::sparse();
        config.base_url = Some("https://scheduler-a.invalid/v1".into());
        config.api_key = Some("credential-a".into());
        config.model = Some("model-a".into());
        let held = acquire(&config, Priority::Background).unwrap();
        config.base_url = Some("https://scheduler-b.invalid/v1".into());
        config.scheduling = Some(p.clone());
        config.api_key = Some("credential-b".into());
        config.model = Some("model-b".into());
        let token = CancellationToken::default();
        let check = token.check();
        let handle = thread::spawn(move || {
            with_cancellation(Some(check), || acquire(&config, Priority::Interactive))
        });
        until(|| a.diagnostics().queued_interactive == 1);
        token.cancel();
        assert!(matches!(
            handle.join().unwrap(),
            Err(SchedulingError::Cancelled)
        ));
        drop(held);
        let mut conflict = p.clone();
        conflict.max_in_flight = Some(2);
        assert!(matches!(
            register_backend("https://scheduler-a.invalid/v1", Some(&conflict)),
            Err(SchedulingError::Configuration(_))
        ));
        let printed = serde_json::to_string(&a.diagnostics()).unwrap();
        assert!(!printed.contains("scheduler-a"));
        assert!(!printed.contains("credential"));
    }
    #[test]
    fn unlimited_is_default_and_adds_no_admission_cap() {
        let mut config = EmbeddingConfig::sparse();
        config.base_url = Some("https://unlimited-scheduler-test.invalid/v1".into());
        config.scheduling = Some(EmbeddingSchedulingConfig::default());
        let permits: Vec<_> = (0..32)
            .map(|_| acquire(&config, Priority::Background).unwrap())
            .collect();
        assert!(permits.iter().all(|p| p.group.is_none()));
        assert!(background_batch_size(&config).is_none());
        assert!(
            diagnostics_for_backend(config.base_url().unwrap(), config.scheduling.as_ref())
                .is_none()
        );
    }
    /// Synthetic one-slot backend: 16 queued 20ms indexing requests, followed by
    /// 32 queued 2ms searches. Compare FIFO with class-aware 8:1 admission.
    #[test]
    #[ignore = "manual synthetic contention benchmark; no real provider involved"]
    fn priority_scheduler_contention_benchmark() {
        for prioritise in [false, true] {
            let g = group(EmbeddingSchedulingConfig {
                interactive_wait_timeout_ms: 10_000,
                ..policy()
            });
            let held = g.acquire(Priority::Background).unwrap();
            let clock = Arc::new(OnceLock::<Instant>::new());
            let (tx, rx) = mpsc::channel();
            let mut handles = Vec::new();
            for search in [false, true] {
                let count = if search { 32 } else { 16 };
                for _ in 0..count {
                    let (g, tx, clock) = (g.clone(), tx.clone(), clock.clone());
                    handles.push(thread::spawn(move || {
                        let class = if search && prioritise {
                            Priority::Interactive
                        } else {
                            Priority::Background
                        };
                        let mut permit = g.acquire(class).unwrap();
                        thread::sleep(Duration::from_millis(if search { 2 } else { 20 }));
                        if search {
                            tx.send(clock.get().unwrap().elapsed()).unwrap();
                        }
                        permit.success();
                    }));
                }
                until(|| {
                    let d = g.diagnostics();
                    d.queued_interactive + d.queued_background == if search { 48 } else { 16 }
                });
            }
            clock.set(Instant::now()).unwrap();
            drop(held);
            let mut latency: Vec<_> = (0..32)
                .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
                .collect();
            latency.sort();
            eprintln!(
                "{} search completion under indexing: p50={}ms p95={}ms (synthetic)",
                if prioritise { "priority-8:1" } else { "FIFO" },
                millis(latency[15]),
                millis(latency[30])
            );
            for handle in handles {
                handle.join().unwrap();
            }
        }
    }
}
