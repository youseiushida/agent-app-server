//! Live test against the installed `codex` CLI (spends one small turn).
//!
//! Run with: `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-codex --test live -- --ignored`

use std::time::Duration;

use aas_adapter_codex::CodexAdapter;
use aas_harness::protocol::{HarnessKind, ItemBody};
use aas_harness::{
    AdapterContext, AdapterEvent, AdapterPolicy, CommandContext, HarnessAdapter, HarnessConfig,
    StartMode, StartRequest, ThreadId, ThreadSettings, TurnInput, TurnStatus,
};
use aas_supervisor::{StopReason, Supervisor, SupervisorPolicy};
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
