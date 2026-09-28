//! Live tests against the installed `pi` (spend a few tokens).
//!
//! ```text
//! AAS_LIVE_TESTS=1 cargo test -p aas-adapter-pi --test live -- --ignored --nocapture
//! ```
//!
//! Sessions go to a temporary `session_dir`, so the user's own pi session list is untouched.

use std::path::Path;
use std::time::Duration;

use aas_adapter_pi::PiAdapter;
use aas_harness::{
    AdapterContext, AdapterError, AdapterEvent, AdapterPolicy, CommandContext, HarnessAdapter,
    HarnessConfig, HarnessKind, InteractionRequest, InteractionResolution, ItemBody, SessionHandle,
    StartMode, StartRequest, StopReason, ThreadId, ThreadSettings, TurnInput, TurnStatus,
};
use aas_supervisor::{Supervisor, SupervisorPolicy};
use serde_json::json;

fn live() -> bool {
    std::env::var_os("AAS_LIVE_TESTS").is_some()
}

async fn next_event(h: &mut SessionHandle) -> AdapterEvent {
    tokio::time::timeout(Duration::from_secs(180), h.events.recv())
        .await
        .expect("event within 3 minutes")
        .expect("event")
}

/// Runs one turn, approving (or not) every interaction, and returns its events.
async fn run_turn(h: &mut SessionHandle, text: &str, allow: bool) -> Vec<AdapterEvent> {
    h.control.send(TurnInput::text(text)).await.unwrap();
    rest_of_turn(h, allow).await
}

/// Collects the events of the running turn up to its `TurnCompleted`, approving (or not)
/// every interaction.
async fn rest_of_turn(h: &mut SessionHandle, allow: bool) -> Vec<AdapterEvent> {
    let mut events = Vec::new();
    loop {
        let ev = next_event(h).await;
        if let AdapterEvent::InteractionRequested { request_id, .. } = &ev {
            let option = if allow { "allow" } else { "deny" };
            h.control
                .respond(
                    request_id,
                    &InteractionResolution::Approval {
                        option_id: option.into(),
                        feedback: None,
                    },
                )
                .await
                .unwrap();
        }
        let done = matches!(ev, AdapterEvent::TurnCompleted { .. });
        events.push(ev);
        if done {
            return events;
        }
    }
}

fn agent_text(events: &[AdapterEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ItemCompleted {
                body: Some(ItemBody::AgentMessage { text }),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
#[ignore = "runs the real pi CLI; set AAS_LIVE_TESTS=1"]
async fn live_session_lifecycle() {
    if !live() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let supervisor = Supervisor::new(
        &root.path().join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: root.path().join("pi-state"),
        policy: AdapterPolicy::default(),
    };
    let config = HarnessConfig {
        id: "pi".into(),
        kind: HarnessKind::Pi,
        display_name: None,
        command: "pi".into(),
        args: vec![],
        env: Default::default(),
        options: json!({ "session_dir": root.path().join("sessions") }),
    };
    let adapter = PiAdapter::new(config, ctx);

    let info = adapter.probe().await;
    assert!(info.available, "{:?}", info.unavailable_reason);
    println!(
        "pi {:?}, {} models, default {:?}",
        info.version,
        info.models.len(),
        info.default_model
    );
    // Cheapest model available here; fall back to the default.
    let model = info
        .models
        .iter()
        .find(|m| m.id.contains("flash"))
        .map(|m| m.id.clone())
        .or(info.default_model.clone());

    let settings = ThreadSettings {
        model: model.clone(),
        effort: Some("low".into()),
        permission_mode: Some("ask".into()),
    };
    let mut h = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: cwd.clone(),
            settings: settings.clone(),
            mode: StartMode::New,
        })
        .await
        .expect("start");
    let native_id = h.native_session_id.clone().unwrap();

    // 1. plain turn
    let events = run_turn(&mut h, "Reply with exactly: OK", true).await;
    assert!(
        matches!(
            events.last().unwrap(),
            AdapterEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ),
        "{events:#?}"
    );
    assert!(agent_text(&events).contains("OK"), "{events:#?}");
    // The context occupancy comes from `get_session_stats`.
    match events.last().unwrap() {
        AdapterEvent::TurnCompleted {
            usage: Some(usage), ..
        } => {
            let context = usage.context.expect("context reported");
            assert!(
                context.used_tokens > 0 && context.window_tokens >= context.used_tokens,
                "{context:?}"
            );
        }
        other => panic!("{other:?}"),
    }

    // 2. gated shell command, approved
    let events = run_turn(&mut h, "Use your bash tool to run exactly `echo hello-aas` and then reply with the single word DONE.", true).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::InteractionRequested { .. })),
        "gate did not ask: {events:#?}"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted { body: Some(ItemBody::CommandExecution { output, .. }), .. } if output.contains("hello-aas")
    )), "{events:#?}");

    // 3. switch to auto live: no approval requested
    h.control
        .apply_settings(&ThreadSettings {
            permission_mode: Some("auto".into()),
            ..settings.clone()
        })
        .await
        .unwrap();
    let events = run_turn(&mut h, "Use your bash tool to run exactly `echo auto-aas` and then reply with the single word DONE.", false).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AdapterEvent::InteractionRequested { .. })),
        "{events:#?}"
    );

    let commands = adapter
        .commands(CommandContext {
            cwd: cwd.clone(),
            native_session_id: Some(native_id.clone()),
        })
        .await
        .unwrap();
    println!("{} commands", commands.len());
    assert!(
        commands.iter().any(|c| c.name == "compact"),
        "the RPC compaction is offered as /compact"
    );

    let exit = h.control.shutdown(StopReason::User).await;
    println!("exit: {exit:?}");
    loop {
        if let AdapterEvent::Exited { .. } = next_event(&mut h).await {
            break;
        }
    }
    assert!(h.events.recv().await.is_none());

    // Native sessions and history
    let sessions = adapter.list_native_sessions(&cwd).await.unwrap();
    assert!(
        sessions.iter().any(|s| s.native_session_id == native_id),
        "{sessions:#?}"
    );
    let history = adapter.read_native_history(&cwd, &native_id).await.unwrap();
    assert!(history.turns.len() >= 3, "{history:#?}");

    // Resume and fork
    let resumed = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: cwd.clone(),
            settings: settings.clone(),
            mode: StartMode::Resume {
                native_session_id: native_id.clone(),
            },
        })
        .await
        .expect("resume");
    assert_eq!(
        resumed.native_session_id.as_deref(),
        Some(native_id.as_str())
    );
    resumed.control.shutdown(StopReason::User).await;

    let forked = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: cwd.clone(),
            settings,
            mode: StartMode::Fork {
                native_session_id: native_id.clone(),
            },
        })
        .await
        .expect("fork");
    let fork_id = forked.native_session_id.clone().unwrap();
    assert_ne!(fork_id, native_id);
    forked.control.shutdown(StopReason::User).await;
    let history = adapter.read_native_history(&cwd, &fork_id).await.unwrap();
    assert!(
        history.turns.len() >= 3,
        "fork keeps the history: {history:#?}"
    );

    // Nothing left running.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(supervisor.running_count(), 0);
}

