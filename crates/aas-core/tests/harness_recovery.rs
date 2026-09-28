//! Recovery of harnesses a probe found unavailable (design.md §9.4): the retry schedule, the
//! probe a request triggers before it is refused, and what clients see of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy, RequestCtx};
use aas_harness::{
    AdapterContext, AdapterError, CommandContext, HarnessAdapter, HarnessInfo, HarnessKind,
    NativeHistory, NativeSessionSummary, SessionHandle, StartRequest,
};
use aas_protocol::events::Event;
use aas_protocol::methods::{spec, *};
use aas_protocol::*;
use aas_supervisor::{Supervisor, SupervisorPolicy};
use async_trait::async_trait;

const WAIT: Duration = Duration::from_secs(20);

/// The in-process fake harness behind a switch: while it is off, probes report the harness
/// unavailable ("not logged in"), as a CLI does before the user logs in to it.
struct Switched {
    inner: FakeAdapter,
    on: AtomicBool,
    probes: AtomicUsize,
    /// How long a probe takes (milliseconds).
    probe_ms: AtomicU64,
    /// While set, an available harness no longer lists the effort level `high`, as after an
    /// update of the CLI that dropped it.
    without_high_effort: AtomicBool,
}

#[async_trait]
impl HarnessAdapter for Switched {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn kind(&self) -> HarnessKind {
        self.inner.kind()
    }
    fn display_name(&self) -> &str {
        self.inner.display_name()
    }
    async fn probe(&self) -> HarnessInfo {
        self.probes.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(self.probe_ms.load(Ordering::SeqCst))).await;
        if self.on.load(Ordering::SeqCst) {
            let mut info = self.inner.probe().await;
            if self.without_high_effort.load(Ordering::SeqCst) {
                info.effort_levels.retain(|e| e.id != "high");
            }
            info
        } else {
            HarnessInfo::unavailable("not logged in")
        }
    }
    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError> {
        self.inner.start(req).await
    }
    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        self.inner.commands(ctx).await
    }
    async fn list_native_sessions(
        &self,
        cwd: &Path,
    ) -> Result<Vec<NativeSessionSummary>, AdapterError> {
        self.inner.list_native_sessions(cwd).await
    }
    async fn read_native_history(
        &self,
        cwd: &Path,
        id: &str,
    ) -> Result<NativeHistory, AdapterError> {
        self.inner.read_native_history(cwd, id).await
    }
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    engine: Arc<Engine>,
    switch: Arc<Switched>,
    ctx: RequestCtx,
}

async fn env(on: bool, adjust: impl FnOnce(&mut Policy)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(root.join("p")).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let mut policy = Policy {
        stop_grace: Duration::from_millis(500),
        prevent_sleep_while_running: false,
        // Only what a test turns on retries by itself.
        harness_retry_initial_delay: Duration::from_secs(3600),
        harness_retry_max_delay: Duration::from_secs(3600),
        harness_probe_min_interval: Duration::ZERO,
        ..Policy::default()
    };
    adjust(&mut policy);
    let supervisor = Supervisor::new(
        &data.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..policy.supervisor_policy()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: data.join("adapters").join("fake"),
        policy: policy.adapter_policy(),
    };
    let switch = Arc::new(Switched {
        inner: FakeAdapter::in_process("fake", ctx),
        on: AtomicBool::new(on),
        probes: AtomicUsize::new(0),
        probe_ms: AtomicU64::new(0),
        without_high_effort: AtomicBool::new(false),
    });
    let registry = HarnessRegistry::new(vec![switch.clone()]);
    let config = EngineConfig {
        data_dir: data,
        server_name: "test".into(),
        hostname: "host".into(),
        project_roots: vec![root.clone()],
        policy,
        heuristics: Default::default(),
        git: None,
    };
    let engine = Engine::start(config, registry, supervisor).await.unwrap();
    Env {
        _dir: dir,
        root,
        engine,
        switch,
        ctx: RequestCtx {
            device_id: DeviceId::from("dev_test"),
        },
    }
}

fn crid() -> String {
    format!("crid-{}", ulid::Ulid::generate())
}

