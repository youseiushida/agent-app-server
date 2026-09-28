//! Replays recorded Devin (`devin acp`, 3000.11.3) transcripts and hand-written scripts
//! through the adapter's protocol core, over in-memory pipes.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use aas_adapter_acp::testing::{AdapterOptions, FakeLink, Mode, launch_with_io, list_with_io};
use aas_harness::protocol::{
    ApprovalOptionKind, FileChangeKind, InteractionRequest, InteractionResolution, ItemBody,
    ItemStatus, NoticeLevel, Subject, TurnStatus,
};
use aas_harness::{
    AdapterError, AdapterEvent, AdapterPolicy, ThreadSettings, TurnInput, TurnInputPart,
};
use common::*;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

fn cwd() -> PathBuf {
    PathBuf::from(r"C:\work\proj")
}

fn policy() -> AdapterPolicy {
    AdapterPolicy {
        stop_grace: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(10),
        ..AdapterPolicy::default()
    }
}

fn settings_model(model: &str) -> ThreadSettings {
    ThreadSettings {
        model: Some(model.into()),
        ..ThreadSettings::default()
    }
}

/// Initialize + session/new steps of a minimal hand-written agent.
fn handshake_steps(caps: Value, config_options: Value) -> Vec<Step> {
    vec![
        Step::request("initialize", 1),
        Step::Send(json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": 1, "agentCapabilities": caps, "authMethods": [],
            "agentInfo": {"name": "scripted", "version": "1.0.0"}
        }})),
        Step::request("session/new", 2),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "configOptions": config_options}}),
        ),
    ]
}

fn update(u: Value) -> Step {
    Step::Send(
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}}),
    )
}

fn text(t: &str) -> TurnInput {
    TurnInput::text(t)
}

