//! Storage failures (design.md §6.2), driven by the database's test-only write failpoint:
//! a transient failure loses nothing once a retry succeeds; a persistent one takes the
//! fail-stop path — no more work is accepted, every agent process is stopped, and a restart
//! recovers as after any stop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aas_adapter_fake::FakeAdapter;
use aas_harness::{
    AdapterContext, AdapterError, AdapterEvent, CommandContext, ExitInfo, HarnessAdapter,
    HarnessInfo, NativeHistory, NativeSessionSummary, SessionControl, SessionHandle,
    SettingsApplied, StartRequest, StopReason, TurnInput,
};
use aas_protocol::events::Event;
use aas_protocol::methods::*;
use aas_protocol::*;
use aas_supervisor::{Supervisor, SupervisorPolicy};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use crate::{Engine, EngineConfig, HarnessRegistry, Policy, RequestCtx};

const WAIT: Duration = Duration::from_secs(30);

fn policy() -> Policy {
    Policy {
        stop_grace: Duration::from_millis(300),
        interrupt_grace: Duration::from_millis(1500),
        prevent_sleep_while_running: false,
        storage_retry_attempts: 4,
        storage_retry_initial_backoff: Duration::from_millis(20),
        storage_retry_max_backoff: Duration::from_millis(80),
        ..Policy::default()
    }
}

fn supervisor(data: &Path, policy: &Policy) -> Supervisor {
    Supervisor::new(
        &data.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..policy.supervisor_policy()
        },
    )
    .unwrap()
}

async fn start(
    data: &Path,
    root: &Path,
    adapter: Arc<dyn HarnessAdapter>,
    supervisor: Supervisor,
) -> Arc<Engine> {
    let config = EngineConfig {
        data_dir: data.to_path_buf(),
        server_name: "test".into(),
        hostname: "host".into(),
        project_roots: vec![root.to_path_buf()],
        policy: policy(),
        heuristics: Default::default(),
        git: None,
    };
    Engine::start(config, HarnessRegistry::new(vec![adapter]), supervisor)
        .await
        .unwrap()
}

fn fake(data: &Path, supervisor: &Supervisor) -> Arc<dyn HarnessAdapter> {
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: data.join("adapters").join("fake"),
        policy: policy().adapter_policy(),
    };
    Arc::new(FakeAdapter::in_process("fake", ctx))
}

async fn call(
    engine: &Arc<Engine>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let ctx = RequestCtx {
        device_id: DeviceId::from("dev_test"),
    };
    engine
        .handle(&ctx, ClientRequest::parse(method, Some(params)).unwrap())
        .await
}

/// A project folder under a fresh temporary root: (temp dir, data dir, canonical root, project path).
fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("projects");
    std::fs::create_dir_all(root.join("p")).unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let project = root.join("p");
    (dir, data, root, project)
}

async fn open_project(engine: &Arc<Engine>, path: &Path) -> ProjectId {
    let v = call(engine, "project/open", json!({"clientRequestId": ulid::Ulid::generate().to_string(), "path": path.display().to_string()}))
        .await
        .unwrap();
    serde_json::from_value::<ProjectResult>(v)
        .unwrap()
        .project
        .id
}