fn notices(events: &[AdapterEvent], code: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Notice {
                message,
                code: Some(c),
                ..
            } if c == code => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn status(events: &[AdapterEvent]) -> TurnStatus {
    match events.last() {
        Some(AdapterEvent::TurnCompleted { status, .. }) => *status,
        other => panic!("not a completed turn: {other:?}"),
    }
}

/// The turn of an extension command that returns at once (it schedules what it starts).
async fn command_turn(h: &mut SessionHandle, command: &str) {
    // An extension command: `send` does not wait for pi's answer.
    h.control.send(TurnInput::text(command)).await.unwrap();
    let events = rest_of_turn(h, true).await;
    assert_eq!(status(&events), TurnStatus::Completed, "{events:#?}");
}

/// Waits for the `TurnStarted` of a run pi starts by itself.
async fn agent_turn_starts(h: &mut SessionHandle) {
    let ev = next_event(h).await;
    assert_eq!(ev, AdapterEvent::TurnStarted);
}

/// Runs pi starts by itself (the extension `tests/extension/aas-live.ts` makes pi start them),
/// the race of a prompt with such a run, a run started from a run's end, an abort, the gate
/// inside such a run, and a dialog outside any turn.
#[tokio::test]
#[ignore = "runs the real pi CLI; set AAS_LIVE_TESTS=1"]
async fn live_runs_pi_starts_by_itself() {
    if !live() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let supervisor = Supervisor::new(
        &root.path().join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: root.path().join("pi-state"),
        policy: AdapterPolicy::default(),
    };
    let extension = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/extension/aas-live.ts");
    let config = HarnessConfig {
        id: "pi".into(),
        kind: HarnessKind::Pi,
        display_name: None,
        command: "pi".into(),
        args: vec!["-e".into(), extension.to_string_lossy().into_owned()],
        env: Default::default(),
        options: json!({ "session_dir": root.path().join("sessions") }),
    };
    let adapter = PiAdapter::new(config, ctx);
    let info = adapter.probe().await;
    assert!(info.available, "{:?}", info.unavailable_reason);
    let model = info
        .models
        .iter()
        .find(|m| m.id.contains("flash"))
        .map(|m| m.id.clone())
        .or(info.default_model.clone());
    let mut h = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: cwd.clone(),
            settings: ThreadSettings {
                model,
                effort: Some("low".into()),
                permission_mode: Some("ask".into()),
            },
            mode: StartMode::New,
        })
        .await
        .expect("start");

    // 1. A run pi starts by itself after the command's turn: a turn without input.
    command_turn(&mut h, "/aas-later 1500").await;
    agent_turn_starts(&mut h).await;
    // While it goes on, the user's input waits (nothing is written to pi).
    assert_eq!(
        h.control
            .send(TurnInput::text("Reply with exactly: LATER"))
            .await,
        Err(AdapterError::TurnInProgress)
    );
    let events = rest_of_turn(&mut h, true).await;
    println!("run of pi's own: {}", agent_text(&events));
    assert_eq!(status(&events), TurnStatus::Completed, "{events:#?}");
    assert_eq!(
        notices(&events, "extensionMessage"),
        ["Reply with exactly: WOKE"]
    );
    assert!(agent_text(&events).contains("WOKE"), "{events:#?}");
    // Afterwards the input goes through.
    let events = run_turn(&mut h, "Reply with exactly: LATER", true).await;
    assert!(agent_text(&events).contains("LATER"), "{events:#?}");

    // 2. The race: the extension starts a run while pi checks the prompt; pi refuses the
    // prompt, the run is a turn of its own and the input waits for it.
    assert_eq!(
        h.control
            .send(TurnInput::text("aas-race Reply with exactly: MINE"))
            .await,
        Err(AdapterError::TurnInProgress)
    );
    agent_turn_starts(&mut h).await;
    let events = rest_of_turn(&mut h, true).await;
    assert_eq!(status(&events), TurnStatus::Completed, "{events:#?}");
    assert!(agent_text(&events).contains("RACED"), "{events:#?}");

    // 3. A run started from the end of a run gets a turn of its own.
    command_turn(&mut h, "/aas-chain").await;
    let first = run_turn(&mut h, "Reply with exactly: FIRST", true).await;
    assert!(agent_text(&first).contains("FIRST"), "{first:#?}");
    agent_turn_starts(&mut h).await;
    let chained = rest_of_turn(&mut h, true).await;
    assert!(agent_text(&chained).contains("CHAINED"), "{chained:#?}");

    // 4. A user message an extension sends opens its run as a notice.
    command_turn(&mut h, "/aas-later-user 300").await;
    agent_turn_starts(&mut h).await;
    let events = rest_of_turn(&mut h, true).await;
    assert_eq!(
        notices(&events, "extensionPrompt"),
        ["Reply with exactly: WOKE-USER"]
    );

    // 5. Interrupting a run of pi's own aborts it.
    command_turn(
        &mut h,
        "/aas-later 300 Count from 1 to 300, one number per line, nothing else.",
    )
    .await;
    agent_turn_starts(&mut h).await;
    let mut events = Vec::new();
    loop {
        let ev = next_event(&mut h).await;
        let streaming = matches!(ev, AdapterEvent::ItemStarted { .. });
        let done = matches!(ev, AdapterEvent::TurnCompleted { .. });
        events.push(ev);
        if streaming || done {
            break;
        }
    }
    h.control.interrupt().await.unwrap();
    events.extend(rest_of_turn(&mut h, true).await);
    assert_eq!(status(&events), TurnStatus::Interrupted, "{events:#?}");

    // 6. The gate asks inside a run of pi's own.
    command_turn(
        &mut h,
        "/aas-later 300 Use your bash tool to run exactly `echo agent-aas` and then reply with the single word DONE.",
    )
    .await;
    agent_turn_starts(&mut h).await;
    let events = rest_of_turn(&mut h, true).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::InteractionRequested { .. })),
        "the gate did not ask: {events:#?}"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted { body: Some(ItemBody::CommandExecution { output, .. }), .. } if output.contains("agent-aas")
    )), "{events:#?}");

    // 7. A dialog outside any turn: relayed, and pi gets the answer.
    command_turn(&mut h, "/aas-ask-later 300").await;
    let request_id = match next_event(&mut h).await {
        AdapterEvent::InteractionRequested {
            request_id,
            request: InteractionRequest::Approval { title, .. },
            background_key: None,
            ..
        } => {
            assert_eq!(title, "aas-live");
            request_id
        }
        other => panic!("{other:?}"),
    };
    h.control
        .respond(&request_id, &InteractionResolution::Dismissed)
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut h).await,
        AdapterEvent::Notice {
            level: aas_harness::NoticeLevel::Info,
            message: "aas-live answered: false".into(),
            code: Some("extensionNotify".into()),
        }
    );

    let exit = h.control.shutdown(StopReason::User).await;
    println!("exit: {exit:?}");
    loop {
        if let AdapterEvent::Exited { .. } = next_event(&mut h).await {
            break;
        }
    }
    assert!(h.events.recv().await.is_none());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(supervisor.running_count(), 0);
}