#[tokio::test]
async fn devin_recorded_turns() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(
        load_fixture("devin_turns.jsonl"),
        link.clone(),
        End::OnClientEof,
    );
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        settings_model("swe-1-7-lightning-medium"),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .expect("launch");
    assert_eq!(
        launched.handle.native_session_id.as_deref(),
        Some("oceanic-kayak")
    );
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();

    // Turn 1: plain answer.
    control.send(text("Reply with exactly: OK")).await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    // Turn 2: a command that runs without asking.
    control.send(text("Run the shell command `echo aas-probe` and tell me its output. Do not do anything else.")).await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    // Turn 3: another command.
    control
        .send(text(
            "Run the shell command `echo second-probe`. If it is not allowed, just say DENIED.",
        ))
        .await
        .unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    // Turn 4: a file write.
    control.send(text("Create a file named hello.txt containing exactly the single line: hello from aas. Then reply DONE.")).await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    // Turn 5: a command that needs permission; allow it, then interrupt while it runs.
    control
        .send(text(
            "Run the shell command `ping -n 30 127.0.0.1` and report the summary line.",
        ))
        .await
        .unwrap();
    let mut interrupted = false;
    loop {
        let ev = pump_until(&mut rx, &mut f, |ev| {
            matches!(
                ev,
                AdapterEvent::InteractionRequested { .. } | AdapterEvent::TurnCompleted { .. }
            ) || matches!(
                ev,
                AdapterEvent::ItemDelta {
                    field: aas_harness::DeltaField::Output,
                    ..
                }
            )
        })
        .await;
        match ev {
            AdapterEvent::InteractionRequested { request_id, .. } => control
                .respond(
                    &request_id,
                    &InteractionResolution::Approval {
                        option_id: "allow_once".into(),
                        feedback: None,
                    },
                )
                .await
                .unwrap(),
            AdapterEvent::ItemDelta { .. } if !interrupted => {
                interrupted = true;
                control.interrupt().await.unwrap();
            }
            AdapterEvent::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    let info = control.shutdown(aas_harness::StopReason::User).await;
    assert_eq!(info.code, Some(0));
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;

    // Turns and usage.
    let statuses: Vec<_> = f.turns.iter().map(|t| t.0).collect();
    assert_eq!(
        statuses,
        vec![
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Interrupted
        ]
    );
    let u = f.turns[0].1.expect("usage");
    assert_eq!((u.input_tokens, u.output_tokens), (11093, 18));
    assert_eq!(f.turns[1].1.unwrap().cached_input_tokens, 11136);
    // Context occupancy: `used` / `size` of each turn's last `usage_update`.
    let contexts: Vec<Option<(u64, u64)>> = f
        .turns
        .iter()
        .map(|t| {
            t.1.and_then(|u| u.context)
                .map(|c| (c.used_tokens, c.window_tokens))
        })
        .collect();
    assert_eq!(
        contexts,
        vec![
            Some((11111, 202752)),
            Some((11265, 202752)),
            Some((11388, 202752)),
            Some((11512, 202752)),
            Some((11646, 202752))
        ]
    );
    assert!(
        f.turns.iter().all(|t| t.2.is_none()),
        "no turn errors: {:?}",
        f.turns
    );

    // Turn 1 items: a reasoning run then the message.
    assert!(
        matches!(&f.items[0], (_, ItemBody::Reasoning { text }, ItemStatus::Completed) if text.starts_with("The user asked me to"))
    );
    assert!(
        matches!(&f.items[1], (_, ItemBody::AgentMessage { text }, ItemStatus::Completed) if text == "OK")
    );

    // Turn 2: the command item.
    let cmd = f.item("tool-functions.exec:0#0d7bd2301c92478f9622c41814a8fde2");
    match cmd {
        (
            _,
            ItemBody::CommandExecution {
                command,
                output,
                exit_code,
                ..
            },
            ItemStatus::Completed,
        ) => {
            assert_eq!(command, "echo aas-probe");
            assert_eq!(output, "aas-probe");
            assert_eq!(*exit_code, Some(0));
        }
        other => panic!("{other:?}"),
    }
    let messages: Vec<&str> = f
        .items
        .iter()
        .filter_map(|(_, b, _)| match b {
            ItemBody::AgentMessage { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec!["OK", "aas-probe", "second-probe", "DONE"]);

    // Turn 4: file change from the diff content.
    match f.item("tool-functions.write:2#d4db5a531439428092a6c4acbdc50b51") {
        (_, ItemBody::FileChange { changes }, ItemStatus::Completed) => {
            assert_eq!(changes[0].path, "hello.txt");
            assert_eq!(changes[0].kind, FileChangeKind::Add);
            assert_eq!(changes[0].added, Some(1));
        }
        other => panic!("{other:?}"),
    }

    // Turn 5: approval with every option, the command then failed by the cancel.
    assert_eq!(f.interactions.len(), 1);
    let (_, request, item_key) = &f.interactions[0];
    assert_eq!(
        item_key.as_deref(),
        Some("tool-functions.exec:3#a1bfee4aa7794a6c9ef480ac6d7d27db")
    );
    match request {
        InteractionRequest::Approval {
            subject, options, ..
        } => {
            assert_eq!(
                *subject,
                Subject::Command {
                    command: "ping -n 30 127.0.0.1".into(),
                    cwd: None
                }
            );
            let kinds: Vec<_> = options.iter().map(|o| o.kind).collect();
            assert_eq!(
                kinds,
                vec![
                    ApprovalOptionKind::AllowOnce,
                    ApprovalOptionKind::AllowAlways,
                    ApprovalOptionKind::AllowAlways,
                    ApprovalOptionKind::AllowAlways,
                    ApprovalOptionKind::AllowAlways,
                    ApprovalOptionKind::Deny,
                ]
            );
        }
        other => panic!("{other:?}"),
    }
    let ping = f.item("tool-functions.exec:3#a1bfee4aa7794a6c9ef480ac6d7d27db");
    assert_eq!(ping.2, ItemStatus::Failed);

    // Session state.
    assert!(
        f.infos
            .iter()
            .any(|i| i.0.as_deref() == Some("swe-1-7-lightning-medium")
                && i.1.as_deref() == Some("accept-edits"))
    );
    assert!(f.commands.iter().any(|c| c.contains(&"plan".to_owned())));
    // Extension notifications are not forwarded by default; session titles become SessionTitle.
    assert!(
        f.natives.iter().all(|n| n.get("method").is_none()),
        "{:?}",
        f.natives
    );
    assert!(
        f.events
            .iter()
            .any(|e| matches!(e, AdapterEvent::SessionTitle { title } if title == "OK"))
    );
    assert!(f.notices.is_empty(), "{:?}", f.notices);

    // What the client sent.
    let perm_answer = client
        .iter()
        .find(|m| m.get("result").is_some_and(|r| r.get("outcome").is_some()))
        .unwrap();
    assert_eq!(
        perm_answer["result"],
        json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}})
    );
    assert!(client.iter().any(|m| m["method"] == "session/cancel" && m["params"]["sessionId"] == "oceanic-kayak"));
    let init = &client[0];
    assert_eq!(
        init["params"]["clientCapabilities"],
        json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false, "elicitation": {"form": {}, "url": {}}})
    );
    let prompt = client
        .iter()
        .find(|m| m["method"] == "session/prompt")
        .unwrap();
    assert_eq!(
        prompt["params"]["prompt"],
        json!([{"type": "text", "text": "Reply with exactly: OK"}])
    );
}

/// Steps of the recorded load transcript up to (and including) the load response.
fn load_steps() -> Vec<Step> {
    let mut steps = load_fixture("devin_load.jsonl");
    let end = steps
        .iter()
        .position(|s| matches!(s, Step::Send(m) if m["id"] == 2 && m.get("result").is_some()))
        .unwrap();
    steps.truncate(end + 1);
    steps
}

#[tokio::test]
async fn devin_load_for_resume_suppresses_the_replay() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(load_steps(), link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::Resume("oceanic-kayak".into()),
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .expect("launch");
    assert_eq!(
        launched.handle.native_session_id.as_deref(),
        Some("oceanic-kayak")
    );
    assert!(launched.history.is_none());
    let mut rx = launched.handle.events;
    launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::Idle)
        .await;
    let mut f = Folded::default();
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    assert_eq!(client[1]["method"], "session/load");
    assert!(
        f.items.is_empty(),
        "replayed history must not become live items: {:?}",
        f.items
    );
    assert!(f.turns.is_empty());
    // State from the replay is still applied.
    assert!(
        f.infos
            .iter()
            .any(|i| i.0.as_deref() == Some("swe-1-7-lightning-medium"))
    );
    assert!(!f.commands.is_empty());
}

