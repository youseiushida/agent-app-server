//! Replays recorded Claude Code sessions (`tests/fixtures/*.jsonl`) against the protocol
//! core over in-memory pipes.
//!
//! A fixture is the verbatim (sanitized) exchange with the real CLI: `{"dir":"in"|"out"|"exit",
//! "msg":…}`. The fake CLI writes every `out` line and, at every `in` line, reads what the
//! adapter wrote and checks it against the recording (request ids of adapter-originated
//! control requests are remapped). The driver reproduces the recorded user actions through
//! the public `SessionControl` API only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aas_harness::protocol::{
    InteractionRequest, InteractionResolution, ItemBody, ItemStatus, QuestionAnswer,
    ThreadSettings, TurnStatus,
};
use aas_harness::{AdapterEvent, ExitInfo, SessionControl, StopReason, TurnInput, TurnInputPart};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, watch};

use crate::session::{ClaudeSession, ProcessLink, SessionParams};

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct Line {
    dir: String,
    msg: Value,
}

fn load(name: &str) -> Vec<Line> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    parse_lines(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
}

fn parse_lines(text: &str) -> Vec<Line> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            Line {
                dir: v["dir"].as_str().unwrap().to_owned(),
                msg: v["msg"].clone(),
            }
        })
        .collect()
}

/// Plays the CLI side. Panics (failing the test) on any divergence.
async fn fake_cli(
    script: Vec<Line>,
    from_adapter: DuplexStream,
    mut to_adapter: DuplexStream,
    exit_tx: watch::Sender<Option<ExitInfo>>,
) {
    let mut reader = BufReader::new(from_adapter).lines();
    let mut id_map: HashMap<String, String> = HashMap::new();
    for line in script {
        match line.dir.as_str() {
            "out" => {
                let mut msg = line.msg.clone();
                if msg["type"] == "control_response" {
                    let rid = msg["response"]["request_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    if let Some(actual) = id_map.get(&rid) {
                        msg["response"]["request_id"] = json!(actual);
                    }
                }
                let mut bytes = serde_json::to_vec(&msg).unwrap();
                bytes.push(b'\n');
                to_adapter.write_all(&bytes).await.unwrap();
            }
            "in" => {
                let got = tokio::time::timeout(STEP_TIMEOUT, reader.next_line())
                    .await
                    .unwrap_or_else(|_| panic!("adapter did not write the expected {}", line.msg))
                    .unwrap()
                    .unwrap_or_else(|| panic!("adapter closed stdin; expected {}", line.msg));
                let got: Value = serde_json::from_str(&got).unwrap();
                let want = &line.msg;
                assert_eq!(got["type"], want["type"], "message type; got {got}");
                match want["type"].as_str().unwrap() {
                    "control_request" => {
                        let (g, w) = (&got["request"], &want["request"]);
                        assert_eq!(g["subtype"], w["subtype"], "control subtype; got {got}");
                        for key in ["mode", "model", "settings", "detail"] {
                            if !w[key].is_null() {
                                assert_eq!(g[key], w[key], "control field {key}");
                            }
                        }
                        id_map.insert(
                            want["request_id"].as_str().unwrap().to_owned(),
                            got["request_id"].as_str().unwrap().to_owned(),
                        );
                    }
                    "control_response" => {
                        assert_eq!(
                            got["response"]["request_id"],
                            want["response"]["request_id"]
                        );
                        assert_eq!(got["response"]["subtype"], want["response"]["subtype"]);
                        assert_eq!(
                            got["response"]["response"], want["response"]["response"],
                            "permission response body"
                        );
                    }
                    "user" => {
                        assert_eq!(
                            got["message"]["content"], want["message"]["content"],
                            "user message content"
                        );
                        assert!(got["parent_tool_use_id"].is_null());
                    }
                    other => panic!("unexpected recorded input type {other}"),
                }
            }
            "exit" => {
                // The recording ends with the process exiting after stdin closed.
                let _ = tokio::time::timeout(STEP_TIMEOUT, async {
                    while let Ok(Some(_)) = reader.next_line().await {}
                })
                .await;
                drop(to_adapter);
                exit_tx.send_replace(Some(ExitInfo {
                    code: line.msg["code"].as_i64().map(|c| c as i32),
                    stopped: None,
                    stderr_tail: String::new(),
                    exited_at_ms: 1,
                }));
                return;
            }
            other => panic!("unexpected dir {other}"),
        }
    }
    drop(to_adapter);
    exit_tx.send_replace(Some(ExitInfo {
        code: Some(0),
        stopped: None,
        stderr_tail: String::new(),
        exited_at_ms: 1,
    }));
}

