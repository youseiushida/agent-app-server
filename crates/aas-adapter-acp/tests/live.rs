//! Live tests against a real ACP agent (Devin CLI by default). They spend tokens, so they
//! run only with `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-acp -- --ignored`.
//!
//! Environment:
//! * `AAS_ACP_COMMAND` / `AAS_ACP_ARGS` (space separated): agent command, default `devin acp`.
//! * `AAS_ACP_MODEL`: model id to select, default `swe-1-7-lightning-medium` (Devin's default
//!   model may be rate limited on free plans).

mod common;

use std::time::Duration;

use aas_adapter_acp::AcpAdapter;
use aas_harness::protocol::{
    HarnessKind, InteractionRequest, InteractionResolution, ItemBody, Subject, ThreadId, TurnStatus,
};
use aas_harness::{
    AdapterContext, AdapterEvent, AdapterPolicy, CommandContext, HarnessAdapter, HarnessConfig,
    StartMode, StartRequest, StopReason, ThreadSettings, TurnInput,
};
use aas_supervisor::{Supervisor, SupervisorPolicy};
use common::*;

fn live() -> bool {
    let on = std::env::var_os("AAS_LIVE_TESTS").is_some();
    if !on {
        eprintln!("skipped: set AAS_LIVE_TESTS=1 to run live tests");
    }
    on
}

struct Env {
    adapter: AcpAdapter,
    supervisor: Supervisor,
    work: tempfile::TempDir,
    _state: tempfile::TempDir,
    model: Option<String>,
}

fn env() -> Env {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let supervisor = Supervisor::new(
        state.path(),
        SupervisorPolicy {
            prevent_sleep: false,
            ..SupervisorPolicy::default()
        },
    )
    .unwrap();
    let ctx = AdapterContext {
        supervisor: supervisor.clone(),
        state_dir: state.path().join("adapter"),
        policy: AdapterPolicy {
            stop_grace: Duration::from_secs(5),
            ..AdapterPolicy::default()
        },
    };
    let command = std::env::var("AAS_ACP_COMMAND").unwrap_or_else(|_| "devin".into());
    let args = std::env::var("AAS_ACP_ARGS").unwrap_or_else(|_| "acp".into());
    let config = HarnessConfig {
        id: "devin".into(),
        kind: HarnessKind::Acp,
        display_name: Some("Devin".into()),
        command,
        args: args.split_whitespace().map(str::to_owned).collect(),
        env: Default::default(),
        options: serde_json::json!({ "auth_hint": "Run `devin auth login`." }),
    };
    let model = match std::env::var("AAS_ACP_MODEL") {
        Ok(m) if m.is_empty() => None,
        Ok(m) => Some(m),
        Err(_) => Some("swe-1-7-lightning-medium".into()),
    };
    Env {
        adapter: AcpAdapter::new(config, ctx),
        supervisor,
        work,
        _state: state,
        model,
    }
}

async fn run_turn(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AdapterEvent>,
    f: &mut Folded,
    mut on: impl FnMut(&AdapterEvent),
) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(240), rx.recv())
            .await
            .expect("turn timed out")
            .expect("events closed");
        on(&ev);
        f.apply(ev.clone());
        if is_turn_completed(&ev) {
            return;
        }
    }
}

#[tokio::test]
#[ignore]
async fn live_turn_list_history_and_cache() {
    if !live() {
        return;
    }
    let env = env();
    let info = env.adapter.probe().await;
    assert!(info.available, "unavailable: {:?}", info.unavailable_reason);
    assert!(info.capabilities.approvals && info.capabilities.interrupt);

    let handle = env
        .adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: env.work.path().to_path_buf(),
            settings: ThreadSettings {
                model: env.model.clone(),
                ..ThreadSettings::default()
            },
            mode: StartMode::New,
        })
        .await
        .expect("start");
    let session_id = handle.native_session_id.clone().expect("session id");
    let mut rx = handle.events;
    let mut f = Folded::default();
    handle
        .control
        .send(TurnInput::text("Reply with exactly: OK"))
        .await
        .unwrap();
    run_turn(&mut rx, &mut f, |_| {}).await;
    let (status, usage, error) = &f.turns[0];
    assert_eq!(*status, TurnStatus::Completed, "{error:?}");
    assert!(usage.is_some());
    // Devin reports the context occupancy with `usage_update`.
    let context = usage.and_then(|u| u.context).expect("context reported");
    assert!(
        context.used_tokens > 0 && context.window_tokens >= context.used_tokens,
        "{context:?}"
    );
    assert!(
        f.items
            .iter()
            .any(|(_, b, _)| matches!(b, ItemBody::AgentMessage { text } if text.contains("OK")))
    );
    handle.control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;

    // The session taught the cache the agent's options and commands.
    let info = env.adapter.probe().await;
    assert!(
        !info.models.is_empty(),
        "models are learned from the first session"
    );
    assert!(!info.permission_modes.is_empty());
    let commands = env
        .adapter
        .commands(CommandContext {
            cwd: env.work.path().to_path_buf(),
            native_session_id: None,
        })
        .await
        .unwrap();
    assert!(!commands.is_empty());

    let sessions = env
        .adapter
        .list_native_sessions(env.work.path())
        .await
        .unwrap();
    assert!(
        sessions.iter().any(|s| s.native_session_id == session_id),
        "{sessions:?}"
    );
    let history = env
        .adapter
        .read_native_history(env.work.path(), &session_id)
        .await
        .unwrap();
    assert_eq!(history.turns.len(), 1);
    assert!(
        matches!(&history.turns[0].items[0].body, ItemBody::UserMessage { text, .. } if text == "Reply with exactly: OK")
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        env.supervisor.running_count(),
        0,
        "no agent process may remain"
    );
}

#[tokio::test]
#[ignore]
async fn live_permission_request_can_be_rejected() {
    if !live() {
        return;
    }
    let env = env();
    let handle = env
        .adapter
        .start(StartRequest {
            thread_id: ThreadId::generate(),
            cwd: env.work.path().to_path_buf(),
            settings: ThreadSettings {
                model: env.model.clone(),
                ..ThreadSettings::default()
            },
            mode: StartMode::New,
        })
        .await
        .expect("start");
    let control = handle.control.clone();
    let mut rx = handle.events;
    let mut f = Folded::default();
    control
        .send(TurnInput::text(
            "Run the shell command `ping -n 2 127.0.0.1` exactly once. If you are not allowed to run it, reply with the single word DENIED and do nothing else.",
        ))
        .await
        .unwrap();
    // The turn blocks on the request, so answer it (dismiss = the agent's reject option).
    let mut requests = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(240), rx.recv())
            .await
            .expect("turn timed out")
            .expect("events closed");
        f.apply(ev.clone());
        match &ev {
            AdapterEvent::InteractionRequested {
                request_id,
                request,
                ..
            } => {
                requests.push(request.clone());
                control
                    .respond(request_id, &InteractionResolution::Dismissed)
                    .await
                    .unwrap();
            }
            AdapterEvent::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    assert!(
        !requests.is_empty(),
        "the agent ran ping without asking (check the agent's permission mode)"
    );
    for request in &requests {
        assert!(
            matches!(
                request,
                InteractionRequest::Approval {
                    subject: Subject::Command { .. },
                    ..
                }
            ),
            "{request:?}"
        );
    }
    assert_eq!(f.turns[0].0, TurnStatus::Completed, "{:?}", f.turns[0].2);
    control.shutdown(StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.supervisor.running_count(), 0);
}