impl Env {
    async fn call<M: MethodSpec>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let req = ClientRequest::parse(M::NAME, Some(serde_json::to_value(params).unwrap()))?;
        let value = self.engine.handle(&self.ctx, req).await?;
        Ok(serde_json::from_value(value).unwrap())
    }

    async fn project(&self) -> Project {
        self.call::<spec::ProjectOpen>(ProjectOpenParams {
            client_request_id: crid(),
            path: self.root.join("p").display().to_string(),
            name: None,
        })
        .await
        .unwrap()
        .project
    }

    fn create(&self, project: &Project, crid: &str) -> ThreadCreateParams {
        ThreadCreateParams {
            client_request_id: crid.into(),
            project_id: project.id.clone(),
            harness_id: "fake".into(),
            settings: None,
            workspace: None,
            title: None,
            input: None,
        }
    }

    fn probes(&self) -> usize {
        self.switch.probes.load(Ordering::SeqCst)
    }

    async fn harness(&self) -> Harness {
        self.call::<spec::HarnessList>(Empty {})
            .await
            .unwrap()
            .harnesses
            .remove(0)
    }

    /// Waits for a `harness/updated` on the workspace stream after `after` matching `pred`.
    async fn wait_harness_event(&self, after: u64, pred: impl Fn(&Harness) -> bool) -> u64 {
        let deadline = tokio::time::Instant::now() + WAIT;
        let mut cursor = after;
        loop {
            let mut rx = self.engine.subscribe_head(WORKSPACE_STREAM);
            let batch = self
                .engine
                .read_batch(WORKSPACE_STREAM.to_owned(), cursor)
                .await
                .unwrap();
            cursor = batch.last_seq;
            for ev in batch.events {
                if let Event::HarnessUpdated { harness } = &ev.event
                    && pred(harness)
                {
                    return ev.seq;
                }
            }
            if *rx.borrow_and_update() > cursor {
                continue;
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .expect("the harness event arrives")
                .unwrap();
        }
    }

    async fn head(&self) -> u64 {
        self.engine
            .stream_head(WORKSPACE_STREAM)
            .await
            .unwrap()
            .unwrap()
    }
}

