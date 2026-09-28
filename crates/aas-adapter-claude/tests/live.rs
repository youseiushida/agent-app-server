//! Live tests against the real `claude` CLI. They spend tokens, so they only run with
//! `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-claude -- --ignored`.

use std::time::Duration;

use aas_adapter_claude::ClaudeAdapter;
use aas_harness::protocol::{
    HarnessKind, InteractionRequest, InteractionResolution, ItemBody, ItemStatus, ThreadSettings,
    TurnStatus,
};
use aas_harness::{
    AdapterContext, AdapterEvent, AdapterPolicy, CommandContext, HarnessAdapter, HarnessConfig,
    StartMode, StartRequest, StopReason, ThreadId, TurnInput,
};
use aas_supervisor::{Supervisor, SupervisorPolicy};
use tokio::sync::mpsc::UnboundedReceiver;

/// Removes the transcripts a live test created (and their project folder when empty),
/// so repeated runs do not clutter the user's Claude Code history.
fn remove_transcripts(session_ids: &[String]) {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
        .map(|d| d.join("projects"));
    let Some(root) = root else { return };
    let Ok(dirs) = std::fs::read_dir(&root) else {
        return;
    };
    for dir in dirs.flatten() {
        let mut removed = false;
        for id in session_ids {
            let file = dir.path().join(format!("{id}.jsonl"));
            if file.is_file() {
                removed |= std::fs::remove_file(&file).is_ok();
                let _ = std::fs::remove_dir_all(dir.path().join(id));
            }
        }
        // Only a folder this test emptied is removed.
        if removed && std::fs::read_dir(dir.path()).is_ok_and(|mut d| d.next().is_none()) {
            let _ = std::fs::remove_dir(dir.path());
        }
    }
}

fn live() -> bool {
    std::env::var("AAS_LIVE_TESTS").is_ok_and(|v| v == "1")
}

fn adapter(state: &std::path::Path) -> (ClaudeAdapter, Supervisor) {
    let supervisor = Supervisor::new(
        &state.join("supervisor"),
        SupervisorPolicy {
            prevent_sleep: false,
            ..Default::default()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: state.join("claude"),
        policy: AdapterPolicy::default(),
    };
    let config = HarnessConfig {
        id: "claude".into(),
        kind: HarnessKind::Claude,
        display_name: None,
        command: "claude".into(),
        args: Vec::new(),
        env: Default::default(),
        options: serde_json::Value::Null,
    };
    (ClaudeAdapter::new(config, ctx), supervisor)
}

async fn until_turn_end(
    events: &mut UnboundedReceiver<AdapterEvent>,
    mut on: impl FnMut(&AdapterEvent),
) -> Vec<AdapterEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(180), events.recv())
            .await
            .expect("turn timed out")
            .expect("stream closed");
        on(&ev);
        let end = matches!(ev, AdapterEvent::TurnCompleted { .. });
        out.push(ev);
        if end {
            return out;
        }
    }
}

