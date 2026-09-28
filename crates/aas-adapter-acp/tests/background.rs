//! Background work and requests outside a turn.
//!
//! * Devin's background sub-agents and shells, replayed from recordings of Devin CLI 3000.11.3
//!   (`tests/fixtures/devin_bg_*.jsonl`, sanitised; see docs/adapters/acp.md §14).
//! * Requests that arrive outside a turn and requests the engine expires, for any ACP agent
//!   (hand-written scripts).

mod common;

use std::path::PathBuf;
use std::time::Duration;

use aas_adapter_acp::testing::{AdapterOptions, FakeLink, Mode, launch_with_io};
use aas_harness::protocol::{
    BackgroundTaskKind, ExpireReason, InteractionRequest, InteractionResolution, ItemBody,
    ItemStatus, Subject, ToolCategory, TurnStatus,
};
use aas_harness::{
    AdapterError, AdapterEvent, AdapterPolicy, BackgroundState, SessionControl, ThreadSettings,
    TurnInput,
};
use common::*;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

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

fn text(t: &str) -> TurnInput {
    TurnInput::text(t)
}

struct Session {
    control: std::sync::Arc<dyn SessionControl>,
    rx: UnboundedReceiver<AdapterEvent>,
    f: Folded,
    agent: Agent,
}

async fn start(steps: Vec<Step>) -> Session {
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
    .expect("launch");
    Session {
        control: launched.handle.control,
        rx: launched.handle.events,
        f: Folded::default(),
        agent,
    }
}

impl Session {
    async fn until(&mut self, stop: impl FnMut(&AdapterEvent) -> bool) -> AdapterEvent {
        pump_until(&mut self.rx, &mut self.f, stop).await
    }