#[tokio::test]
async fn devin_load_collects_history() {
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(load_steps(), link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::History("oceanic-kayak".into()),
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .expect("launch");
    launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::Shutdown)
        .await;
    agent.finish().await;
    let history = launched.history.expect("history");
    assert_eq!(history.title.as_deref(), Some("OK"));
    assert_eq!(history.turns.len(), 5);
    let first_user = |i: usize| match &history.turns[i].items[0].body {
        ItemBody::UserMessage { text, .. } => text.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(first_user(0), "Reply with exactly: OK");
    assert_eq!(
        first_user(3),
        "Create a file named hello.txt containing exactly the single line: hello from aas. Then reply DONE."
    );
    let kinds: Vec<&str> = history.turns[1]
        .items
        .iter()
        .map(|i| i.body.kind_str())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "userMessage",
            "reasoning",
            "commandExecution",
            "reasoning",
            "agentMessage"
        ]
    );
    match &history.turns[1].items[2] {
        aas_harness::HistoryItem {
            body: ItemBody::CommandExecution {
                output, exit_code, ..
            },
            status,
        } => {
            assert_eq!(output, "aas-probe\n");
            assert_eq!(*exit_code, Some(0));
            assert_eq!(*status, ItemStatus::Completed);
        }
        other => panic!("{other:?}"),
    }
    let last = history.turns[4].items.last().unwrap();
    assert_eq!(
        last.status,
        ItemStatus::Failed,
        "cancelled ping stays failed"
    );
    assert!(
        history
            .turns
            .iter()
            .flat_map(|t| &t.items)
            .all(|i| i.status != ItemStatus::InProgress)
    );
}

#[tokio::test]
async fn prompt_error_fails_the_turn() {
    let mut steps = handshake_steps(json!({}), json!([]));
    steps.extend([
        Step::request("session/prompt", 3),
        update(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}})),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "error": {
            "code": -32010,
            "message": "Reached free model rate limit. Upgrade to Max for higher limits, or switch to a different model.",
            "data": {"cognition.ai/errorKind": "unavailable", "cognition.ai/retryable": true}
        }})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    launched.handle.control.send(text("hi")).await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    let (status, _, error) = &f.turns[0];
    assert_eq!(*status, TurnStatus::Failed);
    let error = error.clone().unwrap();
    assert_eq!(error.kind, "harnessError");
    assert!(error.message.contains("rate limit"));
    assert_eq!(
        f.items[0].2,
        ItemStatus::Completed,
        "the reasoning run is closed before the turn ends"
    );
    launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::User)
        .await;
    drain_to_exit(&mut rx, &mut f).await;
    agent.finish().await;
}

#[tokio::test]
async fn auth_required_makes_start_fail_with_the_hint() {
    let steps = vec![
        Step::request("initialize", 1),
        Step::Send(json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": 1, "agentCapabilities": {},
            "authMethods": [{"id": "devin-browser", "name": "Log in with browser", "description": "Sign in via your browser"}]
        }})),
        Step::request("session/new", 2),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32000, "message": "Authentication required"}}),
        ),
    ];
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let options = AdapterOptions {
        auth_hint: Some("Run `devin auth login`.".into()),
        ..AdapterOptions::default()
    };
    let err = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        options,
        policy(),
    )
    .await
    .err()
    .expect("must fail");
    match err {
        AdapterError::Unavailable(msg) => {
            assert!(msg.contains("requires authentication"), "{msg}");
            assert!(msg.contains("Log in with browser"), "{msg}");
            assert!(msg.contains("devin auth login"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
    agent.finish().await;
}

#[tokio::test]
async fn protocol_version_mismatch_is_unavailable() {
    let steps = vec![
        Step::request("initialize", 1),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 2, "agentCapabilities": {}}}),
        ),
    ];
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let err = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(err, AdapterError::Unavailable(ref m) if m.contains("version 2")),
        "{err:?}"
    );
    agent.finish().await;
}

fn permission_steps() -> Vec<Step> {
    let mut steps = handshake_steps(json!({}), json!([]));
    steps.extend([
        Step::request("session/prompt", 3),
        update(json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit main.rs", "kind": "edit",
            "content": [{"type": "diff", "path": "src/main.rs", "oldText": "a\n", "newText": "b\n"}]})),
        Step::Send(json!({"jsonrpc": "2.0", "id": "p1", "method": "session/request_permission", "params": {
            "sessionId": "s1", "toolCall": {"toolCallId": "t1"},
            "options": [
                {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                {"optionId": "no", "name": "Reject", "kind": "reject_once"},
                {"optionId": "never", "name": "Never", "kind": "reject_always"}
            ]
        }})),
    ]);
    steps
}