struct Harness {
    session: Arc<ClaudeSession>,
    events: mpsc::UnboundedReceiver<AdapterEvent>,
    fake: tokio::task::JoinHandle<()>,
    tmp: tempfile::TempDir,
}

fn start(script: Vec<Line>) -> Harness {
    let (adapter_in, fake_out) = tokio::io::duplex(1 << 20);
    let (fake_in, adapter_out) = tokio::io::duplex(1 << 20);
    let (exit_tx, exit_rx) = watch::channel(None);
    let fake = tokio::spawn(fake_cli(script, fake_in, fake_out, exit_tx));
    let tmp = tempfile::tempdir().unwrap();
    let params = SessionParams {
        label: "claude[test]".into(),
        native_session_id: "replay".into(),
        cwd: tmp.path().to_path_buf(),
        settings: ThreadSettings::default(),
        stop_grace: Duration::from_secs(5),
        request_timeout: STEP_TIMEOUT,
        max_line_bytes: 1 << 24,
        command_cache: Arc::new(Mutex::new(HashMap::new())),
    };
    let (session, events) = ClaudeSession::start(
        adapter_in,
        adapter_out,
        ProcessLink::Manual(exit_rx),
        params,
    );
    Harness {
        session,
        events,
        fake,
        tmp,
    }
}

/// TurnInput reproducing a recorded user message content.
fn input_from(content: &Value, dir: &std::path::Path) -> TurnInput {
    match content {
        Value::String(s) => TurnInput::text(s.clone()),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for (i, b) in blocks.iter().enumerate() {
                match b["type"].as_str() {
                    Some("text") => {
                        parts.push(TurnInputPart::Text(b["text"].as_str().unwrap().to_owned()))
                    }
                    Some("image") => {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(b["source"]["data"].as_str().unwrap())
                            .unwrap();
                        let path = dir.join(format!("img{i}.png"));
                        std::fs::write(&path, bytes).unwrap();
                        parts.push(TurnInputPart::Image {
                            path,
                            mime: b["source"]["media_type"].as_str().unwrap().to_owned(),
                        });
                    }
                    other => panic!("unexpected block {other:?}"),
                }
            }
            TurnInput { parts }
        }
        other => panic!("unexpected content {other}"),
    }
}