#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_turn_approval_history_and_clean_exit() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());

    let info = adapter.probe().await;
    assert!(info.available, "{:?}", info.unavailable_reason);
    assert!(
        info.models.iter().any(|m| m.id == "haiku"),
        "{:?}",
        info.models
    );
    assert!(!info.permission_modes.is_empty());

    let settings = ThreadSettings {
        model: Some("haiku".into()),
        effort: None,
        permission_mode: Some("default".into()),
    };
    let mut handle = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.path().to_path_buf(),
            settings,
            mode: StartMode::New,
        })
        .await
        .unwrap();
    let native = handle.native_session_id.clone().unwrap();

    handle
        .control
        .send(TurnInput::text("Reply with exactly: OK"))
        .await
        .unwrap();
    let events = until_turn_end(&mut handle.events, |_| {}).await;
    assert!(matches!(
        events.last(),
        Some(AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        })
    ));
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::ItemCompleted { body: Some(ItemBody::AgentMessage { text }), .. } if text.contains("OK"))));
    // The context occupancy Claude Code reports through `get_context_usage`.
    match events.last() {
        Some(AdapterEvent::TurnCompleted {
            usage: Some(usage), ..
        }) => {
            let context = usage.context.expect("context reported");
            assert!(
                context.used_tokens > 0 && context.window_tokens >= context.used_tokens,
                "{context:?}"
            );
        }
        other => panic!("{other:?}"),
    }

    handle
        .control
        .send(TurnInput::text(
            "Use the Bash tool to run `mkdir live-dir`. If it is denied, reply with just: denied",
        ))
        .await
        .unwrap();
    let mut pending = None;
    let events = {
        let control = handle.control.clone();
        let mut out = Vec::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(180), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AdapterEvent::InteractionRequested {
                request_id,
                request: InteractionRequest::Approval { .. },
                ..
            } = &ev
            {
                pending = Some(request_id.clone());
                control
                    .respond(
                        request_id,
                        &InteractionResolution::Approval {
                            option_id: "deny".into(),
                            feedback: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            let end = matches!(ev, AdapterEvent::TurnCompleted { .. });
            out.push(ev);
            if end {
                break out;
            }
        }
    };
    assert!(
        pending.is_some(),
        "mkdir should ask for approval in default mode"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted {
            status: ItemStatus::Declined,
            ..
        }
    )));
    assert!(!work.path().join("live-dir").exists());

    let commands = adapter
        .commands(CommandContext {
            cwd: work.path().to_path_buf(),
            native_session_id: Some(native.clone()),
        })
        .await
        .unwrap();
    assert!(!commands.is_empty());

    let exit = handle.control.shutdown(StopReason::User).await;
    assert!(
        exit.stopped.is_none(),
        "claude should exit by itself once stdin closes: {exit:?}"
    );
    let mut saw_exit = false;
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(30), handle.events.recv()).await
    {
        saw_exit |= matches!(ev, AdapterEvent::Exited { .. });
    }
    assert!(saw_exit);
    assert_eq!(
        supervisor.running_count(),
        0,
        "no supervised process may remain"
    );

    let sessions = adapter.list_native_sessions(work.path()).await.unwrap();
    assert!(
        sessions.iter().any(|s| s.native_session_id == native),
        "{sessions:?}"
    );
    let history = adapter
        .read_native_history(work.path(), &native)
        .await
        .unwrap();
    assert_eq!(history.turns.len(), 2);

    // Resume continues the same native session.
    let mut resumed = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.path().to_path_buf(),
            settings: ThreadSettings {
                model: Some("haiku".into()),
                ..Default::default()
            },
            mode: StartMode::Resume {
                native_session_id: native.clone(),
            },
        })
        .await
        .unwrap();
    resumed
        .control
        .send(TurnInput::text(
            "What was the single word you replied with first? Reply with that word only.",
        ))
        .await
        .unwrap();
    let events = until_turn_end(&mut resumed.events, |_| {}).await;
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::ItemCompleted { body: Some(ItemBody::AgentMessage { text }), .. } if text.contains("OK"))));
    resumed.control.shutdown(StopReason::User).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(supervisor.running_count(), 0);
    remove_transcripts(&[native]);
}

#[tokio::test]
#[ignore = "spends tokens; set AAS_LIVE_TESTS=1 and pass --ignored"]
async fn live_interrupt_and_shutdown_mid_turn() {
    if !live() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (adapter, supervisor) = adapter(state.path());
    let mut handle = adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: work.path().to_path_buf(),
            settings: ThreadSettings {
                model: Some("haiku".into()),
                ..Default::default()
            },
            mode: StartMode::New,
        })
        .await
        .unwrap();
    let native = handle.native_session_id.clone().unwrap();
    handle
        .control
        .send(TurnInput::text(
            "Write a 300-line poem about rivers. Do not use tools.",
        ))
        .await
        .unwrap();
    // Interrupt as soon as text streams.
    let control = handle.control.clone();
    let mut interrupted = false;
    let events = until_turn_end(&mut handle.events, |ev| {
        if !interrupted && matches!(ev, AdapterEvent::ItemDelta { .. }) {
            interrupted = true;
            let c = control.clone();
            tokio::spawn(async move { c.interrupt().await });
        }
    })
    .await;
    assert!(
        matches!(
            events.last(),
            Some(AdapterEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            })
        ),
        "{:?}",
        events.last()
    );

    // Shutdown in the middle of a new turn: the tree is gone afterwards.
    handle
        .control
        .send(TurnInput::text("Count from 1 to 500, one per line."))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    handle.control.shutdown(StopReason::User).await;
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(30), handle.events.recv()).await
    {
        if matches!(ev, AdapterEvent::Exited { .. }) {
            break;
        }
    }
    assert_eq!(supervisor.running_count(), 0);
    remove_transcripts(&[native]);
}
