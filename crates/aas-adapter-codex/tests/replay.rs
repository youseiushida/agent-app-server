//! Transcript replay tests (recorded from codex-cli 0.148.0; see tests/fixtures/README.md).

mod support;

use aas_harness::protocol::{
    ApprovalOptionKind, FileChangeKind, InteractionRequest, InteractionResolution, ItemBody,
    ItemStatus, NoticeLevel, Subject,
};
use aas_harness::{AdapterError, AdapterEvent, StartMode, ThreadSettings, TurnInput, TurnStatus};
use aas_supervisor::StopReason;
use pretty_assertions::assert_eq;
use support::Replay;

const THREAD: &str = "01a0e0fb-454f-7e80-9b68-b91a3e491330";

async fn start(script: &str) -> Replay {
    support::start(script, StartMode::New, ThreadSettings::default()).await
}

fn is_turn_completed(e: &AdapterEvent) -> bool {
    matches!(e, AdapterEvent::TurnCompleted { .. })
}

fn turn_completed(
    e: AdapterEvent,
) -> (
    TurnStatus,
    Option<aas_harness::Usage>,
    Option<aas_harness::TurnError>,
) {
    match e {
        AdapterEvent::TurnCompleted {
            status,
            usage,
            error,
        } => (status, usage, error),
        other => panic!("expected TurnCompleted, got {other:?}"),
    }
}

async fn shutdown_cleanly(mut r: Replay) {
    let info = r.handle.control.shutdown(StopReason::Shutdown).await;
    assert_eq!(
        info.code,
        Some(0),
        "fake app-server exits by itself on stdin EOF"
    );
    assert_eq!(info.stopped, None);
    // A second shutdown is idempotent.
    assert_eq!(r.handle.control.shutdown(StopReason::User).await, info);
    let exited = r.until_closed().await;
    assert_eq!(exited, info);
    assert!(
        r.natives().is_empty(),
        "unexpected native events: {:?}",
        r.natives()
    );
    r.finish().await.expect("script fully replayed");
}