/// Resolution that makes the adapter write the recorded permission response.
fn resolution_from(recorded: &Value, request: &InteractionRequest) -> InteractionResolution {
    let body = &recorded["response"]["response"];
    if let InteractionRequest::Question { questions, .. } = request {
        let answers_map = body["updatedInput"]["answers"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let answers = questions
            .iter()
            .filter_map(|q| {
                let label = answers_map.get(&q.prompt)?.as_str()?;
                let choice = q.choices.iter().find(|c| c.label == label)?;
                Some(QuestionAnswer {
                    question_id: q.id.clone(),
                    choice_ids: vec![choice.id.clone()],
                    text: None,
                })
            })
            .collect();
        return InteractionResolution::Question { answers };
    }
    match body["behavior"].as_str() {
        Some("allow") => {
            let option = match body["updatedPermissions"].as_array() {
                None => "allow",
                Some(list) if list.iter().all(|p| p["destination"] == "session") => "allow_session",
                Some(_) => "allow_always",
            };
            InteractionResolution::Approval {
                option_id: option.into(),
                feedback: None,
            }
        }
        Some("deny") => InteractionResolution::Approval {
            option_id: "deny_feedback".into(),
            feedback: body["message"].as_str().map(str::to_owned),
        },
        other => panic!("unexpected behavior {other:?}"),
    }
}

/// Drives a recorded session through the public API and returns every event.
async fn replay(name: &str) -> Vec<AdapterEvent> {
    let script = load(name);
    let inputs: Vec<Line> = script.iter().filter(|l| l.dir == "in").cloned().collect();
    let mut h = start(script);
    let mut cursor = 0;
    assert_eq!(inputs[cursor].msg["request"]["subtype"], "initialize");
    h.session.initialize().await.unwrap();
    cursor += 1;

    let mut settings = ThreadSettings {
        permission_mode: Some("default".into()),
        ..ThreadSettings::default()
    };
    let mut all = Vec::new();
    let mut turn_running = false;
    let mut interrupts = Vec::new();
    loop {
        while let Some(line) = inputs.get(cursor) {
            let msg = &line.msg;
            match (
                msg["type"].as_str().unwrap(),
                msg["request"]["subtype"].as_str(),
            ) {
                ("user", _) if !turn_running => {
                    h.session
                        .send(input_from(&msg["message"]["content"], h.tmp.path()))
                        .await
                        .unwrap();
                    turn_running = true;
                }
                // Sent by the adapter itself after every `result`.
                ("control_request", Some("get_context_usage")) => {}
                ("control_request", Some("interrupt")) => {
                    let s = h.session.clone();
                    interrupts.push(tokio::spawn(async move { s.interrupt().await }));
                }
                ("control_request", Some("set_permission_mode")) if !turn_running => {
                    settings.permission_mode = msg["request"]["mode"].as_str().map(str::to_owned);
                    h.session.apply_settings(&settings).await.unwrap();
                }
                ("control_request", Some("set_model")) if !turn_running => {
                    settings.model = msg["request"]["model"].as_str().map(str::to_owned);
                    h.session.apply_settings(&settings).await.unwrap();
                }
                ("control_request", Some("apply_flag_settings")) if !turn_running => {
                    settings.effort = msg["request"]["settings"]["effortLevel"]
                        .as_str()
                        .map(str::to_owned);
                    h.session.apply_settings(&settings).await.unwrap();
                }
                _ => break,
            }
            cursor += 1;
        }
        if cursor >= inputs.len() && !turn_running {
            break;
        }
        let ev = tokio::time::timeout(STEP_TIMEOUT, h.events.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "no event; waiting for {}",
                    inputs
                        .get(cursor)
                        .map(|l| l.msg.to_string())
                        .unwrap_or_default()
                )
            })
            .expect("event stream closed early");
        match &ev {
            AdapterEvent::TurnCompleted { .. } => turn_running = false,
            AdapterEvent::InteractionRequested {
                request_id,
                request,
                ..
            } => {
                let recorded = &inputs[cursor].msg;
                assert_eq!(
                    recorded["type"], "control_response",
                    "unexpected interaction {request_id}"
                );
                assert_eq!(
                    recorded["response"]["request_id"].as_str(),
                    Some(request_id.as_str())
                );
                h.session
                    .respond(request_id, &resolution_from(recorded, request))
                    .await
                    .unwrap();
                cursor += 1;
            }
            AdapterEvent::Exited { .. } => panic!("process exited early"),
            _ => {}
        }
        all.push(ev);
    }
    for i in interrupts {
        i.await.unwrap().unwrap();
    }
    let exit = h.session.shutdown(StopReason::Shutdown).await;
    assert_eq!(
        exit.stopped, None,
        "the fake exits by itself once stdin closes"
    );
    while let Some(ev) = tokio::time::timeout(STEP_TIMEOUT, h.events.recv())
        .await
        .unwrap()
    {
        all.push(ev);
    }
    h.fake.await.unwrap();
    all
}