fn assert_unavailable(err: &RpcError) {
    assert_eq!(err.kind(), Some(ErrorKind::HarnessUnavailable), "{err:?}");
    let data = err.data.as_ref().unwrap();
    assert_eq!(data["harnessId"], "fake");
    assert_eq!(data["reason"], "not logged in");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_harness_unavailable_at_startup_recovers_by_itself_on_the_retry_schedule() {
    let env = env(false, |p| {
        p.harness_retry_initial_delay = Duration::from_secs(1);
        p.harness_retry_max_delay = Duration::from_secs(2);
    })
    .await;
    assert!(!env.harness().await.available);
    let seen = env.head().await;
    // Unavailable probes on the schedule publish nothing new while nothing changes.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(env.probes() >= 2, "probed again: {}", env.probes());
    // The user logs in to the CLI; no request is made.
    env.switch.on.store(true, Ordering::SeqCst);
    env.wait_harness_event(seen, |h| h.available).await;
    assert!(env.harness().await.available);
    // Once available it is not probed on a schedule any more.
    let probes = env.probes();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(env.probes(), probes);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_probes_an_unavailable_harness_again_before_it_is_refused() {
    let env = env(false, |_| {}).await;
    let project = env.project().await;
    let probes = env.probes();
    let id = crid();
    // Refused after a fresh probe, with what the client needs to show it.
    let err = env
        .call::<spec::ThreadCreate>(env.create(&project, &id))
        .await
        .unwrap_err();
    assert_unavailable(&err);
    assert_eq!(env.probes(), probes + 1);
    // Not definitive, so not stored: the same request is tried (and probed) again.
    let err = env
        .call::<spec::ThreadCreate>(env.create(&project, &id))
        .await
        .unwrap_err();
    assert_unavailable(&err);
    assert_eq!(env.probes(), probes + 2);
    // Logged in since: the resent request succeeds, and clients hear about the harness.
    let seen = env.head().await;
    env.switch.on.store(true, Ordering::SeqCst);
    let thread = env
        .call::<spec::ThreadCreate>(env.create(&project, &id))
        .await
        .unwrap()
        .thread;
    env.wait_harness_event(seen, |h| h.available).await;

    // A thread whose harness becomes unavailable: `turn/start` probes before refusing too.
    env.switch.on.store(false, Ordering::SeqCst);
    let refreshed = env
        .call::<spec::HarnessRefresh>(HarnessRefreshParams { harness_id: None })
        .await
        .unwrap();
    assert!(!refreshed.harnesses[0].available);
    let start = TurnStartParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
        input: vec![InputPart::Text {
            text: "hello".into(),
        }],
        delivery: Delivery::Auto,
    };
    let probes = env.probes();
    let err = env
        .call::<spec::TurnStart>(start.clone())
        .await
        .unwrap_err();
    assert_unavailable(&err);
    assert_eq!(env.probes(), probes + 1);
    env.switch.on.store(true, Ordering::SeqCst);
    let started = env.call::<spec::TurnStart>(start).await.unwrap();
    assert!(started.turn_id.is_some(), "{started:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn recent_probes_are_reused_and_a_slow_probe_does_not_hold_the_request() {
    let env = env(false, |p| {
        p.harness_probe_min_interval = Duration::from_secs(3600);
        p.handshake_timeout = Duration::from_millis(300);
    })
    .await;
    let project = env.project().await;
    let probes = env.probes();
    // The startup probe is recent enough: no new probe.
    let err = env
        .call::<spec::ThreadCreate>(env.create(&project, &crid()))
        .await
        .unwrap_err();
    assert_unavailable(&err);
    assert_eq!(env.probes(), probes);

    // Without the reuse window, a slow probe: the request waits for it only as long as
    // `handshake_timeout`, and the probe finishes (and is published) by itself.
    let env = env_slow().await;
    let project = env.project().await;
    let seen = env.head().await;
    env.switch.probe_ms.store(2000, Ordering::SeqCst);
    env.switch.on.store(true, Ordering::SeqCst);
    let asked = std::time::Instant::now();
    let err = env
        .call::<spec::ThreadCreate>(env.create(&project, &crid()))
        .await
        .unwrap_err();
    assert!(
        asked.elapsed() < Duration::from_millis(1500),
        "{:?}",
        asked.elapsed()
    );
    assert_eq!(err.kind(), Some(ErrorKind::HarnessUnavailable));
    env.wait_harness_event(seen, |h| h.available).await;
    env.call::<spec::ThreadCreate>(env.create(&project, &crid()))
        .await
        .unwrap();
}

async fn env_slow() -> Env {
    env(false, |p| p.handshake_timeout = Duration::from_millis(300)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_about_capabilities_of_an_unavailable_harness_are_not_refused_for_good() {
    let env = env(false, |_| {}).await;
    let project = env.project().await;
    // Without a probe result the capabilities are unknown: `harnessUnavailable` (resent
    // later), not the definitive `capabilityUnsupported`.
    let err = env
        .call::<spec::NativeList>(NativeListParams {
            project_id: project.id.clone(),
            harness_id: "fake".into(),
        })
        .await
        .unwrap_err();
    assert_unavailable(&err);
    let err = env
        .call::<spec::NativeImport>(NativeImportParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "fake".into(),
            native_session_id: "s".into(),
        })
        .await
        .unwrap_err();
    assert_unavailable(&err);
    let err = env
        .call::<spec::HarnessRefresh>(HarnessRefreshParams {
            harness_id: Some("nope".into()),
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::NotFound));
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_changes_are_checked_only_for_what_they_set_against_an_available_harness() {
    let env = env(true, |_| {}).await;
    let project = env.project().await;
    let mut create = env.create(&project, &crid());
    create.settings = Some(ThreadSettings {
        effort: Some("high".into()),
        ..ThreadSettings::default()
    });
    let thread = env.call::<spec::ThreadCreate>(create).await.unwrap().thread;
    let update = |settings: ThreadSettings| ThreadUpdateParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
        title: None,
        settings: Some(settings),
        pinned: None,
    };
    let mode = |id: &str| ThreadSettings {
        permission_mode: Some(id.into()),
        ..ThreadSettings::default()
    };
    let effort = |id: &str| ThreadSettings {
        effort: Some(id.into()),
        ..ThreadSettings::default()
    };

    // Logged out: the harness lists nothing, which says nothing about the values. Changing
    // the permission mode of the thread (whose effort is set) is not refused for good.
    env.switch.on.store(false, Ordering::SeqCst);
    let refreshed = env
        .call::<spec::HarnessRefresh>(HarnessRefreshParams { harness_id: None })
        .await
        .unwrap();
    assert!(!refreshed.harnesses[0].available);
    let updated = env
        .call::<spec::ThreadUpdate>(update(mode("auto")))
        .await
        .unwrap();
    assert_eq!(
        updated.thread.settings.permission_mode.as_deref(),
        Some("auto")
    );
    assert_eq!(updated.thread.settings.effort.as_deref(), Some("high"));
    assert_eq!(
        updated.settings_outcome,
        Some(SettingsOutcome::AppliesNextTurn)
    );

    // Available again, but the CLI no longer lists the thread's effort level: another value
    // can still be changed, while setting an unlisted value is refused.
    env.switch.without_high_effort.store(true, Ordering::SeqCst);
    env.switch.on.store(true, Ordering::SeqCst);
    let refreshed = env
        .call::<spec::HarnessRefresh>(HarnessRefreshParams { harness_id: None })
        .await
        .unwrap();
    assert!(refreshed.harnesses[0].available);
    let updated = env
        .call::<spec::ThreadUpdate>(update(mode("ask")))
        .await
        .unwrap();
    assert_eq!(
        updated.thread.settings.permission_mode.as_deref(),
        Some("ask")
    );
    assert_eq!(updated.thread.settings.effort.as_deref(), Some("high"));
    let err = env
        .call::<spec::ThreadUpdate>(update(effort("high")))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::InvalidParams), "{err:?}");
    let err = env
        .call::<spec::ThreadUpdate>(update(mode("bogus")))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::InvalidParams), "{err:?}");
    let updated = env
        .call::<spec::ThreadUpdate>(update(effort("low")))
        .await
        .unwrap();
    assert_eq!(updated.thread.settings.effort.as_deref(), Some("low"));
    assert_eq!(
        updated.thread.settings.permission_mode.as_deref(),
        Some("ask")
    );
}