    /// Answers the next permission request with `option`.
    async fn allow(&mut self, option: &str) {
        let ev = self
            .until(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
            .await;
        let AdapterEvent::InteractionRequested { request_id, .. } = ev else {
            unreachable!()
        };
        self.control
            .respond(
                &request_id,
                &InteractionResolution::Approval {
                    option_id: option.into(),
                    feedback: None,
                },
            )
            .await
            .unwrap();
    }

    /// Stops the session and returns what the client sent.
    async fn finish(mut self) -> (Folded, Vec<Value>) {
        self.control.shutdown(aas_harness::StopReason::User).await;
        drain_to_exit(&mut self.rx, &mut self.f).await;
        let client = self.agent.finish().await;
        (self.f, client)
    }
}

/// A report of task `key` with at least `n` tool uses in its progress.
fn tool_uses(key: &'static str, n: u64) -> impl FnMut(&AdapterEvent) -> bool {
    move |ev| {
        matches!(ev, AdapterEvent::BackgroundTask { task } if task.key == key
            && task.progress.as_ref().and_then(|p| p.tool_uses).is_some_and(|u| u >= n))
    }
}

fn messages(f: &Folded) -> Vec<String> {
    f.items
        .iter()
        .filter_map(|(_, b, _)| match b {
            ItemBody::AgentMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

const SHELL_TOOL_A1: &str = "tool-call_d88d7d461a9349569107bd16#043527e027bf4c10b6da34395392f8f8";

/// Recording a1: a background shell (`ping -n 60`) and a background sub-agent. Devin keeps the
/// prompt open until the sub-agent completes and the root agent answered it; the shell outlives
/// the turn and ends later with `terminal_exit`.
#[tokio::test]
async fn devin_background_shell_and_sub_agent_are_tasks() {
    // Gated after the prompt's answer: the rest came half a minute later.
    let mut s = start(gate_after_response(
        load_fixture("devin_bg_complete.jsonl"),
        3,
    ))
    .await;
    s.control
        .send(text("start a background shell and a background sub-agent"))
        .await
        .unwrap();
    s.allow("allow_once").await;
    s.until(is_turn_completed).await;

    // At the end of the turn the shell still runs; the sub-agent completed within the prompt.
    let shell = s.f.task("shell:c3af77").clone();
    assert_eq!(
        (shell.state, shell.live, shell.kind),
        (BackgroundState::Running, true, BackgroundTaskKind::Shell)
    );
    assert_eq!(shell.title, "ping -n 60 127.0.0.1");
    assert_eq!(shell.origin_item_key.as_deref(), Some(SHELL_TOOL_A1));
    assert_eq!(shell.parent_key, None);
    assert!(shell.stoppable);

    let agent = s.f.task("subagent:7b148160").clone();
    assert_eq!(agent.kind, BackgroundTaskKind::Agent);
    assert_eq!(agent.title, "pinger");
    assert_eq!(
        (agent.state, agent.live),
        (BackgroundState::Completed, false)
    );
    assert!(agent.stoppable);
    // `success: true` means the run ended normally (here the sub-agent could not run its
    // command: background tools that need approval are denied); the summary is verbatim.
    let summary = agent
        .result
        .as_ref()
        .and_then(|r| r.summary.clone())
        .unwrap();
    assert!(
        summary.starts_with("I was unable to run the command."),
        "{summary}"
    );
    let progress = agent.progress.clone().expect("progress");
    assert_eq!(progress.last_tool_name.as_deref(), Some("exec"));
    assert_eq!(progress.tool_uses, Some(1));
    // Its two model responses: (5641 + 118) + (5829 + 345) tokens.
    assert_eq!(progress.tokens, Some(11_933));

    // The shell's command item ends as backgrounded, after the task that names it and before
    // the turn completed.
    assert_eq!(s.f.item(SHELL_TOOL_A1).2, ItemStatus::Backgrounded);
    let task_at = s.f.position(
        |e| matches!(e, AdapterEvent::BackgroundTask { task } if task.key == "shell:c3af77"),
    );
    let item_at = s.f.position(|e| {
        matches!(e, AdapterEvent::ItemCompleted { key, status: ItemStatus::Backgrounded, .. } if key == SHELL_TOOL_A1)
    });
    let turn_at = s.f.position(is_turn_completed);
    assert!(task_at < item_at && item_at < turn_at);

    // The sub-agent's own work is not part of the turn: neither its text nor its tool calls.
    assert_eq!(
        messages(&s.f),
        vec![
            "STARTED".to_owned(),
            "The `pinger` subagent failed because background agents don’t have `exec` permission in this session. I did not start anything else.".to_owned()
        ]
    );
    assert!(
        !s.f.index
            .contains_key("tool-call_840d197a4cf14fa5bccda566#19cd6de001a84365a8414f477e17f448")
    );
    // The launching `run_subagent` call is a completed sub-agent tool call (nothing links it
    // to the agent id).
    match s
        .f
        .item("tool-call_d26333db34f44728b57166a3#48de16fe938a4e15a647d9665ed5fb72")
    {
        (_, ItemBody::ToolCall { category, .. }, ItemStatus::Completed) => {
            assert_eq!(*category, ToolCategory::Subagent)
        }
        other => panic!("{other:?}"),
    }
    // The root agent's context: its own last `usage_update`, not the sub-agent's.
    let context = s.f.turns[0].1.and_then(|u| u.context).unwrap();
    assert_eq!(context.used_tokens, 13_415);

    // The shell ends after the turn with its exit code and whole output.
    s.agent.release();
    s.until(task_ended("shell:c3af77")).await;
    let shell = s.f.task("shell:c3af77").clone();
    assert_eq!(
        (shell.state, shell.live),
        (BackgroundState::Completed, false)
    );
    let result = shell.result.unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(
        result.output.as_ref().map(|o| o.chars().count()),
        Some(2197)
    );

    let (f, client) = s.finish().await;
    assert_eq!(f.turns.len(), 1);
    assert!(f.notices.is_empty(), "{:?}", f.notices);
    assert_eq!(
        client[0]["params"]["clientCapabilities"]["_meta"],
        json!({"cognition.ai/subagentSupport": true, "cognition.ai/subagentControl": true})
    );
}

/// Recording a2: the sub-agent is stopped with `_cognition.ai/subagent/cancel` while the prompt
/// is open, the shell with `_cognition.ai/terminal/killBackgroundShell` after the turn.
#[tokio::test]
async fn devin_background_tasks_stop_through_the_extension_methods() {
    let mut s = start(load_fixture("devin_bg_stop.jsonl")).await;
    s.control.send(text("start background work")).await.unwrap();
    s.allow("allow_session").await;
    // The sub-agent runs its (foreground) command.
    s.until(tool_uses("subagent:699a202f", 1)).await;
    assert_eq!(s.f.task("shell:1b9780").state, BackgroundState::Running);

    s.control
        .stop_background("subagent:699a202f")
        .await
        .unwrap();
    s.until(task_ended("subagent:699a202f")).await;
    let agent = s.f.task("subagent:699a202f").clone();
    assert_eq!(agent.state, BackgroundState::Failed);
    assert_eq!(
        agent.result.and_then(|r| r.summary).as_deref(),
        Some("[Error] Canceled by user")
    );
    s.until(is_turn_completed).await;
    assert_eq!(s.f.turns[0].0, TurnStatus::Completed);
    // The sub-agent's own command reported `terminal_exit` too, but it never was a background
    // shell.
    assert_eq!(
        s.f.tasks.keys().filter(|k| k.starts_with("shell:")).count(),
        1
    );

    // Ended and unknown tasks are refused without asking the agent.
    assert!(matches!(
        s.control.stop_background("subagent:699a202f").await,
        Err(AdapterError::Other(_))
    ));
    assert!(matches!(
        s.control.stop_background("shell:nope").await,
        Err(AdapterError::Other(_))
    ));

    s.control.stop_background("shell:1b9780").await.unwrap();
    s.until(task_ended("shell:1b9780")).await;
    let shell = s.f.task("shell:1b9780").clone();
    // Devin reports a killed shell as completed with exit code 1.
    assert_eq!(shell.state, BackgroundState::Completed);
    assert_eq!(shell.result.and_then(|r| r.exit_code), Some(1));

    let (_, client) = s.finish().await;
    let cancel = client
        .iter()
        .find(|m| m["method"] == "_cognition.ai/subagent/cancel")
        .unwrap();
    assert_eq!(
        cancel["params"],
        json!({"sessionId": "bg-stop", "agentId": "699a202f"})
    );
    let kill = client
        .iter()
        .find(|m| m["method"] == "_cognition.ai/terminal/killBackgroundShell")
        .unwrap();
    assert_eq!(
        kill["params"],
        json!({"sessionId": "bg-stop", "shellId": "1b9780"})
    );
}

/// Recording a5: `session/cancel` ends the prompt but only pauses the background sub-agent (no
/// signal says so: it stays running and keeps the process); its command moves to a background
/// shell of its own. Cancelling the paused sub-agent ends it outside any turn.
#[tokio::test]
async fn devin_cancel_leaves_the_sub_agent_running_until_it_is_stopped() {
    let mut s = start(load_fixture("devin_bg_pause.jsonl")).await;
    s.control.send(text("start background work")).await.unwrap();
    s.allow("allow_session").await;
    s.until(tool_uses("subagent:5a65831b", 1)).await;
    s.control.interrupt().await.unwrap();
    s.until(is_turn_completed).await;
    assert_eq!(s.f.turns[0].0, TurnStatus::Interrupted);

    let agent = s.f.task("subagent:5a65831b").clone();
    assert_eq!((agent.state, agent.live), (BackgroundState::Running, true));
    let moved = s.f.task("shell:40f57b").clone();
    assert_eq!(moved.state, BackgroundState::Running);
    assert_eq!(moved.parent_key.as_deref(), Some("subagent:5a65831b"));
    assert_eq!(moved.origin_item_key, None);
    assert_eq!(moved.title, "ping -n 90 127.0.0.1");
    assert_eq!(
        s.f.task("shell:5d1e01").origin_item_key.as_deref(),
        Some("tool-call_d04ea051f7c14f269ee449ad#cd0ac6024fd04dc3b0faeaa7a9d2c4b1")
    );

    s.control
        .stop_background("subagent:5a65831b")
        .await
        .unwrap();
    s.until(task_ended("subagent:5a65831b")).await;
    assert_eq!(s.f.task("subagent:5a65831b").state, BackgroundState::Failed);
    // Its shell ended with it (the end names the shell by its terminal id).
    s.until(task_ended("shell:40f57b")).await;
    assert_eq!(
        s.f.task("shell:40f57b")
            .result
            .as_ref()
            .and_then(|r| r.exit_code),
        Some(1)
    );

    s.control.stop_background("shell:5d1e01").await.unwrap();
    s.until(task_ended("shell:5d1e01")).await;
    let (f, _) = s.finish().await;
    assert!(f.tasks.values().all(|t| t.state.is_ended() && !t.live));
    // Nothing of the sub-agent became an item.
    assert_eq!(messages(&f), vec!["STARTED".to_owned()]);
}

/// Recording b1: an agent that does not confirm the extension (the recording declared no
/// client capability) gets the standard ACP treatment: no background tasks, the command that
/// went on running is interrupted with the turn, and its later updates are forwarded as
/// `Native`.
#[tokio::test]
async fn without_the_confirmed_extension_nothing_is_a_background_task() {
    let mut s = start(gate_after_response(
        load_fixture("devin_bg_undeclared.jsonl"),
        3,
    ))
    .await;
    s.control.send(text("start background work")).await.unwrap();
    s.allow("allow_once").await;
    s.until(is_turn_completed).await;
    s.agent.release();
    assert_eq!(
        s.f.item("tool-call_96f1ff5b92b442978b6ef99a#8becd35db6f74b00bc9bf9d3b095d2cc")
            .2,
        ItemStatus::Interrupted
    );
    s.until(|e| {
        matches!(e, AdapterEvent::Native { payload } if payload["outsideTurn"]["_meta"]["terminal_exit"]["terminal_id"] == "95a30d")
    })
    .await;
    assert!(matches!(
        s.control.stop_background("shell:95a30d").await,
        Err(AdapterError::Unsupported("backgroundStop"))
    ));
    let (f, _) = s.finish().await;
    assert!(f.task_events.is_empty(), "{:?}", f.task_events);
}

// ----- requests outside a turn and expired requests (any ACP agent) ---------------------------

fn handshake_steps() -> Vec<Step> {
    vec![
        Step::request("initialize", 1),
        Step::Send(json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": 1, "agentCapabilities": {}, "authMethods": [],
            "agentInfo": {"name": "scripted", "version": "1.0.0"}
        }})),
        Step::request("session/new", 2),
        Step::Send(json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1"}})),
    ]
}

fn update(u: Value) -> Step {
    Step::Send(
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}}),
    )
}

