//! Thread actor behaviour around process starts, stops and adapter events, with a scripted
//! harness whose starts, events and exits are driven by each test.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aas_core::{Engine, EngineConfig, HarnessRegistry, Policy, RequestCtx};
use aas_harness::{
    AdapterError, AdapterEvent, CommandContext, ExitInfo, HarnessAdapter, HarnessInfo,
    NativeHistory, NativeSessionSummary, SessionControl, SessionHandle, SettingsApplied, StartMode,
    StartRequest, StopReason, TurnInput,
};
use aas_protocol::events::{Event, EventEnvelope};
use aas_protocol::methods::{spec, *};
use aas_protocol::*;
use aas_supervisor::{Supervisor, SupervisorPolicy};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

const WAIT: Duration = Duration::from_secs(20);

// ----- scripted harness ---------------------------------------------------------------------

/// Shared state of the scripted harness: gates, sessions and an ordered call log.
#[derive(Default)]
struct Script {
    /// `start` waits while this is `false`.
    start_open: Gate,
    /// Number of `start` calls that have begun (also those waiting at the gate).
    starts_begun: std::sync::atomic::AtomicUsize,
    /// `shutdown` waits while this is `false` before the process "exits".
    exit_open: Gate,
    /// `apply_settings` answers `RequiresRestart` instead of `Live`.
    settings_need_restart: AtomicBool,
    /// `apply_settings` fails (the CLI did not answer its control request).
    settings_fail: AtomicBool,
    /// `interrupt` never returns (a CLI whose event loop is stuck).
    interrupt_hangs: AtomicBool,
    /// `probe` reports the harness as unavailable (e.g. while its CLI is being updated).
    unavailable: AtomicBool,
    /// What `list_native_sessions` returns (`None`: one session, `imported`).
    native_sessions: Mutex<Option<Vec<NativeSessionSummary>>>,
    /// What `commands` returns.
    commands: Mutex<Vec<Command>>,
    sessions: Mutex<Vec<Arc<ScriptedSession>>>,
    log: Mutex<Vec<String>>,
}

struct Gate(watch::Sender<bool>);

impl Default for Gate {
    fn default() -> Self {
        Gate(watch::channel(true).0)
    }
}

impl Gate {
    fn set(&self, open: bool) {
        self.0.send_replace(open);
    }

    async fn pass(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|open| *open).await;
    }
}

impl Script {
    fn log(&self, line: String) {
        self.log.lock().push(line);
    }

    fn logged(&self) -> Vec<String> {
        self.log.lock().clone()
    }

    fn session(&self, n: usize) -> Arc<ScriptedSession> {
        self.sessions.lock()[n - 1].clone()
    }

    fn session_count(&self) -> usize {
        self.sessions.lock().len()
    }
}

struct ScriptedAdapter(Arc<Script>);

fn info() -> HarnessInfo {
    HarnessInfo {
        available: true,
        unavailable_reason: None,
        version: Some("1".into()),
        executable: None,
        capabilities: HarnessCapabilities {
            background_tasks: false,
            background_stop: false,
            interrupt: true,
            steer: false,
            approvals: true,
            questions: false,
            resume: true,
            fork: true,
            images: false,
            model_switch_live: true,
            native_sessions: true,
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
impl HarnessAdapter for ScriptedAdapter {
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
        if self.0.unavailable.load(Ordering::SeqCst) {
            HarnessInfo::unavailable("the CLI is being updated")
        } else {
            info()
        }
    }
    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError> {
        self.0.starts_begun.fetch_add(1, Ordering::SeqCst);
        self.0.start_open.pass().await;
        let (tx, rx) = mpsc::unbounded_channel();
        let n = {
            let mut sessions = self.0.sessions.lock();
            let n = sessions.len() + 1;
            let session = Arc::new(ScriptedSession {
                n,
                script: self.0.clone(),
                events: tx,
                exited: AtomicBool::new(false),
                sent: Mutex::new(Vec::new()),
            });
            sessions.push(session);
            n
        };
        let native = match &req.mode {
            StartMode::Resume { native_session_id } => native_session_id.clone(),
            StartMode::New | StartMode::Fork { .. } => format!("native-{n}"),
        };
        self.0.log(format!(
            "start#{n} mode={}",
            req.settings.permission_mode.as_deref().unwrap_or("-")
        ));
        let control = self.0.session(n);
        Ok(SessionHandle {
            native_session_id: Some(native),
            control,
            events: rx,
        })
    }
    async fn commands(&self, _ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        Ok(self.0.commands.lock().clone())
    }
    fn session_switching_commands(&self) -> &'static [&'static str] {
        &["new-session"]
    }
    async fn list_native_sessions(
        &self,
        _cwd: &Path,
    ) -> Result<Vec<NativeSessionSummary>, AdapterError> {
        if let Some(sessions) = self.0.native_sessions.lock().clone() {
            return Ok(sessions);
        }
        Ok(vec![NativeSessionSummary {
            native_session_id: "imported".into(),
            title: Some("Old".into()),
            updated_at: None,
            cwd: None,
        }])
    }
    async fn read_native_history(
        &self,
        _cwd: &Path,
        _id: &str,
    ) -> Result<NativeHistory, AdapterError> {
        // Reading takes a moment (files on disk): concurrent imports overlap here.
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(NativeHistory {
            title: Some("Old".into()),
            turns: Vec::new(),
        })
    }
}

struct ScriptedSession {
    n: usize,
    script: Arc<Script>,
    events: mpsc::UnboundedSender<AdapterEvent>,
    exited: AtomicBool,
    sent: Mutex<Vec<String>>,
}

impl ScriptedSession {
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

    fn sent(&self) -> Vec<String> {
        self.sent.lock().clone()
    }

    /// Runs a complete turn of the agent (as the CLI would after `send`).
    fn complete_turn(&self, text: &str) {
        self.emit(AdapterEvent::TurnStarted);
        self.emit(AdapterEvent::ItemStarted {
            key: format!("m{}", text.len()),
            body: ItemBody::AgentMessage { text: text.into() },
        });
        self.emit(AdapterEvent::ItemCompleted {
            key: format!("m{}", text.len()),
            body: None,
            status: ItemStatus::Completed,
        });
        self.emit(AdapterEvent::TurnCompleted {
            trigger: None,
            status: TurnStatus::Completed,
            usage: None,
            error: None,
        });
    }
}

#[async_trait]
impl SessionControl for ScriptedSession {
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        if self.exited.load(Ordering::SeqCst) {
            return Err(AdapterError::Closed);
        }
        let text = input.to_plain_text();
        self.script.log(format!("send#{} {text}", self.n));
        self.sent.lock().push(text);
        Ok(())
    }
    async fn steer(&self, _input: TurnInput) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("steer"))
    }
    async fn interrupt(&self) -> Result<(), AdapterError> {
        self.script.log(format!("interrupt#{}", self.n));
        if self.script.interrupt_hangs.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.emit(AdapterEvent::TurnCompleted {
            trigger: None,
            status: TurnStatus::Interrupted,
            usage: None,
            error: None,
        });
        Ok(())
    }
    async fn respond(
        &self,
        _request_id: &str,
        _resolution: &InteractionResolution,
    ) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn apply_settings(
        &self,
        settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError> {
        self.script.log(format!(
            "apply#{} mode={}",
            self.n,
            settings.permission_mode.as_deref().unwrap_or("-")
        ));
        if self.script.settings_fail.load(Ordering::SeqCst) {
            return Err(AdapterError::Protocol(
                "no response to control request set_permission_mode".into(),
            ));
        }
        Ok(
            if self.script.settings_need_restart.load(Ordering::SeqCst) {
                SettingsApplied::RequiresRestart
            } else {
                SettingsApplied::Live
            },
        )
    }
    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        self.script
            .log(format!("shutdown#{} {}", self.n, reason.as_str()));
        self.script.exit_open.pass().await;
        self.exit(Some(0), Some(reason))
    }
}

