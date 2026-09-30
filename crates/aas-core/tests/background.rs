//! Background work (design.md §5.6): the engine keeps an agent's process while the harness
//! reports work that keeps it busy, records the tasks and their ends, scopes interactions to
//! them, answers every request it gives up on, and routes stop requests — driven by a scripted
//! harness whose events each test sends, and end to end with the fake agent's `@bg` scenarios.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy, RequestCtx};
use aas_harness::{
    AdapterContext, AdapterError, AdapterEvent, BackgroundOutcome, BackgroundState,
    BackgroundTaskInfo, CommandContext, ExitInfo, HarnessAdapter, HarnessInfo, NativeHistory,
    NativeSessionSummary, OutputUpdate, SessionControl, SessionHandle, SettingsApplied,
    StartRequest, StopReason, TurnInput,
};
use aas_protocol::events::{Event, EventEnvelope};
use aas_protocol::methods::{spec, *};
use aas_protocol::*;
use aas_supervisor::{Supervisor, SupervisorPolicy};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(30);

// ----- scripted harness -------------------------------------------------------------------------

#[derive(Default)]
struct Script {
    sessions: Mutex<Vec<Arc<Session>>>,
    log: Mutex<Vec<String>>,
    /// The next `send`s refused because the agent started a run by itself (the session emits
    /// that run's `TurnStarted` first, as the contract requires).
    sends_in_progress: AtomicUsize,
    /// `apply_settings` fails (a restart becomes necessary).
    settings_fail: AtomicBool,
    /// `expire_request` does not return until `expire_release` is notified (it keeps the actor
    /// busy for as long as a test needs).
    expire_held: AtomicBool,
    expire_release: tokio::sync::Notify,
    /// `interrupt` is accepted but the turn never ends.
    interrupt_ignored: AtomicBool,
    /// The harness does not offer `backgroundStop`.
    no_background_stop: AtomicBool,
}

impl Script {
    fn log(&self, line: String) {
        self.log.lock().push(line);
    }

    fn logged(&self) -> Vec<String> {
        self.log.lock().clone()
    }

    fn has(&self, line: &str) -> bool {
        self.log.lock().iter().any(|l| l == line)
    }

    fn session(&self, n: usize) -> Arc<Session> {
        self.sessions.lock()[n - 1].clone()
    }

    fn session_count(&self) -> usize {
        self.sessions.lock().len()
    }
}

struct Scripted(Arc<Script>);

fn info(script: &Script) -> HarnessInfo {
    let stop = !script.no_background_stop.load(Ordering::SeqCst);
    HarnessInfo {
        available: true,
        unavailable_reason: None,
        version: Some("1".into()),
        executable: None,
        capabilities: HarnessCapabilities {
            interrupt: true,
            steer: false,
            approvals: true,
            questions: false,
            resume: true,
            fork: false,
            images: false,
            model_switch_live: true,
            native_sessions: false,
            background_tasks: true,
            background_stop: stop,
        },
        models: Vec::new(),
        default_model: None,
        effort_levels: Vec::new(),
        permission_modes: vec![
            PermissionMode {
                id: "ask".into(),
                label: "Ask".into(),
                description: None,
                is_default: true,
            },
            PermissionMode {
                id: "auto".into(),
                label: "Auto".into(),
                description: None,
                is_default: false,
            },
        ],
        default_permission_mode: Some("ask".into()),
    }
}

#[async_trait]
impl HarnessAdapter for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }
    fn kind(&self) -> HarnessKind {
        HarnessKind::Fake
    }
    fn display_name(&self) -> &str {
        "Scripted"
    }
    async fn probe(&self) -> HarnessInfo {
        info(&self.0)
    }
    async fn start(&self, _req: StartRequest) -> Result<SessionHandle, AdapterError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let n = {
            let mut sessions = self.0.sessions.lock();
            let n = sessions.len() + 1;
            sessions.push(Arc::new(Session {
                n,
                script: self.0.clone(),
                events: tx,
                exited: AtomicBool::new(false),
            }));
            n
        };
        self.0.log(format!("start#{n}"));
        Ok(SessionHandle {
            native_session_id: Some(format!("native-{n}")),
            control: self.0.session(n),
            events: rx,
        })
    }
    async fn commands(&self, _ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        Ok(Vec::new())
    }
    async fn list_native_sessions(
        &self,
        _cwd: &Path,
    ) -> Result<Vec<NativeSessionSummary>, AdapterError> {
        Err(AdapterError::Unsupported("nativeSessions"))
    }
    async fn read_native_history(
        &self,
        _cwd: &Path,
        _id: &str,
    ) -> Result<NativeHistory, AdapterError> {
        Err(AdapterError::Unsupported("nativeSessions"))
    }
}

struct Session {
    n: usize,
    script: Arc<Script>,
    events: mpsc::UnboundedSender<AdapterEvent>,
    exited: AtomicBool,
}

impl Session {
    fn emit(&self, event: AdapterEvent) {
        let _ = self.events.send(event);
    }

    fn exit(&self, code: Option<i32>, stopped: Option<StopReason>) -> ExitInfo {
        let info = ExitInfo {
            code,
            stopped,
            stderr_tail: String::new(),
            exited_at_ms: 0,
        };
        if !self.exited.swap(true, Ordering::SeqCst) {
            self.emit(AdapterEvent::Exited { info: info.clone() });
        }
        info
    }

    fn task(&self, task: BackgroundTaskInfo) {
        self.emit(AdapterEvent::BackgroundTask {
            task: Box::new(task),
        });
    }

    fn turn_started(&self) {
        self.emit(AdapterEvent::TurnStarted);
    }

    fn turn_completed(&self, trigger: Option<TurnTrigger>) {
        self.emit(AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: None,
            error: None,
            trigger,
        });
    }

    fn message(&self, key: &str, text: &str) {
        self.emit(AdapterEvent::ItemStarted {
            key: key.into(),
            body: ItemBody::AgentMessage { text: text.into() },
        });
        self.emit(AdapterEvent::ItemCompleted {
            key: key.into(),
            body: None,
            status: ItemStatus::Completed,
        });
    }

    fn ask(&self, request_id: &str, background_key: Option<&str>) {
        self.emit(AdapterEvent::InteractionRequested {
            request_id: request_id.into(),
            request: InteractionRequest::Approval {
                title: "Run?".into(),
                detail: None,
                subject: Subject::Command {
                    command: "make".into(),
                    cwd: None,
                },
                options: vec![
                    ApprovalOption {
                        id: "allow".into(),
                        label: "Allow".into(),
                        kind: ApprovalOptionKind::AllowOnce,
                    },
                    ApprovalOption {
                        id: "deny".into(),
                        label: "Deny".into(),
                        kind: ApprovalOptionKind::Deny,
                    },
                ],
            },
            item_key: None,
            background_key: background_key.map(str::to_owned),
        });
    }
}

#[async_trait]
impl SessionControl for Session {
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        if self.exited.load(Ordering::SeqCst) {
            return Err(AdapterError::Closed);
        }
        let text = input.to_plain_text();
        let refuse = self
            .script
            .sends_in_progress
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if refuse {
            // The agent started a run of its own; its start is reported before the refusal.
            self.emit(AdapterEvent::TurnStarted);
            self.script.log(format!("refused#{} {text}", self.n));
            return Err(AdapterError::TurnInProgress);
        }
        self.script.log(format!("send#{} {text}", self.n));
        Ok(())
    }
    async fn steer(&self, _input: TurnInput) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("steer"))
    }
    async fn interrupt(&self) -> Result<(), AdapterError> {
        self.script.log(format!("interrupt#{}", self.n));
        if !self.script.interrupt_ignored.load(Ordering::SeqCst) {
            self.emit(AdapterEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                usage: None,
                error: None,
                trigger: None,
            });
        }
        Ok(())
    }
    async fn respond(
        &self,
        request_id: &str,
        resolution: &InteractionResolution,
    ) -> Result<(), AdapterError> {
        let what = match resolution {
            InteractionResolution::Approval { option_id, .. } => option_id.clone(),
            InteractionResolution::Question { .. } => "question".into(),
            InteractionResolution::Dismissed => "dismissed".into(),
        };
        self.script
            .log(format!("respond#{} {request_id} {what}", self.n));
        Ok(())
    }
    async fn apply_settings(
        &self,
        _settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError> {
        self.script.log(format!("apply#{}", self.n));
        if self.script.settings_fail.load(Ordering::SeqCst) {
            return Err(AdapterError::Protocol(
                "no answer to set_permission_mode".into(),
            ));
        }
        Ok(SettingsApplied::Live)
    }
    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        self.script
            .log(format!("shutdown#{} {}", self.n, reason.as_str()));
        self.exit(Some(0), Some(reason))
    }
    async fn stop_background(&self, key: &str) -> Result<(), AdapterError> {
        self.script.log(format!("stop#{} {key}", self.n));
        Ok(())
    }
    async fn expire_request(
        &self,
        request_id: &str,
        reason: ExpireReason,
    ) -> Result<(), AdapterError> {
        self.script
            .log(format!("expire#{} {request_id} {reason:?}", self.n));
        if self.script.expire_held.load(Ordering::SeqCst) {
            self.script.expire_release.notified().await;
        }
        Ok(())
    }
}

// ----- environment ------------------------------------------------------------------------------

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    engine: Arc<Engine>,
    supervisor: Supervisor,
    script: Arc<Script>,
    ctx: RequestCtx,
}

fn test_policy() -> Policy {
    Policy {
        stop_grace: Duration::from_millis(300),
        interrupt_grace: Duration::from_millis(600),
        // Leases are counted (the supervisor's power guard is a counter in tests).
        prevent_sleep_while_running: true,
        background_progress_interval: Duration::ZERO,
        ..Policy::default()
    }
}

async fn start(
    data: &Path,
    root: &Path,
    policy: Policy,
    adapter: Arc<dyn HarnessAdapter>,
) -> (Arc<Engine>, Supervisor) {
    let supervisor = Supervisor::new(
        &data.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..policy.supervisor_policy()
        },
    )
    .unwrap();
    let config = EngineConfig {
        data_dir: data.to_path_buf(),
        server_name: "test".into(),
        hostname: "host".into(),
        project_roots: vec![root.to_path_buf()],
        policy,
        heuristics: Default::default(),
        git: None,
    };
    let engine = Engine::start(
        config,
        HarnessRegistry::new(vec![adapter]),
        supervisor.clone(),
    )
    .await
    .unwrap();
    (engine, supervisor)
}

