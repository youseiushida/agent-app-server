//! Live tests against the installed `codex` CLI.
//!
//! Run with: `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-codex --test live -- --ignored`
//!
//! * `live_codex_round_trip` spends one small turn of the configured model.
//! * `live_codex_background_work` and `live_codex_features` run the installed app-server
//!   against a scripted model (`mock_model`, a local Responses API endpoint) in a temporary
//!   `CODEX_HOME`: no tokens, and nothing of the user's Codex configuration or sessions is read
//!   or written.
//! * `live_codex_texts_match_the_installed_binary` reads the installed Codex binary (no
//!   process).

mod mock_model;

use std::time::Duration;

use std::sync::Arc;

use aas_adapter_codex::CodexAdapter;
use aas_harness::protocol::{BackgroundTaskKind, HarnessKind, ItemBody, ItemStatus};
use aas_harness::{
    AdapterContext, AdapterError, AdapterEvent, AdapterPolicy, BackgroundState, BackgroundTaskInfo,
    CommandContext, ForkPoint, HarnessAdapter, HarnessConfig, OutputUpdate, SessionControl,
    SessionHandle, StartMode, StartOptions, StartRequest, ThreadId, ThreadModes, ThreadSettings,
    TurnInput, TurnStatus,
};
use aas_supervisor::{StopReason, Supervisor, SupervisorPolicy};
use mock_model::{
    LONG, MockModel, PLAN_BODY, SHORT, WORKER_CMD, background_script, features_script,
};
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
            project_trusted: None,
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
    // While it ran after its turn, Codex streamed its output (`outputDelta` of the backgrounded
    // terminal), reported as appended output of the task.
    let turn_end = ev
        .seen
        .iter()
        .position(|e| matches!(e, AdapterEvent::TurnCompleted { .. }))
        .expect("the terminals' turn ended");
    let streamed: String = ev.seen[turn_end..]
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::BackgroundOutput { key, output } if key == "mock_TERM_1" => {
                match output {
                    OutputUpdate::Append(text) => Some(text.as_str()),
                    OutputUpdate::Replace(text) => {
                        panic!("Codex streams deltas, not snapshots: {text:?}")
                    }
                }
            }
            _ => None,
        })
        .collect();
    assert!(
        streamed.contains("TICK_S 20"),
        "streamed after the turn: {streamed:?}"
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

// ----- the port's features against a scripted model -------------------------------------------

/// An adapter of the installed CLI whose `CODEX_HOME` is `home` (the scripted model's).
fn scripted_adapter(
    home: &std::path::Path,
    supervisor: &Supervisor,
    state: &std::path::Path,
) -> CodexAdapter {
    CodexAdapter::new(
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
    )
}

fn anchor_of(events: &[AdapterEvent]) -> Value {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AdapterEvent::TurnAnchor { anchor } => Some(anchor.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no anchor: {events:#?}"))
}

/// Sends `text` and returns the events up to its turn's end.
async fn run_turn(
    ev: &mut Events<'_>,
    control: &Arc<dyn SessionControl>,
    text: &str,
) -> Vec<AdapterEvent> {
    let from = ev.seen.len();
    control.send(TurnInput::text(text)).await.unwrap();
    let done = ev
        .until(&format!("the turn of {text:?}"), |e| {
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
    ev.seen[from..].to_vec()
}

/// Plan mode, fast mode, rename, `/init`, the inline review, goals, the status, a thread another
/// app-server holds (resume refused, forks at a turn accepted) and anchored history, with the
/// installed app-server and a scripted model.
#[tokio::test]
#[ignore = "runs the installed codex CLI; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_codex_features() {
    if !live_enabled() {
        eprintln!("AAS_LIVE_TESTS not set; skipping");
        return;
    }
    let model = MockModel::start(features_script).await;
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("hello.txt"), "Hello world\n").unwrap();
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
    let adapter = scripted_adapter(&home, &supervisor, &state);

    // The probe learns the fast mode of Codex's bundled GPT models.
    let info = adapter.probe().await;
    assert!(info.available, "{:?}", info.unavailable_reason);
    let features = adapter.features();
    let fast_model = features
        .fast_mode_models
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("no fast mode model: {features:?}"));
    let plan = features.plan_mode.clone().expect("plan mode");
    assert_eq!(
        plan.implement_prompt.as_deref(),
        Some(aas_adapter_codex::IMPLEMENT_PLAN_PROMPT)
    );
    assert!(
        features.fork_at_turn && features.fork_while_held && features.rename && features.status
    );

    // A thread that starts in plan mode and fast mode.
    let settings = ThreadSettings {
        model: Some(fast_model.clone()),
        permission_mode: Some("fullAccess".into()),
        ..Default::default()
    };
    let mut session = adapter
        .start_with(
            StartRequest {
                thread_id: ThreadId::generate(),
                cwd: ws.clone(),
                settings: settings.clone(),
                mode: StartMode::New,
            },
            StartOptions {
                modes: ThreadModes {
                    plan: true,
                    fast: true,
                },
                ..StartOptions::default()
            },
        )
        .await
        .unwrap();
    let native = session.native_session_id.clone().expect("thread id");
    let control = session.control.clone();
    let mut ev = Events {
        session: &mut session,
        seen: Vec::new(),
    };
    ev.until(
        "the fast mode state",
        |e| matches!(e, AdapterEvent::ModesReported { fast_state: Some(s), .. } if s == "Fast"),
    )
    .await;

    // 1. Plan mode: the proposed plan is an item.
    let events = run_turn(
        &mut ev,
        &control,
        "ROLE=PLAN# Plan how to make hello.txt say \"Hello, world\".",
    )
    .await;
    let plan_anchor = anchor_of(&events);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AdapterEvent::ModesReported {
                plan: Some(true),
                ..
            }
        )),
        "{events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(e,
            AdapterEvent::ItemCompleted { body: Some(ItemBody::ProposedPlan { text }), .. }
                if text.trim_end() == PLAN_BODY)),
        "{events:#?}"
    );

    // 2. Implement: plan mode and fast mode off, Codex's own text.
    control
        .apply_modes(&ThreadModes {
            plan: false,
            fast: false,
        })
        .await
        .unwrap();
    let events = run_turn(&mut ev, &control, aas_adapter_codex::IMPLEMENT_PLAN_PROMPT).await;
    let implement_anchor = anchor_of(&events);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AdapterEvent::ModesReported {
                plan: Some(false),
                ..
            }
        )),
        "{events:#?}"
    );

    // 3. Rename: Codex echoes the name.
    control.rename("Live features").await.unwrap();
    ev.until(
        "the rename's echo",
        |e| matches!(e, AdapterEvent::SessionTitle { title } if title == "Live features"),
    )
    .await;

    // 4. `/init` sends Codex's own prompt.
    run_turn(&mut ev, &control, "/init").await;

    // 5. The inline review: the rendered findings, not the reviewer's JSON.
    let events = run_turn(
        &mut ev,
        &control,
        "/review ROLE=REVIEW# Review hello.txt for wording problems.",
    )
    .await;
    let texts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ItemCompleted {
                body: Some(ItemBody::AgentMessage { text }),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("[P2] Greeting is missing a comma")),
        "{events:#?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("\"findings\"")),
        "{texts:#?}"
    );
    assert!(events.iter().any(|e| matches!(e,
        AdapterEvent::ItemCompleted { body: Some(ItemBody::Notice { message, .. }), .. }
            if message == "Review finished")));

    // 6. The status: the thread, the account (no sign-in with this provider), the rate limits
    // (Codex refuses to read them without an OpenAI sign-in).
    let sections = control.status().await.unwrap();
    let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Codex thread", "Account", "Rate limits"],
        "{sections:#?}"
    );
    let harness_sections = adapter.status(&ws).await.unwrap();
    assert_eq!(harness_sections.len(), 2, "{harness_sections:#?}");

    // 7. A goal: its command turn, then Codex's continuation, which the model completes.
    let from = ev.seen.len();
    control
        .send(TurnInput::text("/goal ROLE=GOAL# Finish the goal."))
        .await
        .unwrap();
    ev.until("the goal's completion notice", |e| {
        matches!(e, AdapterEvent::Notice { message, .. }
            if message == "Goal complete: ROLE=GOAL# Finish the goal.")
    })
    .await;
    ev.until("the continuation's end", |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let goal_events = ev.seen[from..].to_vec();
    assert!(goal_events.iter().any(|e| matches!(e,
        AdapterEvent::Notice { message, .. } if message == "Goal active: ROLE=GOAL# Finish the goal.")));
    let sections = control.status().await.unwrap();
    assert!(
        sections
            .iter()
            .any(|s| s.title == "Goal" && s.rows.iter().any(|r| r.value == "complete")),
        "{sections:#?}"
    );

    // 7b. Stopping a continuation of an active goal pauses the goal, as Codex's TUI does: the
    // interrupted turn says so, and Codex reports the goal paused.
    control
        .send(TurnInput::text(
            "/goal ROLE=GOALSTOP# Wait for the command.",
        ))
        .await
        .unwrap();
    ev.until("the waiting continuation's command", |e| {
        matches!(e, AdapterEvent::ItemStarted { body: ItemBody::CommandExecution { command, .. }, .. }
            if command.contains("GOAL_WAITED"))
    })
    .await;
    let from = ev.seen.len();
    control.interrupt().await.unwrap();
    ev.until("the interrupted continuation's end", |e| {
        matches!(
            e,
            AdapterEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        )
    })
    .await;
    assert!(
        ev.seen[from..].iter().any(|e| matches!(e,
            AdapterEvent::Notice { message, .. }
                if message == "Goal paused: ROLE=GOALSTOP# Wait for the command.")),
        "{:#?}",
        &ev.seen[from..]
    );
    let sections = control.status().await.unwrap();
    assert!(
        sections
            .iter()
            .any(|s| s.title == "Goal" && s.rows.iter().any(|r| r.value == "paused")),
        "{sections:#?}"
    );

    // 8. Another app-server cannot resume the thread this one holds, but forks it at a turn.
    let adapter_b = scripted_adapter(&home, &supervisor, &dir.path().join("state-b"));
    assert!(adapter_b.probe().await.available);
    let resumed = adapter_b
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: ws.clone(),
            settings: settings.clone(),
            mode: StartMode::Resume {
                native_session_id: native.clone(),
            },
        })
        .await;
    match resumed {
        Err(error @ AdapterError::Harness(_)) => {
            assert!(
                error.detail().contains("already has an active writer"),
                "{error}"
            );
        }
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("a thread another app-server holds was resumed"),
    }
    for (anchor, before, role) in [
        (&plan_anchor, false, "FORKED"),
        (&implement_anchor, true, "BEFORE"),
    ] {
        let mut fork = adapter_b
            .start_with(
                StartRequest {
                    thread_id: ThreadId::generate(),
                    cwd: ws.clone(),
                    settings: settings.clone(),
                    mode: StartMode::Fork {
                        native_session_id: native.clone(),
                    },
                },
                StartOptions {
                    fork_at: Some(ForkPoint {
                        anchor: anchor.clone(),
                        before,
                        previous: None,
                    }),
                    ..StartOptions::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("fork at {anchor} (before: {before}): {e}"));
        assert_ne!(fork.native_session_id.as_deref(), Some(native.as_str()));
        let fork_control = fork.control.clone();
        let mut fork_ev = Events {
            session: &mut fork,
            seen: Vec::new(),
        };
        run_turn(
            &mut fork_ev,
            &fork_control,
            &format!("ROLE={role}# prompt on the fork"),
        )
        .await;
        fork_control.shutdown(StopReason::Shutdown).await;
        while fork.events.recv().await.is_some() {}
    }

    // 9. The history carries the turns' anchors, the ones the live turns reported.
    let (history, anchors) = adapter
        .read_native_history_anchored(&ws, &native)
        .await
        .unwrap();
    assert_eq!(history.turns.len(), anchors.len());
    assert!(anchors.contains(&Some(plan_anchor.clone())), "{anchors:?}");
    assert!(
        anchors.contains(&Some(implement_anchor.clone())),
        "{anchors:?}"
    );

    assert!(
        ev.seen
            .iter()
            .all(|e| !matches!(e, AdapterEvent::Native { .. })),
        "native events: {:#?}",
        ev.seen
    );
    let sampled = model.sampled.lock().clone();
    eprintln!("scripted model answered: {sampled:#?}");
    let find = |role: &str| {
        sampled
            .iter()
            .find(|s| s.role.as_deref() == Some(role))
            .unwrap_or_else(|| panic!("no {role} request: {sampled:#?}"))
            .clone()
    };
    let planned = find("PLAN");
    assert_eq!(planned.mode.as_deref(), Some("plan"), "{planned:?}");
    assert_eq!(
        planned.service_tier.as_deref(),
        Some("priority"),
        "{planned:?}"
    );
    let implemented = find("IMPL");
    assert_eq!(
        implemented.mode.as_deref(),
        Some("default"),
        "{implemented:?}"
    );
    assert_eq!(implemented.service_tier, None, "{implemented:?}");
    assert_eq!(
        implemented.user_text,
        aas_adapter_codex::IMPLEMENT_PLAN_PROMPT
    );
    assert_eq!(find("INIT").user_text, aas_adapter_codex::INIT_PROMPT);
    // The first turn on each fork states the mode Codex does not carry over.
    assert_eq!(find("FORKED").mode.as_deref(), Some("default"));
    assert_eq!(find("BEFORE").mode.as_deref(), Some("default"));
    assert!(
        sampled.iter().all(|s| !s.reply.contains("MOCK_")),
        "{sampled:#?}"
    );
    assert!(model.errors.lock().is_empty(), "{:?}", model.errors.lock());

    control.shutdown(StopReason::Shutdown).await;
    while session.events.recv().await.is_some() {}
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may outlive the test"
    );
}