fn turn_statuses(events: &[AdapterEvent]) -> Vec<TurnStatus> {
    events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::TurnCompleted { status, .. } => Some(*status),
            _ => None,
        })
        .collect()
}

/// Final bodies of completed items, in completion order.
fn completed(events: &[AdapterEvent]) -> Vec<(String, ItemBody, ItemStatus)> {
    let mut bodies: HashMap<String, ItemBody> = HashMap::new();
    let mut out = Vec::new();
    for e in events {
        match e {
            AdapterEvent::ItemStarted { key, body } | AdapterEvent::ItemUpdated { key, body } => {
                bodies.insert(key.clone(), body.clone());
            }
            AdapterEvent::ItemDelta { key, text, field } => {
                bodies.get_mut(key).unwrap().append(*field, text);
            }
            AdapterEvent::ItemCompleted { key, body, status } => {
                let body = body.clone().unwrap_or_else(|| bodies[key].clone());
                bodies.insert(key.clone(), body.clone());
                out.push((key.clone(), body, *status));
            }
            _ => {}
        }
    }
    out
}

fn assert_well_formed(events: &[AdapterEvent]) {
    // Exited exactly once, last.
    let exits = events
        .iter()
        .filter(|e| matches!(e, AdapterEvent::Exited { .. }))
        .count();
    assert_eq!(exits, 1);
    assert!(matches!(events.last(), Some(AdapterEvent::Exited { .. })));
    // Every item is started before it gets deltas or completes, and completes at most once.
    let mut started = std::collections::HashSet::new();
    let mut done = std::collections::HashSet::new();
    for e in events {
        match e {
            AdapterEvent::ItemStarted { key, .. } => {
                assert!(started.insert(key.clone()), "{key} started twice")
            }
            AdapterEvent::ItemDelta { key, .. } | AdapterEvent::ItemUpdated { key, .. } => {
                assert!(started.contains(key), "{key} not started");
            }
            AdapterEvent::ItemCompleted { key, .. } => {
                assert!(started.contains(key), "{key} not started");
                assert!(done.insert(key.clone()), "{key} completed twice");
            }
            _ => {}
        }
    }
    // Turns alternate: started → completed.
    let mut open = false;
    for e in events {
        match e {
            AdapterEvent::TurnStarted => {
                assert!(!open, "turn started twice");
                open = true;
            }
            AdapterEvent::TurnCompleted { .. } => {
                assert!(open, "turn completed without start");
                open = false;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn replays_basic_session() {
    let events = replay("session_basic.jsonl").await;
    assert_well_formed(&events);
    assert_eq!(
        turn_statuses(&events),
        vec![
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Interrupted
        ]
    );
    // Every turn carries the context Claude Code reported for it (the fixture answers turn N
    // with 35000 + N * 1000 tokens of a 200000-token window).
    let contexts: Vec<Option<(u64, u64)>> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::TurnCompleted { usage, .. } => Some(
                usage
                    .and_then(|u| u.context)
                    .map(|c| (c.used_tokens, c.window_tokens)),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        contexts,
        (1..=6)
            .map(|n| Some((35000 + n * 1000, 200000)))
            .collect::<Vec<_>>()
    );
    // Commands from initialize (with descriptions) come first.
    match &events[0] {
        AdapterEvent::CommandsChanged { commands } => {
            assert_eq!(
                commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                vec!["compact", "review", "init"]
            );
            assert!(commands[0].description.is_some());
        }
        other => panic!("{other:?}"),
    }
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::SessionInfo { model: Some(m), .. } if m.starts_with("claude-haiku"))));
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::SessionIdentified { native_session_id } if native_session_id.starts_with("5b7cffda"))));

    let items = completed(&events);
    let messages: Vec<String> = items
        .iter()
        .filter_map(|(_, b, _)| match b {
            ItemBody::AgentMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(messages[0], "OK");
    assert_eq!(messages[1], "done");
    assert_eq!(messages[3], "Blue");
    let expected: Vec<String> = (1..=200).map(|n| n.to_string()).collect();
    assert_eq!(messages[4], expected.join("\n"));
    // Thinking was redacted: no reasoning items.
    assert!(
        !items
            .iter()
            .any(|(_, b, _)| matches!(b, ItemBody::Reasoning { .. }))
    );

    let commands: Vec<(String, String, ItemStatus)> = items
        .iter()
        .filter_map(|(_, b, s)| match b {
            ItemBody::CommandExecution {
                command, output, ..
            } => Some((command.clone(), output.clone(), *s)),
            _ => None,
        })
        .collect();
    assert_eq!(
        commands,
        vec![
            ("echo hi".into(), "hi".into(), ItemStatus::Completed),
            (
                "echo deny-me".into(),
                "deny-me".into(),
                ItemStatus::Completed
            )
        ]
    );

    let questions: Vec<&InteractionRequest> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::InteractionRequested { request, .. } => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(questions.len(), 1);
    match questions[0] {
        InteractionRequest::Question { questions, .. } => {
            assert_eq!(questions[0].prompt, "Which color do you prefer?");
            assert_eq!(questions[0].choices.len(), 2);
        }
        other => panic!("{other:?}"),
    }
    let usage: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::TurnCompleted { usage: Some(u), .. } => Some(*u),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 6);
    assert!(usage[0].cost_usd.unwrap() > 0.0);
    assert!(usage[1].cost_usd.unwrap() < usage[0].cost_usd.unwrap() + usage[1].cost_usd.unwrap());
    assert!(usage[0].input_tokens > usage[0].cached_input_tokens);
    match events.last() {
        Some(AdapterEvent::Exited { info }) => assert_eq!(info.code, Some(1)),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn replays_approvals_session() {
    let events = replay("session_approvals.jsonl").await;
    assert_well_formed(&events);
    assert_eq!(
        turn_statuses(&events),
        vec![
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Interrupted,
            TurnStatus::Completed
        ]
    );
    let approvals: Vec<(Vec<String>, String)> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::InteractionRequested {
                request: InteractionRequest::Approval { options, title, .. },
                item_key,
                ..
            } => {
                assert!(
                    item_key.as_deref().is_some_and(|k| k.starts_with("tool:")),
                    "approval linked to its tool item"
                );
                Some((
                    options.iter().map(|o| o.id.clone()).collect(),
                    title.clone(),
                ))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        approvals.len(),
        3,
        "Write, mkdir and the first Edit ask; the second Edit is allowed by the session rule"
    );
    assert_eq!(
        approvals[0].0,
        vec!["allow", "allow_session", "deny", "deny_feedback", "abort"]
    );
    assert!(approvals[0].1.starts_with("Write "));
    assert_eq!(
        approvals[1].0,
        vec![
            "allow",
            "allow_session",
            "allow_always",
            "deny",
            "deny_feedback",
            "abort"
        ]
    );
    assert_eq!(approvals[1].1, "Run command?");

    let items = completed(&events);
    let files: Vec<_> = items
        .iter()
        .filter_map(|(_, b, s)| match b {
            ItemBody::FileChange { changes } => Some((
                changes[0].kind,
                changes[0].diff.clone().unwrap_or_default(),
                *s,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].0, aas_harness::protocol::FileChangeKind::Add);
    assert_eq!(files[0].1, "@@ -0,0 +1,1 @@\n+hello\n");
    assert!(files[1].1.contains("-hello") && files[1].1.contains("+world"));
    assert!(files[2].1.contains("-world") && files[2].1.contains("+again"));
    let mkdir = items
        .iter()
        .find_map(|(_, b, s)| match b {
            ItemBody::CommandExecution { command, .. } if command == "mkdir deny-dir" => Some(*s),
            _ => None,
        })
        .unwrap();
    assert_eq!(mkdir, ItemStatus::Declined);
    assert!(
        items
            .iter()
            .any(|(_, b, _)| matches!(b, ItemBody::AgentMessage { text } if text == "red"))
    );
    assert!(
        items
            .iter()
            .any(|(_, b, _)| matches!(b, ItemBody::ToolCall { name, .. } if name == "Read"))
    );
    // The permission mode changes (session rule, then set_permission_mode) are reported.
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::SessionInfo { permission_mode: Some(m), .. } if m == "acceptEdits")));
}

fn synthetic(lines: &[Value]) -> Vec<Line> {
    lines
        .iter()
        .map(|l| Line {
            dir: l["dir"].as_str().unwrap().to_owned(),
            msg: l["msg"].clone(),
        })
        .collect()
}

fn init_exchange() -> Vec<Value> {
    vec![
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "r0", "request": {"subtype": "initialize"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "r0",
            "response": {"commands": [], "models": [], "current_permission_mode": "default"}}}}),
    ]
}

