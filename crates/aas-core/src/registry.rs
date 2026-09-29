//! The configured harnesses, their probe results, and the explicit recovery of harnesses a
//! probe found unavailable (design.md §9.4).
//!
//! A harness is probed at startup, on `harness/refresh`, when its adapter reports that its
//! information changed, when a request needs it while it is unavailable, and — while it stays
//! unavailable — again on a schedule with a doubling wait (`policy.harness_retry_*`). The
//! schedule is a retry policy, not an inference: nothing about the harness is guessed from
//! time passing, it is simply asked again.

use std::collections::HashMap;
use std::sync::Arc;

use aas_harness::{HarnessAdapter, HarnessFeatures, HarnessInfo};
use aas_protocol::{ErrorKind, Harness, RpcError};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::config::Policy;
use crate::error::CoreError;

/// Adapters injected by the daemon, in configuration order.
pub struct HarnessRegistry {
    entries: Vec<Entry>,
    /// Set once the initial probe of every harness has finished.
    ready: watch::Sender<bool>,
    /// Counts completed probes; wakes the retry schedule, whose due times they change.
    probes: watch::Sender<u64>,
}

struct Entry {
    adapter: Arc<dyn HarnessAdapter>,
    /// Held while this harness is probed: one CLI start at a time, and a caller that waited
    /// for a probe in progress can use its result.
    probing: tokio::sync::Mutex<()>,
    state: Mutex<ProbeState>,
}

#[derive(Default)]
struct ProbeState {
    info: Option<HarnessInfo>,
    /// What the adapter offered beyond `info` right after that probe (none while unavailable).
    features: HarnessFeatures,
    /// When the probe that produced `info` started, and when it ended.
    started: Option<Instant>,
    finished: Option<Instant>,
    /// Consecutive probes that found the harness unavailable (0 once one finds it available).
    failures: u32,
    /// Start of the probe that runs right now.
    running_since: Option<Instant>,
}

/// Result of [`HarnessRegistry::probe`].
#[derive(Debug, Clone, PartialEq)]
pub struct Probed {
    /// The protocol view after the probe.
    pub harness: Harness,
    /// Whether the view differs from the one before (a result that was reused never does).
    pub changed: bool,
}

/// Clears `running_since` when a probe ends, also when its future is dropped half-way.
struct RunningProbe<'a>(&'a Mutex<ProbeState>);

impl Drop for RunningProbe<'_> {
    fn drop(&mut self) {
        self.0.lock().running_since = None;
    }
}

/// The error of a request that needs harness `id` while it is unavailable. Not definitive
/// (protocol.md §1.3): the daemon probes an unavailable harness again on its own and before
/// every such request, so the same request can succeed later. `data.harnessId` and
/// `data.reason` tell the client which harness waits for what.
pub(crate) fn unavailable_error(id: &str, info: Option<&HarnessInfo>) -> CoreError {
    let reason = info
        .and_then(|i| i.unavailable_reason.clone())
        .unwrap_or_else(|| "not probed yet".to_owned());
    CoreError::Rpc(
        RpcError::new(
            ErrorKind::HarnessUnavailable,
            format!("harness {id} is unavailable: {reason}"),
        )
        .with("harnessId", id)
        .with("reason", reason),
    )
}

impl HarnessRegistry {
    pub fn new(adapters: Vec<Arc<dyn HarnessAdapter>>) -> Self {
        Self {
            entries: adapters
                .into_iter()
                .map(|adapter| Entry {
                    adapter,
                    probing: tokio::sync::Mutex::new(()),
                    state: Mutex::new(ProbeState::default()),
                })
                .collect(),
            ready: watch::channel(false).0,
            probes: watch::channel(0).0,
        }
    }

