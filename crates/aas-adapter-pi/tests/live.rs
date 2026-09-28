//! Live tests against the installed `pi` (spend a few tokens).
//!
//! ```text
//! AAS_LIVE_TESTS=1 cargo test -p aas-adapter-pi --test live -- --ignored --nocapture
//! ```
//!
//! Sessions go to a temporary `session_dir`, so the user's own pi session list is untouched.

use std::time::Duration;

use aas_adapter_pi::PiAdapter;
use aas_harness::{
    AdapterContext, AdapterEvent, AdapterPolicy, CommandContext, HarnessAdapter, HarnessConfig,
    HarnessKind, InteractionResolution, ItemBody, SessionHandle, StartMode, StartRequest,
    StopReason, ThreadId, ThreadSettings, TurnInput, TurnStatus,
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