#[tokio::test]
async fn basic_turn_streams_and_completes() {
    let mut r = start("main_basic_turn.jsonl").await;
    assert_eq!(r.handle.native_session_id.as_deref(), Some(THREAD));
    match r.next().await.unwrap() {
        AdapterEvent::SessionInfo { model, .. } => assert!(model.is_some()),
        other => panic!("first event must be SessionInfo, got {other:?}"),
    }
    r.handle
        .control
        .send(TurnInput::text("Reply with exactly: OK"))
        .await
        .unwrap();
    let (status, usage, error) = turn_completed(r.until(is_turn_completed).await);
    assert_eq!(status, TurnStatus::Completed);
    assert_eq!(error, None);
    let usage = usage.expect("usage reported");
    assert_eq!(
        (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens
        ),
        (12426, 11392, 2)
    );
    // The recorded `thread/tokenUsage/updated` carries `last.totalTokens` and `modelContextWindow`.
    let context = aas_harness::ContextUsage {
        used_tokens: 12428,
        window_tokens: 996147,
    };
    assert_eq!(usage.context, Some(context));
    assert!(
        r.events.iter().any(
            |e| matches!(e, AdapterEvent::TurnUsage { usage } if usage.context == Some(context))
        )
    );

    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnStarted))
    );
    let key = "431ab09d-d177-4070-a0f4-c7a30e666033";
    assert_eq!(r.deltas_of(key), "OK");
    assert!(r.events.iter().any(|e| matches!(e,
        AdapterEvent::ItemCompleted { key: k, body: Some(ItemBody::AgentMessage { text }), status: ItemStatus::Completed }
            if k == key && text == "OK")));
    // The engine owns user messages: Codex's echo is not forwarded.
    assert!(!r.events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemStarted {
            body: ItemBody::UserMessage { .. },
            ..
        }
    )));
    // Codex repeats identical plugin warnings; each distinct message is surfaced once.
    let warnings: Vec<&AdapterEvent> = r
        .events
        .iter()
        .filter(|e| matches!(e, AdapterEvent::Notice { code: Some(c), level: NoticeLevel::Warning, .. } if c == "warning"))
        .collect();
    assert_eq!(warnings.len(), 4);
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn command_approval_accept() {
    let mut r = start("main_approval_accept.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text(
            "Run the shell command `whoami` exactly once",
        ))
        .await
        .unwrap();
    let (request_id, request, item_key) = match r
        .until(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        .await
    {
        AdapterEvent::InteractionRequested {
            request_id,
            request,
            item_key,
        } => (request_id, request, item_key),
        _ => unreachable!(),
    };
    let item = "call_00_kHr5tx6XSBHb8cDsQrj18877";
    assert_eq!(item_key.as_deref(), Some(item));
    let InteractionRequest::Approval {
        options, subject, ..
    } = request
    else {
        panic!("approval expected")
    };
    let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "accept",
            "acceptWithExecpolicyAmendment",
            "decline",
            "cancel"
        ]
    );
    assert_eq!(options[1].kind, ApprovalOptionKind::AllowAlways);
    assert!(
        matches!(subject, Subject::Command { command, .. } if command.ends_with("-Command whoami"))
    );

    r.handle
        .control
        .respond(
            &request_id,
            &InteractionResolution::Approval {
                option_id: "accept".into(),
                feedback: None,
            },
        )
        .await
        .unwrap();
    // Answering twice is rejected: the request is no longer pending.
    let again = r
        .handle
        .control
        .respond(&request_id, &InteractionResolution::Dismissed)
        .await;
    assert!(matches!(again, Err(AdapterError::UnknownRequest(_))));

    let (status, usage, _) = turn_completed(r.until(is_turn_completed).await);
    assert_eq!(status, TurnStatus::Completed);
    assert_eq!(usage.unwrap().input_tokens, 12451 + 12866);
    // whoami failed on that machine (exit code 1): the item says so.
    assert!(r.events.iter().any(|e| matches!(e,
        AdapterEvent::ItemCompleted { key, body: Some(ItemBody::CommandExecution { exit_code: Some(1), duration_ms: Some(796), .. }), status: ItemStatus::Failed }
            if key == item)));
    assert!(r.deltas_of(item).contains("whoami.exe"));
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn command_approval_decline() {
    let mut r = start("main_approval_decline.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("Run the shell command `hostname`"))
        .await
        .unwrap();
    let request_id = match r
        .until(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        .await
    {
        AdapterEvent::InteractionRequested { request_id, .. } => request_id,
        _ => unreachable!(),
    };
    r.handle
        .control
        .respond(
            &request_id,
            &InteractionResolution::Approval {
                option_id: "decline".into(),
                feedback: None,
            },
        )
        .await
        .unwrap();
    let (status, _, _) = turn_completed(r.until(is_turn_completed).await);
    assert_eq!(status, TurnStatus::Completed);
    assert!(r.events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted {
            body: Some(ItemBody::CommandExecution { .. }),
            status: ItemStatus::Declined,
            ..
        }
    )));
    assert!(r.events.iter().any(|e| matches!(e,
        AdapterEvent::ItemCompleted { body: Some(ItemBody::AgentMessage { text }), .. } if text == "DECLINED")));
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn interrupt_ends_the_turn_as_interrupted() {
    let mut r = start("main_interrupt.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("Count from 1 to 60"))
        .await
        .unwrap();
    r.until(|e| matches!(e, AdapterEvent::ItemDelta { key, .. } if key == "44b1492a-a471-4d71-aec6-83d767ed598c")).await;
    r.handle.control.interrupt().await.unwrap();
    let (status, _, error) = turn_completed(r.until(is_turn_completed).await);
    assert_eq!(status, TurnStatus::Interrupted);
    assert_eq!(error, None);
    // The partially streamed message was never completed by Codex (the engine closes it).
    assert!(!r.events.iter().any(|e| matches!(e, AdapterEvent::ItemCompleted { key, .. } if key == "44b1492a-a471-4d71-aec6-83d767ed598c")));
    // Interrupt without a running turn is a no-op.
    r.handle.control.interrupt().await.unwrap();
    shutdown_cleanly(r).await;
}

/// `main_interrupt.jsonl` up to (and including) the line the adapter must send matching
/// `method`: from there on the fake app-server receives requests and never answers them (a
/// wedged event loop).
fn cut_after(method: &str) -> Vec<serde_json::Value> {
    let mut entries = support::script("main_interrupt.jsonl");
    let at = entries
        .iter()
        .position(|e| e["c"]["method"] == method)
        .expect("the script sends it");
    entries.truncate(at + 1);
    entries
}

#[tokio::test]
async fn an_interrupt_the_app_server_never_answers_fails_within_the_stop_grace() {
    let policy = support::policy();
    let mut r = support::start_entries(
        cut_after("turn/interrupt"),
        StartMode::New,
        ThreadSettings::default(),
        policy.clone(),
    )
    .await;
    r.handle
        .control
        .send(TurnInput::text("Count from 1 to 60"))
        .await
        .unwrap();
    r.until(|e| matches!(e, AdapterEvent::ItemDelta { .. }))
        .await;
    let asked = tokio::time::Instant::now();
    let err = r.handle.control.interrupt().await.unwrap_err();
    let waited = asked.elapsed();
    assert!(
        matches!(&err, AdapterError::Harness(m) if m.contains("turn/interrupt")),
        "{err:?}"
    );
    assert!(
        waited >= policy.stop_grace && waited < policy.stop_grace * 3,
        "answered after {waited:?}"
    );
    // The staged stop still works (the app-server is killed when stdin EOF is not enough).
    r.handle.control.shutdown(StopReason::User).await;
    r.until_closed().await;
}

#[tokio::test]
async fn a_turn_start_the_app_server_never_answers_fails_within_the_request_deadline() {
    let policy = aas_harness::AdapterPolicy {
        handshake_timeout: std::time::Duration::from_secs(1),
        ..support::policy()
    };
    let mut r = support::start_entries(
        cut_after("turn/start"),
        StartMode::New,
        ThreadSettings::default(),
        policy.clone(),
    )
    .await;
    let asked = tokio::time::Instant::now();
    let err = r
        .handle
        .control
        .send(TurnInput::text("Count from 1 to 60"))
        .await
        .unwrap_err();
    let waited = asked.elapsed();
    assert!(
        matches!(&err, AdapterError::Harness(m) if m.contains("turn/start")),
        "{err:?}"
    );
    assert!(
        waited >= policy.handshake_timeout && waited < policy.handshake_timeout * 3,
        "answered after {waited:?}"
    );
    r.handle.control.shutdown(StopReason::User).await;
    r.until_closed().await;
}

#[tokio::test]
async fn steer_injects_into_the_running_turn() {
    let mut r = start("main_steer.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("Write a four line poem about the sea."))
        .await
        .unwrap();
    r.handle
        .control
        .steer(TurnInput::text("Actually make it about mountains instead."))
        .await
        .unwrap();
    let (status, usage, _) = turn_completed(r.until(is_turn_completed).await);
    assert_eq!(status, TurnStatus::Completed);
    // Two model calls: the context is the latest one's, not a sum.
    let context = usage.expect("usage").context;
    assert_eq!(
        context,
        Some(aas_harness::ContextUsage {
            used_tokens: 13460,
            window_tokens: 996147
        })
    );
    let messages: Vec<&str> = r
        .events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ItemCompleted {
                body: Some(ItemBody::AgentMessage { text }),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].starts_with("The mountains"));
    // Steering outside a turn is refused.
    assert!(
        r.handle
            .control
            .steer(TurnInput::text("late"))
            .await
            .is_err()
    );
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn failed_turn_reports_the_provider_error() {
    let settings = ThreadSettings {
        model: Some("nonexistent-model-aas-probe".into()),
        ..Default::default()
    };
    let mut r = support::start("main_error_turn.jsonl", StartMode::New, settings).await;
    r.handle
        .control
        .send(TurnInput::text("Reply with exactly: OK"))
        .await
        .unwrap();
    let (status, usage, error) = turn_completed(r.until(is_turn_completed).await);
    assert_eq!(status, TurnStatus::Failed);
    assert_eq!(usage, None);
    let error = error.expect("error");
    assert_eq!(error.kind, "harnessError");
    assert!(
        error
            .message
            .starts_with("The supported API model names are"),
        "{}",
        error.message
    );
    assert!(r.events.iter().any(|e| matches!(e, AdapterEvent::Notice { message, .. } if message.contains("nonexistent-model-aas-probe"))));
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn process_exit_mid_turn_emits_exited_without_hanging() {
    let mut r = start("exit_mid_turn.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("Count from 1 to 60"))
        .await
        .unwrap();
    let info = r.until_closed().await;
    assert_eq!(info.code, Some(1));
    assert!(
        !r.events.iter().any(is_turn_completed),
        "the engine, not the adapter, fails the turn"
    );
    let err = r
        .handle
        .control
        .send(TurnInput::text("again"))
        .await
        .unwrap_err();
    assert_eq!(err, AdapterError::Closed);
    r.finish().await.unwrap();
}

#[tokio::test]
async fn resume_ignores_the_replayed_usage_of_older_turns() {
    let mode = StartMode::Resume {
        native_session_id: THREAD.into(),
    };
    let mut r = support::start("resume_turn.jsonl", mode, ThreadSettings::default()).await;
    assert_eq!(r.handle.native_session_id.as_deref(), Some(THREAD));
    r.handle
        .control
        .send(TurnInput::text("Reply with exactly: OK2"))
        .await
        .unwrap();
    let (status, usage, error) = turn_completed(r.until(is_turn_completed).await);
    assert_eq!(status, TurnStatus::Failed);
    assert!(error.is_some());
    assert_eq!(
        usage, None,
        "the only usage notification belonged to an older turn"
    );
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn fork_then_compact_command() {
    let mode = StartMode::Fork {
        native_session_id: THREAD.into(),
    };
    let mut r = support::start("fork_compact.jsonl", mode, ThreadSettings::default()).await;
    assert_eq!(
        r.handle.native_session_id.as_deref(),
        Some("01a0e0fd-5270-7093-ad2a-3b59d7e9e6c0")
    );
    r.handle
        .control
        .send(TurnInput::text("/compact"))
        .await
        .unwrap();
    r.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    r.until(|e| matches!(e, AdapterEvent::ItemStarted { body: ItemBody::Notice { code: Some(c), .. }, .. } if c == "contextCompacted")).await;
    // The recording ends while compacting: the process exits.
    let info = r.until_closed().await;
    assert_eq!(info.code, Some(0));
    assert!(r.natives().is_empty(), "{:?}", r.natives());
    r.finish().await.unwrap();
}

#[tokio::test]
async fn file_change_approval_shows_the_patch() {
    let mut r = start("file_change.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("Create a new file named hello.txt"))
        .await
        .unwrap();
    let (request_id, request) = match r
        .until(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        .await
    {
        AdapterEvent::InteractionRequested {
            request_id,
            request,
            ..
        } => (request_id, request),
        _ => unreachable!(),
    };
    let InteractionRequest::Approval {
        subject: Subject::FileChange { changes },
        options,
        ..
    } = request
    else {
        panic!("file change approval expected")
    };
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "hello.txt");
    assert_eq!(changes[0].kind, FileChangeKind::Add);
    assert_eq!(changes[0].diff.as_deref(), Some("@@ -0,0 +1,1 @@\n+hi\n"));
    assert_eq!((changes[0].added, changes[0].removed), (Some(1), Some(0)));
    let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["accept", "acceptForSession", "decline", "cancel"]);
    r.handle
        .control
        .respond(
            &request_id,
            &InteractionResolution::Approval {
                option_id: "accept".into(),
                feedback: None,
            },
        )
        .await
        .unwrap();
    let (status, _, _) = turn_completed(r.until(is_turn_completed).await);
    assert_eq!(status, TurnStatus::Completed);
    assert!(r.events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted {
            body: Some(ItemBody::FileChange { .. }),
            status: ItemStatus::Completed,
            ..
        }
    )));
    shutdown_cleanly(r).await;
}

#[tokio::test]
async fn shutdown_before_any_turn_closes_stdin() {
    let mut r = start("main_basic_turn.jsonl").await;
    // app-server exits by itself on stdin EOF: no kill needed.
    let info = r.handle.control.shutdown(StopReason::Idle).await;
    assert_eq!(info.stopped, None);
    assert_eq!(r.link.kill_reason(), None);
    let exited = r.until_closed().await;
    assert_eq!(exited.code, Some(0));
    // The script expected a turn that never came.
    assert!(r.finish().await.is_err());
}

#[tokio::test]
async fn shutdown_mid_turn_interrupts_then_stops() {
    // exit_mid_turn's script stops expecting input after turn/start and exits only when killed
    // or on EOF; shutdown sends turn/interrupt first, which this recording does not answer.
    let mut r = start("main_interrupt.jsonl").await;
    r.handle
        .control
        .send(TurnInput::text("Count from 1 to 60"))
        .await
        .unwrap();
    r.until(|e| matches!(e, AdapterEvent::ItemDelta { .. }))
        .await;
    // The recorded turn/interrupt expectation is satisfied by shutdown's protocol-level cancel.
    let info = r.handle.control.shutdown(StopReason::User).await;
    assert_eq!(info.code, Some(0));
    let exited = r.until_closed().await;
    assert_eq!(exited, info);
    assert!(r.events.iter().any(|e| matches!(
        e,
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        }
    )));
    r.finish().await.unwrap();
}