// ----- environment --------------------------------------------------------------------------

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    engine: Arc<Engine>,
    script: Arc<Script>,
    ctx: RequestCtx,
}

async fn env_with(git: bool, f: impl FnOnce(&mut Policy)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let mut policy = Policy {
        stop_grace: Duration::from_millis(300),
        interrupt_grace: Duration::from_millis(1500),
        prevent_sleep_while_running: false,
        ..Policy::default()
    };
    f(&mut policy);
    let supervisor = Supervisor::new(
        &data.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..policy.supervisor_policy()
        },
    )
    .unwrap();
    let script = Arc::new(Script::default());
    let registry = HarnessRegistry::new(vec![Arc::new(ScriptedAdapter(script.clone()))]);
    let config = EngineConfig {
        data_dir: data,
        server_name: "test".into(),
        hostname: "host".into(),
        project_roots: vec![root.clone()],
        policy,
        heuristics: Default::default(),
        git: if git {
            aas_supervisor::resolve_program("git").ok()
        } else {
            None
        },
    };
    let engine = Engine::start(config, registry, supervisor).await.unwrap();
    Env {
        _dir: dir,
        root,
        engine,
        script,
        ctx: RequestCtx {
            device_id: DeviceId::from("dev_test"),
        },
    }
}

async fn env() -> Env {
    env_with(false, |_| {}).await
}

fn git_available() -> bool {
    aas_supervisor::resolve_program("git").is_ok()
}