async fn env_with(f: impl FnOnce(&mut Policy)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let mut policy = test_policy();
    f(&mut policy);
    let script = Arc::new(Script::default());
    let (engine, supervisor) =
        start(&data, &root, policy, Arc::new(Scripted(script.clone()))).await;
    Env {
        _dir: dir,
        root,
        data,
        engine,
        supervisor,
        script,
        ctx: RequestCtx {
            device_id: DeviceId::from("dev_test"),
        },
    }
}

async fn env() -> Env {
    env_with(|_| {}).await
}

fn crid() -> String {
    format!("crid-{}", ulid::Ulid::generate())
}

async fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn agent(key: &str) -> BackgroundTaskInfo {
    BackgroundTaskInfo {
        stoppable: true,
        ..BackgroundTaskInfo::new(key, BackgroundTaskKind::Agent, format!("task {key}"))
    }
}

fn ended(task: BackgroundTaskInfo, state: BackgroundState) -> BackgroundTaskInfo {
    BackgroundTaskInfo {
        state,
        live: false,
        result: Some(BackgroundOutcome {
            summary: Some(format!("{} {state:?}", task.key)),
            ..Default::default()
        }),
        ..task
    }
}

impl Env {
    async fn call<M: MethodSpec>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let req = ClientRequest::parse(M::NAME, Some(serde_json::to_value(params).unwrap()))?;
        let value = self.engine.handle(&self.ctx, req).await?;
        Ok(
            serde_json::from_value(value)
                .unwrap_or_else(|e| panic!("{}: bad result: {e}", M::NAME)),
        )
    }

    async fn thread(&self) -> Thread {
        let dir = self.root.join(format!("p{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = self
            .call::<spec::ProjectOpen>(ProjectOpenParams {
                client_request_id: crid(),
                path: dir.display().to_string(),
                name: None,
            })
            .await
            .unwrap()
            .project;
        self.call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id,
            harness_id: "scripted".into(),
            settings: None,
            workspace: None,
            title: None,
            input: None,
        })
        .await
        .unwrap()
        .thread
    }

    async fn send(&self, thread: &ThreadId, text: &str) -> TurnStartResult {
        self.call::<spec::TurnStart>(TurnStartParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
            input: vec![InputPart::Text { text: text.into() }],
            delivery: Delivery::Auto,
        })
        .await
        .unwrap()
    }

    async fn get(&self, thread: &ThreadId) -> Thread {
        self.call::<spec::ThreadGet>(ThreadGetParams {
            thread_id: thread.clone(),
        })
        .await
        .unwrap()
        .thread
    }

    async fn read(&self, thread: &ThreadId) -> ThreadReadResult {
        self.call::<spec::ThreadRead>(ThreadReadParams {
            thread_id: thread.clone(),
            before_turn_index: None,
            limit_turns: Some(100),
        })
        .await
        .unwrap()
    }

    async fn wait_until(
        &self,
        thread: &ThreadId,
        what: &str,
        pred: impl Fn(&ThreadReadResult) -> bool,
    ) -> ThreadReadResult {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let read = self.read(thread).await;
            if pred(&read) {
                return read;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {read:#?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn task(&self, thread: &ThreadId, native: &str) -> BackgroundTask {
        self.read(thread)
            .await
            .background_tasks
            .into_iter()
            .find(|t| t.native_id == native)
            .unwrap_or_else(|| panic!("no task {native}"))
    }

    async fn wait_task(
        &self,
        thread: &ThreadId,
        native: &str,
        what: &str,
        pred: impl Fn(&BackgroundTask) -> bool,
    ) -> BackgroundTask {
        let read = self
            .wait_until(thread, what, |r| {
                r.background_tasks
                    .iter()
                    .any(|t| t.native_id == native && pred(t))
            })
            .await;
        read.background_tasks
            .into_iter()
            .find(|t| t.native_id == native)
            .unwrap()
    }

    async fn wait_status(&self, thread: &ThreadId, status: ThreadStatus) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let t = self.get(thread).await;
            if t.status == status {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "thread stayed {:?}, expected {status:?}",
                t.status
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_logged(&self, line: &str) {
        eventually(line, || self.script.has(line).then_some(())).await;
    }

    /// Starts the thread's process with a turn the test runs (`in_turn` emits what the agent
    /// does while it runs), then completes it.
    async fn turn(&self, thread: &ThreadId, text: &str, in_turn: impl FnOnce(&Session)) -> TurnId {
        let r = self.send(thread, text).await;
        let turn = r.turn_id.unwrap();
        let n = eventually("a process", || {
            let n = self.script.session_count();
            (n > 0).then_some(n)
        })
        .await;
        let session = self.script.session(n);
        self.wait_logged(&format!("send#{n} {text}")).await;
        session.turn_started();
        in_turn(&session);
        session.turn_completed(None);
        self.wait_until(thread, "the turn to complete", |r| {
            r.turns
                .iter()
                .any(|t| t.id == turn && t.status == TurnStatus::Completed)
        })
        .await;
        turn
    }

    /// Every event of `stream` so far.
    async fn events(&self, stream: &str) -> Vec<EventEnvelope> {
        let mut cursor = 0;
        let mut out = Vec::new();
        loop {
            let batch = self
                .engine
                .read_batch(stream.to_owned(), cursor)
                .await
                .unwrap();
            if batch.events.is_empty() {
                return out;
            }
            cursor = batch.last_seq;
            out.extend(batch.events);
        }
    }

    /// Reads `stream` from the start until `pred` matches; returns everything read.
    async fn wait_for(&self, stream: &str, pred: impl Fn(&Event) -> bool) -> Vec<EventEnvelope> {
        let mut cursor = 0;
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let mut rx = self.engine.subscribe_head(stream);
            let batch = self
                .engine
                .read_batch(stream.to_owned(), cursor)
                .await
                .unwrap();
            cursor = batch.last_seq;
            for ev in batch.events {
                let done = pred(&ev.event);
                seen.push(ev);
                if done {
                    return seen;
                }
            }
            if *rx.borrow_and_update() > cursor {
                continue;
            }
            if tokio::time::timeout_at(deadline, rx.changed())
                .await
                .is_err()
            {
                panic!("timed out waiting on {stream}");
            }
        }
    }

    fn leases(&self) -> usize {
        self.supervisor.power().active()
    }

    async fn status(&self) -> ServerStatusResult {
        self.call::<spec::ServerStatus>(Empty {}).await.unwrap()
    }

    async fn stop_task(
        &self,
        thread: &ThreadId,
        task: &BackgroundTaskId,
    ) -> Result<BackgroundTask, RpcError> {
        self.call::<spec::BackgroundTaskStop>(BackgroundTaskStopParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
            task_id: task.clone(),
        })
        .await
        .map(|r| r.task)
    }
}

// ----- keeping the process: D1 ---------------------------------------------------------------

/// A task in the harness's live set keeps the process however long the thread is idle; the
/// idle stop runs once the set is empty, the sleep lease and the daemon's count follow it.
#[tokio::test(flavor = "multi_thread")]
async fn busy_background_work_keeps_the_process_until_it_ends() {
    let env = env_with(|p| p.idle_process_ttl = Duration::from_millis(300)).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "start a dev server", |s| {
        s.task(BackgroundTaskInfo {
            kind: BackgroundTaskKind::Shell,
            ..agent("dev")
        })
    })
    .await;
    env.wait_status(&thread.id, ThreadStatus::Ready).await;
    // Many idle periods pass: the process stays, and so does the lease.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !env.script
            .logged()
            .iter()
            .any(|l| l.starts_with("shutdown"))
    );
    let t = env.get(&thread.id).await;
    assert_eq!(t.status, ThreadStatus::Ready);
    assert_eq!(t.background.running, 1);
    assert_eq!(env.leases(), 1, "background work keeps the PC awake");
    assert_eq!(env.status().await.running_background_tasks, 1);

    // The harness reports the end: from then on the thread is idle, and it is reaped.
    env.script.session(1).task(ended(
        BackgroundTaskInfo {
            kind: BackgroundTaskKind::Shell,
            ..agent("dev")
        },
        BackgroundState::Completed,
    ));
    env.wait_logged("shutdown#1 idle").await;
    env.wait_status(&thread.id, ThreadStatus::Idle).await;
    let t = env.get(&thread.id).await;
    assert_eq!(t.background.running, 0);
    let last = t.background.last_ended.unwrap();
    assert_eq!(
        (last.kind, last.status),
        (BackgroundTaskKind::Shell, BackgroundTaskStatus::Completed)
    );
    assert_eq!(env.leases(), 0);
    assert_eq!(env.status().await.running_background_tasks, 0);
    let task = env.task(&thread.id, "dev").await;
    assert_eq!(task.end_reason, Some(BackgroundEndReason::Harness));
}

/// Work the harness marks as ambient never keeps the process; it ends with the idle stop.
#[tokio::test(flavor = "multi_thread")]
async fn ambient_work_never_keeps_the_process() {
    let env = env_with(|p| p.idle_process_ttl = Duration::from_millis(300)).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "watch", |s| {
        s.task(BackgroundTaskInfo {
            kind: BackgroundTaskKind::Monitor,
            ambient: true,
            ..agent("mon")
        })
    })
    .await;
    assert_eq!(
        env.get(&thread.id).await.background.running,
        0,
        "ambient work is not counted"
    );
    env.wait_logged("shutdown#1 idle").await;
    let task = env
        .wait_task(&thread.id, "mon", "the idle stop", |t| {
            t.status.is_terminal()
        })
        .await;
    assert_eq!(task.status, BackgroundTaskStatus::Stopped);
    assert_eq!(task.end_reason, Some(BackgroundEndReason::IdleStop));
    assert_eq!(env.leases(), 0);
    // Its end is not the end of the thread's work: clients notify from `lastEnded`.
    assert_eq!(
        env.get(&thread.id).await.background,
        ThreadBackground::default(),
        "an ambient task never becomes lastEnded"
    );
}