#[tokio::test]
async fn dismissed_permission_answers_reject_once() {
    let mut steps = permission_steps();
    steps.extend([
        Step::response_to(json!("p1")),
        update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"})),
        update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Rejected."}})),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    control.send(text("edit")).await.unwrap();
    let ev = pump_until(&mut rx, &mut f, |e| {
        matches!(e, AdapterEvent::InteractionRequested { .. })
    })
    .await;
    let AdapterEvent::InteractionRequested {
        request_id,
        request,
        ..
    } = ev
    else {
        unreachable!()
    };
    match &request {
        InteractionRequest::Approval {
            subject: Subject::FileChange { changes },
            options,
            ..
        } => {
            assert_eq!(changes[0].kind, FileChangeKind::Update);
            assert_eq!((changes[0].added, changes[0].removed), (Some(1), Some(1)));
            assert_eq!(options.len(), 3);
        }
        other => panic!("{other:?}"),
    }
    // An unknown option is refused and the request stays answerable.
    let err = control
        .respond(
            &request_id,
            &InteractionResolution::Approval {
                option_id: "bogus".into(),
                feedback: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Other(_)));
    control
        .respond(&request_id, &InteractionResolution::Dismissed)
        .await
        .unwrap();
    assert!(matches!(
        control
            .respond(&request_id, &InteractionResolution::Dismissed)
            .await,
        Err(AdapterError::UnknownRequest(_))
    ));
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    assert_eq!(f.item("tool-t1").2, ItemStatus::Failed);
    control.shutdown(aas_harness::StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    let answer = client.iter().find(|m| m["id"] == "p1").unwrap();
    assert_eq!(
        answer["result"],
        json!({"outcome": {"outcome": "selected", "optionId": "no"}})
    );
}

#[tokio::test]
async fn interrupt_cancels_pending_permissions() {
    let mut steps = permission_steps();
    steps.extend([
        Step::notification("session/cancel"),
        Step::response_to(json!("p1")),
        update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"}),
        ),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "cancelled"}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    control.send(text("edit")).await.unwrap();
    let ev = pump_until(&mut rx, &mut f, |e| {
        matches!(e, AdapterEvent::InteractionRequested { .. })
    })
    .await;
    let AdapterEvent::InteractionRequested { request_id, .. } = ev else {
        unreachable!()
    };
    control.interrupt().await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    assert_eq!(f.withdrawn, vec![request_id]);
    assert_eq!(f.turns[0].0, TurnStatus::Interrupted);
    control.shutdown(aas_harness::StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    let answer = client.iter().find(|m| m["id"] == "p1").unwrap();
    assert_eq!(
        answer["result"],
        json!({"outcome": {"outcome": "cancelled"}})
    );
}

/// A form elicitation (shape of the ACP v1 schema 1.21 `CreateElicitationRequest`) tied to a
/// tool call of the running turn.
fn form_elicitation(id: &str) -> Step {
    Step::Send(
        json!({"jsonrpc": "2.0", "id": id, "method": "elicitation/create", "params": {
            "sessionId": "s1", "toolCallId": "t1", "mode": "form", "message": "Which environment?",
            "requestedSchema": {"type": "object", "properties": {
                "env": {"type": "string", "title": "Environment", "oneOf": [
                    {"const": "prod", "title": "Production"}, {"const": "dev", "title": "Development"}]},
                "note": {"type": "string", "maxLength": 20}
            }, "required": ["env"]}
        }}),
    )
}

fn answer(
    question: &str,
    choices: &[&str],
    text: Option<&str>,
) -> aas_harness::protocol::QuestionAnswer {
    aas_harness::protocol::QuestionAnswer {
        question_id: question.into(),
        choice_ids: choices.iter().map(|c| (*c).to_owned()).collect(),
        text: text.map(str::to_owned),
    }
}

#[tokio::test]
async fn form_elicitation_is_answered_through_a_question() {
    let mut steps = handshake_steps(json!({}), json!([]));
    steps.extend([
        Step::request("session/prompt", 3),
        update(json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "deploy", "kind": "other"})),
        form_elicitation("e1"),
        Step::response_to(json!("e1")),
        update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"})),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    control.send(text("deploy")).await.unwrap();
    let ev = pump_until(&mut rx, &mut f, |e| {
        matches!(e, AdapterEvent::InteractionRequested { .. })
    })
    .await;
    let AdapterEvent::InteractionRequested {
        request_id,
        request,
        item_key,
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(item_key.as_deref(), Some("tool-t1"));
    match &request {
        InteractionRequest::Question { questions, .. } => {
            let ids: Vec<&str> = questions.iter().map(|q| q.id.as_str()).collect();
            assert_eq!(ids, ["env", "note"]);
            assert!(questions[0].prompt.starts_with("Which environment?"));
            assert_eq!(questions[0].choices.len(), 2);
            assert!(questions[1].allow_free_text);
        }
        other => panic!("{other:?}"),
    }
    // A missing required answer is refused and the question stays open.
    let err = control
        .respond(
            &request_id,
            &InteractionResolution::Question {
                answers: vec![answer("note", &[], Some("hi"))],
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, AdapterError::Other(ref m) if m.contains("env")),
        "{err:?}"
    );
    let answers = vec![
        answer("env", &["dev"], None),
        answer("note", &[], Some("hi")),
    ];
    control
        .respond(&request_id, &InteractionResolution::Question { answers })
        .await
        .unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    assert_eq!(f.turns[0].0, TurnStatus::Completed);
    control.shutdown(aas_harness::StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    let reply = client.iter().find(|m| m["id"] == "e1").unwrap();
    assert_eq!(
        reply["result"],
        json!({"action": "accept", "content": {"env": "dev", "note": "hi"}})
    );
}

#[tokio::test]
async fn url_elicitation_completed_by_the_agent_is_withdrawn() {
    let mut steps = handshake_steps(json!({}), json!([]));
    steps.extend([
        Step::request("session/prompt", 3),
        Step::Send(json!({"jsonrpc": "2.0", "id": "e2", "method": "elicitation/create", "params": {
            "sessionId": "s1", "mode": "url", "elicitationId": "el-1",
            "url": "https://auth.example.com/device", "message": "Sign in to the MCP server"
        }})),
        Step::Send(json!({"jsonrpc": "2.0", "method": "elicitation/complete", "params": {"elicitationId": "el-1"}})),
        Step::response_to(json!("e2")),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    control.send(text("sign in")).await.unwrap();
    let ev = pump_until(&mut rx, &mut f, |e| {
        matches!(e, AdapterEvent::InteractionRequested { .. })
    })
    .await;
    let AdapterEvent::InteractionRequested {
        request_id,
        request,
        ..
    } = ev
    else {
        unreachable!()
    };
    assert!(
        matches!(&request, InteractionRequest::Question { questions, .. }
        if questions[0].prompt.ends_with("https://auth.example.com/device"))
    );
    pump_until(&mut rx, &mut f, |e| {
        matches!(e, AdapterEvent::InteractionWithdrawn { .. })
    })
    .await;
    assert_eq!(f.withdrawn, vec![request_id.clone()]);
    // The agent's completion answered it; a late answer from the user is refused.
    assert!(matches!(
        control
            .respond(&request_id, &InteractionResolution::Dismissed)
            .await,
        Err(AdapterError::UnknownRequest(_))
    ));
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    control.shutdown(aas_harness::StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    let reply = client.iter().find(|m| m["id"] == "e2").unwrap();
    assert_eq!(reply["result"], json!({"action": "accept"}));
}

#[tokio::test]
async fn elicitations_that_cannot_be_shown_are_cancelled() {
    let mut steps = handshake_steps(json!({}), json!([]));
    steps.extend([
        // Request scope (outside any session, e.g. during authentication): nothing to attach it to.
        Step::Send(json!({"jsonrpc": "2.0", "id": "r1", "method": "elicitation/create", "params": {
            "requestId": 7, "mode": "form", "message": "API key?", "requestedSchema": {"type": "object"}
        }})),
        Step::response_to(json!("r1")),
        Step::request("session/prompt", 3),
        // A custom mode must not be rendered as a known one.
        Step::Send(json!({"jsonrpc": "2.0", "id": "r2", "method": "elicitation/create", "params": {
            "sessionId": "s1", "mode": "_vendor.picker", "message": "Pick"
        }})),
        Step::response_to(json!("r2")),
        form_elicitation("r3"),
        Step::notification("session/cancel"),
        Step::response_to(json!("r3")),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "cancelled"}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    control.send(text("go")).await.unwrap();
    let ev = pump_until(&mut rx, &mut f, |e| {
        matches!(e, AdapterEvent::InteractionRequested { .. })
    })
    .await;
    let AdapterEvent::InteractionRequested { request_id, .. } = ev else {
        unreachable!()
    };
    // Interrupting cancels the open elicitation.
    control.interrupt().await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    assert_eq!(f.withdrawn, vec![request_id]);
    control.shutdown(aas_harness::StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    for id in ["r1", "r2", "r3"] {
        let reply = client.iter().find(|m| m["id"] == id).unwrap();
        assert_eq!(reply["result"], json!({"action": "cancel"}), "{id}");
    }
    let codes: Vec<&str> = f.notices.iter().filter_map(|n| n.2.as_deref()).collect();
    assert_eq!(codes, ["elicitationOutsideTurn", "unsupportedElicitation"]);
    assert!(
        f.natives
            .iter()
            .any(|n| n["params"]["mode"] == "_vendor.picker")
    );
}

/// A process that dies on its own mid-turn ends the turn with `Exited` only: the engine fails
/// the turn and quotes as many stderr lines as `policy.exit_message_stderr_lines` says, as for
/// every other harness, so the adapter must not write its own message.
#[tokio::test]
async fn process_exit_mid_turn_closes_the_turn_state_and_leaves_the_turn_to_exited() {
    let mut steps = handshake_steps(json!({}), json!([]));
    steps.extend([
        Step::request("session/prompt", 3),
        update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Work"}})),
        Step::Send(json!({"jsonrpc": "2.0", "id": "p9", "method": "session/request_permission", "params": {
            "sessionId": "s1", "toolCall": {"toolCallId": "t9", "title": "Run", "kind": "execute", "rawInput": {"command": "x"}},
            "options": [{"optionId": "ok", "name": "OK", "kind": "allow_once"}]
        }})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) =
        spawn_agent(steps, link.clone(), End::Crash(3, "panic: out of memory\n"));
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    launched.handle.control.send(text("go")).await.unwrap();
    drain_to_exit(&mut rx, &mut f).await;
    agent.finish().await;
    assert!(f.turns.is_empty(), "no TurnCompleted: {:?}", f.turns);
    let open: Vec<_> = f
        .items
        .iter()
        .filter(|i| i.2 == ItemStatus::InProgress)
        .map(|i| i.0.clone())
        .collect();
    assert!(open.is_empty(), "items left open: {open:?}");
    assert!(
        f.items.iter().any(|i| i.2 == ItemStatus::Failed),
        "{:?}",
        f.items
    );
    assert_eq!(f.withdrawn.len(), 1, "the pending permission is withdrawn");
    // What the engine builds the turn's error from.
    let info = f.exited.clone().unwrap();
    assert_eq!(info.code, Some(3));
    assert!(info.stderr_tail.contains("out of memory"), "{info:?}");
    assert!(matches!(f.events.last(), Some(AdapterEvent::Exited { .. })));
    // Control calls after the exit fail instead of hanging.
    assert!(matches!(
        launched.handle.control.send(text("again")).await,
        Err(AdapterError::Closed)
    ));
    let info = launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::User)
        .await;
    assert_eq!(info.code, Some(3));
}

#[tokio::test]
async fn client_methods_we_do_not_provide_are_rejected() {
    let mut steps = handshake_steps(json!({}), json!([]));
    steps.extend([
        Step::request("session/prompt", 3),
        Step::Send(json!({"jsonrpc": "2.0", "id": 77, "method": "fs/read_text_file", "params": {"sessionId": "s1", "path": "x"}})),
        Step::response_to(json!(77)),
        Step::Send(json!({"jsonrpc": "2.0", "method": "_vendor/progress", "params": {"n": 1}})),
        Step::Send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "brand_new", "x": 1}}})),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let options = AdapterOptions {
        forward_extension_notifications: true,
        ..AdapterOptions::default()
    };
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        options,
        policy(),
    )
    .await
    .unwrap();
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    launched.handle.control.send(text("go")).await.unwrap();
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::User)
        .await;
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    let answer = client.iter().find(|m| m["id"] == 77).unwrap();
    assert_eq!(answer["error"]["code"], -32601);
    assert!(
        f.notices
            .iter()
            .any(|n| n.0 == NoticeLevel::Warning && n.1.contains("fs/read_text_file"))
    );
    assert!(
        f.natives.iter().any(|n| n["method"] == "_vendor/progress"),
        "forwarded when enabled"
    );
    assert!(
        f.natives.iter().any(|n| n["sessionUpdate"] == "brand_new"),
        "unknown updates are forwarded"
    );
}

fn config_options(mode: &str, model: &str) -> Value {
    json!([
        {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": mode,
         "options": [{"value": "code", "name": "Code"}, {"value": "ask", "name": "Ask"}]},
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
         "options": [{"value": "m1", "name": "M1"}, {"value": "m2", "name": "M2"}]},
        {"id": "effort", "name": "Thinking", "category": "thought_level", "type": "select", "currentValue": "low",
         "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]}
    ])
}

#[tokio::test]
async fn settings_use_config_options_and_report_session_info() {
    let mut steps = handshake_steps(json!({}), config_options("code", "m1"));
    steps.extend([
        // Start-up settings: model m2 differs, mode "code" matches.
        Step::request("session/set_config_option", 3),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"configOptions": config_options("code", "m2")}})),
        // Later: switch the permission mode and the thought level.
        Step::request("session/set_config_option", 4),
        Step::Send(json!({"jsonrpc": "2.0", "id": 4, "result": {"configOptions": config_options("ask", "m2")}})),
        Step::request("session/set_config_option", 5),
        Step::Send(json!({"jsonrpc": "2.0", "id": 5, "result": {"configOptions": [
            {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": "ask", "options": [{"value": "ask", "name": "Ask"}]},
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "m2", "options": [{"value": "m2", "name": "M2"}]},
            {"id": "effort", "name": "Thinking", "category": "thought_level", "type": "select", "currentValue": "high", "options": [{"value": "high", "name": "High"}]}
        ]}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let settings = ThreadSettings {
        model: Some("m2".into()),
        permission_mode: Some("code".into()),
        effort: None,
    };
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        settings,
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let control = launched.handle.control;
    let applied = control
        .apply_settings(&ThreadSettings {
            model: None,
            permission_mode: Some("ask".into()),
            effort: Some("high".into()),
        })
        .await
        .unwrap();
    assert_eq!(applied, aas_harness::SettingsApplied::Live);
    let err = control
        .apply_settings(&ThreadSettings {
            model: Some("m9".into()),
            ..ThreadSettings::default()
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, AdapterError::Other(ref m) if m.contains("m9")),
        "{err:?}"
    );
    control.shutdown(aas_harness::StopReason::User).await;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    let sets: Vec<(String, String)> = client
        .iter()
        .filter(|m| m["method"] == "session/set_config_option")
        .map(|m| {
            (
                m["params"]["configId"].as_str().unwrap().to_owned(),
                m["params"]["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        sets,
        vec![
            ("model".into(), "m2".into()),
            ("mode".into(), "ask".into()),
            ("effort".into(), "high".into())
        ]
    );
    assert_eq!(
        f.infos.last().unwrap(),
        &(Some("m2".into()), Some("ask".into()), Some("high".into()))
    );
}

#[tokio::test]
async fn modes_without_config_options_use_set_mode() {
    let mut steps = vec![
        Step::request("initialize", 1),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {}}}),
        ),
        Step::request("session/new", 2),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1",
            "modes": {"currentModeId": "a", "availableModes": [{"id": "a", "name": "A"}, {"id": "b", "name": "B"}]}}}),
        ),
        Step::request("session/set_mode", 3),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {}})),
    ];
    steps.push(update(
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "b"}),
    ));
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let settings = ThreadSettings {
        permission_mode: Some("b".into()),
        ..ThreadSettings::default()
    };
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        settings,
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::User)
        .await;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    assert_eq!(
        client[2]["params"],
        json!({"sessionId": "s1", "modeId": "b"})
    );
    assert_eq!(f.infos.last().unwrap().1.as_deref(), Some("b"));
}

