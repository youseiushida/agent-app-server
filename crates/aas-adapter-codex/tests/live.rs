//! Live tests against the installed `codex` CLI.
//!
//! Run with: `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-codex --test live -- --ignored`
//!
//! * `live_codex_round_trip` spends one small turn of the configured model.
//! * `live_codex_background_work` runs the installed app-server against a scripted model
//!   (`mock_model`, a local Responses API endpoint) in a temporary `CODEX_HOME`: no tokens, and
//!   nothing of the user's Codex configuration or sessions is read or written.

mod mock_model;

use std::time::Duration;

use aas_adapter_codex::CodexAdapter;
use aas_harness::protocol::{BackgroundTaskKind, HarnessKind, ItemBody, ItemStatus};
use aas_harness::{
    AdapterContext, AdapterEvent, AdapterPolicy, BackgroundState, BackgroundTaskInfo,
    CommandContext, HarnessAdapter, HarnessConfig, SessionHandle, StartMode, StartRequest,
    ThreadId, ThreadSettings, TurnInput, TurnStatus,
};
use aas_supervisor::{StopReason, Supervisor, SupervisorPolicy};
use mock_model::{LONG, MockModel, SHORT, WORKER_CMD, background_script};
use serde_json::Value;

fn live_enabled() -> bool {
    std::env::var_os("AAS_LIVE_TESTS").is_some()
}

#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_codex_round_trip() {
    if !live_enabled() {
        eprintln!("AAS_LIVE_TESTS not set; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = dir.path().join("state");
    let supervisor = Supervisor::new(
        &state,
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: state.join("codex"),
        policy: AdapterPolicy::default(),
    };
    let adapter = CodexAdapter::new(
        HarnessConfig {
            id: "codex".into(),
            kind: HarnessKind::Codex,
            display_name: None,
            command: "codex".into(),
            args: Vec::new(),
            env: Default::default(),
            options: Value::Null,
        },
        ctx,
    );

    let info = adapter.probe().await;
    assert!(info.available, "{:?}", info.unavailable_reason);
    assert!(!info.models.is_empty());
    assert!(
        info.version.as_deref().is_some_and(|v| v.contains("codex")),
        "{:?}",
        info.version
    );

    let mut session = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: ws.clone(),
            settings: ThreadSettings {
                permission_mode: Some("readOnly".into()),
                ..Default::default()
            },
            mode: StartMode::New,
        })
        .await
        .unwrap();
    let native = session.native_session_id.clone().expect("thread id");
    session
        .control
        .send(TurnInput::text("Reply with exactly: OK"))
        .await
        .unwrap();

    let mut text = String::new();
    let status = loop {
        let event = tokio::time::timeout(Duration::from_secs(240), session.events.recv())
            .await
            .expect("turn took too long")
            .expect("events closed");
        match event {
            AdapterEvent::ItemCompleted {
                body: Some(ItemBody::AgentMessage { text: t }),
                ..
            } => text.push_str(&t),
            AdapterEvent::TurnCompleted { status, usage, .. } => {
                // `thread/tokenUsage/updated` carries the model's context window.
                let context = usage.and_then(|u| u.context).expect("context reported");
                assert!(
                    context.used_tokens > 0 && context.window_tokens >= context.used_tokens,
                    "{context:?}"
                );
                break status;
            }
            AdapterEvent::Native { payload } => eprintln!("native: {payload}"),
            _ => {}
        }
    };
    assert_eq!(status, TurnStatus::Completed);
    assert!(text.contains("OK"), "{text}");

    // Listings reuse the live session's app-server.
    let commands = adapter
        .commands(CommandContext {
            cwd: ws.clone(),
            native_session_id: Some(native.clone()),
        })
        .await
        .unwrap();
    assert!(commands.iter().any(|c| c.name == "compact"));
    let sessions = adapter.list_native_sessions(&ws).await.unwrap();
    assert!(
        sessions.iter().any(|s| s.native_session_id == native),
        "{sessions:?}"
    );
    let history = adapter.read_native_history(&ws, &native).await.unwrap();
    assert!(history.turns.iter().any(|t| t
        .items
        .iter()
        .any(|i| matches!(&i.body, ItemBody::UserMessage { text, .. } if text == "Reply with exactly: OK"))));

    let exit = session.control.shutdown(StopReason::Shutdown).await;
    let mut last = None;
    while let Some(event) = session.events.recv().await {
        last = Some(event);
    }
    assert!(
        matches!(last, Some(AdapterEvent::Exited { .. })),
        "{last:?}"
    );
    assert!(exit.stopped.is_none() || exit.stopped == Some(StopReason::Shutdown));
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may outlive the test"
    );
}