/// A task that leaves the live set without an end (the harness no longer counts it as busy)
/// stops keeping the process, and the idle wait starts from that moment.
#[tokio::test(flavor = "multi_thread")]
async fn the_live_set_decides_and_the_idle_wait_starts_when_it_empties() {
    let env = env_with(|p| p.idle_process_ttl = Duration::from_millis(600)).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "work", |s| s.task(agent("a"))).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !env.script
            .logged()
            .iter()
            .any(|l| l.starts_with("shutdown"))
    );
    let left = std::time::Instant::now();
    env.script.session(1).task(BackgroundTaskInfo {
        live: false,
        ..agent("a")
    });
    env.wait_logged("shutdown#1 idle").await;
    assert!(
        left.elapsed() >= Duration::from_millis(550),
        "the idle wait starts when the live set empties ({:?})",
        left.elapsed()
    );
    // It had not ended: it ends with the process.
    let task = env
        .wait_task(&thread.id, "a", "its end", |t| t.status.is_terminal())
        .await;
    assert_eq!(
        (task.status, task.end_reason),
        (
            BackgroundTaskStatus::Stopped,
            Some(BackgroundEndReason::IdleStop)
        )
    );
}

/// The agent's events that are queued when the idle deadline passes are applied first: work it
/// reported meanwhile keeps the process.
#[tokio::test(flavor = "multi_thread")]
async fn queued_events_beat_the_idle_deadline() {
    let ttl = Duration::from_millis(200);
    let env = env_with(|p| p.idle_process_ttl = ttl).await;
    let thread = env.thread().await;
    // A request of an ambient task holds the process (nothing else does).
    env.turn(&thread.id, "watch", |s| {
        s.task(BackgroundTaskInfo {
            ambient: true,
            ..agent("mon")
        });
        s.ask("mon-ask", Some("mon"));
    })
    .await;
    env.wait_until(&thread.id, "the request", |r| !r.interactions.is_empty())
        .await;
    // The task's end expires the request: the commit that makes the process idle arms the idle
    // deadline and then answers the request, and that answer keeps the actor busy until the
    // test lets it return.
    env.script.expire_held.store(true, Ordering::SeqCst);
    env.script.session(1).task(BackgroundTaskInfo {
        ambient: true,
        ..ended(agent("mon"), BackgroundState::Completed)
    });
    env.wait_logged("expire#1 mon-ask TaskEnded").await;
    // The agent reports background work while the actor is busy; the deadline, armed before the
    // answer started, has passed when the actor returns.
    env.script.session(1).task(agent("late"));
    tokio::time::sleep(ttl).await;
    env.script.expire_release.notify_one();
    // Had the deadline gone first, the idle stop would have been committed before the task:
    // the thread would never be seen `ready` with it.
    env.wait_task(&thread.id, "late", "the late task", |t| {
        t.status == BackgroundTaskStatus::Running
    })
    .await;
    let read = env.get(&thread.id).await;
    assert_eq!(
        (read.status, read.background.running),
        (ThreadStatus::Ready, 1)
    );
    // And the task keeps the process from then on.
    tokio::time::sleep(ttl * 3).await;
    assert!(
        !env.script
            .logged()
            .iter()
            .any(|l| l.starts_with("shutdown")),
        "{:?}",
        env.script.logged()
    );
}

// ----- tasks, items, runs -------------------------------------------------------------------

/// The item that launched a task closes as backgrounded and names the task; the task names the
/// item and the turn; `thread/read` returns it; the turn keeps the trigger of the agent's own
/// follow-up run.
#[tokio::test(flavor = "multi_thread")]
async fn a_backgrounded_item_and_its_task_name_each_other() {
    let env = env().await;
    let thread = env.thread().await;
    let turn = env
        .turn(&thread.id, "review in the background", |s| {
            s.emit(AdapterEvent::ItemStarted {
                key: "tool1".into(),
                body: ItemBody::ToolCall {
                    category: ToolCategory::Subagent,
                    name: "Agent".into(),
                    title: "Review".into(),
                    server: None,
                    input: None,
                    output: None,
                    output_truncated: false,
                    output_blob_id: None,
                },
            });
            s.task(BackgroundTaskInfo {
                origin_item_key: Some("tool1".into()),
                ..agent("a1")
            });
            s.emit(AdapterEvent::ItemCompleted {
                key: "tool1".into(),
                body: None,
                status: ItemStatus::Backgrounded,
            });
        })
        .await;
    let read = env.read(&thread.id).await;
    let item = read
        .items
        .iter()
        .find(|i| i.status == ItemStatus::Backgrounded)
        .expect("the launching item");
    let task = &read.background_tasks[0];
    assert_eq!(item.background_task_id.as_ref(), Some(&task.id));
    assert_eq!(task.origin_item_id.as_ref(), Some(&item.id));
    assert_eq!(task.turn_id.as_ref(), Some(&turn));
    assert_eq!(
        (task.status, task.runs, task.kind),
        (BackgroundTaskStatus::Running, 1, BackgroundTaskKind::Agent)
    );

    // The task ends; the agent starts a run by itself about it.
    let session = env.script.session(1);
    session.task(ended(agent("a1"), BackgroundState::Completed));
    session.turn_started();
    session.message("m", "the review is done");
    session.turn_completed(Some(TurnTrigger::BackgroundTask));
    let read = env
        .wait_until(&thread.id, "the agent's run", |r| {
            r.turns.len() == 2 && r.turns[1].status == TurnStatus::Completed
        })
        .await;
    assert_eq!(read.turns[1].trigger, Some(TurnTrigger::BackgroundTask));
    assert_eq!(read.turns[0].trigger, None);
    let task = &read.background_tasks[0];
    assert_eq!(
        task.result.as_ref().unwrap().summary.as_deref(),
        Some("a1 Completed")
    );
    assert!(task.ended_at.is_some());
    // Every update reached the thread stream, whole.
    let events = env
        .wait_for(&thread_stream(&thread.id), |e| {
            matches!(e, Event::BackgroundTaskUpdated { task } if task.status == BackgroundTaskStatus::Completed)
        })
        .await;
    assert!(events.iter().any(|e| matches!(&e.event, Event::BackgroundTaskUpdated { task } if task.status == BackgroundTaskStatus::Running)));
}

/// A task that starts again under the same key is a new run: `runs` goes up and the run starts
/// again; a parent task is linked; the whole state twice changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn runs_parents_and_idempotent_reports() {
    let env = env().await;
    let thread = env.thread().await;
    env.turn(&thread.id, "go", |s| {
        s.task(agent("parent"));
        s.task(BackgroundTaskInfo {
            parent_key: Some("parent".into()),
            kind: BackgroundTaskKind::Shell,
            ..agent("child")
        });
    })
    .await;
    let read = env.read(&thread.id).await;
    let parent = read
        .background_tasks
        .iter()
        .find(|t| t.native_id == "parent")
        .unwrap()
        .clone();
    let child = read
        .background_tasks
        .iter()
        .find(|t| t.native_id == "child")
        .unwrap();
    assert_eq!(child.parent_task_id.as_ref(), Some(&parent.id));

    let session = env.script.session(1);
    let before = env.events(&thread_stream(&thread.id)).await.len();
    session.task(agent("parent"));
    session.task(agent("parent"));
    session.task(ended(agent("parent"), BackgroundState::Completed));
    let first_end = env
        .wait_task(&thread.id, "parent", "the first end", |t| {
            t.status == BackgroundTaskStatus::Completed
        })
        .await;
    assert_eq!(first_end.runs, 1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    session.task(BackgroundTaskInfo {
        runs: 2,
        ..agent("parent")
    });
    let second = env
        .wait_task(&thread.id, "parent", "the second run", |t| t.runs == 2)
        .await;
    assert_eq!(second.status, BackgroundTaskStatus::Running);
    assert_eq!(second.id, parent.id, "the same task");
    assert!(second.started_at >= first_end.ended_at.unwrap());
    assert_eq!(second.ended_at, None);
    assert_eq!(second.result, None);
    assert_eq!(env.get(&thread.id).await.background.running, 2);
    let updates = env
        .wait_for(&thread_stream(&thread.id), |e| matches!(e, Event::BackgroundTaskUpdated { task } if task.runs == 2))
        .await
        .into_iter()
        .skip(before)
        .filter(|e| matches!(&e.event, Event::BackgroundTaskUpdated { task } if task.native_id == "parent"))
        .count();
    assert_eq!(updates, 2, "the repeated report changed nothing");
}

/// The thread's `lastEnded` never goes back: when a task that ended starts a new run under the
/// same key, the summary keeps that task's end (clients announce a finished task when
/// `lastEnded` changes, so going back to an older task's end would announce it again); the new
/// run's end is the next `lastEnded`.
#[tokio::test(flavor = "multi_thread")]
async fn the_last_end_does_not_go_back_when_a_task_runs_again() {
    let env = env().await;
    let thread = env.thread().await;
    let shell = || BackgroundTaskInfo {
        kind: BackgroundTaskKind::Shell,
        ..agent("b")
    };
    env.turn(&thread.id, "go", |s| {
        s.task(shell());
        s.task(agent("a"));
    })
    .await;
    let session = env.script.session(1);
    session.task(ended(shell(), BackgroundState::Completed));
    let b = env
        .wait_task(&thread.id, "b", "b's end", |t| t.ended_at.is_some())
        .await;
    // Ends are stamped in milliseconds: A ends strictly later than B.
    tokio::time::sleep(Duration::from_millis(20)).await;
    session.task(ended(agent("a"), BackgroundState::Completed));
    let a = env
        .wait_task(&thread.id, "a", "a's end", |t| t.ended_at.is_some())
        .await;
    assert!(a.ended_at > b.ended_at);
    let summary = env.get(&thread.id).await.background;
    assert_eq!(summary.last_ended.as_ref().unwrap().task_id, a.id);

    // A starts again under the same key (a resumed agent).
    session.task(BackgroundTaskInfo {
        runs: 2,
        ..agent("a")
    });
    env.wait_task(&thread.id, "a", "the second run", |t| t.runs == 2)
        .await;
    let summary = env.get(&thread.id).await.background;
    assert_eq!(summary.running, 1);
    let last = summary.last_ended.unwrap();
    assert_eq!(
        (&last.task_id, Some(last.ended_at)),
        (&a.id, a.ended_at),
        "still A's first end, not B's"
    );
    // No summary the clients were sent went back to B once A had ended.
    let summaries: Vec<Option<BackgroundTaskId>> = env
        .events(&thread_stream(&thread.id))
        .await
        .into_iter()
        .filter_map(|e| match e.event {
            Event::ThreadUpdated { thread } => {
                Some(thread.background.last_ended.map(|l| l.task_id))
            }
            _ => None,
        })
        .collect();
    let first_a = summaries
        .iter()
        .position(|l| l.as_ref() == Some(&a.id))
        .expect("A's end was sent");
    assert!(
        summaries[first_a..]
            .iter()
            .all(|l| l.as_ref() == Some(&a.id)),
        "{summaries:?}"
    );

    // The second run's end is the new last end.
    tokio::time::sleep(Duration::from_millis(20)).await;
    session.task(ended(
        BackgroundTaskInfo {
            runs: 2,
            ..agent("a")
        },
        BackgroundState::Failed,
    ));
    let second = env
        .wait_task(&thread.id, "a", "the second end", |t| {
            t.status == BackgroundTaskStatus::Failed
        })
        .await;
    let last = env.get(&thread.id).await.background.last_ended.unwrap();
    assert_eq!(
        (&last.task_id, Some(last.ended_at), last.status),
        (&a.id, second.ended_at, BackgroundTaskStatus::Failed)
    );
    assert!(second.ended_at > a.ended_at);
}