#[tokio::test]
async fn unknown_start_settings_become_notices() {
    let steps = handshake_steps(json!({}), json!([]));
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let settings = ThreadSettings {
        model: Some("gpt-x".into()),
        ..ThreadSettings::default()
    };
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        settings,
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::User)
        .await;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    drain_to_exit(&mut rx, &mut f).await;
    agent.finish().await;
    assert!(
        f.notices
            .iter()
            .any(|n| n.2.as_deref() == Some("settingIgnored")),
        "{:?}",
        f.notices
    );
}

#[tokio::test]
async fn resume_prefers_session_resume_and_fork_needs_the_capability() {
    let steps = vec![
        Step::request("initialize", 1),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1,
            "agentCapabilities": {"loadSession": true, "sessionCapabilities": {"resume": {}}}}}),
        ),
        Step::request("session/resume", 2),
        Step::Send(json!({"jsonrpc": "2.0", "id": 2, "result": {}})),
    ];
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::Resume("old".into()),
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    assert_eq!(launched.handle.native_session_id.as_deref(), Some("old"));
    launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::User)
        .await;
    let client = agent.finish().await;
    assert_eq!(client[1]["params"]["sessionId"], "old");
    assert_eq!(client[1]["params"]["mcpServers"], json!([]));

    let steps = vec![
        Step::request("initialize", 1),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}}),
        ),
    ];
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let err = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::Fork("old".into()),
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err, AdapterError::Unsupported("fork"));
    agent.finish().await;

    let steps = vec![
        Step::request("initialize", 1),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1,
            "agentCapabilities": {"sessionCapabilities": {"fork": {}}}}}),
        ),
        Step::request("session/fork", 2),
        Step::Send(json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "forked"}})),
    ];
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::Fork("old".into()),
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    assert_eq!(launched.handle.native_session_id.as_deref(), Some("forked"));
    launched
        .handle
        .control
        .shutdown(aas_harness::StopReason::User)
        .await;
    agent.finish().await;
}