async fn collect_until_exit(h: &mut Harness) -> Vec<AdapterEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(STEP_TIMEOUT, h.events.recv())
            .await
            .unwrap()
            .expect("stream closed before Exited");
        let last = matches!(ev, AdapterEvent::Exited { .. });
        out.push(ev);
        if last {
            return out;
        }
    }
}

#[tokio::test]
async fn process_exit_mid_turn_reports_exit_without_completing() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "message": {"role": "user", "content": "hi"}}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "init", "session_id": "s1", "model": "m", "permissionMode": "default"}}),
        json!({"dir": "out", "msg": {"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "message_start", "message": {"id": "m1"}}}}),
        json!({"dir": "out", "msg": {"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}}}),
        json!({"dir": "out", "msg": {"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("hi")).await.unwrap();
    let events = collect_until_exit(&mut h).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnStarted))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::ItemDelta { text, .. } if text == "partial"))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnCompleted { .. }))
    );
    assert!(matches!(events.last(), Some(AdapterEvent::Exited { info }) if info.code == Some(0)));
    // Control after exit fails cleanly.
    assert!(h.session.interrupt().await.is_err());
}

#[tokio::test]
async fn error_result_fails_the_turn_and_cancel_withdraws_requests() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "message": {"role": "user", "content": "go"}}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "init", "session_id": "replay"}}),
        json!({"dir": "out", "msg": {"type": "control_request", "request_id": "cu1", "request": {"subtype": "can_use_tool",
            "tool_name": "Bash", "input": {"command": "rm x"}, "tool_use_id": "t1"}}}),
        json!({"dir": "out", "msg": {"type": "control_cancel_request", "request_id": "cu1"}}),
        json!({"dir": "out", "msg": {"type": "control_request", "request_id": "h1", "request": {"subtype": "hook_callback", "callback_id": "x"}}}),
        json!({"dir": "in", "msg": {"type": "control_response", "response": {"subtype": "error", "request_id": "h1", "response": null}}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "compact_boundary"}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "error_max_turns", "is_error": true, "errors": ["limit"],
            "total_cost_usd": 0.25, "usage": {"input_tokens": 1, "output_tokens": 2}}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "ctx1",
            "request": {"subtype": "get_context_usage", "detail": "summary"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "error", "request_id": "ctx1",
            "error": "get_context_usage is not supported in this context (onGetContextUsage callback not registered)"}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("go")).await.unwrap();
    let mut events = Vec::new();
    loop {
        let ev = tokio::time::timeout(STEP_TIMEOUT, h.events.recv())
            .await
            .unwrap()
            .unwrap();
        let done = matches!(ev, AdapterEvent::TurnCompleted { .. });
        events.push(ev);
        if done {
            break;
        }
    }
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::InteractionRequested { request_id, .. } if request_id == "cu1")));
    assert!(events.iter().any(
        |e| matches!(e, AdapterEvent::InteractionWithdrawn { request_id } if request_id == "cu1")
    ));
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::Native { payload } if payload["request"]["subtype"] == "hook_callback")));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::Notice { code: Some(c), .. } if c == "compacted"))
    );
    match events.last() {
        Some(AdapterEvent::TurnCompleted {
            status,
            error,
            usage,
        }) => {
            assert_eq!(*status, TurnStatus::Failed);
            assert_eq!(error.as_ref().unwrap().message, "error_max_turns: limit");
            assert_eq!(usage.unwrap().cost_usd, Some(0.25));
            assert_eq!(
                usage.unwrap().context,
                None,
                "a failed context request leaves the context out"
            );
        }
        other => panic!("{other:?}"),
    }
    // A withdrawn request can no longer be answered.
    assert!(
        h.session
            .respond(
                "cu1",
                &InteractionResolution::Approval {
                    option_id: "allow".into(),
                    feedback: None
                }
            )
            .await
            .is_err()
    );
    h.session.shutdown(StopReason::Shutdown).await;
    let rest = collect_until_exit(&mut h).await;
    assert!(matches!(rest.last(), Some(AdapterEvent::Exited { .. })));
}