/// Progress of a running task is written at most once per `background_progress_interval`; the
/// latest state is written when it passes, and an end is written at once with the last
/// progress.
#[tokio::test(flavor = "multi_thread")]
async fn progress_is_coalesced_by_policy() {
    let env = env_with(|p| p.background_progress_interval = Duration::from_millis(400)).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "go", |s| s.task(agent("w"))).await;
    let session = env.script.session(1);
    let progress = |n: u64| BackgroundTaskInfo {
        progress: Some(BackgroundProgress {
            tool_uses: Some(n),
            ..Default::default()
        }),
        ..agent("w")
    };
    for n in 1..=20 {
        session.task(progress(n));
    }
    let task = env
        .wait_task(&thread.id, "w", "the latest progress", |t| {
            t.progress.as_ref().and_then(|p| p.tool_uses) == Some(20)
        })
        .await;
    assert_eq!(task.status, BackgroundTaskStatus::Running);
    let written = env
        .events(&thread_stream(&thread.id))
        .await
        .iter()
        .filter(|e| matches!(e.event, Event::BackgroundTaskUpdated { .. }))
        .count();
    assert!(
        written <= 3,
        "{written} updates were written for 20 reports"
    );
    session.task(progress(21));
    session.task(ended(progress(22), BackgroundState::Failed));
    let task = env
        .wait_task(&thread.id, "w", "the end", |t| {
            t.status == BackgroundTaskStatus::Failed
        })
        .await;
    assert_eq!(task.progress.unwrap().tool_uses, Some(22));
    assert_eq!(
        env.get(&thread.id)
            .await
            .background
            .last_ended
            .unwrap()
            .status,
        BackgroundTaskStatus::Failed
    );
}

/// A long output is kept inline up to `max_inline_output_bytes`; the whole of it is in a blob.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_result_output_goes_to_a_blob() {
    let env = env_with(|p| p.max_inline_output_bytes = 1024).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "build", |s| {
        s.task(BackgroundTaskInfo {
            kind: BackgroundTaskKind::Shell,
            ..agent("b")
        })
    })
    .await;
    let output = "line of output\n".repeat(500);
    env.script.session(1).task(BackgroundTaskInfo {
        result: Some(BackgroundOutcome {
            exit_code: Some(0),
            output: Some(output.clone()),
            summary: None,
            output_omitted_bytes: None,
        }),
        state: BackgroundState::Completed,
        live: false,
        kind: BackgroundTaskKind::Shell,
        ..agent("b")
    });
    let task = env
        .wait_task(&thread.id, "b", "the end", |t| t.status.is_terminal())
        .await;
    let result = task.result.unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.output_truncated);
    assert!(result.output.unwrap().len() <= 1024);
    let blob = result.output_blob_id.unwrap();
    let (path, _) = env.engine.blob(&blob).await.unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), output);
}

// ----- output of running tasks -----------------------------------------------------------------

fn shell(key: &str) -> BackgroundTaskInfo {
    BackgroundTaskInfo {
        kind: BackgroundTaskKind::Shell,
        ..agent(key)
    }
}

fn append(key: &str, text: &str) -> AdapterEvent {
    AdapterEvent::BackgroundOutput {
        key: key.into(),
        output: OutputUpdate::Append(text.into()),
    }
}

/// The output deltas of task `task` in `events`, in order.
fn output_deltas(events: &[EventEnvelope], task: &BackgroundTaskId) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::BackgroundTaskOutputDelta { task_id, text } if task_id == task => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

/// What a running task streams is kept with it up to the inline limit and relayed as deltas;
/// the moment it reaches the limit the task says so (`outputTruncated`) and nothing more is
/// relayed; a replaced output is a whole update; the end's own output supersedes it, and a new
/// run starts without output. Output of an unknown or ended task is ignored.
#[tokio::test(flavor = "multi_thread")]
async fn streamed_output_is_kept_inline_and_relayed() {
    let env = env_with(|p| p.max_inline_output_bytes = 24).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "serve", |s| s.task(shell("dev")))
        .await;
    let session = env.script.session(1);
    session.emit(append("dev", "listening\n"));
    session.emit(append("dev", "GET /\n"));
    let task = env
        .wait_task(&thread.id, "dev", "the output", |t| {
            t.output.as_deref() == Some("listening\nGET /\n")
        })
        .await;
    assert!(!task.output_truncated);
    let stream = thread_stream(&thread.id);
    // (Consecutive deltas of the task arrive merged: `seqFrom`.)
    let events = env
        .wait_for(&stream, |e| matches!(e, Event::BackgroundTaskOutputDelta { text, .. } if text.ends_with("GET /\n")))
        .await;
    assert_eq!(
        output_deltas(&events, &task.id).concat(),
        "listening\nGET /\n"
    );
    // Past the limit: what fits is relayed, then the task says it is cut.
    session.emit(append("dev", "GET /favicon.ico 404\n"));
    session.emit(append("dev", "never shown\n"));
    let task = env
        .wait_task(&thread.id, "dev", "the cut", |t| t.output_truncated)
        .await;
    assert_eq!(task.output.as_deref(), Some("listening\nGET /\nGET /fav"));
    // Only then is the cut known; nothing after it was relayed.
    let events = env.events(&stream).await;
    assert_eq!(
        output_deltas(&events, &task.id).concat(),
        "listening\nGET /\nGET /fav"
    );
    let cut = events
        .iter()
        .rposition(
            |e| matches!(&e.event, Event::BackgroundTaskUpdated { task } if task.output_truncated),
        )
        .unwrap();
    let last_delta = events
        .iter()
        .rposition(|e| matches!(&e.event, Event::BackgroundTaskOutputDelta { .. }))
        .unwrap();
    assert!(last_delta < cut);
    // A snapshot replaces what was streamed (and fits again).
    session.emit(AdapterEvent::BackgroundOutput {
        key: "dev".into(),
        output: OutputUpdate::Replace("restarted\n".into()),
    });
    let task = env
        .wait_task(&thread.id, "dev", "the replacement", |t| {
            t.output.as_deref() == Some("restarted\n")
        })
        .await;
    assert!(!task.output_truncated);
    // Output of a task nobody reported is ignored.
    session.emit(append("nope", "x"));
    // The end brings the whole output: it supersedes the streamed one.
    session.task(BackgroundTaskInfo {
        result: Some(BackgroundOutcome {
            exit_code: Some(0),
            output: Some("the whole output\n".into()),
            ..Default::default()
        }),
        ..ended(shell("dev"), BackgroundState::Completed)
    });
    let task = env
        .wait_task(&thread.id, "dev", "the end", |t| t.status.is_terminal())
        .await;
    assert_eq!((task.output, task.output_truncated), (None, false));
    assert_eq!(
        task.result.unwrap().output.as_deref(),
        Some("the whole output\n")
    );
    // Output after the end is ignored.
    session.emit(append("dev", "late\n"));
    // A new run starts without output; an end without an output of its own keeps the stream.
    let second = BackgroundTaskInfo {
        runs: 2,
        ..shell("dev")
    };
    session.task(second.clone());
    session.emit(append("dev", "again\n"));
    env.wait_task(&thread.id, "dev", "the second run's output", |t| {
        t.runs == 2 && t.output.as_deref() == Some("again\n")
    })
    .await;
    session.task(ended(second, BackgroundState::Completed));
    let task = env
        .wait_task(&thread.id, "dev", "the second end", |t| {
            t.runs == 2 && t.status.is_terminal()
        })
        .await;
    assert_eq!(task.output.as_deref(), Some("again\n"));
    let events = env.events(&stream).await;
    assert!(
        !events.iter().any(|e| matches!(&e.event, Event::BackgroundTaskOutputDelta { text, .. } if text.contains("late") || text.contains('x') && text.len() == 1)),
        "nothing of an ended or unknown task"
    );
}

/// A shell that goes on from a command of the turn starts with what the command printed: its
/// output continues it.
#[tokio::test(flavor = "multi_thread")]
async fn a_shell_from_a_command_continues_its_output() {
    let env = env().await;
    let thread = env.thread().await;
    env.turn(&thread.id, "npm run dev", |s| {
        s.emit(AdapterEvent::ItemStarted {
            key: "cmd".into(),
            body: ItemBody::CommandExecution {
                command: "npm run dev".into(),
                cwd: None,
                output: String::new(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: None,
                duration_ms: None,
            },
        });
        s.emit(AdapterEvent::ItemDelta {
            key: "cmd".into(),
            field: DeltaField::Output,
            text: "> vite\n".into(),
        });
        s.task(BackgroundTaskInfo {
            origin_item_key: Some("cmd".into()),
            ..shell("term")
        });
        s.emit(AdapterEvent::ItemCompleted {
            key: "cmd".into(),
            body: None,
            status: ItemStatus::Backgrounded,
        });
        s.emit(append("term", "ready\n"));
    })
    .await;
    let task = env
        .wait_task(&thread.id, "term", "the output", |t| {
            t.output.as_deref() == Some("> vite\nready\n")
        })
        .await;
    assert!(task.origin_item_id.is_some());
}

// ----- what the harness waits on ----------------------------------------------------------------

/// A question the agent asks outside a turn (one of the thread, like a pi extension's dialog)
/// keeps the process: the harness waits for its answer, which a stopped process could never
/// take. Once it is answered the idle stop runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_request_of_the_thread_keeps_the_process() {
    let env = env_with(|p| p.idle_process_ttl = Duration::from_millis(300)).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "go", |_| {}).await;
    let session = env.script.session(1);
    session.ask("dialog", None);
    let read = env
        .wait_until(&thread.id, "the request", |r| !r.interactions.is_empty())
        .await;
    let pending = read.interactions[0].clone();
    assert_eq!(
        (
            pending.turn_id.as_ref(),
            pending.background_task_id.as_ref()
        ),
        (None, None)
    );
    // Many idle periods pass: the process stays.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !env.script
            .logged()
            .iter()
            .any(|l| l.starts_with("shutdown")),
        "{:?}",
        env.script.logged()
    );
    assert_eq!(env.get(&thread.id).await.status, ThreadStatus::Ready);
    env.call::<spec::InteractionRespond>(InteractionRespondParams {
        client_request_id: crid(),
        interaction_id: pending.id,
        resolution: InteractionResolution::Dismissed,
    })
    .await
    .unwrap();
    env.wait_logged("respond#1 dialog dismissed").await;
    env.wait_logged("shutdown#1 idle").await;
    env.wait_status(&thread.id, ThreadStatus::Idle).await;
}