#[tokio::test]
async fn session_list_pages_and_filters_by_cwd() {
    let steps = vec![
        Step::request("session/list", 1),
        Step::Send(json!({"jsonrpc": "2.0", "id": 1, "result": {
            "sessions": [
                {"sessionId": "a", "cwd": r"C:\work\proj", "title": "OK", "updatedAt": "2026-09-27T03:52:09+00:00"},
                {"sessionId": "b", "cwd": r"C:\work\other"}
            ],
            "nextCursor": "c1"
        }})),
        Step::request("session/list", 2),
        Step::Send(json!({"jsonrpc": "2.0", "id": 2, "result": {
            "sessions": [{"sessionId": "c", "cwd": r"C:\work\proj\", "title": ""}],
            "nextCursor": "c1"
        }})),
    ];
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link, End::OnClientEof);
    let sessions = list_with_io(reader, writer, &cwd()).await.unwrap();
    let ids: Vec<_> = sessions
        .iter()
        .map(|s| s.native_session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["a", "c"]);
    assert_eq!(sessions[0].updated_at, Some(1_790_481_129_000));
    assert_eq!(sessions[1].title, None);
    // The client stopped at the repeated cursor; the agent is left waiting for EOF.
    drop(sessions);
    agent.task.abort();
}

#[tokio::test]
async fn prompt_blocks_map_mentions_and_images() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("a.png");
    std::fs::write(&img, [1u8, 2, 3]).unwrap();
    let input = TurnInput {
        parts: vec![
            TurnInputPart::Text("see".into()),
            TurnInputPart::Mention {
                relative: "src/main.rs".into(),
                absolute: dir.path().join("src").join("main.rs"),
            },
            TurnInputPart::Image {
                path: img.clone(),
                mime: "image/png".into(),
            },
        ],
    };
    let blocks = aas_adapter_acp::testing::prompt_blocks(&input, true)
        .await
        .unwrap();
    assert_eq!(blocks[0], json!({"type": "text", "text": "see"}));
    assert_eq!(blocks[1]["type"], "resource_link");
    assert_eq!(blocks[1]["name"], "src/main.rs");
    assert!(blocks[1]["uri"].as_str().unwrap().starts_with("file:///"));
    assert_eq!(
        blocks[2],
        json!({"type": "image", "data": "AQID", "mimeType": "image/png"})
    );
    let err = aas_adapter_acp::testing::prompt_blocks(&input, false)
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Unsupported(_)));
    assert!(
        aas_adapter_acp::testing::prompt_blocks(&TurnInput::default(), true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_unanswered_settings_request_fails_the_start_after_the_handshake_timeout() {
    let mut steps = handshake_steps(json!({}), config_options("code", "m1"));
    // The agent never answers the model switch.
    steps.push(Step::request("session/set_config_option", 3));
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let policy = AdapterPolicy {
        handshake_timeout: Duration::from_millis(300),
        stop_grace: Duration::from_millis(300),
        ..policy()
    };
    let started = tokio::time::timeout(
        STEP_TIMEOUT,
        launch_with_io(
            reader,
            writer,
            link,
            cwd(),
            Mode::New,
            settings_model("m2"),
            AdapterOptions::default(),
            policy,
        ),
    )
    .await
    .expect("the start is bounded by the handshake timeout");
    let err = started.err().expect("the start fails");
    assert!(
        matches!(err, AdapterError::Harness(ref m) if m.contains("session/set_config_option")),
        "{err:?}"
    );
    agent.finish().await;
}

#[tokio::test]
async fn permission_requests_are_served_while_a_settings_change_is_pending() {
    let mut steps = handshake_steps(json!({}), config_options("code", "m1"));
    steps.extend([
        Step::request("session/prompt", 3),
        update(json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run tests", "kind": "execute"})),
        // The agent handles the mode switch only after the pending permission is answered.
        Step::request("session/set_config_option", 4),
        Step::Send(json!({"jsonrpc": "2.0", "id": "p1", "method": "session/request_permission", "params": {
            "sessionId": "s1", "toolCall": {"toolCallId": "t1"},
            "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"}]
        }})),
        Step::response_to(json!("p1")),
        Step::Send(json!({"jsonrpc": "2.0", "id": 4, "result": {"configOptions": config_options("ask", "m1")}})),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}})),
    ]);
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy(),
    )
    .await
    .unwrap();
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    control.send(text("edit")).await.unwrap();
    // The prompt has reached the agent before the settings request is written.
    pump_until(&mut rx, &mut f, |e| {
        matches!(e, AdapterEvent::ItemStarted { .. })
    })
    .await;
    let pending = {
        let control = control.clone();
        tokio::spawn(async move {
            control
                .apply_settings(&ThreadSettings {
                    permission_mode: Some("ask".into()),
                    ..ThreadSettings::default()
                })
                .await
        })
    };
    let ev = pump_until(&mut rx, &mut f, |e| {
        matches!(e, AdapterEvent::InteractionRequested { .. })
    })
    .await;
    let AdapterEvent::InteractionRequested { request_id, .. } = ev else {
        unreachable!()
    };
    control
        .respond(
            &request_id,
            &InteractionResolution::Approval {
                option_id: "yes".into(),
                feedback: None,
            },
        )
        .await
        .unwrap();
    let applied = tokio::time::timeout(STEP_TIMEOUT, pending)
        .await
        .expect("settings change finished")
        .unwrap();
    assert_eq!(applied.unwrap(), aas_harness::SettingsApplied::Live);
    pump_until(&mut rx, &mut f, is_turn_completed).await;
    assert_eq!(f.turns[0].0, TurnStatus::Completed);
    assert_eq!(f.infos.last().unwrap().1.as_deref(), Some("ask"));
    control.shutdown(aas_harness::StopReason::User).await;
    drain_to_exit(&mut rx, &mut f).await;
    agent.finish().await;
}