fn crid() -> String {
    format!("crid-{}", ulid::Ulid::generate())
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
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

impl Env {
    /// Waits until the adapter was asked to start a process (the start then waits at the gate).
    async fn wait_start_begun(&self, n: usize) {
        eventually("the adapter start", || {
            (self.script.starts_begun.load(Ordering::SeqCst) >= n).then_some(())
        })
        .await;
    }

    async fn call<M: MethodSpec>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let req = ClientRequest::parse(M::NAME, Some(serde_json::to_value(params).unwrap()))?;
        let value = self.engine.handle(&self.ctx, req).await?;
        Ok(
            serde_json::from_value(value)
                .unwrap_or_else(|e| panic!("{}: bad result: {e}", M::NAME)),
        )
    }

    async fn project(&self, name: &str, git: bool) -> Project {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if git {
            run_git(&dir, &["init", "-q"]);
            run_git(&dir, &["config", "user.email", "t@example.com"]);
            run_git(&dir, &["config", "user.name", "t"]);
            run_git(&dir, &["config", "core.autocrlf", "false"]);
            std::fs::write(dir.join("README.md"), "hello\n").unwrap();
            run_git(&dir, &["add", "-A"]);
            run_git(&dir, &["commit", "-q", "-m", "init"]);
        }
        self.call::<spec::ProjectOpen>(ProjectOpenParams {
            client_request_id: crid(),
            path: dir.display().to_string(),
            name: None,
        })
        .await
        .unwrap()
        .project
    }

    async fn thread(&self, project: &Project, workspace: Option<WorkspaceSpec>) -> Thread {
        self.call::<spec::ThreadCreate>(ThreadCreateParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "scripted".into(),
            settings: None,
            workspace,
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

    async fn read(&self, thread: &ThreadId) -> ThreadReadResult {
        self.call::<spec::ThreadRead>(ThreadReadParams {
            thread_id: thread.clone(),
            before_turn_index: None,
            limit_turns: Some(100),
        })
        .await
        .unwrap()
    }

    async fn turn(&self, thread: &ThreadId, turn: &TurnId) -> Turn {
        self.read(thread)
            .await
            .turns
            .into_iter()
            .find(|t| &t.id == turn)
            .expect("turn exists")
    }

    async fn wait_turn_status(&self, thread: &ThreadId, turn: &TurnId, status: TurnStatus) -> Turn {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let t = self.turn(thread, turn).await;
            if t.status == status {
                return t;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "turn stayed {:?}, expected {status:?}",
                t.status
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Reads `stream` until `pred` matches; returns everything read.
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

    async fn update(
        &self,
        thread: &ThreadId,
        title: Option<&str>,
        mode: Option<&str>,
        pinned: Option<bool>,
    ) -> Result<ThreadUpdateResult, RpcError> {
        self.call::<spec::ThreadUpdate>(ThreadUpdateParams {
            client_request_id: crid(),
            thread_id: thread.clone(),
            title: title.map(str::to_owned),
            settings: mode.map(|m| ThreadSettings {
                permission_mode: Some(m.into()),
                ..ThreadSettings::default()
            }),
            pinned,
            modes: None,
        })
        .await
    }

    /// Starts the thread's process with a first turn and completes it.
    async fn warm_up(&self, thread: &ThreadId) -> Arc<ScriptedSession> {
        let r = self.send(thread, "warm up").await;
        let session = eventually("the first process", || {
            (self.script.session_count() > 0).then(|| self.script.session(1))
        })
        .await;
        eventually("the first input", || {
            session.sent().contains(&"warm up".to_owned()).then_some(())
        })
        .await;
        session.complete_turn("ready");
        self.wait_turn_status(thread, &r.turn_id.unwrap(), TurnStatus::Completed)
            .await;
        self.wait_status(thread, ThreadStatus::Ready).await;
        session
    }
}

// ----- tests --------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stopping_during_a_start_stops_the_started_process_and_keeps_its_slot_until_then() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    env.script.start_open.set(false);
    let r = env.send(&thread.id, "hello").await;
    env.wait_status(&thread.id, ThreadStatus::Starting).await;
    env.wait_start_begun(1).await;
    let stop = {
        let engine = env.engine.clone();
        let ctx = env.ctx.clone();
        let params = ThreadStopParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
        };
        tokio::spawn(async move {
            let req =
                ClientRequest::parse("thread/stop", Some(serde_json::to_value(params).unwrap()))
                    .unwrap();
            engine.handle(&ctx, req).await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !stop.is_finished(),
        "the stop waits for the process being started"
    );
    assert_eq!(
        env.engine.running_processes(),
        1,
        "the start keeps its capacity slot"
    );
    env.script.start_open.set(true);
    let stopped = tokio::time::timeout(WAIT, stop)
        .await
        .expect("the stop completes")
        .unwrap()
        .unwrap();
    let stopped: ThreadResult = serde_json::from_value(stopped).unwrap();
    assert_eq!(stopped.thread.status, ThreadStatus::Idle);
    let session = env.script.session(1);
    assert!(
        session.sent().is_empty(),
        "nothing was sent to the stopped process"
    );
    assert!(
        env.script.logged().contains(&"shutdown#1 user".to_owned()),
        "{:?}",
        env.script.logged()
    );
    assert!(session.exited.load(Ordering::SeqCst));
    assert_eq!(env.engine.running_processes(), 0);
    let turn = env.turn(&thread.id, &r.turn_id.unwrap()).await;
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert_eq!(turn.error.unwrap().kind, "stopped");
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupting_during_a_start_keeps_the_process_and_the_next_turn_uses_it() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    env.script.start_open.set(false);
    let first = env.send(&thread.id, "never sent").await;
    env.wait_status(&thread.id, ThreadStatus::Starting).await;
    env.wait_start_begun(1).await;
    let r = env
        .call::<spec::TurnInterrupt>(TurnInterruptParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap();
    assert!(r.interrupted);
    assert_eq!(
        env.turn(&thread.id, &first.turn_id.unwrap()).await.status,
        TurnStatus::Interrupted
    );
    env.script.start_open.set(true);
    // The started process is tracked (not orphaned): it is ready and holds its slot.
    env.wait_status(&thread.id, ThreadStatus::Ready).await;
    assert_eq!(env.engine.running_processes(), 1);
    let session = env.script.session(1);
    assert!(session.sent().is_empty());
    let second = env.send(&thread.id, "second").await;
    eventually("the second input", || {
        session.sent().contains(&"second".to_owned()).then_some(())
    })
    .await;
    session.complete_turn("done");
    env.wait_turn_status(&thread.id, &second.turn_id.unwrap(), TurnStatus::Completed)
        .await;
    assert_eq!(env.script.session_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_sent_during_an_idle_stop_runs_on_a_new_process() {
    let env = env_with(false, |p| p.idle_process_ttl = Duration::from_millis(200)).await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    let first = env.warm_up(&thread.id).await;
    // A settings change that needs a restart is pending when the idle stop begins.
    env.script
        .settings_need_restart
        .store(true, Ordering::SeqCst);
    let updated = env
        .call::<spec::ThreadUpdate>(ThreadUpdateParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
            title: None,
            settings: Some(ThreadSettings {
                permission_mode: Some("auto".into()),
                ..ThreadSettings::default()
            }),
            pinned: None,
            modes: None,
        })
        .await
        .unwrap();
    assert_eq!(
        updated.settings_outcome,
        Some(SettingsOutcome::AppliesNextTurn)
    );
    env.script.exit_open.set(false);
    env.wait_status(&thread.id, ThreadStatus::Stopping).await;
    let r = env.send(&thread.id, "second").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        first.sent(),
        vec!["warm up".to_owned()],
        "nothing goes to the process being stopped"
    );
    env.script.exit_open.set(true);
    let second = eventually("a new process", || {
        (env.script.session_count() == 2).then(|| env.script.session(2))
    })
    .await;
    eventually("the second input", || {
        second.sent().contains(&"second".to_owned()).then_some(())
    })
    .await;
    assert!(
        env.script
            .logged()
            .contains(&"start#2 mode=auto".to_owned()),
        "{:?}",
        env.script.logged()
    );
    second.complete_turn("done");
    env.wait_turn_status(&thread.id, &r.turn_id.unwrap(), TurnStatus::Completed)
        .await;
    env.wait_status(&thread.id, ThreadStatus::Ready).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_changed_while_starting_are_applied_before_the_input_is_sent() {
    for restart in [false, true] {
        let env = env().await;
        let project = env.project("p", false).await;
        let thread = env.thread(&project, None).await;
        env.script
            .settings_need_restart
            .store(restart, Ordering::SeqCst);
        env.script.start_open.set(false);
        env.send(&thread.id, "hello").await;
        env.wait_status(&thread.id, ThreadStatus::Starting).await;
        env.wait_start_begun(1).await;
        let updated = env
            .call::<spec::ThreadUpdate>(ThreadUpdateParams {
                client_request_id: crid(),
                thread_id: thread.id.clone(),
                title: None,
                settings: Some(ThreadSettings {
                    permission_mode: Some("auto".into()),
                    ..ThreadSettings::default()
                }),
                pinned: None,
                modes: None,
            })
            .await
            .unwrap();
        assert_eq!(
            updated.settings_outcome,
            Some(SettingsOutcome::AppliesNextTurn)
        );
        env.script.start_open.set(true);
        let last = if restart { 2 } else { 1 };
        let session = eventually("the process that runs the turn", || {
            (env.script.session_count() == last).then(|| env.script.session(last))
        })
        .await;
        eventually("the input", || {
            session.sent().contains(&"hello".to_owned()).then_some(())
        })
        .await;
        let log = env.script.logged();
        let expected: Vec<String> = if restart {
            [
                "start#1 mode=ask",
                "apply#1 mode=auto",
                "shutdown#1 idle",
                "start#2 mode=auto",
                "send#2 hello",
            ]
            .map(String::from)
            .to_vec()
        } else {
            ["start#1 mode=ask", "apply#1 mode=auto", "send#1 hello"]
                .map(String::from)
                .to_vec()
        };
        assert_eq!(log, expected, "restart = {restart}");
    }
}

#[tokio::test]
async fn an_interaction_requested_and_withdrawn_in_one_batch_is_announced_before_it_closes() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    let session = env.warm_up(&thread.id).await;
    env.send(&thread.id, "go").await;
    eventually("the input", || {
        session.sent().contains(&"go".to_owned()).then_some(())
    })
    .await;
    // Single-threaded runtime: both events are queued before the actor runs again, so they
    // land in the same batch.
    session.emit(AdapterEvent::TurnStarted);
    session.emit(AdapterEvent::InteractionRequested {
        background_key: None,
        request_id: "r1".into(),
        request: InteractionRequest::Approval {
            title: "Run?".into(),
            detail: None,
            subject: Subject::Other {
                description: "x".into(),
            },
            options: vec![ApprovalOption {
                id: "allow".into(),
                label: "Allow".into(),
                kind: ApprovalOptionKind::AllowOnce,
            }],
        },
        item_key: None,
    });
    session.emit(AdapterEvent::InteractionWithdrawn {
        request_id: "r1".into(),
    });
    let ws = env
        .wait_for(WORKSPACE_STREAM, |e| {
            matches!(e, Event::InteractionClosed { .. })
        })
        .await;
    let pending = ws
        .iter()
        .position(|e| matches!(e.event, Event::InteractionPending { .. }));
    let closed = ws
        .iter()
        .position(|e| matches!(e.event, Event::InteractionClosed { .. }));
    assert!(
        pending.is_some() && pending < closed,
        "pending must precede closed: {:?}",
        ws.iter().map(|e| e.event.type_name()).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_run_during_the_start_snapshot_does_not_swallow_the_input() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    // In a git repository a new turn first takes its base snapshot (the process is alive, so
    // that is all it waits for); events of the process keep arriving meanwhile.
    let env = env_with(true, |_| {}).await;
    let project = env.project("repo", true).await;
    let thread = env.thread(&project, None).await;
    let session = env.warm_up(&thread.id).await;
    for round in 0..3 {
        let text = format!("user {round}");
        let r = env.send(&thread.id, &text).await;
        let turn_id = r.turn_id.expect("started");
        // The agent starts (and finishes) a run by itself before the input went out.
        session.complete_turn(&format!("agent run {round}"));
        eventually(&format!("round {round}: the user's input"), || {
            session.sent().contains(&text).then_some(())
        })
        .await;
        assert_ne!(
            env.turn(&thread.id, &turn_id).await.status,
            TurnStatus::Completed,
            "round {round}: the user's turn completed before its input was sent"
        );
        session.complete_turn(&format!("answer {round}"));
        env.wait_turn_status(&thread.id, &turn_id, TurnStatus::Completed)
            .await;
        env.wait_status(&thread.id, ThreadStatus::Ready).await;
        let read = env.read(&thread.id).await;
        let answers: Vec<String> = read
            .items
            .iter()
            .filter(|i| i.turn_id == turn_id)
            .filter_map(|i| match &i.body {
                ItemBody::AgentMessage { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            vec![format!("answer {round}")],
            "round {round}: only the answer belongs to the user's turn"
        );
        let agent_turn = read.turns.iter().find(|t| {
            read.items.iter().any(|i| i.turn_id == t.id && matches!(&i.body, ItemBody::AgentMessage { text } if *text == format!("agent run {round}")))
        });
        assert!(
            agent_turn.is_some_and(|t| t.status == TurnStatus::Completed),
            "round {round}: the agent's run is a turn of its own"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_process_that_exits_during_the_start_snapshot_is_replaced() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env_with(true, |_| {}).await;
    let project = env.project("repo", true).await;
    let thread = env.thread(&project, None).await;
    env.warm_up(&thread.id).await;
    for round in 0..3 {
        let current = env.script.session(env.script.session_count());
        let text = format!("again {round}");
        let r = env.send(&thread.id, &text).await;
        let turn_id = r.turn_id.expect("started");
        // The process crashes while the turn's snapshot is taken.
        current.exit(Some(1), None);
        let session = eventually(
            &format!("round {round}: a process that got the input"),
            || {
                let s = env.script.session(env.script.session_count());
                s.sent().contains(&text).then_some(s)
            },
        )
        .await;
        assert!(
            !Arc::ptr_eq(&session, &current),
            "round {round}: the input went to the exited process"
        );
        session.complete_turn("ok");
        let turn = env
            .wait_turn_status(&thread.id, &turn_id, TurnStatus::Completed)
            .await;
        assert!(turn.error.is_none());
        env.wait_status(&thread.id, ThreadStatus::Ready).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replaced_output_is_bounded_like_streamed_output() {
    let env = env_with(false, |p| p.max_inline_output_bytes = 1024).await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    let session = env.warm_up(&thread.id).await;
    let r = env.send(&thread.id, "run it").await;
    eventually("the input", || {
        session.sent().contains(&"run it".to_owned()).then_some(())
    })
    .await;
    let command = |output: &str| ItemBody::CommandExecution {
        command: "build".into(),
        cwd: None,
        output: output.into(),
        output_truncated: false,
        output_blob_id: None,
        exit_code: None,
        duration_ms: None,
    };
    let big = "0123456789".repeat(500);
    session.emit(AdapterEvent::TurnStarted);
    session.emit(AdapterEvent::ItemStarted {
        key: "c".into(),
        body: command("starting"),
    });
    session.emit(AdapterEvent::ItemUpdated {
        key: "c".into(),
        body: command(&big),
    });
    session.emit(AdapterEvent::ItemCompleted {
        key: "c".into(),
        body: None,
        status: ItemStatus::Completed,
    });
    session.emit(AdapterEvent::TurnCompleted {
        trigger: None,
        status: TurnStatus::Completed,
        usage: None,
        error: None,
    });
    let turn_id = r.turn_id.unwrap();
    env.wait_turn_status(&thread.id, &turn_id, TurnStatus::Completed)
        .await;
    let stream = thread_stream(&thread.id);
    let events = env
        .wait_for(
            &stream,
            |e| matches!(e, Event::TurnCompleted { turn } if turn.id == turn_id),
        )
        .await;
    for e in &events {
        if let Event::ItemUpdated { item } = &e.event
            && let ItemBody::CommandExecution {
                output,
                output_truncated,
                ..
            } = &item.body
        {
            assert!(
                output.len() <= 1024,
                "item/updated carried {} bytes inline",
                output.len()
            );
            if output.len() > 100 {
                assert!(*output_truncated);
            }
        }
    }
    let read = env.read(&thread.id).await;
    let (output, truncated, blob) = read
        .items
        .iter()
        .find_map(|i| match &i.body {
            ItemBody::CommandExecution {
                output,
                output_truncated,
                output_blob_id,
                ..
            } => Some((output.clone(), *output_truncated, output_blob_id.clone())),
            _ => None,
        })
        .unwrap();
    assert!(truncated);
    assert_eq!(output, big[..1024]);
    let (path, _) = env
        .engine
        .blob(&blob.expect("the full output is stored"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), big);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_shared_with_a_fork_is_not_removed() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env_with(true, |_| {}).await;
    let project = env.project("repo", true).await;
    let parent = env
        .thread(
            &project,
            Some(WorkspaceSpec::Worktree {
                base_ref: None,
                branch: None,
            }),
        )
        .await;
    let Workspace::Worktree { path, .. } = parent.workspace.clone() else {
        panic!("expected a worktree")
    };
    env.warm_up(&parent.id).await;
    let fork = env
        .call::<spec::ThreadFork>(ThreadForkParams {
            client_request_id: crid(),
            thread_id: parent.id.clone(),
            at_turn_id: None,
            before: false,
        })
        .await
        .unwrap()
        .thread;
    assert_eq!(
        fork.workspace, parent.workspace,
        "the fork works in the same folder"
    );
    for thread in [&fork.id, &parent.id] {
        let err = env
            .call::<spec::ThreadArchive>(ThreadArchiveParams {
                client_request_id: crid(),
                thread_id: thread.clone(),
                archived: true,
                remove_worktree: true,
                force: true,
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), Some(ErrorKind::InvalidState), "{err:?}");
        assert!(
            Path::new(&path).exists(),
            "the shared worktree is still there"
        );
    }
    // Archiving without removing the folder works.
    let archived = env
        .call::<spec::ThreadArchive>(ThreadArchiveParams {
            client_request_id: crid(),
            thread_id: fork.id.clone(),
            archived: true,
            remove_worktree: false,
            force: false,
        })
        .await
        .unwrap();
    assert!(archived.thread.archived);
    assert!(Path::new(&path).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn importing_the_same_session_twice_at_once_creates_one_thread() {
    let env = env().await;
    let project = env.project("p", false).await;
    let import = || {
        env.call::<spec::NativeImport>(NativeImportParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "scripted".into(),
            native_session_id: "imported".into(),
        })
    };
    let (a, b) = tokio::join!(import(), import());
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(
        a.thread.id, b.thread.id,
        "both imports return the same thread"
    );
    let list = env
        .call::<spec::ThreadList>(ThreadListParams {
            project_id: Some(project.id.clone()),
            include_archived: true,
            limit: None,
            before: None,
        })
        .await
        .unwrap();
    assert_eq!(
        list.threads
            .iter()
            .filter(|t| t.native_session_id.as_deref() == Some("imported"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_the_agent_never_answers_still_stops_it_within_the_grace() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    let session = env.warm_up(&thread.id).await;
    let r = env.send(&thread.id, "long").await;
    eventually("the input", || {
        session.sent().contains(&"long".to_owned()).then_some(())
    })
    .await;
    // The CLI's event loop is stuck: it never answers the interrupt.
    env.script.interrupt_hangs.store(true, Ordering::SeqCst);
    let asked = tokio::time::Instant::now();
    let interrupted = tokio::time::timeout(
        WAIT,
        env.call::<spec::TurnInterrupt>(TurnInterruptParams {
            client_request_id: crid(),
            thread_id: thread.id.clone(),
        }),
    )
    .await
    .expect("turn/interrupt is answered")
    .unwrap();
    assert!(interrupted.interrupted);
    let turn = env
        .wait_turn_status(&thread.id, &r.turn_id.unwrap(), TurnStatus::Interrupted)
        .await;
    assert_eq!(turn.error.unwrap().kind, "forced");
    assert!(
        asked.elapsed() < Duration::from_millis(1500) * 4,
        "forced after about interrupt_grace, not later: {:?}",
        asked.elapsed()
    );
    assert!(
        env.script
            .logged()
            .contains(&"shutdown#1 interruptTimeout".to_owned()),
        "{:?}",
        env.script.logged()
    );
    // The thread takes requests again.
    env.wait_status(&thread.id, ThreadStatus::Idle).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_update_leaves_the_thread_as_it_was() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    // Invalid settings: the title that came with them is not taken either.
    let err = env
        .update(&thread.id, Some("Refactor"), Some("bogus"), Some(true))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::InvalidParams));
    // The process refuses the settings (its control request times out).
    env.warm_up(&thread.id).await;
    env.script.settings_fail.store(true, Ordering::SeqCst);
    let err = env
        .update(&thread.id, Some("Refactor"), Some("auto"), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::AdapterError), "{err:?}");
    env.script.settings_fail.store(false, Ordering::SeqCst);
    // A later commit (here: another update) stores and publishes the thread as it is.
    let view = env
        .update(&thread.id, None, None, Some(false))
        .await
        .unwrap()
        .thread;
    assert_eq!(view.title, "warm up", "the refused title was not kept");
    assert!(!view.pinned);
    assert_eq!(
        view.settings.permission_mode.as_deref(),
        Some("ask"),
        "the refused mode was not kept"
    );
    // The process may have applied a part of what it refused: the next turn gets a new one,
    // started with the thread's settings.
    let r = env.send(&thread.id, "next").await;
    let second = eventually("a new process", || {
        (env.script.session_count() == 2).then(|| env.script.session(2))
    })
    .await;
    eventually("the input", || {
        second.sent().contains(&"next".to_owned()).then_some(())
    })
    .await;
    assert!(
        env.script.logged().contains(&"start#2 mode=ask".to_owned()),
        "{:?}",
        env.script.logged()
    );
    second.complete_turn("done");
    env.wait_turn_status(&thread.id, &r.turn_id.unwrap(), TurnStatus::Completed)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_changed_during_a_running_turn_reach_the_process_before_the_next_turn() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    let session = env.warm_up(&thread.id).await;
    let first = env.send(&thread.id, "running").await;
    eventually("the input", || {
        session.sent().contains(&"running".to_owned()).then_some(())
    })
    .await;
    let updated = env
        .update(&thread.id, None, Some("auto"), None)
        .await
        .unwrap();
    assert_eq!(
        updated.settings_outcome,
        Some(SettingsOutcome::AppliesNextTurn)
    );
    assert_eq!(
        updated.thread.settings.permission_mode.as_deref(),
        Some("auto")
    );
    assert!(
        !env.script.logged().iter().any(|l| l.starts_with("apply#")),
        "the running turn is not affected: {:?}",
        env.script.logged()
    );
    session.complete_turn("done");
    env.wait_turn_status(&thread.id, &first.turn_id.unwrap(), TurnStatus::Completed)
        .await;
    let second = env.send(&thread.id, "next").await;
    eventually("the next input", || {
        session.sent().contains(&"next".to_owned()).then_some(())
    })
    .await;
    let log = env.script.logged();
    let apply = log
        .iter()
        .position(|l| l == "apply#1 mode=auto")
        .expect("applied");
    let send = log.iter().position(|l| l == "send#1 next").expect("sent");
    assert!(apply < send, "applied before the next turn: {log:?}");
    session.complete_turn("done again");
    env.wait_turn_status(&thread.id, &second.turn_id.unwrap(), TurnStatus::Completed)
        .await;
    // An idle process takes a change right away.
    let updated = env
        .update(&thread.id, None, Some("ask"), None)
        .await
        .unwrap();
    assert_eq!(updated.settings_outcome, Some(SettingsOutcome::AppliedLive));
    assert!(env.script.logged().contains(&"apply#1 mode=ask".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_queue_resume_leaves_the_queue_as_it_was() {
    let env = env().await;
    let project = env.project("p", false).await;
    let thread = env.thread(&project, None).await;
    let session = env.warm_up(&thread.id).await;
    let running = env.send(&thread.id, "running").await;
    eventually("the input", || {
        session.sent().contains(&"running".to_owned()).then_some(())
    })
    .await;
    let queued = env.send(&thread.id, "queued").await;
    assert_eq!(queued.disposition, Disposition::Queued);
    env.call::<spec::TurnInterrupt>(TurnInterruptParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    })
    .await
    .unwrap();
    env.wait_turn_status(
        &thread.id,
        &running.turn_id.unwrap(),
        TurnStatus::Interrupted,
    )
    .await;
    assert!(env.get(&thread.id).await.queue_paused);
    // The harness is unavailable for a moment (its CLI is being updated).
    let refresh = || {
        env.call::<spec::HarnessRefresh>(HarnessRefreshParams {
            harness_id: Some("scripted".into()),
        })
    };
    env.script.unavailable.store(true, Ordering::SeqCst);
    refresh().await.unwrap();
    let resume = QueueResumeParams {
        client_request_id: crid(),
        thread_id: thread.id.clone(),
    };
    let err = env
        .call::<spec::QueueResume>(resume.clone())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::HarnessUnavailable));
    let t = env.get(&thread.id).await;
    assert!(t.queue_paused, "still paused");
    assert_eq!(t.queued_inputs, 1);
    // The client resends the request once the harness is back: the queued input starts.
    env.script.unavailable.store(false, Ordering::SeqCst);
    refresh().await.unwrap();
    let resumed = env.call::<spec::QueueResume>(resume).await.unwrap();
    let turn = resumed.turn_id.expect("the queued input starts");
    eventually("the queued input", || {
        session.sent().contains(&"queued".to_owned()).then_some(())
    })
    .await;
    let t = env.get(&thread.id).await;
    assert!(!t.queue_paused);
    assert_eq!(t.queued_inputs, 0);
    session.complete_turn("done");
    env.wait_turn_status(&thread.id, &turn, TurnStatus::Completed)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_whose_parent_has_moved_on_is_refused() {
    let env = env().await;
    let project = env.project("p", false).await;
    let parent = env.thread(&project, None).await;
    let session = env.warm_up(&parent.id).await;
    let fork = env
        .call::<spec::ThreadFork>(ThreadForkParams {
            client_request_id: crid(),
            thread_id: parent.id.clone(),
            at_turn_id: None,
            before: false,
        })
        .await
        .unwrap()
        .thread;
    // The parent goes on before the fork's first turn.
    let more = env.send(&parent.id, "delete the legacy module").await;
    eventually("the parent's input", || {
        session
            .sent()
            .contains(&"delete the legacy module".to_owned())
            .then_some(())
    })
    .await;
    session.complete_turn("deleted");
    env.wait_turn_status(&parent.id, &more.turn_id.unwrap(), TurnStatus::Completed)
        .await;
    let err = env
        .call::<spec::TurnStart>(TurnStartParams {
            client_request_id: crid(),
            thread_id: fork.id.clone(),
            input: vec![InputPart::Text {
                text: "hello".into(),
            }],
            delivery: Delivery::Auto,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::InvalidState), "{err:?}");
    assert!(err.message.contains("fork it again"), "{err:?}");
    assert_eq!(env.script.session_count(), 1, "no session was branched off");
    // A fork made now starts from the parent's current point.
    let again = env
        .call::<spec::ThreadFork>(ThreadForkParams {
            client_request_id: crid(),
            thread_id: parent.id.clone(),
            at_turn_id: None,
            before: false,
        })
        .await
        .unwrap()
        .thread;
    let r = env.send(&again.id, "hello").await;
    let forked = eventually("the fork's process", || {
        (env.script.session_count() == 2).then(|| env.script.session(2))
    })
    .await;
    eventually("the fork's input", || {
        forked.sent().contains(&"hello".to_owned()).then_some(())
    })
    .await;
    forked.complete_turn("hi");
    env.wait_turn_status(&again.id, &r.turn_id.unwrap(), TurnStatus::Completed)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_whose_parent_moves_on_while_it_waits_for_its_process_is_refused_then() {
    let env = env_with(false, |p| p.max_running_processes = 1).await;
    let project = env.project("p", false).await;
    let parent = env.thread(&project, None).await;
    let session = env.warm_up(&parent.id).await;
    let fork = env
        .call::<spec::ThreadFork>(ThreadForkParams {
            client_request_id: crid(),
            thread_id: parent.id.clone(),
            at_turn_id: None,
            before: false,
        })
        .await
        .unwrap()
        .thread;
    // The fork's first turn is accepted and waits for the parent's process slot.
    let waiting = env.send(&fork.id, "hello").await;
    env.wait_status(&fork.id, ThreadStatus::Queued).await;
    let more = env.send(&parent.id, "delete the legacy module").await;
    eventually("the parent's input", || {
        session
            .sent()
            .contains(&"delete the legacy module".to_owned())
            .then_some(())
    })
    .await;
    session.complete_turn("deleted");
    env.wait_turn_status(&parent.id, &more.turn_id.unwrap(), TurnStatus::Completed)
        .await;
    env.call::<spec::ThreadStop>(ThreadStopParams {
        client_request_id: crid(),
        thread_id: parent.id.clone(),
    })
    .await
    .unwrap();
    let turn = env
        .wait_turn_status(&fork.id, &waiting.turn_id.unwrap(), TurnStatus::Failed)
        .await;
    assert_eq!(turn.error.unwrap().kind, "forkOutdated");
    assert_eq!(env.script.session_count(), 1, "no session was branched off");
    assert_eq!(
        env.get(&fork.id).await.last_error.unwrap().kind,
        "forkOutdated"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_during_a_project_removal_that_fails_are_handled_normally() {
    if !git_available() {
        eprintln!("git not installed; skipping");
        return;
    }
    let env = env_with(true, |_| {}).await;
    let project = env.project("repo", true).await;
    let local = env.thread(&project, None).await;
    let worktree = env
        .thread(
            &project,
            Some(WorkspaceSpec::Worktree {
                base_ref: None,
                branch: None,
            }),
        )
        .await;
    let Workspace::Worktree { path, .. } = worktree.workspace.clone() else {
        panic!("expected a worktree")
    };
    // Uncommitted work: the removal is refused, after `git status` has looked at all of it.
    for i in 0..3000 {
        std::fs::write(Path::new(&path).join(format!("untracked-{i}.txt")), "work").unwrap();
    }
    let removal = {
        let (engine, ctx) = (env.engine.clone(), env.ctx.clone());
        let params = ProjectRemoveParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
        };
        tokio::spawn(async move {
            let req = ClientRequest::parse(
                "project/remove",
                Some(serde_json::to_value(params).unwrap()),
            )
            .unwrap();
            engine.handle(&ctx, req).await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Sent while the removal runs: answered once it is decided, and not with `notFound`.
    let update = env.update(&local.id, None, None, Some(true)).await;
    let refused = tokio::time::timeout(WAIT, removal)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(refused.kind(), Some(ErrorKind::InvalidState), "{refused:?}");
    assert!(
        update
            .expect("the request is handled once the removal failed")
            .thread
            .pinned
    );
    // Both threads carry on.
    let r = env.send(&local.id, "still here").await;
    let session = eventually("the thread's process", || {
        (env.script.session_count() == 1).then(|| env.script.session(1))
    })
    .await;
    eventually("the input", || {
        session
            .sent()
            .contains(&"still here".to_owned())
            .then_some(())
    })
    .await;
    session.complete_turn("yes");
    env.wait_turn_status(&local.id, &r.turn_id.unwrap(), TurnStatus::Completed)
        .await;
    assert!(Path::new(&path).join("untracked-0.txt").exists());
}

fn native_session(id: &str, title: &str, updated_at: Option<i64>) -> NativeSessionSummary {
    NativeSessionSummary {
        native_session_id: id.into(),
        title: Some(title.into()),
        updated_at,
        cwd: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_list_names_each_session_once_whatever_the_adapter_returns() {
    let env = env().await;
    let project = env.project("p", false).await;
    // An adapter that repeats sessions (the shape Codex's thread/list has for a resumed
    // thread: one entry per rollout, same id), in no particular order.
    *env.script.native_sessions.lock() = Some(vec![
        native_session("thread-a", "Fix the parser", Some(1_000)),
        native_session("thread-b", "Add tests", Some(2_000)),
        native_session("thread-a", "Fix the parser (resumed)", Some(3_000)),
        native_session("thread-c", "Docs", None),
        native_session("thread-a", "Fix the parser (first rollout)", Some(500)),
        native_session("thread-b", "Add tests", Some(2_000)),
    ]);
    // One of them is imported already: the merged entry carries its thread.
    let imported = env
        .call::<spec::NativeImport>(NativeImportParams {
            client_request_id: crid(),
            project_id: project.id.clone(),
            harness_id: "scripted".into(),
            native_session_id: "thread-a".into(),
        })
        .await
        .unwrap()
        .thread;
    let listed = env
        .call::<spec::NativeList>(NativeListParams {
            project_id: project.id.clone(),
            harness_id: "scripted".into(),
        })
        .await
        .unwrap()
        .sessions;
    let summary: Vec<(&str, Option<&str>, Option<i64>)> = listed
        .iter()
        .map(|s| {
            (
                s.native_session_id.as_str(),
                s.title.as_deref(),
                s.updated_at,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            // First position, latest entry.
            ("thread-a", Some("Fix the parser (resumed)"), Some(3_000)),
            ("thread-b", Some("Add tests"), Some(2_000)),
            ("thread-c", Some("Docs"), None),
        ]
    );
    assert_eq!(listed[0].imported_thread_id.as_ref(), Some(&imported.id));
    assert_eq!(listed[1].imported_thread_id, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_switching_harness_commands_are_never_offered() {
    let env = env().await;
    let project = env.project("p", false).await;
    let harness_command = |name: &str| Command {
        name: name.into(),
        description: None,
        source: CommandSource::Harness,
        argument_hint: None,
        action: CommandAction::InsertText {
            text: format!("/{name} "),
        },
    };
    let offered = [
        harness_command("compact"),
        // Reserved for every harness (the app's own /resume opens the session import).
        harness_command("resume"),
        // Named by this adapter (`session_switching_commands`).
        harness_command("new-session"),
        harness_command("review"),
    ];
    *env.script.commands.lock() = offered.to_vec();
    let names = |commands: Vec<Command>| -> Vec<String> {
        commands
            .into_iter()
            .filter(|c| c.source == CommandSource::Harness)
            .map(|c| c.name)
            .collect()
    };
    // Without a thread: the adapter's `commands`.
    let listed = env
        .call::<spec::CommandList>(CommandListParams {
            thread_id: None,
            project_id: Some(project.id.clone()),
            harness_id: Some("scripted".into()),
        })
        .await
        .unwrap()
        .commands;
    assert_eq!(names(listed), ["compact", "review"]);
    // In a thread: the list the running session reported (`CommandsChanged`).
    let thread = env.thread(&project, None).await;
    let session = env.warm_up(&thread.id).await;
    session.emit(AdapterEvent::CommandsChanged {
        commands: vec![
            harness_command("resume"),
            harness_command("init"),
            harness_command("new-session"),
        ],
    });
    env.wait_for(&thread_stream(&thread.id), |e| {
        matches!(e, Event::CommandsChanged {})
    })
    .await;
    let listed = env
        .call::<spec::CommandList>(CommandListParams {
            thread_id: Some(thread.id.clone()),
            project_id: None,
            harness_id: None,
        })
        .await
        .unwrap()
        .commands;
    assert_eq!(names(listed), ["init"]);
}