/// A request of work that does not keep the process by itself (an ambient task) keeps it while
/// it waits: the task's end expires it, and then the idle stop runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_request_of_an_ambient_task_keeps_the_process() {
    let env = env_with(|p| p.idle_process_ttl = Duration::from_millis(300)).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "watch", |s| {
        s.task(BackgroundTaskInfo {
            ambient: true,
            ..agent("mon")
        });
        s.ask("mon-ask", Some("mon"));
    })
    .await;
    env.wait_until(&thread.id, "the request", |r| !r.interactions.is_empty())
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !env.script
            .logged()
            .iter()
            .any(|l| l.starts_with("shutdown"))
    );
    env.script.session(1).task(BackgroundTaskInfo {
        ambient: true,
        ..ended(agent("mon"), BackgroundState::Completed)
    });
    env.wait_logged("expire#1 mon-ask TaskEnded").await;
    env.wait_logged("shutdown#1 idle").await;
}

// ----- ends with the process ------------------------------------------------------------------

/// Tasks that have not ended end with the process: stopped with why we stopped it, or lost when
/// it ended by itself (the thread's last error says so).
#[tokio::test(flavor = "multi_thread")]
async fn tasks_end_with_their_process() {
    let env = env().await;
    // thread/stop
    let stopped = env.thread().await;
    env.turn(&stopped.id, "a", |s| s.task(agent("s1"))).await;
    env.call::<spec::ThreadStop>(ThreadStopParams {
        client_request_id: crid(),
        thread_id: stopped.id.clone(),
    })
    .await
    .unwrap();
    let task = env.task(&stopped.id, "s1").await;
    assert_eq!(
        (task.status, task.end_reason),
        (
            BackgroundTaskStatus::Stopped,
            Some(BackgroundEndReason::ThreadStopped)
        )
    );
    assert_eq!(env.get(&stopped.id).await.last_error, None);

    // The process ends by itself.
    let crashed = env.thread().await;
    env.turn(&crashed.id, "b", |s| {
        s.task(agent("l1"));
        s.task(BackgroundTaskInfo {
            ambient: true,
            ..agent("l2")
        });
    })
    .await;
    env.script.session(2).exit(Some(0), None);
    let task = env
        .wait_task(&crashed.id, "l1", "the loss", |t| t.status.is_terminal())
        .await;
    assert_eq!(
        (task.status, task.end_reason),
        (
            BackgroundTaskStatus::Lost,
            Some(BackgroundEndReason::ProcessExited)
        )
    );
    // Both ended at the same moment; the ambient one is never the summary's last end.
    let ambient = env.task(&crashed.id, "l2").await;
    assert_eq!(
        (ambient.status, ambient.ended_at),
        (BackgroundTaskStatus::Lost, task.ended_at)
    );
    assert_eq!(
        env.get(&crashed.id)
            .await
            .background
            .last_ended
            .map(|e| e.task_id),
        Some(task.id.clone())
    );
    let error = env.get(&crashed.id).await.last_error.unwrap();
    assert_eq!(error.kind, "agentExited");
    assert!(
        error.message.contains("1 background task was lost"),
        "{}",
        error.message
    );
    assert_eq!(env.leases(), 0);

    // The daemon stops.
    let third = env.thread().await;
    env.turn(&third.id, "c", |s| s.task(agent("d1"))).await;
    env.engine.shutdown(false).await;
    let task = env.task(&third.id, "d1").await;
    assert_eq!(
        (task.status, task.end_reason),
        (
            BackgroundTaskStatus::Stopped,
            Some(BackgroundEndReason::DaemonShutdown)
        )
    );
}