#[tokio::test]
async fn shutdown_terminates_an_agent_that_stopped_reading_stdin() {
    // After the handshake the agent never reads its stdin again (a wedged event loop).
    let steps = handshake_steps(json!({}), json!([]));
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::Hang);
    let grace = Duration::from_millis(300);
    let policy = AdapterPolicy {
        stop_grace: grace,
        ..policy()
    };
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::New,
        ThreadSettings::default(),
        AdapterOptions::default(),
        policy,
    )
    .await
    .unwrap();
    let control = launched.handle.control;
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    // A prompt larger than the pipe: its write blocks while holding the writer, so the
    // protocol-level cancel of the stop cannot be written either.
    control.send(text(&"x".repeat(3 << 20))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let interrupted = tokio::time::timeout(STEP_TIMEOUT, control.interrupt())
        .await
        .expect("interrupt is bounded");
    assert!(
        interrupted.is_err(),
        "the cancel request cannot reach the agent"
    );
    let info = tokio::time::timeout(
        STEP_TIMEOUT,
        control.shutdown(aas_harness::StopReason::User),
    )
    .await
    .expect("the staged stop reached the termination stage");
    assert_eq!(info.stopped, Some(aas_harness::StopReason::User));
    drain_to_exit(&mut rx, &mut f).await;
    assert_eq!(f.turns.len(), 1, "the lost turn is closed");
    agent.finish().await;
}