// ----- background work against a scripted model ------------------------------------------------

/// Upper bound of any single wait of the background test (test harness only).
const EVENT_WAIT: Duration = Duration::from_secs(120);

struct Events<'a> {
    session: &'a mut SessionHandle,
    seen: Vec<AdapterEvent>,
}

impl Events<'_> {
    async fn until(&mut self, what: &str, pred: impl Fn(&AdapterEvent) -> bool) -> AdapterEvent {
        loop {
            let event = tokio::time::timeout(EVENT_WAIT, self.session.events.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; seen: {:#?}", self.seen))
                .unwrap_or_else(|| panic!("events closed while waiting for {what}"));
            if let AdapterEvent::Native { payload } = &event {
                eprintln!("native: {payload}");
            }
            self.seen.push(event.clone());
            if pred(&event) {
                return event;
            }
        }
    }

    async fn until_task(
        &mut self,
        what: &str,
        pred: impl Fn(&BackgroundTaskInfo) -> bool,
    ) -> BackgroundTaskInfo {
        match self
            .until(
                what,
                |e| matches!(e, AdapterEvent::BackgroundTask { task } if pred(task)),
            )
            .await
        {
            AdapterEvent::BackgroundTask { task } => *task,
            _ => unreachable!(),
        }
    }

    fn task(&self, key: &str) -> Option<BackgroundTaskInfo> {
        self.seen.iter().rev().find_map(|e| match e {
            AdapterEvent::BackgroundTask { task } if task.key == key => Some((**task).clone()),
            _ => None,
        })
    }
}

/// The background work of the installed app-server end to end: two commands outlive their
/// turn (one stopped from the phone, one ending by itself), and a v2 sub-agent runs after the
/// parent's turn, is stopped, and leaves its command as a terminal that is stopped too.
#[tokio::test]
#[ignore = "runs the installed codex CLI; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_codex_background_work() {
    if !live_enabled() {
        eprintln!("AAS_LIVE_TESTS not set; skipping");
        return;
    }
    let model = MockModel::start(background_script).await;
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let home = mock_model::codex_home(dir.path(), &model.base_url);
    let state = dir.path().join("state");
    let supervisor = Supervisor::new(
        &state,
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .unwrap();
    let adapter = CodexAdapter::new(
        HarnessConfig {
            id: "codex".into(),
            kind: HarnessKind::Codex,
            display_name: None,
            command: "codex".into(),
            args: Vec::new(),
            env: [("CODEX_HOME".to_owned(), home.to_string_lossy().into_owned())].into(),
            options: Value::Null,
        },
        AdapterContext {
            supervisor: supervisor.clone(),
            state_dir: state.join("codex"),
            policy: AdapterPolicy::default(),
        },
    );
    let mut session = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: ws.clone(),
            settings: ThreadSettings {
                permission_mode: Some("fullAccess".into()),
                ..Default::default()
            },
            mode: StartMode::New,
        })
        .await
        .unwrap();
    let control = session.control.clone();
    let mut ev = Events {
        session: &mut session,
        seen: Vec::new(),
    };

    // 1. Two commands outlive their turn.
    control
        .send(TurnInput::text("ROLE=TERM# Start two long commands."))
        .await
        .unwrap();
    let done = ev
        .until("the terminals' turn", |e| {
            matches!(e, AdapterEvent::TurnCompleted { .. })
        })
        .await;
    assert!(
        matches!(
            done,
            AdapterEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ),
        "{done:?}"
    );
    for (key, command) in [("mock_TERM_0", LONG), ("mock_TERM_1", SHORT)] {
        let task = ev
            .task(key)
            .unwrap_or_else(|| panic!("task {key}: {:#?}", ev.seen));
        assert_eq!(
            (task.kind, task.title.as_str(), task.live, task.state),
            (
                BackgroundTaskKind::Shell,
                command,
                true,
                BackgroundState::Running
            )
        );
        assert!(ev.seen.iter().any(|e| matches!(
            e,
            AdapterEvent::ItemCompleted { key: k, status: ItemStatus::Backgrounded, .. } if k == key
        )));
    }
    control.stop_background("mock_TERM_0").await.unwrap();
    let long = ev
        .until_task("the long command's stop", |t| {
            t.key == "mock_TERM_0" && t.state.is_ended()
        })
        .await;
    assert_eq!(long.state, BackgroundState::Stopped, "{long:?}");
    assert!(!long.live);
    let short = ev
        .until_task("the short command's end", |t| {
            t.key == "mock_TERM_1" && t.state.is_ended()
        })
        .await;
    assert_eq!(short.state, BackgroundState::Completed, "{short:?}");
    let result = short.result.expect("result");
    assert_eq!(result.exit_code, Some(0));
    assert!(
        result
            .output
            .as_deref()
            .is_some_and(|o| o.contains("SHORT_DONE")),
        "{result:?}"
    );

    // 2. A sub-agent runs after the parent's turn.
    control
        .send(TurnInput::text(
            "ROLE=SPAWN# Spawn the worker sub-agent, do not wait.",
        ))
        .await
        .unwrap();
    ev.until("the spawning turn", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let worker = ev
        .seen
        .iter()
        .rev()
        .find_map(|e| match e {
            AdapterEvent::BackgroundTask { task } if task.kind == BackgroundTaskKind::Agent => {
                Some((**task).clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no sub-agent task: {:#?}", ev.seen));
    assert_eq!(worker.title, "/root/worker");
    assert!(worker.origin_item_key.is_some());
    let key = worker.key.clone();
    // Its command started (its progress counts it), then it is stopped.
    ev.until_task("the sub-agent's command", |t| {
        t.key == key && t.progress.as_ref().and_then(|p| p.tool_uses) == Some(1)
    })
    .await;
    control.stop_background(&key).await.unwrap();
    let stopped = ev
        .until_task("the sub-agent's stop", |t| {
            t.key == key && t.state.is_ended()
        })
        .await;
    assert_eq!(stopped.state, BackgroundState::Stopped);
    let terminal_key = format!("{key}:mock_worker_0");
    let terminal = ev
        .task(&terminal_key)
        .unwrap_or_else(|| panic!("{:#?}", ev.seen));
    assert_eq!(
        (
            terminal.title.as_str(),
            terminal.live,
            terminal.parent_key.as_deref()
        ),
        (WORKER_CMD, true, Some(key.as_str()))
    );
    control.stop_background(&terminal_key).await.unwrap();
    let terminal = ev
        .until_task("the sub-agent's terminal stop", |t| {
            t.key == terminal_key && t.state.is_ended()
        })
        .await;
    assert_eq!(terminal.state, BackgroundState::Stopped);
    assert!(
        ev.seen
            .iter()
            .all(|e| !matches!(e, AdapterEvent::Native { .. })),
        "native events: {:#?}",
        ev.seen
    );

    let sampled = model.sampled.lock().clone();
    eprintln!("scripted model answered: {sampled:#?}");
    assert!(
        sampled.iter().all(|s| !s.reply.contains("MOCK_")),
        "{sampled:#?}"
    );
    let steps = |role: &str| -> Vec<usize> {
        sampled
            .iter()
            .filter(|s| s.role.as_deref() == Some(role))
            .map(|s| s.step)
            .collect()
    };
    assert_eq!(steps("TERM"), [0, 1, 2], "{sampled:#?}");
    assert_eq!(steps("SPAWN"), [0, 1], "{sampled:#?}");
    assert_eq!(steps("worker"), [0], "stopped while its command ran");
    assert!(model.errors.lock().is_empty(), "{:?}", model.errors.lock());

    control.shutdown(StopReason::Shutdown).await;
    let mut last = None;
    while let Some(event) = session.events.recv().await {
        last = Some(event);
    }
    assert!(
        matches!(last, Some(AdapterEvent::Exited { .. })),
        "{last:?}"
    );
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may outlive the test"
    );
}