/// Tasks a crashed daemon left running are lost when it starts again.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_marks_what_ran_as_lost() {
    let env = env().await;
    let thread = env.thread().await;
    env.turn(&thread.id, "go", |s| s.task(agent("r1"))).await;
    env.engine.shutdown(false).await;
    env.engine.close().await.unwrap();
    // As if the daemon had crashed before the stop was recorded: the task is still running.
    {
        let conn = rusqlite::Connection::open(env.data.join("aas.db")).unwrap();
        conn.execute(
            "UPDATE background_tasks SET status = 'running', ended_at = NULL,
               task = json_remove(json_set(task, '$.status', 'running'), '$.endedAt', '$.endReason')",
            [],
        )
        .unwrap();
    }
    let (engine, _supervisor) = start(
        &env.data,
        &env.root,
        test_policy(),
        Arc::new(Scripted(env.script.clone())),
    )
    .await;
    let read: ThreadReadResult = serde_json::from_value(
        engine
            .handle(
                &env.ctx,
                ClientRequest::parse(
                    "thread/read",
                    Some(serde_json::json!({"threadId": thread.id})),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let task = &read.background_tasks[0];
    assert_eq!(
        (task.status, task.end_reason),
        (
            BackgroundTaskStatus::Lost,
            Some(BackgroundEndReason::DaemonRestarted)
        )
    );
    assert_eq!(read.thread.background.running, 0);
    assert_eq!(
        read.thread.background.last_ended.unwrap().status,
        BackgroundTaskStatus::Lost
    );
}

/// A drain waits for background work that keeps an agent busy, like it waits for turns.
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_waits_for_background_work() {
    let env = env().await;
    let thread = env.thread().await;
    env.turn(&thread.id, "go", |s| s.task(agent("d"))).await;
    let engine = env.engine.clone();
    let drained = tokio::spawn(async move { engine.wait_drained().await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!drained.is_finished(), "the drain waits for the task");
    env.script
        .session(1)
        .task(ended(agent("d"), BackgroundState::Completed));
    tokio::time::timeout(WAIT, drained).await.unwrap().unwrap();
}

// ----- interactions (D4) --------------------------------------------------------------------

/// A background task's request outlives the turn, is answered through the adapter, and expires
/// when the task ends — the agent is answered then too.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_request_belongs_to_its_task() {
    let env = env().await;
    let thread = env.thread().await;
    let turn = env
        .turn(&thread.id, "go", |s| {
            s.task(agent("a"));
            s.ask("r1", Some("a"));
        })
        .await;
    let read = env.read(&thread.id).await;
    let interaction = read
        .interactions
        .iter()
        .find(|i| i.status == InteractionStatus::Pending)
        .expect("still pending after the turn");
    let task = &read.background_tasks[0];
    assert_eq!(interaction.turn_id, None);
    assert_eq!(interaction.background_task_id.as_ref(), Some(&task.id));
    assert_eq!(env.get(&thread.id).await.pending_interactions, 1);
    assert!(!env.script.logged().iter().any(|l| l.starts_with("expire")));

    // A second request of the task is answered by the user.
    let session = env.script.session(1);
    session.ask("r2", Some("a"));
    let second = env
        .wait_until(&thread.id, "the second request", |r| {
            r.interactions.len() == 2
        })
        .await
        .interactions
        .into_iter()
        .find(|i| i.id != interaction.id)
        .unwrap();
    let answered = env
        .call::<spec::InteractionRespond>(InteractionRespondParams {
            client_request_id: crid(),
            interaction_id: second.id.clone(),
            resolution: InteractionResolution::Approval {
                option_id: "allow".into(),
                feedback: None,
            },
        })
        .await
        .unwrap();
    assert_eq!(answered.interaction.status, InteractionStatus::Resolved);
    env.wait_logged("respond#1 r2 allow").await;

    // The task ends: its open request expires, and the agent is answered.
    session.task(ended(agent("a"), BackgroundState::Stopped));
    env.wait_logged("expire#1 r1 TaskEnded").await;
    let read = env.read(&thread.id).await;
    let first = read
        .interactions
        .iter()
        .find(|i| i.id == interaction.id)
        .unwrap();
    assert_eq!(first.status, InteractionStatus::Expired);
    assert_eq!(first.expire_reason, Some(ExpireReason::TaskEnded));
    assert_eq!(read.turns[0].id, turn);
    assert_eq!(env.get(&thread.id).await.pending_interactions, 0);
}

/// A request while no turn runs and no task is named belongs to the thread: recorded (never
/// dropped), answerable, and expired when the process ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_outside_a_turn_belongs_to_the_thread() {
    let env = env().await;
    let thread = env.thread().await;
    let turn = env.turn(&thread.id, "go", |_| {}).await;
    let session = env.script.session(1);
    session.ask("t1", None);
    let read = env
        .wait_until(&thread.id, "the request", |r| !r.interactions.is_empty())
        .await;
    let pending = &read.interactions[0];
    assert_eq!(
        (
            pending.turn_id.as_ref(),
            pending.background_task_id.as_ref()
        ),
        (None, None)
    );
    assert_eq!(pending.status, InteractionStatus::Pending);
    let listed = env
        .call::<spec::InteractionList>(InteractionListParams { status: None })
        .await
        .unwrap();
    assert_eq!(listed.interactions.len(), 1);
    // Paged to the turn it was asked after, it is still returned with it.
    let page = env
        .call::<spec::ThreadRead>(ThreadReadParams {
            thread_id: thread.id.clone(),
            before_turn_index: Some(1),
            limit_turns: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(page.turns[0].id, turn);
    assert_eq!(page.interactions.len(), 1);

    // A second one is answered; the first expires with the process.
    session.ask("t2", None);
    let second = env
        .wait_until(&thread.id, "the second request", |r| {
            r.interactions.len() == 2
        })
        .await
        .interactions[1]
        .clone();
    env.call::<spec::InteractionRespond>(InteractionRespondParams {
        client_request_id: crid(),
        interaction_id: second.id,
        resolution: InteractionResolution::Dismissed,
    })
    .await
    .unwrap();
    env.wait_logged("respond#1 t2 dismissed").await;
    session.exit(Some(0), None);
    let read = env
        .wait_until(&thread.id, "the expiry", |r| {
            r.interactions[0].status == InteractionStatus::Expired
        })
        .await;
    assert_eq!(
        read.interactions[0].expire_reason,
        Some(ExpireReason::ProcessExited)
    );
}

/// A request that expires because its turn ended is answered through the adapter; one the
/// agent withdrew is not.
#[tokio::test(flavor = "multi_thread")]
async fn expired_turn_requests_are_answered_and_withdrawn_ones_are_not() {
    let env = env().await;
    let thread = env.thread().await;
    env.turn(&thread.id, "go", |s| {
        s.ask("q1", None);
        s.ask("q2", None);
        s.emit(AdapterEvent::InteractionWithdrawn {
            request_id: "q2".into(),
        });
    })
    .await;
    env.wait_logged("expire#1 q1 TurnEnded").await;
    let read = env.read(&thread.id).await;
    let mut reasons: Vec<String> = read
        .interactions
        .iter()
        .map(|i| format!("{:?}", i.expire_reason))
        .collect();
    reasons.sort();
    assert_eq!(reasons, vec!["Some(HarnessCancelled)", "Some(TurnEnded)"]);
    assert!(!env.script.has("expire#1 q2 HarnessCancelled"));
    assert!(read.interactions.iter().all(|i| i.turn_id.is_some()));
}

// ----- stopping a task -----------------------------------------------------------------------

/// `backgroundTask/stop` asks the adapter; the task shows the request until the harness reports
/// the end. The errors say why a stop cannot be asked for.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_task_asks_the_harness() {
    let env = env_with(|p| p.background_stop_confirm_timeout = Duration::from_millis(400)).await;
    let thread = env.thread().await;
    env.turn(&thread.id, "go", |s| {
        s.task(agent("a"));
        s.task(BackgroundTaskInfo {
            kind: BackgroundTaskKind::Remote,
            stoppable: false,
            ..agent("cloud")
        });
    })
    .await;
    let task = env.task(&thread.id, "a").await;
    let requested = env.stop_task(&thread.id, &task.id).await.unwrap();
    assert!(requested.stop_requested_at.is_some());
    env.wait_logged("stop#1 a").await;
    // Unconfirmed: the request is taken back, and nothing else happens.
    let unconfirmed = env
        .wait_task(&thread.id, "a", "the unconfirmed stop", |t| {
            t.stop_unconfirmed_at.is_some()
        })
        .await;
    assert_eq!(unconfirmed.stop_requested_at, None);
    assert_eq!(unconfirmed.status, BackgroundTaskStatus::Running);
    assert!(
        !env.script
            .logged()
            .iter()
            .any(|l| l.starts_with("shutdown"))
    );
    // Asked again, and this time the harness stops it.
    let again = env.stop_task(&thread.id, &task.id).await.unwrap();
    assert!(again.stop_requested_at.is_some() && again.stop_unconfirmed_at.is_none());
    env.script
        .session(1)
        .task(ended(agent("a"), BackgroundState::Stopped));
    let stopped = env
        .wait_task(&thread.id, "a", "the stop", |t| {
            t.status == BackgroundTaskStatus::Stopped
        })
        .await;
    assert_eq!(stopped.end_reason, Some(BackgroundEndReason::Harness));
    assert_eq!(stopped.stop_requested_at, None);

    let kind = |e: RpcError| e.kind().unwrap();
    assert_eq!(
        kind(env.stop_task(&thread.id, &task.id).await.unwrap_err()),
        ErrorKind::InvalidState,
        "not running"
    );
    let cloud = env.task(&thread.id, "cloud").await;
    assert_eq!(
        kind(env.stop_task(&thread.id, &cloud.id).await.unwrap_err()),
        ErrorKind::InvalidState,
        "not stoppable"
    );
    assert_eq!(
        kind(
            env.stop_task(&thread.id, &BackgroundTaskId::generate())
                .await
                .unwrap_err()
        ),
        ErrorKind::NotFound
    );
    let other = env.thread().await;
    assert_eq!(
        kind(env.stop_task(&other.id, &cloud.id).await.unwrap_err()),
        ErrorKind::NotFound,
        "another thread's task"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_harness_without_background_stop_refuses_the_stop() {
    let env = env().await;
    env.script.no_background_stop.store(true, Ordering::SeqCst);
    env.engine.refresh_harnesses(None).await.unwrap();
    let thread = env.thread().await;
    env.turn(&thread.id, "go", |s| s.task(agent("a"))).await;
    let task = env.task(&thread.id, "a").await;
    let err = env.stop_task(&thread.id, &task.id).await.unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::CapabilityUnsupported));
    assert_eq!(err.data.as_ref().unwrap()["capability"], "backgroundStop");
    assert!(!env.script.has("stop#1 a"));
}

// ----- the sleep lease (design.md §4.6) ----------------------------------------------------

impl Env {
    /// Waits until the supervisor's power guard counts `n` leases.
    async fn wait_leases(&self, n: usize, what: &str) {
        eventually(&format!("{n} leases ({what})"), || {
            (self.leases() == n).then_some(())
        })
        .await;
    }

    /// Waits until turn `turn` has `status`; returns the turn.
    async fn wait_turn(&self, thread: &ThreadId, turn: &TurnId, status: TurnStatus) -> Turn {
        self.wait_until(thread, "the turn's end", |r| {
            r.turns.iter().any(|t| &t.id == turn && t.status == status)
        })
        .await
        .turns
        .into_iter()
        .find(|t| &t.id == turn)
        .unwrap()
    }
}

/// Every turn holds a lease while it runs and gives it back however it ends: completed, failed
/// by the harness, interrupted, force-stopped after an unhonoured interrupt, failed because the
/// process died, and a turn the agent started by itself.
#[tokio::test(flavor = "multi_thread")]
async fn turns_hold_the_sleep_lease_until_they_end_however_they_end() {
    let env = env().await;
    let thread = env.thread().await;
    assert_eq!(env.leases(), 0);

    // Completed.
    let turn = env.send(&thread.id, "one").await.turn_id.unwrap();
    env.wait_logged("send#1 one").await;
    let session = env.script.session(1);
    session.turn_started();
    env.wait_leases(1, "a running turn").await;
    session.turn_completed(None);
    env.wait_turn(&thread.id, &turn, TurnStatus::Completed)
        .await;
    env.wait_leases(0, "after a completed turn").await;

    // Failed by the harness.
    let turn = env.send(&thread.id, "two").await.turn_id.unwrap();
    env.wait_logged("send#1 two").await;
    session.turn_started();
    env.wait_leases(1, "a running turn").await;
    session.emit(AdapterEvent::TurnCompleted {
        status: TurnStatus::Failed,
        usage: None,
        error: Some(TurnError {
            message: "model error".into(),
            kind: "harnessError".into(),
        }),
        trigger: None,
    });
    env.wait_turn(&thread.id, &turn, TurnStatus::Failed).await;
    env.wait_leases(0, "after a failed turn").await;

    // Interrupted (the harness honours the interrupt).
    let turn = env.send(&thread.id, "three").await.turn_id.unwrap();
    env.wait_logged("send#1 three").await;
    session.turn_started();
    env.wait_leases(1, "a running turn").await;
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    env.wait_turn(&thread.id, &turn, TurnStatus::Interrupted)
        .await;
    env.wait_leases(0, "after an interrupted turn").await;

    // A turn the agent starts by itself.
    session.turn_started();
    env.wait_leases(1, "the agent's own turn").await;
    session.turn_completed(Some(TurnTrigger::BackgroundTask));
    env.wait_until(&thread.id, "the agent's own turn to end", |r| {
        r.turns
            .iter()
            .any(|t| t.trigger.is_some() && t.status == TurnStatus::Completed)
    })
    .await;
    env.wait_leases(0, "after the agent's own turn").await;

    // Interrupt not honoured, nothing in the background: the process is stopped (forced).
    env.script.interrupt_ignored.store(true, Ordering::SeqCst);
    let turn = env.send(&thread.id, "four").await.turn_id.unwrap();
    env.wait_logged("send#1 four").await;
    session.turn_started();
    env.wait_leases(1, "a running turn").await;
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    let forced = env
        .wait_turn(&thread.id, &turn, TurnStatus::Interrupted)
        .await;
    assert_eq!(forced.error.map(|e| e.kind).as_deref(), Some("forced"));
    env.wait_leases(0, "after a forced stop").await;
    env.script.interrupt_ignored.store(false, Ordering::SeqCst);

    // The process dies during a turn (a new process for this one).
    let turn = env.send(&thread.id, "five").await.turn_id.unwrap();
    env.wait_logged("send#2 five").await;
    let second = env.script.session(2);
    second.turn_started();
    env.wait_leases(1, "a running turn").await;
    second.exit(Some(1), None);
    let failed = env.wait_turn(&thread.id, &turn, TurnStatus::Failed).await;
    assert_eq!(failed.error.map(|e| e.kind).as_deref(), Some("agentExited"));
    env.wait_leases(0, "after the process died").await;
}

/// A turn and the thread's busy background work hold separate leases (they are counted): the
/// turn's ends with the turn, the background one with the work, whichever comes first.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_and_background_work_hold_separate_leases() {
    let env = env().await;
    let thread = env.thread().await;
    let turn = env.send(&thread.id, "go").await.turn_id.unwrap();
    env.wait_logged("send#1 go").await;
    let session = env.script.session(1);
    session.turn_started();
    session.task(agent("a"));
    env.wait_task(&thread.id, "a", "the task", |t| {
        t.status == BackgroundTaskStatus::Running
    })
    .await;
    env.wait_leases(2, "a turn and busy background work").await;

    session.turn_completed(None);
    env.wait_turn(&thread.id, &turn, TurnStatus::Completed)
        .await;
    env.wait_leases(1, "background work outlives the turn")
        .await;

    // Ambient work never holds one.
    session.task(BackgroundTaskInfo {
        kind: BackgroundTaskKind::Monitor,
        ambient: true,
        ..agent("mon")
    });
    env.wait_task(&thread.id, "mon", "the ambient task", |t| {
        t.status == BackgroundTaskStatus::Running
    })
    .await;
    assert_eq!(env.leases(), 1, "ambient work does not add a lease");

    session.task(ended(agent("a"), BackgroundState::Completed));
    env.wait_task(&thread.id, "a", "its end", |t| t.status.is_terminal())
        .await;
    env.wait_leases(0, "nothing keeps the agent busy").await;

    // Background work that starts while no turn runs takes the lease again.
    session.task(agent("b"));
    env.wait_leases(1, "new busy work").await;
    session.task(ended(agent("b"), BackgroundState::Stopped));
    env.wait_leases(0, "the work ended").await;
}

/// With `prevent_sleep_while_running = false` neither turns nor background work take a lease.
#[tokio::test(flavor = "multi_thread")]
async fn no_lease_is_taken_when_sleep_prevention_is_off() {
    let env = env_with(|p| p.prevent_sleep_while_running = false).await;
    let thread = env.thread().await;
    let turn = env.send(&thread.id, "go").await.turn_id.unwrap();
    env.wait_logged("send#1 go").await;
    let session = env.script.session(1);
    session.turn_started();
    session.task(agent("a"));
    env.wait_task(&thread.id, "a", "the task", |t| {
        t.status == BackgroundTaskStatus::Running
    })
    .await;
    assert_eq!(env.leases(), 0, "during a turn with busy background work");
    session.turn_completed(None);
    env.wait_turn(&thread.id, &turn, TurnStatus::Completed)
        .await;
    assert_eq!(env.get(&thread.id).await.background.running, 1);
    assert_eq!(env.leases(), 0, "with busy background work only");
}

// ----- turns and restarts ------------------------------------------------------------------

/// Input sent while the agent runs a turn of its own (the adapter says so) waits for that turn
/// and is sent after it: it is never failed.
#[tokio::test(flavor = "multi_thread")]
async fn input_refused_during_the_agents_own_turn_follows_it() {
    let env = env().await;
    let thread = env.thread().await;
    env.turn(&thread.id, "first", |_| {}).await;
    env.script.sends_in_progress.store(1, Ordering::SeqCst);
    let r = env.send(&thread.id, "second").await;
    let user_turn = r.turn_id.unwrap();
    env.wait_logged("refused#1 second").await;
    let session = env.script.session(1);
    // The agent's own run (its TurnStarted came with the refusal) completes.
    session.message("own", "a task finished");
    session.turn_completed(Some(TurnTrigger::BackgroundTask));
    env.wait_logged("send#1 second").await;
    session.turn_started();
    session.turn_completed(None);
    let read = env
        .wait_until(&thread.id, "the user's turn", |r| {
            r.turns
                .iter()
                .any(|t| t.id == user_turn && t.status == TurnStatus::Completed)
        })
        .await;
    let own = read
        .turns
        .iter()
        .find(|t| t.trigger.is_some())
        .expect("the agent's own turn");
    assert_eq!(own.status, TurnStatus::Completed);
    assert!(
        read.turns.iter().all(|t| t.error.is_none()),
        "{:#?}",
        read.turns
    );
}

/// A process that must be replaced for new settings is not replaced while background work keeps
/// it busy: the next turn waits (with a notice) and starts on a new process once the work ended.
#[tokio::test(flavor = "multi_thread")]
async fn a_settings_restart_waits_for_background_work() {
    let env = env().await;
    let thread = env.thread().await;
    env.turn(&thread.id, "first", |s| s.task(agent("a"))).await;
    env.script.settings_fail.store(true, Ordering::SeqCst);
    let failed = env
        .call::<spec::ThreadUpdate>(ThreadUpdateParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            title: None,
            settings: Some(ThreadSettings {
                permission_mode: Some("auto".into()),
                ..Default::default()
            }),
            pinned: None,
            modes: None,
        })
        .await;
    assert!(failed.is_err(), "the process refused the settings");
    let r = env.send(&thread.id, "second").await;
    let turn = r.turn_id.unwrap();
    let read = env
        .wait_until(&thread.id, "the notice", |r| {
            r.items.iter().any(|i| matches!(&i.body, ItemBody::Notice { code: Some(c), .. } if c == "waitingForBackgroundWork"))
        })
        .await;
    assert_eq!(
        read.turns.iter().find(|t| t.id == turn).unwrap().status,
        TurnStatus::Running
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        env.script.session_count(),
        1,
        "the busy process was not replaced"
    );
    assert!(
        !env.script
            .logged()
            .iter()
            .any(|l| l.starts_with("shutdown"))
    );

    env.script.settings_fail.store(false, Ordering::SeqCst);
    env.script
        .session(1)
        .task(ended(agent("a"), BackgroundState::Completed));
    env.wait_logged("send#2 second").await;
    assert!(env.script.has("shutdown#1 idle"));
}