    /// Resolves once the initial probe has finished (requests that need harness information
    /// wait for it instead of seeing "not probed yet").
    pub async fn wait_ready(&self) {
        let mut rx = self.ready.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn mark_ready(&self) {
        self.ready.send_replace(true);
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.adapter.id() == id)
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn HarnessAdapter>> {
        self.entry(id).map(|e| e.adapter.clone())
    }

    pub fn ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| e.adapter.id().to_owned())
            .collect()
    }

    /// Probes harness `id` and stores the result — unless a probe that started at or after
    /// `fresh_since` has already finished (`None`: any earlier probe will do), whose result is
    /// returned instead. Probes of one harness never overlap: a caller arriving during a probe
    /// waits for it and, when that probe is fresh enough, uses its result. `None` for an
    /// unknown harness.
    pub async fn probe(&self, id: &str, fresh_since: Option<Instant>) -> Option<Probed> {
        let entry = self.entry(id)?;
        let _probing = entry.probing.lock().await;
        let reusable = {
            let state = entry.state.lock();
            state
                .started
                .is_some_and(|started| fresh_since.is_none_or(|since| started >= since))
        };
        if reusable {
            return Some(Probed {
                harness: self.view(entry),
                changed: false,
            });
        }
        let before = self.view(entry);
        let started = Instant::now();
        entry.state.lock().running_since = Some(started);
        let running = RunningProbe(&entry.state);
        let info = entry.adapter.probe().await;
        // Read right after the probe, which may have changed them; an unavailable harness
        // offers nothing.
        let features = if info.available {
            entry.adapter.features()
        } else {
            HarnessFeatures::default()
        };
        drop(running);
        if !info.available {
            tracing::warn!(harness = %id, reason = ?info.unavailable_reason, "harness unavailable");
        }
        {
            let mut state = entry.state.lock();
            state.failures = if info.available {
                0
            } else {
                state.failures.saturating_add(1)
            };
            state.info = Some(info);
            state.features = features;
            state.started = Some(started);
            state.finished = Some(Instant::now());
        }
        self.probes.send_modify(|n| *n = n.wrapping_add(1));
        let after = self.view(entry);
        Some(Probed {
            changed: after != before,
            harness: after,
        })
    }

    /// Notified after every completed probe (the retry schedule waits on it).
    pub fn probes_completed(&self) -> watch::Receiver<u64> {
        self.probes.subscribe()
    }

    /// When each unavailable harness is due to be probed again by itself: the end of its last
    /// probe plus `policy.harness_retry_delay(failures)`. Harnesses being probed right now and
    /// harnesses not probed yet (the initial probe is running) are not listed.
    pub fn retry_schedule(&self, policy: &Policy) -> Vec<(String, Instant)> {
        self.entries
            .iter()
            .filter_map(|e| {
                let state = e.state.lock();
                let unavailable = state.info.as_ref().is_some_and(|i| !i.available);
                match (unavailable, state.running_since, state.finished) {
                    (true, None, Some(finished)) => Some((
                        e.adapter.id().to_owned(),
                        finished + policy.harness_retry_delay(state.failures),
                    )),
                    _ => None,
                }
            })
            .collect()
    }

    pub fn info(&self, id: &str) -> Option<HarnessInfo> {
        self.entry(id)?.state.lock().info.clone()
    }

    /// The features harness `id` offered at its last probe (none for an unknown harness, one
    /// not probed yet, or one that was unavailable).
    pub fn features(&self, id: &str) -> HarnessFeatures {
        self.entry(id)
            .map(|e| e.state.lock().features.clone())
            .unwrap_or_default()
    }

    /// Protocol view of one harness (unprobed harnesses are reported unavailable).
    pub fn harness(&self, id: &str) -> Option<Harness> {
        self.entry(id).map(|e| self.view(e))
    }

    fn view(&self, entry: &Entry) -> Harness {
        let adapter = &entry.adapter;
        let (info, features) = {
            let state = entry.state.lock();
            (
                state
                    .info
                    .clone()
                    .unwrap_or_else(|| HarnessInfo::unavailable("not probed yet")),
                state.features.clone(),
            )
        };
        Harness {
            id: adapter.id().to_owned(),
            kind: adapter.kind(),
            display_name: adapter.display_name().to_owned(),
            available: info.available,
            unavailable_reason: info.unavailable_reason,
            version: info.version,
            executable: info.executable.map(|p| p.display().to_string()),
            capabilities: info.capabilities,
            models: info.models,
            default_model: info.default_model,
            effort_levels: info.effort_levels,
            permission_modes: info.permission_modes,
            default_permission_mode: info.default_permission_mode,
            features,
        }
    }

    pub fn harnesses(&self) -> Vec<Harness> {
        self.entries.iter().map(|e| self.view(e)).collect()
    }

    /// Consecutive failed probes of each harness (diagnostics and tests).
    pub fn failures(&self) -> HashMap<String, u32> {
        self.entries
            .iter()
            .map(|e| (e.adapter.id().to_owned(), e.state.lock().failures))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use aas_harness::{
        AdapterError, CommandContext, HarnessKind, NativeHistory, NativeSessionSummary,
        SessionHandle, StartRequest,
    };
    use async_trait::async_trait;

    use super::*;

    /// A harness whose availability the test switches; each probe takes `probe_time`.
    struct Switch {
        available: AtomicBool,
        probes: AtomicUsize,
        probe_time: Duration,
    }

    #[async_trait]
    impl HarnessAdapter for Switch {
        fn id(&self) -> &str {
            "switch"
        }
        fn kind(&self) -> HarnessKind {
            HarnessKind::Fake
        }
        fn display_name(&self) -> &str {
            "Switch"
        }
        async fn probe(&self) -> HarnessInfo {
            self.probes.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.probe_time).await;
            if self.available.load(Ordering::SeqCst) {
                HarnessInfo {
                    available: true,
                    unavailable_reason: None,
                    ..HarnessInfo::unavailable("")
                }
            } else {
                HarnessInfo::unavailable("not logged in")
            }
        }
        async fn start(&self, _req: StartRequest) -> Result<SessionHandle, AdapterError> {
            Err(AdapterError::Unsupported("start"))
        }
        async fn commands(
            &self,
            _ctx: CommandContext,
        ) -> Result<Vec<aas_protocol::Command>, AdapterError> {
            Ok(Vec::new())
        }
        async fn list_native_sessions(
            &self,
            _cwd: &Path,
        ) -> Result<Vec<NativeSessionSummary>, AdapterError> {
            Ok(Vec::new())
        }
        async fn read_native_history(
            &self,
            _cwd: &Path,
            _id: &str,
        ) -> Result<NativeHistory, AdapterError> {
            Err(AdapterError::Unsupported("history"))
        }
    }

    fn registry(available: bool) -> (Arc<HarnessRegistry>, Arc<Switch>) {
        let switch = Arc::new(Switch {
            available: AtomicBool::new(available),
            probes: AtomicUsize::new(0),
            probe_time: Duration::from_secs(2),
        });
        let registry = Arc::new(HarnessRegistry::new(vec![switch.clone()]));
        (registry, switch)
    }

    #[tokio::test(start_paused = true)]
    async fn an_unavailable_harness_is_due_again_with_a_doubling_capped_wait() {
        let (registry, switch) = registry(false);
        let policy = Policy {
            harness_retry_initial_delay: Duration::from_secs(30),
            harness_retry_max_delay: Duration::from_secs(100),
            ..Policy::default()
        };
        assert!(
            registry.retry_schedule(&policy).is_empty(),
            "not probed yet"
        );
        let mut waits = Vec::new();
        for _ in 0..4 {
            let probed = registry
                .probe("switch", Some(Instant::now()))
                .await
                .unwrap();
            assert!(!probed.harness.available);
            let (id, due) = registry.retry_schedule(&policy).pop().unwrap();
            assert_eq!(id, "switch");
            waits.push((due - Instant::now()).as_secs());
        }
        assert_eq!(waits, vec![30, 60, 100, 100]);
        // Recovered: nothing is due any more and the streak starts over.
        switch.available.store(true, Ordering::SeqCst);
        let probed = registry
            .probe("switch", Some(Instant::now()))
            .await
            .unwrap();
        assert!(probed.harness.available && probed.changed);
        assert!(registry.retry_schedule(&policy).is_empty());
        assert_eq!(registry.failures()["switch"], 0);
        switch.available.store(false, Ordering::SeqCst);
        registry
            .probe("switch", Some(Instant::now()))
            .await
            .unwrap();
        let (_, due) = registry.retry_schedule(&policy).pop().unwrap();
        assert_eq!((due - Instant::now()).as_secs(), 30);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_share_one_probe_and_fresh_results_are_reused() {
        let (registry, switch) = registry(false);
        let asked = Instant::now();
        let (a, b) = tokio::join!(
            registry.probe("switch", Some(asked)),
            registry.probe("switch", Some(asked))
        );
        assert_eq!(switch.probes.load(Ordering::SeqCst), 1, "one CLI start");
        assert!(
            a.unwrap().changed,
            "the first probe replaced \"not probed yet\""
        );
        assert!(!b.unwrap().changed, "the second caller used its result");
        // Within the reuse window the stored result answers without a probe...
        tokio::time::advance(Duration::from_secs(5)).await;
        let recent = Instant::now().checked_sub(Duration::from_secs(10));
        registry.probe("switch", recent).await.unwrap();
        assert_eq!(switch.probes.load(Ordering::SeqCst), 1);
        // ...and an explicit refresh (fresh since now) probes again.
        switch.available.store(true, Ordering::SeqCst);
        let refreshed = registry
            .probe("switch", Some(Instant::now()))
            .await
            .unwrap();
        assert_eq!(switch.probes.load(Ordering::SeqCst), 2);
        assert!(refreshed.harness.available && refreshed.changed);
        assert!(registry.probe("nope", None).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_in_progress_is_not_scheduled_again_and_a_dropped_probe_is_not_stuck() {
        let (registry, switch) = registry(false);
        let policy = Policy::default();
        registry.probe("switch", None).await.unwrap();
        assert_eq!(registry.retry_schedule(&policy).len(), 1);
        // A probe dropped half-way (a caller that stopped waiting without a task of its own).
        let probing = registry.probe("switch", Some(Instant::now()));
        let _ = tokio::time::timeout(Duration::from_millis(500), probing).await;
        assert_eq!(switch.probes.load(Ordering::SeqCst), 2);
        assert_eq!(
            registry.retry_schedule(&policy).len(),
            1,
            "still scheduled: the dropped probe does not count as running"
        );
        let mut completed = registry.probes_completed();
        let running = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.probe("switch", Some(Instant::now())).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(registry.retry_schedule(&policy).is_empty(), "being probed");
        running.await.unwrap().unwrap();
        assert!(completed.has_changed().unwrap());
        completed.borrow_and_update();
        assert_eq!(registry.retry_schedule(&policy).len(), 1);
    }

    #[test]
    fn the_unavailable_error_names_the_harness_and_is_not_definitive() {
        let info = HarnessInfo::unavailable("not logged in");
        let CoreError::Rpc(e) = unavailable_error("codex", Some(&info)) else {
            panic!("an rpc error")
        };
        assert_eq!(e.kind(), Some(ErrorKind::HarnessUnavailable));
        assert!(!ErrorKind::HarnessUnavailable.is_definitive());
        let data = e.data.unwrap();
        assert_eq!(data["harnessId"], "codex");
        assert_eq!(data["reason"], "not logged in");
    }
}