fn permission(id: &str, tool_call: Value) -> Step {
    Step::Send(
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
            "sessionId": "s1", "toolCall": tool_call,
            "options": [
                {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                {"optionId": "no", "name": "Reject", "kind": "reject_once"}
            ]
        }}),
    )
}

/// A permission request while no turn runs is shown (it belongs to the thread) and the user's
/// answer reaches the agent; standard ACP has no background work to stop.
#[tokio::test]
async fn a_permission_request_outside_a_turn_is_shown_and_answered() {
    let mut steps = handshake_steps();
    steps.extend([
        permission(
            "p1",
            json!({"toolCallId": "t1", "title": "Run build", "kind": "execute", "rawInput": {"command": "make"}}),
        ),
        Step::response_to(json!("p1")),
    ]);
    let mut s = start(steps).await;
    let ev = s
        .until(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        .await;
    let AdapterEvent::InteractionRequested {
        request_id,
        request,
        item_key,
        background_key,
    } = ev
    else {
        unreachable!()
    };
    assert_eq!((item_key, background_key), (None, None));
    match request {
        InteractionRequest::Approval { subject, .. } => assert_eq!(
            subject,
            Subject::Command {
                command: "make".into(),
                cwd: None
            }
        ),
        other => panic!("{other:?}"),
    }
    assert!(
        s.f.items.is_empty(),
        "no item outside a turn: {:?}",
        s.f.items
    );
    assert!(matches!(
        s.control.stop_background("anything").await,
        Err(AdapterError::Unsupported("backgroundStop"))
    ));
    s.control
        .respond(
            &request_id,
            &InteractionResolution::Approval {
                option_id: "yes".into(),
                feedback: None,
            },
        )
        .await
        .unwrap();
    let (f, client) = s.finish().await;
    assert!(f.withdrawn.is_empty());
    assert!(f.notices.is_empty(), "{:?}", f.notices);
    let answer = client.iter().find(|m| m["id"] == "p1").unwrap();
    assert_eq!(
        answer["result"],
        json!({"outcome": {"outcome": "selected", "optionId": "yes"}})
    );
}

/// A turn that ends with a request still pending leaves it pending (the engine expires it);
/// `expire_request` then answers the agent `cancelled`, for permissions and elicitations alike.
/// An elicitation that arrives while no turn runs is shown too.
#[tokio::test]
async fn expired_requests_are_answered_cancelled() {
    let mut steps = handshake_steps();
    steps.extend([
        Step::request("session/prompt", 3),
        update(json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run", "kind": "execute", "rawInput": {"command": "x"}})),
        permission("p1", json!({"toolCallId": "t1"})),
        // The agent ends the turn without waiting for the answer.
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}})),
        Step::response_to(json!("p1")),
        // Outside any turn: a session-scoped question.
        Step::Send(json!({"jsonrpc": "2.0", "id": "e1", "method": "elicitation/create", "params": {
            "sessionId": "s1", "mode": "form", "message": "Name?",
            "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}
        }})),
        Step::response_to(json!("e1")),
    ]);
    let mut s = start(steps).await;
    s.control.send(text("go")).await.unwrap();
    let ev = s
        .until(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        .await;
    let AdapterEvent::InteractionRequested {
        request_id: p1,
        item_key,
        ..
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(item_key.as_deref(), Some("tool-t1"));
    s.until(is_turn_completed).await;
    assert!(s.f.withdrawn.is_empty(), "not withdrawn at the turn's end");
    s.control
        .expire_request(&p1, ExpireReason::TurnEnded)
        .await
        .unwrap();
    assert!(matches!(
        s.control.expire_request(&p1, ExpireReason::TurnEnded).await,
        Err(AdapterError::UnknownRequest(_))
    ));

    let ev = s
        .until(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        .await;
    let AdapterEvent::InteractionRequested {
        request_id: e1,
        request,
        item_key,
        background_key,
    } = ev
    else {
        unreachable!()
    };
    assert!(matches!(request, InteractionRequest::Question { .. }));
    assert_eq!((item_key, background_key), (None, None));
    s.control
        .expire_request(&e1, ExpireReason::ProcessExited)
        .await
        .unwrap();

    let (f, client) = s.finish().await;
    assert!(f.notices.is_empty(), "{:?}", f.notices);
    let answer = |id: &str| {
        client
            .iter()
            .find(|m| m["id"] == id && m.get("result").is_some())
            .unwrap()["result"]
            .clone()
    };
    assert_eq!(answer("p1"), json!({"outcome": {"outcome": "cancelled"}}));
    assert_eq!(answer("e1"), json!({"action": "cancel"}));
}

/// After `session/cancel` a new permission request is answered `cancelled` (ACP) and recorded
/// as `Native`, not dropped.
#[tokio::test]
async fn a_permission_request_after_cancel_is_answered_and_recorded() {
    let mut steps = handshake_steps();
    steps.extend([
        Step::request("session/prompt", 3),
        update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "working"}})),
        Step::notification("session/cancel"),
        permission(
            "p2",
            json!({"toolCallId": "t2", "title": "Run", "kind": "execute"}),
        ),
        Step::response_to(json!("p2")),
        Step::Send(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "cancelled"}})),
    ]);
    let mut s = start(steps).await;
    s.control.send(text("go")).await.unwrap();
    s.until(|e| matches!(e, AdapterEvent::ItemStarted { .. }))
        .await;
    s.control.interrupt().await.unwrap();
    s.until(is_turn_completed).await;
    let (f, client) = s.finish().await;
    assert!(f.interactions.is_empty());
    assert!(
        f.natives
            .iter()
            .any(|n| n["method"] == "session/request_permission" && n["answered"] == "cancelled"),
        "{:?}",
        f.natives
    );
    let answer = client.iter().find(|m| m["id"] == "p2").unwrap();
    assert_eq!(
        answer["result"],
        json!({"outcome": {"outcome": "cancelled"}})
    );
}