/// Reads `stream` until `pred` matches an event.
async fn wait_event(engine: &Engine, stream: &str, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut seen = Vec::new();
    let mut cursor = 0;
    loop {
        let mut rx = engine.subscribe_head(stream);
        let batch = engine.read_batch(stream.to_owned(), cursor).await.unwrap();
        cursor = batch.last_seq;
        for e in batch.events {
            let done = pred(&e.event);
            seen.push(e.event);
            if done {
                return seen;
            }
        }
        if *rx.borrow_and_update() > cursor {
            continue;
        }
        tokio::time::timeout_at(deadline, rx.changed())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out on {stream}; saw {:?}",
                    seen.iter().map(Event::type_name).collect::<Vec<_>>()
                )
            })
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn output_written_during_a_transient_storage_failure_is_not_lost() {
    let (_dir, data, root, path) = dirs();
    let sup = supervisor(&data, &policy());
    let engine = start(&data, &root, fake(&data, &sup), sup).await;
    let project = open_project(&engine, &path).await;
    let created: ThreadCreateResult = serde_json::from_value(
        call(
            &engine,
            "thread/create",
            json!({"clientRequestId": "c1", "projectId": project, "harnessId": "fake", "input": [{"type": "text", "text": "@stream 60 15"}]}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let stream = thread_stream(&created.thread.id);
    wait_event(&engine, &stream, |e| matches!(e, Event::ItemDelta { .. })).await;
    // Three writes fail in a row while the agent keeps streaming: the actor retries (four
    // attempts are allowed) and everything it received meanwhile goes into the next commit.
    engine.failpoint().fail_next(3);
    let events = wait_event(&engine, &stream, |e| {
        matches!(e, Event::TurnCompleted { .. })
    })
    .await;
    assert_eq!(
        engine.failpoint().injected(),
        3,
        "the failures were injected"
    );
    assert!(
        engine.storage_failure().is_none(),
        "a transient failure does not stop the daemon"
    );

    let expected: String = (0..60).map(|i| format!("tok{i} ")).collect();
    let streamed: String = events
        .iter()
        .filter_map(|e| match e {
            Event::ItemDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, expected, "every delta reached the log");
    let read: ThreadReadResult = serde_json::from_value(
        call(
            &engine,
            "thread/read",
            json!({"threadId": created.thread.id}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(read.turns[0].status, TurnStatus::Completed);
    let text: String = read
        .items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::AgentMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, expected, "the stored item is complete");

    // The daemon keeps working normally afterwards.
    call(
        &engine,
        "turn/start",
        json!({"clientRequestId": "c2", "threadId": created.thread.id, "input": [{"type": "text", "text": "again"}]}),
    )
    .await
    .unwrap();
    wait_event(
        &engine,
        &stream,
        |e| matches!(e, Event::TurnCompleted { turn } if turn.index == 1),
    )
    .await;
    engine.shutdown(false).await;
}

// ----- a harness whose agent is a real process tree ----------------------------------------------

/// Starts `cmd /c ping` (a real process with a child) under the supervisor as the "agent";
/// its events are whatever the test sends through [`ProcessAdapter::events`].
struct ProcessAdapter {
    supervisor: Supervisor,
    sessions: parking_lot::Mutex<Vec<(u32, mpsc::UnboundedSender<AdapterEvent>)>>,
}

impl ProcessAdapter {
    /// PID and event sender of the `n`-th session (1-based), once it exists.
    async fn session(&self, n: usize) -> (u32, mpsc::UnboundedSender<AdapterEvent>) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(s) = self.sessions.lock().get(n - 1).cloned() {
                return s;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "session {n} never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn process_info() -> HarnessInfo {
    HarnessInfo {
        available: true,
        unavailable_reason: None,
        version: Some("1".into()),
        executable: None,
        capabilities: HarnessCapabilities {
            interrupt: true,
            steer: false,
            approvals: false,
            questions: false,
            resume: true,
            fork: false,
            images: false,
            model_switch_live: true,
            native_sessions: false,
        },
        models: Vec::new(),
        default_model: None,
        effort_levels: Vec::new(),
        permission_modes: Vec::new(),
        default_permission_mode: None,
    }
}

#[async_trait]
impl HarnessAdapter for ProcessAdapter {
    fn id(&self) -> &str {
        "proc"
    }
    fn kind(&self) -> HarnessKind {
        HarnessKind::Fake
    }
    fn display_name(&self) -> &str {
        "Process"
    }
    async fn probe(&self) -> HarnessInfo {
        process_info()
    }
    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError> {
        let cmd = aas_supervisor::resolve_program("cmd")
            .map_err(|e| AdapterError::Spawn(e.to_string()))?;
        let spec = aas_supervisor::SpawnSpec::new("proc", cmd, &req.cwd)
            .args(["/c", "ping -n 3600 127.0.0.1"])
            .owner(req.thread_id.as_str());
        let mut child = self
            .supervisor
            .spawn(spec)
            .await
            .map_err(|e| AdapterError::Spawn(e.to_string()))?;
        drop(child.stdin.take());
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = child.handle.clone();
        let exited = tx.clone();
        tokio::spawn(async move {
            let info = handle.wait().await;
            let _ = exited.send(AdapterEvent::Exited { info });
        });
        self.sessions.lock().push((child.handle.pid(), tx));
        let control = Arc::new(ProcessSession {
            handle: child.handle,
            grace: policy().stop_grace,
        });
        Ok(SessionHandle {
            native_session_id: Some(format!("native-{}", ulid::Ulid::generate())),
            control,
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

struct ProcessSession {
    handle: aas_supervisor::ChildHandle,
    grace: Duration,
}

#[async_trait]
impl SessionControl for ProcessSession {
    async fn send(&self, _input: TurnInput) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn steer(&self, _input: TurnInput) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("steer"))
    }
    async fn interrupt(&self) -> Result<(), AdapterError> {
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
        _settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError> {
        Ok(SettingsApplied::Live)
    }
    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        self.handle.shutdown(self.grace, reason).await
    }
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_storage_failure_stops_the_daemon_and_leaves_no_agent_process() {
    let (_dir, data, root, path) = dirs();
    let sup = supervisor(&data, &policy());
    let adapter = Arc::new(ProcessAdapter {
        supervisor: sup.clone(),
        sessions: Default::default(),
    });
    let engine = start(&data, &root, adapter.clone(), sup.clone()).await;
    let project = open_project(&engine, &path).await;
    let created: ThreadCreateResult = serde_json::from_value(
        call(
            &engine,
            "thread/create",
            json!({"clientRequestId": "c1", "projectId": project, "harnessId": "proc", "input": [{"type": "text", "text": "go"}]}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let (pid, events) = adapter.session(1).await;
    let agent = aas_supervisor::process_creation_time(pid)
        .map(|created| (pid, created))
        .expect("the agent runs");
    wait_event(
        &engine,
        &thread_stream(&created.thread.id),
        |e| matches!(e, Event::ThreadUpdated { thread } if thread.status == ThreadStatus::Running),
    )
    .await;

    // The disk stops accepting writes; the agent keeps producing output.
    engine.failpoint().fail_always();
    events.send(AdapterEvent::TurnStarted).unwrap();
    events
        .send(AdapterEvent::ItemStarted {
            key: "m".into(),
            body: ItemBody::AgentMessage {
                text: String::new(),
            },
        })
        .unwrap();
    events
        .send(AdapterEvent::ItemDelta {
            key: "m".into(),
            field: DeltaField::Text,
            text: "lost?".into(),
        })
        .unwrap();

    let failure = tokio::time::timeout(WAIT, engine.wait_storage_failure())
        .await
        .expect("the daemon fail-stops");
    assert!(failure.message.contains("thread changes"), "{failure:?}");
    assert!(
        engine.failpoint().injected() >= policy().storage_retry_attempts as u64,
        "every attempt was made"
    );
    assert_eq!(
        engine.shutdown_reason(),
        aas_protocol::notifications::ShutdownReason::StorageFailure
    );

    // No more work is accepted (the client keeps its request and resends it after the restart).
    let refused = call(&engine, "thread/list", json!({})).await.unwrap_err();
    assert_eq!(refused.kind(), Some(ErrorKind::Draining));
    let refused = call(
        &engine,
        "turn/start",
        json!({"clientRequestId": "c2", "threadId": created.thread.id, "input": [{"type": "text", "text": "more"}]}),
    )
    .await
    .unwrap_err();
    assert!(
        !refused.kind().is_some_and(ErrorKind::is_definitive),
        "not stored as a final answer: {refused:?}"
    );

    // The fail-stop stops the agent's process tree through the supervisor.
    tokio::time::timeout(WAIT, engine.shutdown(false))
        .await
        .expect("the fail-stop completes");
    assert_eq!(engine.running_processes(), 0);
    assert_eq!(sup.running_count(), 0, "no supervised process is left");
    assert!(
        !aas_supervisor::process_is_running(agent.0, agent.1),
        "the agent process is gone"
    );
    engine.close().await.unwrap();

    // The restart: the usual recovery applies.
    let sup = supervisor(&data, &policy());
    let adapter = Arc::new(ProcessAdapter {
        supervisor: sup.clone(),
        sessions: Default::default(),
    });
    let engine = start(&data, &root, adapter, sup).await;
    let read: ThreadReadResult = serde_json::from_value(
        call(
            &engine,
            "thread/read",
            json!({"threadId": created.thread.id}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(read.thread.status, ThreadStatus::Idle);
    assert_eq!(read.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        read.turns[0].error.as_ref().unwrap().kind,
        "daemonRestarted"
    );
    assert!(engine.storage_failure().is_none());
    engine.shutdown(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_failures_of_request_handlers_reach_the_client_without_a_fail_stop() {
    let (_dir, data, root, path) = dirs();
    let sup = supervisor(&data, &policy());
    let engine = start(&data, &root, fake(&data, &sup), sup).await;
    // The startup's own writes (the probe results) are done once harnesses are listed.
    call(&engine, "harness/list", json!({})).await.unwrap();
    engine.failpoint().fail_next(1);
    let err = call(
        &engine,
        "project/open",
        json!({"clientRequestId": "o1", "path": path.display().to_string()}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.kind(),
        Some(ErrorKind::Internal),
        "not definitive: the client resends it"
    );
    assert!(
        engine.storage_failure().is_none(),
        "a client is told; nothing was lost"
    );
    open_project(&engine, &path).await;
    engine.shutdown(false).await;
}