/// The texts the adapter sends in Codex's place (`/init`, plan mode's implement texts) appear in
/// the installed Codex binary word for word (its Windows build embeds them with CRLF). Reads the
/// binary of the npm package next to the resolved `codex` launcher; no process runs.
#[test]
#[ignore = "reads the installed codex CLI; set AAS_LIVE_TESTS=1 and pass --ignored"]
fn live_codex_texts_match_the_installed_binary() {
    if !live_enabled() {
        eprintln!("AAS_LIVE_TESTS not set; skipping");
        return;
    }
    let launcher = aas_supervisor::resolve_program("codex").expect("codex on PATH");
    // The npm package layout (test-only knowledge; the adapter never looks inside the package).
    let binary = launcher
        .parent()
        .expect("launcher directory")
        .join("node_modules")
        .join("@openai")
        .join("codex")
        .join("node_modules")
        .join("@openai")
        .join("codex-win32-x64")
        .join("vendor")
        .join("x86_64-pc-windows-msvc")
        .join("bin")
        .join("codex.exe");
    let bytes = std::fs::read(&binary).unwrap_or_else(|e| panic!("{}: {e}", binary.display()));
    let find = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    for (name, text) in aas_adapter_codex::testing::CODEX_TEXTS {
        let crlf = text.replace('\n', "\r\n");
        assert!(
            find(text.as_bytes()) || find(crlf.as_bytes()),
            "the {name} of codex-cli {} is not in {}",
            aas_adapter_codex::CODEX_TEXTS_VERSION,
            binary.display()
        );
    }
}