/// An interrupt the agent does not honour stops its process — except while background work
/// keeps it busy: that work is never killed because of time; the turn gets a notice.
#[tokio::test(flavor = "multi_thread")]
async fn an_unhonoured_interrupt_keeps_background_work() {
    let env = env().await;
    let thread = env.thread().await;
    let r = env.send(&thread.id, "long").await;
    let turn = r.turn_id.unwrap();
    env.wait_logged("send#1 long").await;
    let session = env.script.session(1);
    session.turn_started();
    session.task(agent("a"));
    env.script.interrupt_ignored.store(true, Ordering::SeqCst);
    env.wait_until(&thread.id, "the task", |r| !r.background_tasks.is_empty())
        .await;
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    env.wait_until(&thread.id, "the notice", |r| {
        r.items.iter().any(|i| matches!(&i.body, ItemBody::Notice { code: Some(c), .. } if c == "interruptNotHonoured"))
    })
    .await;
    assert!(
        !env.script
            .logged()
            .iter()
            .any(|l| l.starts_with("shutdown"))
    );
    // The interrupt can be asked for again, and the turn still ends normally.
    env.script.interrupt_ignored.store(false, Ordering::SeqCst);
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    let read = env
        .wait_until(&thread.id, "the interrupted turn", |r| {
            r.turns
                .iter()
                .any(|t| t.id == turn && t.status == TurnStatus::Interrupted)
        })
        .await;
    assert_eq!(
        read.background_tasks[0].status,
        BackgroundTaskStatus::Running
    );
    assert_eq!(
        env.script
            .logged()
            .iter()
            .filter(|l| l.starts_with("interrupt"))
            .count(),
        2
    );
}

// ----- end to end with the fake agent ----------------------------------------------------------

async fn fake_env(
    policy: impl FnOnce(&mut Policy),
) -> (tempfile::TempDir, Arc<Engine>, RequestCtx, ThreadId) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(root.join("p")).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let mut p = test_policy();
    policy(&mut p);
    let supervisor = Supervisor::new(
        &data.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..p.supervisor_policy()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: data.join("adapters").join("fake"),
        policy: p.adapter_policy(),
    };
    let registry = HarnessRegistry::new(vec![Arc::new(FakeAdapter::in_process("fake", ctx))]);
    let config = EngineConfig {
        data_dir: data,
        server_name: "test".into(),
        hostname: "host".into(),
        project_roots: vec![root.clone()],
        policy: p,
        heuristics: Default::default(),
        git: None,
    };
    let engine = Engine::start(config, registry, supervisor).await.unwrap();
    let rctx = RequestCtx {
        device_id: DeviceId::from("dev_test"),
    };
    let call = |method: &str, params: serde_json::Value| {
        let (engine, rctx) = (engine.clone(), rctx.clone());
        let method = method.to_owned();
        async move {
            engine
                .handle(&rctx, ClientRequest::parse(&method, Some(params)).unwrap())
                .await
                .unwrap()
        }
    };
    let project: ProjectResult = serde_json::from_value(
        call("project/open", serde_json::json!({"clientRequestId": crid(), "path": root.join("p").display().to_string()})).await,
    )
    .unwrap();
    let thread: ThreadCreateResult = serde_json::from_value(
        call(
            "thread/create",
            serde_json::json!({"clientRequestId": crid(), "projectId": project.project.id, "harnessId": "fake"}),
        )
        .await,
    )
    .unwrap();
    (dir, engine, rctx, thread.thread.id)
}

async fn fake_call<T: serde::de::DeserializeOwned>(
    engine: &Arc<Engine>,
    ctx: &RequestCtx,
    method: &str,
    params: serde_json::Value,
) -> Result<T, RpcError> {
    let value = engine
        .handle(ctx, ClientRequest::parse(method, Some(params)).unwrap())
        .await?;
    Ok(serde_json::from_value(value).unwrap())
}