/// Reading a session's history leaves the sub-agents' own work out (with the extension
/// confirmed), and a request made while only the history is read is cancelled with a notice:
/// nobody can answer it there.
#[tokio::test]
async fn history_leaves_sub_agents_out_and_cancels_requests() {
    let sub = |u: Value| {
        let mut u = u;
        u["_meta"] = json!({"cognition.ai/subagent_context": {"parentAgentId": "a1"}});
        update(u)
    };
    let steps = vec![
        Step::request("initialize", 1),
        Step::Send(json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": true, "_meta": {"cognition.ai/subagentControl": true}}
        }})),
        Step::request("session/load", 2),
        update(
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "hello"}}),
        ),
        update(
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "root "}}),
        ),
        update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "a1", "status": "in_progress",
            "_meta": {"cognition.ai/subagent_started": {"agentId": "a1", "title": "helper", "isBackground": true}}}),
        ),
        sub(
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "sub text"}}),
        ),
        sub(
            json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Ran x", "kind": "execute"}),
        ),
        update(
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "answer"}}),
        ),
        Step::Send(
            json!({"jsonrpc": "2.0", "id": "p1", "method": "session/request_permission", "params": {
                "sessionId": "s1", "toolCall": {"toolCallId": "t1"},
                "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"}]
            }}),
        ),
        Step::response_to(json!("p1")),
        update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "a1", "status": "completed",
            "_meta": {"cognition.ai/subagent_completed": {"agentId": "a1", "success": true, "summary": "done"}}}),
        ),
        Step::Send(json!({"jsonrpc": "2.0", "id": 2, "result": {}})),
    ];
    let link = FakeLink::default();
    let (reader, writer, agent) = spawn_agent(steps, link.clone(), End::OnClientEof);
    let launched = launch_with_io(
        reader,
        writer,
        link,
        cwd(),
        Mode::History("s1".into()),
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
    let mut rx = launched.handle.events;
    let mut f = Folded::default();
    drain_to_exit(&mut rx, &mut f).await;
    let client = agent.finish().await;
    let history = launched.history.expect("history");
    assert_eq!(history.turns.len(), 1);
    let bodies: Vec<&ItemBody> = history.turns[0].items.iter().map(|i| &i.body).collect();
    assert!(matches!(bodies[0], ItemBody::UserMessage { text, .. } if text == "hello"));
    assert!(
        matches!(bodies[1], ItemBody::AgentMessage { text } if text == "root answer"),
        "{bodies:?}"
    );
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(f.task_events.is_empty(), "no task from a replay");
    let codes: Vec<&str> = f.notices.iter().filter_map(|n| n.2.as_deref()).collect();
    assert_eq!(codes, ["permissionDuringHistoryRead"]);
    let answer = client.iter().find(|m| m["id"] == "p1").unwrap();
    assert_eq!(
        answer["result"],
        json!({"outcome": {"outcome": "cancelled"}})
    );
}