#[tokio::test]
async fn unsolicited_turn_is_reported() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "out", "msg": {"type": "system", "subtype": "init", "session_id": "replay"}}),
        json!({"dir": "out", "msg": {"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m9",
            "content": [{"type": "text", "text": "background task finished"}]}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.1,
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        json!({"dir": "out", "msg": {"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m10",
            "content": [{"type": "text", "text": "another one"}]}}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "ctx1",
            "request": {"subtype": "get_context_usage", "detail": "summary"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "ctx1",
            "response": {"totalTokens": 30000, "maxTokens": 200000, "rawMaxTokens": 200000, "percentage": 15}}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.2,
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "ctx2",
            "request": {"subtype": "get_context_usage", "detail": "summary"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "ctx2",
            "response": {"totalTokens": 31000, "maxTokens": 200000, "rawMaxTokens": 200000, "percentage": 16}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    let events = collect_until_exit(&mut h).await;
    let contexts: Vec<Option<(u64, u64)>> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::TurnCompleted { usage, .. } => Some(
                usage
                    .and_then(|u| u.context)
                    .map(|c| (c.used_tokens, c.window_tokens)),
            ),
            _ => None,
        })
        .collect();
    // The first turn ended before its answer came (the next turn had begun); the answer that
    // arrived late is not attached to the wrong turn.
    assert_eq!(contexts, vec![None, Some((31000, 200000))]);
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            AdapterEvent::TurnStarted => "turnStarted",
            AdapterEvent::ItemStarted { .. } => "itemStarted",
            AdapterEvent::ItemCompleted { .. } => "itemCompleted",
            AdapterEvent::TurnCompleted { .. } => "turnCompleted",
            AdapterEvent::Exited { .. } => "exited",
            _ => "other",
        })
        .filter(|k| *k != "other")
        .collect();
    assert_eq!(
        kinds,
        vec![
            "turnStarted",
            "itemStarted",
            "itemCompleted",
            "turnCompleted",
            "turnStarted",
            "itemStarted",
            "itemCompleted",
            "turnCompleted",
            "exited"
        ]
    );
}