async fn fake_read_until(
    engine: &Arc<Engine>,
    ctx: &RequestCtx,
    thread: &ThreadId,
    what: &str,
    pred: impl Fn(&ThreadReadResult) -> bool,
) -> ThreadReadResult {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let read: ThreadReadResult = fake_call(
            engine,
            ctx,
            "thread/read",
            serde_json::json!({"threadId": thread, "limitTurns": 100}),
        )
        .await
        .unwrap();
        if pred(&read) {
            return read;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {read:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The fake agent's `@bg` scenarios go through the whole engine: a backgrounded launch, a task
/// with progress, its end with an exit code, and the agent's own turn about it.
#[tokio::test(flavor = "multi_thread")]
async fn fake_background_tasks_end_to_end() {
    let (_dir, engine, ctx, thread) = fake_env(|_| {}).await;
    let started: TurnStartResult = fake_call(
        &engine,
        &ctx,
        "turn/start",
        serde_json::json!({"clientRequestId": crid(), "threadId": thread,
            "input": [{"type": "text", "text": "@bg b1 kind=shell ms=400 progress=2 exit=0 wake npm run build\n@text started"}]}),
    )
    .await
    .unwrap();
    let read = fake_read_until(&engine, &ctx, &thread, "the agent's own turn", |r| {
        r.turns.len() == 2 && r.turns[1].status == TurnStatus::Completed
    })
    .await;
    assert_eq!(read.turns[0].id, started.turn_id.unwrap());
    assert_eq!(read.turns[1].trigger, Some(TurnTrigger::BackgroundTask));
    let item = read
        .items
        .iter()
        .find(|i| i.status == ItemStatus::Backgrounded)
        .unwrap();
    assert!(
        matches!(&item.body, ItemBody::CommandExecution { command, .. } if command == "npm run build")
    );
    let task = &read.background_tasks[0];
    assert_eq!(item.background_task_id.as_ref(), Some(&task.id));
    assert_eq!(
        (task.kind, task.status),
        (BackgroundTaskKind::Shell, BackgroundTaskStatus::Completed)
    );
    assert_eq!(task.result.as_ref().unwrap().exit_code, Some(0));
    assert_eq!(task.progress.as_ref().unwrap().tool_uses, Some(2));
    assert!(read.items.iter().any(|i| matches!(&i.body, ItemBody::AgentMessage { text } if text == "background task b1 completed")));
    let thread_view: ThreadResult = fake_call(
        &engine,
        &ctx,
        "thread/get",
        serde_json::json!({"threadId": thread}),
    )
    .await
    .unwrap();
    assert_eq!(thread_view.thread.background.running, 0);
    assert_eq!(
        thread_view.thread.background.last_ended.unwrap().task_id,
        task.id
    );
    engine.shutdown(false).await;
}

/// A fake shell's output streams while it runs: the command printed the first line before it
/// went on in the background (the task starts with it), the task printed the rest (deltas), and
/// its end brings the whole output.
#[tokio::test(flavor = "multi_thread")]
async fn fake_shell_output_streams_end_to_end() {
    let (_dir, engine, ctx, thread) = fake_env(|_| {}).await;
    let _: TurnStartResult = fake_call(
        &engine,
        &ctx,
        "turn/start",
        serde_json::json!({"clientRequestId": crid(), "threadId": thread,
            "input": [{"type": "text", "text": "@bg b kind=shell ms=400 output=4 early=1 make"}]}),
    )
    .await
    .unwrap();
    let read = fake_read_until(&engine, &ctx, &thread, "the end", |r| {
        r.background_tasks
            .iter()
            .any(|t| t.status == BackgroundTaskStatus::Completed)
    })
    .await;
    let task = &read.background_tasks[0];
    let whole = "b line 1\nb line 2\nb line 3\nb line 4\nran make\n";
    assert_eq!(task.result.as_ref().unwrap().output.as_deref(), Some(whole));
    assert_eq!(task.output, None, "the end's output supersedes the stream");
    let item = read
        .items
        .iter()
        .find(|i| i.status == ItemStatus::Backgrounded)
        .unwrap();
    assert!(
        matches!(&item.body, ItemBody::CommandExecution { output, .. } if output == "b line 1\n")
    );
    let mut events = Vec::new();
    let mut cursor = 0;
    loop {
        let batch = engine
            .read_batch(thread_stream(&thread), cursor)
            .await
            .unwrap();
        if batch.events.is_empty() {
            break;
        }
        cursor = batch.last_seq;
        events.extend(batch.events);
    }
    let first = events
        .iter()
        .find_map(|e| match &e.event {
            Event::BackgroundTaskUpdated { task } => Some(task.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        first.output.as_deref(),
        Some("b line 1\n"),
        "it starts with what the command printed"
    );
    assert_eq!(
        output_deltas(&events, &task.id).concat(),
        "b line 2\nb line 3\nb line 4\n"
    );
    engine.shutdown(false).await;
}

/// A fake wakeup (`@wakeup`, like Claude Code's `ScheduleWakeup`): a scheduled task that is
/// live with the time it comes due, unstoppable, launched by its item; when it comes due the
/// agent runs its prompt in a turn of its own (trigger `scheduled`) and the task completes.
#[tokio::test(flavor = "multi_thread")]
async fn fake_wakeups_fire_as_scheduled_turns() {
    let (_dir, engine, ctx, thread) = fake_env(|_| {}).await;
    let _: TurnStartResult = fake_call(
        &engine,
        &ctx,
        "turn/start",
        serde_json::json!({"clientRequestId": crid(), "threadId": thread,
            "input": [{"type": "text", "text": "@wakeup 300 @text woke-up\n@text scheduled"}]}),
    )
    .await
    .unwrap();
    let read = fake_read_until(&engine, &ctx, &thread, "the wakeup's run", |r| {
        r.turns.len() == 2 && r.turns[1].status == TurnStatus::Completed
    })
    .await;
    assert_eq!(read.turns[1].trigger, Some(TurnTrigger::Scheduled));
    assert!(read.items.iter().any(|i| i.turn_id == read.turns[1].id
        && matches!(&i.body, ItemBody::AgentMessage { text } if text == "woke-up")));
    let task = &read.background_tasks[0];
    assert_eq!(
        (task.kind, task.title.as_str(), task.stoppable, task.status),
        (
            BackgroundTaskKind::Scheduled,
            "@text woke-up",
            false,
            BackgroundTaskStatus::Completed
        )
    );
    assert!(task.native_id.starts_with("wakeup:"));
    let item = read
        .items
        .iter()
        .find(|i| i.status == ItemStatus::Backgrounded)
        .unwrap();
    assert!(matches!(&item.body, ItemBody::ToolCall { name, .. } if name == "ScheduleWakeup"));
    assert_eq!(item.background_task_id.as_ref(), Some(&task.id));
    // While it was pending it said when it comes due.
    let mut cursor = 0;
    let mut pending = None;
    loop {
        let batch = engine
            .read_batch(thread_stream(&thread), cursor)
            .await
            .unwrap();
        if batch.events.is_empty() {
            break;
        }
        cursor = batch.last_seq;
        pending = pending.or(batch.events.iter().find_map(|e| match &e.event {
            Event::BackgroundTaskUpdated { task }
                if task.status == BackgroundTaskStatus::Running =>
            {
                Some(task.clone())
            }
            _ => None,
        }));
    }
    assert!(pending.unwrap().next_run_at.is_some());
    engine.shutdown(false).await;
}

/// A fake dialog (`@dialog`) is a question of the thread, asked right after the turn: it keeps
/// the process until it is answered, and the answer reaches the agent.
#[tokio::test(flavor = "multi_thread")]
async fn a_fake_dialog_keeps_the_process_until_it_is_answered() {
    let (_dir, engine, ctx, thread) =
        fake_env(|p| p.idle_process_ttl = Duration::from_millis(300)).await;
    let _: TurnStartResult = fake_call(
        &engine,
        &ctx,
        "turn/start",
        serde_json::json!({"clientRequestId": crid(), "threadId": thread,
            "input": [{"type": "text", "text": "@dialog Deploy now?\n@text asked"}]}),
    )
    .await
    .unwrap();
    let read = fake_read_until(&engine, &ctx, &thread, "the dialog", |r| {
        r.interactions
            .iter()
            .any(|i| i.status == InteractionStatus::Pending)
    })
    .await;
    let dialog = read.interactions[0].clone();
    assert_eq!(
        (dialog.turn_id.as_ref(), dialog.background_task_id.as_ref()),
        (None, None),
        "asked after the turn: the thread's"
    );
    assert_eq!(read.turns[0].status, TurnStatus::Completed);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t: ThreadResult = fake_call(
        &engine,
        &ctx,
        "thread/get",
        serde_json::json!({"threadId": thread}),
    )
    .await
    .unwrap();
    assert_eq!(
        t.thread.status,
        ThreadStatus::Ready,
        "the agent waits for the answer"
    );
    let _: InteractionRespondResult = fake_call(
        &engine,
        &ctx,
        "interaction/respond",
        serde_json::json!({"clientRequestId": crid(), "interactionId": dialog.id,
            "resolution": {"kind": "question", "answers": [{"questionId": "choice", "choiceIds": ["yes"]}]}}),
    )
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let t: ThreadResult = fake_call(
            &engine,
            &ctx,
            "thread/get",
            serde_json::json!({"threadId": thread}),
        )
        .await
        .unwrap();
        if t.thread.status == ThreadStatus::Idle {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "not reaped");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    engine.shutdown(false).await;
}

/// A fake task that runs until it is stopped is stopped through `backgroundTask/stop`; its
/// approval belongs to it and expires with it; a crash loses what still runs.
#[tokio::test(flavor = "multi_thread")]
async fn fake_tasks_are_stopped_and_lost() {
    let (_dir, engine, ctx, thread) = fake_env(|_| {}).await;
    let _: TurnStartResult = fake_call(
        &engine,
        &ctx,
        "turn/start",
        serde_json::json!({"clientRequestId": crid(), "threadId": thread,
            "input": [{"type": "text", "text": "@bg dev kind=shell ms=0 npm run dev\n@bg ask ms=0 approve review\n@bg w kind=workflow ms=0 progress=1 parent=ask"}]}),
    )
    .await
    .unwrap();
    let read = fake_read_until(&engine, &ctx, &thread, "the tasks and the approval", |r| {
        r.background_tasks.len() == 3
            && r.interactions
                .iter()
                .any(|i| i.status == InteractionStatus::Pending)
            && r.turns[0].status == TurnStatus::Completed
    })
    .await;
    let task = |key: &str| {
        read.background_tasks
            .iter()
            .find(|t| t.native_id == key)
            .unwrap()
            .clone()
    };
    assert_eq!(task("w").parent_task_id, Some(task("ask").id));
    let approval = read
        .interactions
        .iter()
        .find(|i| i.status == InteractionStatus::Pending)
        .unwrap();
    assert_eq!(approval.background_task_id, Some(task("ask").id));
    // Stop the task that asks: the approval expires with it.
    let stopped: BackgroundTaskResult = fake_call(
        &engine,
        &ctx,
        "backgroundTask/stop",
        serde_json::json!({"clientRequestId": crid(), "threadId": thread, "taskId": task("ask").id}),
    )
    .await
    .unwrap();
    assert!(stopped.task.stop_requested_at.is_some());
    let read = fake_read_until(&engine, &ctx, &thread, "the stop", |r| {
        r.background_tasks
            .iter()
            .any(|t| t.native_id == "ask" && t.status == BackgroundTaskStatus::Stopped)
    })
    .await;
    let approval = read
        .interactions
        .iter()
        .find(|i| i.background_task_id.is_some())
        .unwrap();
    assert_eq!(approval.expire_reason, Some(ExpireReason::TaskEnded));
    // The agent crashes: what still runs is lost.
    let _: TurnStartResult = fake_call(
        &engine,
        &ctx,
        "turn/start",
        serde_json::json!({"clientRequestId": crid(), "threadId": thread, "input": [{"type": "text", "text": "@crash 5"}]}),
    )
    .await
    .unwrap();
    let read = fake_read_until(&engine, &ctx, &thread, "the loss", |r| {
        r.background_tasks
            .iter()
            .filter(|t| t.status == BackgroundTaskStatus::Lost)
            .count()
            == 2
    })
    .await;
    assert!(
        read.thread
            .last_error
            .unwrap()
            .message
            .contains("2 background tasks were lost")
    );
    engine.shutdown(false).await;
}