#[tokio::test]
async fn images_and_mentions_become_content_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("a.png");
    std::fs::write(&img, [1u8, 2, 3]).unwrap();
    let input = TurnInput {
        parts: vec![
            TurnInputPart::Text("look at".into()),
            TurnInputPart::Mention {
                relative: "src/main.rs".into(),
                absolute: dir.path().join("src/main.rs"),
            },
            TurnInputPart::Image {
                path: img,
                mime: "image/png".into(),
            },
            TurnInputPart::Text("thanks".into()),
        ],
    };
    let content = crate::session::user_content(&input).await.unwrap();
    assert_eq!(
        content,
        json!([
            {"type": "text", "text": "look at @src/main.rs"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AQID"}},
            {"type": "text", "text": "thanks"}
        ])
    );
    assert_eq!(
        crate::session::user_content(&TurnInput::text("plain"))
            .await
            .unwrap(),
        json!("plain")
    );
}

#[tokio::test]
async fn shutdown_is_not_blocked_by_a_write_the_cli_never_reads() {
    // The CLI reads nothing any more: a large message fills the pipe and its write never
    // completes, holding the writer.
    let (adapter_in, _cli_out) = tokio::io::duplex(1024);
    let (_cli_in, adapter_out) = tokio::io::duplex(1024);
    let (_exit_tx, exit_rx) = watch::channel(None);
    let tmp = tempfile::tempdir().unwrap();
    let params = SessionParams {
        label: "claude[test]".into(),
        native_session_id: "wedged".into(),
        cwd: tmp.path().to_path_buf(),
        settings: ThreadSettings::default(),
        stop_grace: Duration::from_millis(200),
        request_timeout: STEP_TIMEOUT,
        max_line_bytes: 1 << 24,
        command_cache: Arc::new(Mutex::new(HashMap::new())),
    };
    let (session, _events) = ClaudeSession::start(
        adapter_in,
        adapter_out,
        ProcessLink::Manual(exit_rx),
        params,
    );
    let stuck = {
        let session = session.clone();
        tokio::spawn(async move { session.send(TurnInput::text("x".repeat(64 * 1024))).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!stuck.is_finished(), "the write is stuck on the full pipe");
    let info = tokio::time::timeout(STEP_TIMEOUT, session.shutdown(StopReason::User))
        .await
        .expect("the staged stop reached the termination stage");
    assert_eq!(info.stopped, Some(StopReason::User));
    let sent = tokio::time::timeout(STEP_TIMEOUT, stuck)
        .await
        .expect("the stuck write was abandoned")
        .unwrap();
    assert!(sent.is_err());
}

#[tokio::test]
async fn an_interrupt_the_cli_never_answers_fails_within_the_stop_grace() {
    // The CLI reads its input but answers nothing any more (a wedged event loop).
    let (adapter_in, _cli_out) = tokio::io::duplex(1 << 16);
    let (cli_in, adapter_out) = tokio::io::duplex(1 << 16);
    let cli = tokio::spawn(async move {
        let mut lines = BufReader::new(cli_in).lines();
        let mut seen = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            seen.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        seen
    });
    let (_exit_tx, exit_rx) = watch::channel(None);
    let tmp = tempfile::tempdir().unwrap();
    let stop_grace = Duration::from_millis(300);
    let params = SessionParams {
        label: "claude[test]".into(),
        native_session_id: "wedged".into(),
        cwd: tmp.path().to_path_buf(),
        settings: ThreadSettings::default(),
        stop_grace,
        request_timeout: STEP_TIMEOUT,
        max_line_bytes: 1 << 24,
        command_cache: Arc::new(Mutex::new(HashMap::new())),
    };
    let (session, _events) = ClaudeSession::start(
        adapter_in,
        adapter_out,
        ProcessLink::Manual(exit_rx),
        params,
    );
    session.send(TurnInput::text("hi")).await.unwrap();
    let asked = tokio::time::Instant::now();
    let err = session.interrupt().await.unwrap_err();
    let waited = asked.elapsed();
    assert!(
        matches!(&err, aas_harness::AdapterError::Protocol(m) if m.contains("interrupt")),
        "{err:?}"
    );
    assert!(
        waited >= stop_grace && waited < STEP_TIMEOUT / 2,
        "answered after {waited:?}, not within the stop grace"
    );
    let info = tokio::time::timeout(STEP_TIMEOUT, session.shutdown(StopReason::User))
        .await
        .expect("the staged stop ends");
    assert_eq!(info.stopped, Some(StopReason::User));
    drop(session);
    let seen = tokio::time::timeout(STEP_TIMEOUT, cli)
        .await
        .expect("stdin closed")
        .unwrap();
    assert!(
        seen.iter().any(|m| m["request"]["subtype"] == "interrupt"),
        "the interrupt was sent: {seen:?}"
    );
}
