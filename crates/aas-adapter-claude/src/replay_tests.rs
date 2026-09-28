//! Replays recorded Claude Code sessions (`tests/fixtures/*.jsonl`) against the protocol
//! core over in-memory pipes.
//!
//! A fixture is the (sanitized) exchange with the real CLI: `{"dir":"in"|"out"|"exit",
//! "msg":…}`. The fake CLI writes every `out` line and, at every `in` line, reads what the
//! adapter wrote and checks it against the recording (ids the adapter chooses — control request
//! ids and user message uuids — are mapped to the recorded ones). The driver reproduces the
//! recorded user actions through the public `SessionControl` API only, each at the point of
//! the recording where the fake CLI waits for it. How the fixtures were made from the raw
//! recordings: docs/adapters/claude.md §18.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aas_harness::protocol::{
    ExpireReason, InteractionRequest, InteractionResolution, ItemBody, ItemStatus, QuestionAnswer,
    ThreadSettings, ToolCategory, TurnStatus, TurnTrigger,
};
use aas_harness::{
    AdapterError, AdapterEvent, BackgroundState, BackgroundTaskInfo, BackgroundTaskKind, ExitInfo,
    SessionControl, StopReason, TurnInput, TurnInputPart,
};
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

/// How far the fake CLI got: `in` lines consumed, and whether it reached the recorded exit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Progress {
    consumed: usize,
    at_exit: bool,
}

/// Replaces every string equal to a recorded uuid with the adapter's uuid for it.
fn map_uuids(value: &mut Value, uuids: &HashMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(actual) = uuids.get(s.as_str()) {
                *s = actual.clone();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| map_uuids(v, uuids)),
        Value::Object(map) => map.values_mut().for_each(|v| map_uuids(v, uuids)),
        _ => {}
    }
}

/// Checks a control request the adapter wrote against the recorded one.
fn check_control_request(got: &Value, want: &Value, uuids: &HashMap<String, String>) {
    let (g, w) = (&got["request"], &want["request"]);
    assert_eq!(g["subtype"], w["subtype"], "control subtype; got {got}");
    for key in ["mode", "model", "settings", "detail", "task_id"] {
        if !w[key].is_null() {
            assert_eq!(g[key], w[key], "control field {key}; got {got}");
        }
    }
    if let Some(recorded) = w["message_uuid"].as_str() {
        assert_eq!(
            g["message_uuid"].as_str(),
            uuids.get(recorded).map(String::as_str),
            "cancel_async_message names the message; got {got}"
        );
    }
    if w["subtype"] == "initialize" {
        for key in ["perTaskStopAffordance", "agentProgressSummaries"] {
            if !w[key].is_null() {
                assert_eq!(g[key], w[key], "initialize field {key}; got {got}");
            }
        }
        if !w["hooks"].is_null() {
            assert_eq!(g["hooks"], w["hooks"], "initialize hooks; got {got}");
        }
        // What the adapter always declares (docs/adapters/claude.md §2).
        assert_eq!(g["perTaskStopAffordance"], true);
        assert_eq!(
            g["hooks"],
            json!({"Stop": [{"hookCallbackIds": [crate::session::STOP_HOOK_ID]}]})
        );
    }
}

/// Plays the CLI side. Panics (failing the test) on any divergence.
async fn fake_cli(
    script: Vec<Line>,
    from_adapter: DuplexStream,
    mut to_adapter: DuplexStream,
    exit_tx: watch::Sender<Option<ExitInfo>>,
    progress: watch::Sender<Progress>,
) {
    let mut reader = BufReader::new(from_adapter).lines();
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut uuids: HashMap<String, String> = HashMap::new();
    let mut consumed = 0;
    for line in script {
        match line.dir.as_str() {
            "out" => {
                let mut msg = line.msg.clone();
                if msg["type"] == "control_response" {
                    let rid = msg["response"]["request_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    if let Some(actual) = ids.get(&rid) {
                        msg["response"]["request_id"] = json!(actual);
                    }
                }
                map_uuids(&mut msg, &uuids);
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
                assert_eq!(
                    got["type"], want["type"],
                    "message type; got {got}, want {want}"
                );
                match want["type"].as_str().unwrap() {
                    "control_request" => {
                        check_control_request(&got, want, &uuids);
                        ids.insert(
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
                            "control response body"
                        );
                    }
                    "user" => {
                        assert_eq!(
                            got["message"]["content"], want["message"]["content"],
                            "user message content"
                        );
                        assert!(got["parent_tool_use_id"].is_null());
                        assert_eq!(
                            got["origin"],
                            json!({"kind": "human"}),
                            "a typed message says so"
                        );
                        if !want["origin"].is_null() {
                            assert_eq!(got["origin"], want["origin"]);
                        }
                        let actual = got["uuid"].as_str().expect("the message has a uuid");
                        let recorded = want["uuid"].as_str().expect("recorded with a uuid");
                        uuids.insert(recorded.to_owned(), actual.to_owned());
                    }
                    other => panic!("unexpected recorded input type {other}"),
                }
                consumed += 1;
                progress.send_replace(Progress {
                    consumed,
                    at_exit: false,
                });
            }
            "exit" => {
                progress.send_replace(Progress {
                    consumed,
                    at_exit: true,
                });
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
    progress.send_replace(Progress {
        consumed,
        at_exit: true,
    });
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
    progress: watch::Receiver<Progress>,
    tmp: tempfile::TempDir,
}

fn params(dir: &std::path::Path, settings: ThreadSettings) -> SessionParams {
    SessionParams {
        label: "claude[test]".into(),
        native_session_id: "replay".into(),
        cwd: dir.to_path_buf(),
        settings,
        stop_grace: Duration::from_secs(5),
        request_timeout: STEP_TIMEOUT,
        max_line_bytes: 1 << 24,
        command_cache: Arc::new(Mutex::new(HashMap::new())),
        agent_progress_summaries: None,
    }
}

fn start(script: Vec<Line>) -> Harness {
    start_with(script, ThreadSettings::default())
}

fn start_with(script: Vec<Line>, settings: ThreadSettings) -> Harness {
    let (adapter_in, fake_out) = tokio::io::duplex(1 << 20);
    let (fake_in, adapter_out) = tokio::io::duplex(1 << 20);
    let (exit_tx, exit_rx) = watch::channel(None);
    let (progress_tx, progress) = watch::channel(Progress::default());
    let fake = tokio::spawn(fake_cli(script, fake_in, fake_out, exit_tx, progress_tx));
    let tmp = tempfile::tempdir().unwrap();
    let (session, events) = ClaudeSession::start(
        adapter_in,
        adapter_out,
        ProcessLink::Manual(exit_rx),
        params(tmp.path(), settings),
    );
    Harness {
        session,
        events,
        fake,
        progress,
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

/// What a replay produced.
struct Replayed {
    events: Vec<AdapterEvent>,
    /// The outcome of every `send`, in order.
    sends: Vec<Result<(), AdapterError>>,
    /// The outcome of every `stop_background`, in order.
    stops: Vec<Result<(), AdapterError>>,
}

/// Pumps the adapter's events while the driver waits for a point of the recording.
struct Driver {
    h: Harness,
    events: Vec<AdapterEvent>,
    open_turns: i64,
    requests: HashSet<String>,
    tasks: HashSet<String>,
}

impl Driver {
    fn absorb(&mut self, ev: AdapterEvent) {
        match &ev {
            AdapterEvent::TurnStarted => self.open_turns += 1,
            AdapterEvent::TurnCompleted { .. } => self.open_turns -= 1,
            AdapterEvent::InteractionRequested { request_id, .. } => {
                self.requests.insert(request_id.clone());
            }
            AdapterEvent::BackgroundTask { task } => {
                self.tasks.insert(task.key.clone());
            }
            AdapterEvent::Exited { .. } => panic!("process exited early"),
            _ => {}
        }
        self.events.push(ev);
    }

    /// Takes the events the adapter already emitted.
    fn absorb_ready(&mut self) {
        while let Ok(ev) = self.h.events.try_recv() {
            self.absorb(ev);
        }
    }

    /// Waits until the fake CLI waits for its `in` line `index` and `ready` holds.
    async fn wait(&mut self, index: usize, what: &str, ready: impl Fn(&Driver) -> bool) {
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        loop {
            let progress = *self.h.progress.borrow();
            if progress.consumed == index && ready(self) {
                return;
            }
            assert!(
                progress.consumed <= index,
                "the fake CLI went past {what} (line {index})"
            );
            tokio::select! {
                ev = self.h.events.recv() => self.absorb(ev.expect("event stream closed early")),
                changed = self.h.progress.changed() => {
                    changed.expect("the fake CLI ended early");
                }
                _ = tokio::time::sleep_until(deadline) => panic!("timed out waiting for {what}; progress {progress:?}, open turns {}", self.open_turns),
            }
        }
    }

    /// Waits until the fake CLI reached the recorded exit.
    async fn wait_exit(&mut self) {
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        while !self.h.progress.borrow().at_exit {
            tokio::select! {
                ev = self.h.events.recv() => self.absorb(ev.expect("event stream closed early")),
                changed = self.h.progress.changed() => {
                    changed.expect("the fake CLI ended early");
                }
                _ = tokio::time::sleep_until(deadline) => panic!("timed out waiting for the recorded exit; progress {:?}", *self.h.progress.borrow()),
            }
        }
    }
}

/// Drives a recorded session through the public API and returns what it produced.
async fn replay(name: &str) -> Replayed {
    replay_with(name, ThreadSettings::default()).await
}

async fn replay_with(name: &str, start_settings: ThreadSettings) -> Replayed {
    let script = load(name);
    // Requests of the CLI the adapter answers by itself (the Stop hook).
    let hooks: HashSet<String> = script
        .iter()
        .filter(|l| {
            l.dir == "out"
                && l.msg["type"] == "control_request"
                && l.msg["request"]["subtype"] == "hook_callback"
        })
        .map(|l| l.msg["request_id"].as_str().unwrap().to_owned())
        .collect();
    // The recorded requests the adapter answers from the user's resolution.
    let requests: HashMap<String, Value> = script
        .iter()
        .filter(|l| l.dir == "out" && l.msg["type"] == "control_request")
        .map(|l| {
            (
                l.msg["request_id"].as_str().unwrap().to_owned(),
                l.msg["request"].clone(),
            )
        })
        .collect();
    let inputs: Vec<Line> = script.iter().filter(|l| l.dir == "in").cloned().collect();
    let mut d = Driver {
        h: start_with(script, start_settings.clone()),
        events: Vec::new(),
        open_turns: 0,
        requests: HashSet::new(),
        tasks: HashSet::new(),
    };
    assert_eq!(inputs[0].msg["request"]["subtype"], "initialize");
    d.h.session.initialize().await.unwrap();

    let mut settings = ThreadSettings {
        permission_mode: Some("default".into()),
        ..start_settings
    };
    let mut sends = Vec::new();
    let mut spawned_stops = Vec::new();
    let mut interrupts = Vec::new();
    for (index, line) in inputs.iter().enumerate().skip(1) {
        let msg = &line.msg;
        match (
            msg["type"].as_str().unwrap(),
            msg["request"]["subtype"].as_str(),
        ) {
            ("user", _) => {
                d.wait(index, "a user message", |d| d.open_turns == 0).await;
                let sent =
                    d.h.session
                        .send(input_from(&msg["message"]["content"], d.h.tmp.path()))
                        .await;
                if matches!(sent, Err(AdapterError::TurnInProgress)) {
                    // The adapter emitted the TurnStarted of the CLI's own run before it
                    // returned.
                    d.absorb_ready();
                    assert!(
                        d.events.iter().rev().find_map(|e| match e {
                            AdapterEvent::TurnStarted => Some(true),
                            AdapterEvent::TurnCompleted { .. } => Some(false),
                            _ => None,
                        }) == Some(true)
                            || d.open_turns == 0,
                        "the CLI's run was reported before the refusal"
                    );
                }
                sends.push(sent);
            }
            ("control_response", _) => {
                let id = msg["response"]["request_id"].as_str().unwrap().to_owned();
                if hooks.contains(&id) {
                    continue;
                }
                d.wait(index, "an interaction", |d| d.requests.contains(&id))
                    .await;
                let request = d
                    .events
                    .iter()
                    .find_map(|e| match e {
                        AdapterEvent::InteractionRequested {
                            request_id,
                            request,
                            ..
                        } if *request_id == id => Some(request.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("no interaction {id}: {:?}", requests.get(&id)));
                d.h.session
                    .respond(&id, &resolution_from(msg, &request))
                    .await
                    .unwrap();
            }
            // Written by the adapter by itself.
            (
                "control_request",
                Some("get_context_usage" | "get_settings" | "cancel_async_message"),
            ) => {}
            ("control_request", Some("interrupt")) => {
                d.wait(index, "an interrupt", |d| d.open_turns > 0).await;
                let s = d.h.session.clone();
                interrupts.push(tokio::spawn(async move { s.interrupt().await }));
            }
            ("control_request", Some("stop_task")) => {
                let key = msg["request"]["task_id"].as_str().unwrap().to_owned();
                d.wait(index, "a stop", |d| d.tasks.contains(&key)).await;
                let s = d.h.session.clone();
                spawned_stops.push(tokio::spawn(async move { s.stop_background(&key).await }));
            }
            ("control_request", Some(subtype)) => {
                d.wait(index, "a settings change", |d| d.open_turns == 0)
                    .await;
                match subtype {
                    "set_permission_mode" => {
                        settings.permission_mode =
                            msg["request"]["mode"].as_str().map(str::to_owned);
                    }
                    "set_model" => {
                        settings.model = msg["request"]["model"].as_str().map(str::to_owned);
                    }
                    "apply_flag_settings" => {
                        settings.effort = msg["request"]["settings"]["effortLevel"]
                            .as_str()
                            .map(str::to_owned);
                    }
                    other => panic!("unexpected recorded control request {other}"),
                }
                d.h.session.apply_settings(&settings).await.unwrap();
            }
            (other, _) => panic!("unexpected recorded input {other}"),
        }
    }
    d.wait_exit().await;
    for i in interrupts {
        i.await.unwrap().unwrap();
    }
    let mut stops = Vec::new();
    for s in spawned_stops {
        stops.push(s.await.unwrap());
    }
    let exit = d.h.session.shutdown(StopReason::Shutdown).await;
    assert_eq!(
        exit.stopped, None,
        "the fake exits by itself once stdin closes"
    );
    while let Some(ev) = tokio::time::timeout(STEP_TIMEOUT, d.h.events.recv())
        .await
        .unwrap()
    {
        d.events.push(ev);
    }
    d.h.fake.await.unwrap();
    Replayed {
        events: d.events,
        sends,
        stops,
    }
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

fn turn_triggers(events: &[AdapterEvent]) -> Vec<Option<TurnTrigger>> {
    events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::TurnCompleted { trigger, .. } => Some(*trigger),
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

/// Every reported state of each task, in order.
fn task_history(events: &[AdapterEvent]) -> HashMap<String, Vec<BackgroundTaskInfo>> {
    let mut out: HashMap<String, Vec<BackgroundTaskInfo>> = HashMap::new();
    for e in events {
        if let AdapterEvent::BackgroundTask { task } = e {
            out.entry(task.key.clone())
                .or_default()
                .push((**task).clone());
        }
    }
    out
}

fn last_state(events: &[AdapterEvent], key: &str) -> BackgroundTaskInfo {
    task_history(events)
        .remove(key)
        .and_then(|h| h.last().cloned())
        .unwrap_or_else(|| panic!("task {key} never reported"))
}

/// Position of the first event matching `pred`.
fn position(events: &[AdapterEvent], pred: impl Fn(&AdapterEvent) -> bool) -> usize {
    events.iter().position(pred).expect("the event was emitted")
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
    let mut started = HashSet::new();
    let mut done = HashSet::new();
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
    // A backgrounded item names a task reported before it (the port's order).
    for (i, e) in events.iter().enumerate() {
        if let AdapterEvent::ItemCompleted {
            key,
            status: ItemStatus::Backgrounded,
            ..
        } = e
        {
            assert!(
                events[..i].iter().any(|e| matches!(e, AdapterEvent::BackgroundTask { task } if task.origin_item_key.as_deref() == Some(key.as_str()))),
                "{key} was backgrounded before its task named it"
            );
        }
    }
}

#[tokio::test]
async fn replays_basic_session() {
    let Replayed { events, sends, .. } = replay("session_basic.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
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
    // The user's turns are not CLI-started runs.
    assert!(turn_triggers(&events).iter().all(Option::is_none));

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
    let Replayed { events, sends, .. } = replay("session_approvals.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
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
                background_key,
                ..
            } => {
                assert!(
                    item_key.as_deref().is_some_and(|k| k.starts_with("tool:")),
                    "approval linked to its tool item"
                );
                assert_eq!(*background_key, None, "the main thread asks");
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

/// E1 (claude-live): a background agent, a Bash its agent put in the background, the agent's
/// restart under the same id, and the two runs the CLI starts when they end.
#[tokio::test]
async fn replays_a_background_agent_that_restarts() {
    let Replayed { events, sends, .. } = replay("bg_agent_restart.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let agent = "a5462215479566a35";
    let item = "tool:toolu_0169HUUF9mk3xGGa2YkSPjg2";
    // The launching item goes on as the task: the task (naming it) first, the item then, both
    // before the turn ends.
    let task_at = position(
        &events,
        |e| matches!(e, AdapterEvent::BackgroundTask { task } if task.key == agent && task.origin_item_key.as_deref() == Some(item)),
    );
    let item_at = position(
        &events,
        |e| matches!(e, AdapterEvent::ItemCompleted { key, status: ItemStatus::Backgrounded, .. } if key == item),
    );
    let turn_at = position(&events, |e| matches!(e, AdapterEvent::TurnCompleted { .. }));
    assert!(task_at < item_at && item_at < turn_at);
    // Running → completed → running again (second run) → completed.
    let history = task_history(&events).remove(agent).unwrap();
    let runs: Vec<(u32, BackgroundState)> = history.iter().map(|t| (t.runs, t.state)).collect();
    let mut steps = runs.clone();
    steps.dedup();
    assert_eq!(
        steps,
        vec![
            (1, BackgroundState::Running),
            (1, BackgroundState::Completed),
            (2, BackgroundState::Running),
            (2, BackgroundState::Completed)
        ],
        "{runs:?}"
    );
    let first = history.first().unwrap();
    assert_eq!(first.kind, BackgroundTaskKind::Agent);
    assert_eq!(first.title, "Sleep 40 seconds then respond");
    assert!(first.live && first.stoppable);
    // The summary is the CLI's, verbatim; the progress numbers are its explicit fields.
    let last = history.last().unwrap();
    assert!(
        last.result
            .as_ref()
            .and_then(|r| r.summary.as_ref())
            .is_some()
    );
    assert_eq!(last.result.as_ref().unwrap().exit_code, None);
    assert!(history.iter().any(|t| {
        t.progress
            .as_ref()
            .is_some_and(|p| p.last_tool_name.as_deref() == Some("Bash") && p.tool_uses == Some(2))
    }));
    // The Bash the agent started belongs to it.
    let bash = last_state(&events, "bqvfla9t7");
    assert_eq!(bash.kind, BackgroundTaskKind::Shell);
    assert_eq!(bash.parent_key.as_deref(), Some(agent));
    assert_eq!(bash.state, BackgroundState::Completed);
    // Every task left the live set.
    assert!(
        task_history(&events)
            .values()
            .all(|h| !h.last().unwrap().live)
    );
    // The user's turn, then the two runs the CLI started for the finished tasks.
    assert_eq!(
        turn_triggers(&events),
        vec![
            None,
            Some(TurnTrigger::BackgroundTask),
            Some(TurnTrigger::BackgroundTask)
        ]
    );
}

/// s3: stop_task on a background agent (whose permission request belongs to it) and on a
/// background Bash; a subagent's foreground Bash is never shown.
#[tokio::test]
async fn replays_stopping_background_tasks() {
    let Replayed {
        events,
        sends,
        stops,
    } = replay("bg_stop_agent_then_bash.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_eq!(stops.len(), 3);
    assert!(stops.iter().all(Result::is_ok), "{stops:?}");
    assert_well_formed(&events);
    let agent = "a490e13f727a24c33";
    // The background agent's permission request belongs to the agent, not to a turn item.
    let asked = events
        .iter()
        .find_map(|e| match e {
            AdapterEvent::InteractionRequested {
                request_id,
                item_key,
                background_key,
                ..
            } if request_id.starts_with("d901dbbe") => {
                Some((item_key.clone(), background_key.clone()))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(asked, (None, Some(agent.to_owned())));
    let stopped = last_state(&events, agent);
    assert_eq!(stopped.state, BackgroundState::Stopped);
    assert!(!stopped.live);
    assert_eq!(
        stopped.result.unwrap().summary.as_deref(),
        Some("Run ping command and report result")
    );
    let bash = last_state(&events, "bg710iinq");
    assert_eq!(
        (bash.kind, bash.state, bash.live),
        (BackgroundTaskKind::Shell, BackgroundState::Stopped, false)
    );
    assert_eq!(
        bash.origin_item_key.as_deref(),
        Some("tool:toolu_01AZD3RhYVzfZjUQqLMHLX7P")
    );
    // The subagent's foreground Bash (is_backgrounded: false) is part of the agent's work.
    assert!(!task_history(&events).contains_key("bhee2v97d"));
    assert_eq!(
        turn_triggers(&events),
        vec![None, Some(TurnTrigger::BackgroundTask)]
    );
}

/// s4: a workflow (its approvals from its agents, the agents' progress), an interrupt of a user
/// turn that spares it (perTaskStopAffordance), then stop_task.
#[tokio::test]
async fn replays_a_workflow_across_an_interrupt() {
    let Replayed {
        events,
        sends,
        stops,
    } = replay("bg_workflow_interrupt_stop.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert!(stops.iter().all(Result::is_ok), "{stops:?}");
    assert_well_formed(&events);
    let workflow = "w9vtkqfpg";
    let item = completed(&events)
        .into_iter()
        .find(|(k, _, _)| k == "tool:toolu_01Uvf32uYRQcMFwQDQ1NKT4V")
        .unwrap();
    match &item.1 {
        ItemBody::ToolCall {
            category, title, ..
        } => {
            assert_eq!(*category, ToolCategory::Subagent);
            assert_eq!(title, "Workflow: parallel-ping-test");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(item.2, ItemStatus::Backgrounded);
    let history = task_history(&events).remove(workflow).unwrap();
    assert_eq!(history[0].kind, BackgroundTaskKind::Workflow);
    // Both agents of the workflow, merged by index.
    let agents = history
        .iter()
        .rev()
        .find_map(|t| t.progress.as_ref().filter(|p| !p.workflow.is_empty()))
        .unwrap()
        .workflow
        .clone();
    assert_eq!(agents.len(), 2, "{agents:?}");
    assert!(
        agents
            .iter()
            .all(|a| a.label.starts_with("Run the Bash command"))
    );
    // The agents' permission requests belong to the workflow.
    let keys: Vec<Option<String>> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::InteractionRequested {
                request_id,
                background_key,
                ..
            } if !request_id.starts_with("069117db") => Some(background_key.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        vec![Some(workflow.to_owned()), Some(workflow.to_owned())]
    );
    // The interrupted user turn ended while the workflow went on running.
    let interrupted = position(&events, |e| {
        matches!(
            e,
            AdapterEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        )
    });
    let at_interrupt = events[..interrupted]
        .iter()
        .rev()
        .find_map(|e| match e {
            AdapterEvent::BackgroundTask { task } if task.key == workflow => Some(task.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (at_interrupt.state, at_interrupt.live),
        (BackgroundState::Running, true)
    );
    let last = history.last().unwrap();
    assert_eq!((last.state, last.live), (BackgroundState::Stopped, false));
    // The workflow agents' foreground commands and the turn's own Bash are never shown.
    for hidden in ["bvr4txt3p", "btvbmlxw9", "bdxc3nf4d"] {
        assert!(!task_history(&events).contains_key(hidden), "{hidden}");
    }
    assert_eq!(
        turn_statuses(&events),
        vec![TurnStatus::Completed, TurnStatus::Interrupted]
    );
    match events.last() {
        Some(AdapterEvent::Exited { info }) => assert_eq!(info.code, Some(1)),
        other => panic!("{other:?}"),
    }
}

/// w4: CronCreate (one-shot and recurring), the wakeups coming due, CronList and CronDelete;
/// the pending set from the Stop hook.
#[tokio::test]
async fn replays_scheduled_wakeups() {
    let Replayed { events, sends, .. } = replay("scheduled_crons.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let once = "cron:2259ec69";
    let every = "cron:47a7861e";
    let first = &task_history(&events)[once][0];
    assert_eq!(first.kind, BackgroundTaskKind::Scheduled);
    assert_eq!(first.title, "Reply with exactly: cron-fired");
    assert!(first.live && !first.stoppable);
    assert!(
        first
            .origin_item_key
            .as_deref()
            .is_some_and(|k| k.starts_with("tool:")),
        "{first:?}"
    );
    let created = completed(&events)
        .into_iter()
        .find(|(k, _, _)| Some(k.as_str()) == first.origin_item_key.as_deref())
        .unwrap();
    assert!(matches!(&created.1, ItemBody::ToolCall { name, .. } if name == "CronCreate"));
    assert_eq!(created.2, ItemStatus::Backgrounded);
    // It came due, and the list at the end of that run no longer holds it.
    let fired = last_state(&events, once);
    assert_eq!(
        (fired.state, fired.live),
        (BackgroundState::Completed, false)
    );
    // The recurring one keeps the schedule the CLI wrote for people; CronDelete stops it.
    let recurring = task_history(&events).remove(every).unwrap();
    assert_eq!(
        recurring[0].progress.as_ref().unwrap().summary.as_deref(),
        Some("Every minute")
    );
    assert!(
        recurring
            .iter()
            .all(|t| t.progress.as_ref().unwrap().summary.as_deref() == Some("Every minute"))
    );
    let deleted = recurring.last().unwrap();
    assert_eq!(
        (deleted.state, deleted.live),
        (BackgroundState::Stopped, false)
    );
    // Three user turns and two runs the wakeups started, which the CLI marks with nothing
    // explicit (no trigger).
    assert_eq!(turn_triggers(&events), vec![None; 5]);
    assert_eq!(sends.len(), 3);
}

/// r1a: the user's message arrives while the CLI starts a run for a finished task. The adapter
/// withdraws it (cancel_async_message), reports TurnInProgress after the run's TurnStarted,
/// and the message sent again after the run is the user's turn.
#[tokio::test]
async fn replays_a_message_racing_a_run_the_cli_starts() {
    let Replayed { events, sends, .. } = replay("race_notification_turn.jsonl").await;
    assert!(matches!(
        sends.as_slice(),
        [Ok(()), Err(AdapterError::TurnInProgress), Ok(())]
    ));
    assert_well_formed(&events);
    assert_eq!(
        turn_triggers(&events),
        vec![None, Some(TurnTrigger::BackgroundTask), None]
    );
    let items = completed(&events);
    let messages: Vec<&str> = items
        .iter()
        .filter_map(|(_, b, _)| match b {
            ItemBody::AgentMessage { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages.last().copied(),
        Some("second-message"),
        "the answer belongs to the re-sent message's turn"
    );
    assert_eq!(
        last_state(&events, "bgdxprbd7").state,
        BackgroundState::Completed
    );
}

/// u4: ultracode applied with apply_flag_settings and confirmed by get_settings; the user's
/// message carries origin human, which the result echoes.
#[tokio::test]
async fn replays_an_ultracode_turn() {
    let Replayed { events, sends, .. } = replay("ultracode_turn.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    assert!(events.iter().any(
        |e| matches!(e, AdapterEvent::SessionInfo { effort: Some(e), .. } if e == "ultracode")
    ));
    assert_eq!(turn_triggers(&events), vec![None]);
    assert_eq!(turn_statuses(&events), vec![TurnStatus::Completed]);
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

/// `system/init` of a CLI that reports `command_lifecycle` frames.
fn init_with_lifecycle() -> Value {
    json!({"dir": "out", "msg": {"type": "system", "subtype": "init", "session_id": "replay",
        "capabilities": ["interrupt_receipt_v1", "msg_lifecycle_v1"]}})
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

async fn next_matching(
    h: &mut Harness,
    seen: &mut Vec<AdapterEvent>,
    pred: impl Fn(&AdapterEvent) -> bool,
) {
    loop {
        let ev = tokio::time::timeout(STEP_TIMEOUT, h.events.recv())
            .await
            .unwrap()
            .expect("stream closed");
        let hit = pred(&ev);
        seen.push(ev);
        if hit {
            return;
        }
    }
}

#[tokio::test]
async fn process_exit_mid_turn_reports_exit_without_completing() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u1", "message": {"role": "user", "content": "hi"}}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "init", "session_id": "s1", "model": "m", "permissionMode": "default"}}),
        json!({"dir": "out", "msg": {"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "message_start", "message": {"id": "m1"}}}}),
        json!({"dir": "out", "msg": {"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}}}),
        json!({"dir": "out", "msg": {"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    // A CLI without lifecycle frames: the next run takes the message.
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
    assert!(matches!(
        h.session.send(TurnInput::text("again")).await,
        Err(AdapterError::Closed)
    ));
}

#[tokio::test]
async fn error_result_fails_the_turn_and_cancel_withdraws_requests() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u1", "message": {"role": "user", "content": "go"}}}),
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
    next_matching(&mut h, &mut events, |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
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
            ..
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
    // A withdrawn request can no longer be answered, nor expired.
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
    assert!(matches!(
        h.session
            .expire_request("cu1", ExpireReason::TurnEnded)
            .await,
        Err(AdapterError::UnknownRequest(_))
    ));
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
            "origin": {"kind": "task-notification"}, "usage": {"input_tokens": 1, "output_tokens": 1}}}),
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
    // Only the run whose result says why it started has a trigger.
    assert_eq!(
        turn_triggers(&events),
        vec![Some(TurnTrigger::BackgroundTask), None]
    );
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
async fn send_during_a_run_the_cli_started_reports_turn_in_progress() {
    let mut script = init_exchange();
    script.extend([
        init_with_lifecycle(),
        json!({"dir": "out", "msg": {"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m1",
            "content": [{"type": "text", "text": "working"}]}}}),
        // The next thing the adapter writes is the interrupt: `send` wrote nothing.
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "i1", "request": {"subtype": "interrupt"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "i1", "response": {"still_queued": []}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    let mut seen = Vec::new();
    next_matching(&mut h, &mut seen, |e| {
        matches!(e, AdapterEvent::ItemCompleted { .. })
    })
    .await;
    assert!(matches!(
        h.session.send(TurnInput::text("hi")).await,
        Err(AdapterError::TurnInProgress)
    ));
    h.session.interrupt().await.unwrap();
    collect_until_exit(&mut h).await;
}

/// The CLI took the message into the run it started by itself (at a tool boundary) before
/// the withdrawal reached it (`cancelled: false`, then `started` during that run): the run is
/// the user's turn.
#[tokio::test]
async fn a_message_folded_into_the_cli_run_makes_that_run_the_users_turn() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "U", "message": {"role": "user", "content": "hi"}}}),
        json!({"dir": "out", "msg": {"type": "command_lifecycle", "command_uuid": "U", "state": "queued"}}),
        init_with_lifecycle(),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "c1", "request": {"subtype": "cancel_async_message", "message_uuid": "U"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "c1", "response": {"cancelled": false}}}}),
        json!({"dir": "out", "msg": {"type": "command_lifecycle", "command_uuid": "U", "state": "started"}}),
        json!({"dir": "out", "msg": {"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m1",
            "content": [{"type": "text", "text": "hello"}]}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.1,
            "origin": {"kind": "task-notification"}, "user_message_uuid": "U", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "ctx1",
            "request": {"subtype": "get_context_usage", "detail": "summary"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "ctx1",
            "response": {"totalTokens": 1, "maxTokens": 2, "rawMaxTokens": 2, "percentage": 50}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("hi")).await.unwrap();
    let events = collect_until_exit(&mut h).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AdapterEvent::TurnStarted))
            .count(),
        1
    );
    // The user's turn, not a run the CLI started by itself.
    assert_eq!(turn_triggers(&events), vec![None]);
}

#[tokio::test]
async fn a_message_the_cli_refuses_fails_the_send() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "U", "message": {"role": "user", "content": "hi"}}}),
        json!({"dir": "out", "msg": {"type": "command_lifecycle", "command_uuid": "U", "state": "refused"}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    match h.session.send(TurnInput::text("hi")).await {
        Err(AdapterError::Harness(m)) => assert!(m.contains("refused"), "{m}"),
        other => panic!("{other:?}"),
    }
    collect_until_exit(&mut h).await;
}

/// `get_settings` as Claude Code 2.1.283 answers it (u1/u5 recordings).
fn settings_reply(id: &str, model: &str, effort: Option<&str>, ultracode: bool) -> Value {
    json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": id,
        "response": {"effective.effortLevel": "medium", "effective.ultracode": null, "effective.model": "opus",
            "applied": {"model": model, "effort": effort, "advisor": null, "ultracode": ultracode},
            "flagSettings": [{"source": "flagSettings", "settings": {}}]}}}})
}

fn ctl(id: &str, request: Value) -> [Value; 2] {
    [
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": id, "request": request}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": id}}}),
    ]
}

#[tokio::test]
async fn ultracode_is_confirmed_by_reading_back_the_settings() {
    let mut script = init_exchange();
    // On: applied and confirmed.
    script.extend(ctl(
        "a1",
        json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": "ultracode"}}),
    ));
    script.push(json!({"dir": "in", "msg": {"type": "control_request", "request_id": "g1", "request": {"subtype": "get_settings"}}}));
    script.push(settings_reply("g1", "claude-sonnet-5", Some("xhigh"), true));
    // A model without xhigh: the CLI switches silently, the readback says ultracode is off.
    script.extend(ctl("m1", json!({"subtype": "set_model", "model": "haiku"})));
    script.push(json!({"dir": "in", "msg": {"type": "control_request", "request_id": "g2", "request": {"subtype": "get_settings"}}}));
    script.push(settings_reply(
        "g2",
        "claude-haiku-4-5-20251001",
        None,
        false,
    ));
    // Back to a model with xhigh: on again.
    script.extend(ctl(
        "m2",
        json!({"subtype": "set_model", "model": "sonnet"}),
    ));
    script.push(json!({"dir": "in", "msg": {"type": "control_request", "request_id": "g3", "request": {"subtype": "get_settings"}}}));
    script.push(settings_reply("g3", "claude-sonnet-5", Some("xhigh"), true));
    // Leaving ultracode for the CLI's default clears the flag too, and is confirmed.
    script.extend(ctl("a2", json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": null, "ultracode": false}})));
    script.push(json!({"dir": "in", "msg": {"type": "control_request", "request_id": "g4", "request": {"subtype": "get_settings"}}}));
    script.push(settings_reply(
        "g4",
        "claude-sonnet-5",
        Some("medium"),
        false,
    ));
    // Other levels are applied as before, without a readback.
    script.extend(ctl(
        "a3",
        json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": "high"}}),
    ));
    let mut settings = ThreadSettings {
        model: Some("sonnet".into()),
        effort: None,
        permission_mode: Some("default".into()),
    };
    let mut h = start_with(synthetic(&script), settings.clone());
    h.session.initialize().await.unwrap();
    settings.effort = Some("ultracode".into());
    h.session.apply_settings(&settings).await.unwrap();
    settings.model = Some("haiku".into());
    match h.session.apply_settings(&settings).await {
        Err(AdapterError::Harness(m)) => {
            assert!(m.contains("did not turn ultracode on"), "{m}")
        }
        other => panic!("{other:?}"),
    }
    settings.model = Some("sonnet".into());
    h.session.apply_settings(&settings).await.unwrap();
    settings.effort = None;
    h.session.apply_settings(&settings).await.unwrap();
    settings.effort = Some("high".into());
    h.session.apply_settings(&settings).await.unwrap();
    let events = collect_until_exit(&mut h).await;
    let efforts: Vec<Option<String>> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::SessionInfo {
                effort,
                model: None,
                permission_mode: None,
            } => Some(effort.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        efforts,
        vec![
            Some("ultracode".into()),
            None,
            Some("ultracode".into()),
            Some("medium".into())
        ]
    );
}

#[tokio::test]
async fn a_session_started_with_ultracode_confirms_it() {
    let mut script = init_exchange();
    script.push(json!({"dir": "in", "msg": {"type": "control_request", "request_id": "g1", "request": {"subtype": "get_settings"}}}));
    script.push(settings_reply(
        "g1",
        "claude-haiku-4-5-20251001",
        None,
        false,
    ));
    let mut h = start_with(
        synthetic(&script),
        ThreadSettings {
            model: Some("haiku".into()),
            effort: Some("ultracode".into()),
            permission_mode: None,
        },
    );
    match h.session.initialize().await {
        Err(AdapterError::Harness(m)) => assert!(m.contains("claude-haiku"), "{m}"),
        other => panic!("{other:?}"),
    }
    h.session.shutdown(StopReason::Shutdown).await;
    collect_until_exit(&mut h).await;
}

#[tokio::test]
async fn expired_requests_are_denied_with_the_reason() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "out", "msg": {"type": "control_request", "request_id": "cu1", "request": {"subtype": "can_use_tool",
            "tool_name": "Bash", "input": {"command": "ls"}, "tool_use_id": "t1", "agent_id": "a1"}}}),
        json!({"dir": "in", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "cu1",
            "response": {"behavior": "deny", "message": "The request expired unanswered: the background task that asked ended before the user answered."}}}}),
        json!({"dir": "out", "msg": {"type": "control_request", "request_id": "cu2", "request": {"subtype": "can_use_tool",
            "tool_name": "AskUserQuestion", "input": {"questions": [{"question": "Which?", "options": [{"label": "A"}]}]}, "tool_use_id": "t2"}}}),
        json!({"dir": "in", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "cu2",
            "response": {"behavior": "deny", "message": "The request expired unanswered: the turn it belonged to ended before the user answered."}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    let mut seen = Vec::new();
    next_matching(&mut h, &mut seen, |e| matches!(e, AdapterEvent::InteractionRequested { request_id, .. } if request_id == "cu1")).await;
    // An agent the CLI never reported: the request is not the turn's.
    assert!(seen.iter().any(|e| matches!(e, AdapterEvent::InteractionRequested { request_id, background_key: Some(k), .. } if request_id == "cu1" && k == "a1")));
    h.session
        .expire_request("cu1", ExpireReason::TaskEnded)
        .await
        .unwrap();
    next_matching(&mut h, &mut seen, |e| matches!(e, AdapterEvent::InteractionRequested { request_id, .. } if request_id == "cu2")).await;
    h.session
        .expire_request("cu2", ExpireReason::TurnEnded)
        .await
        .unwrap();
    // Answered once.
    assert!(matches!(
        h.session
            .expire_request("cu2", ExpireReason::TurnEnded)
            .await,
        Err(AdapterError::UnknownRequest(_))
    ));
    collect_until_exit(&mut h).await;
}

fn context_exchange(id: &str) -> [Value; 2] {
    [
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": id,
            "request": {"subtype": "get_context_usage", "detail": "summary"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": id,
            "response": {"totalTokens": 30000, "maxTokens": 200000, "rawMaxTokens": 200000, "percentage": 15}}}}),
    ]
}

fn lifecycle(uuid: &str, state: &str) -> Value {
    json!({"dir": "out", "msg": {"type": "command_lifecycle", "command_uuid": uuid, "state": state}})
}

fn text_message(id: &str, text: &str) -> Value {
    json!({"dir": "out", "msg": {"type": "assistant", "parent_tool_use_id": null,
        "message": {"id": id, "content": [{"type": "text", "text": text}]}}})
}

/// A run that ends normally with the Stop hook listing a one-shot wakeup and a recurring one
/// (the shapes of recordings w1 and w4). The CLI enqueued nothing of its own.
fn run_listing_wakeups(uuid: &str) -> Vec<Value> {
    let mut lines = vec![
        json!({"dir": "in", "msg": {"type": "user", "uuid": uuid, "message": {"role": "user", "content": "schedule"}}}),
        lifecycle(uuid, "queued"),
        lifecycle(uuid, "started"),
        init_with_lifecycle(),
        text_message("m1", "scheduled"),
        json!({"dir": "out", "msg": {"type": "control_request", "request_id": "h1", "request": {
            "subtype": "hook_callback", "callback_id": crate::session::STOP_HOOK_ID, "input": {
                "hook_event_name": "Stop", "stop_hook_active": false, "background_tasks": [],
                "session_crons": [
                    {"id": "once", "schedule": "48 20 * * *", "recurring": false, "prompt": "Reply with exactly: woke-up"},
                    {"id": "every", "schedule": "* * * * *", "recurring": true, "prompt": "tick"}]}}}}),
        json!({"dir": "in", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "h1", "response": {}}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.1,
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    lines.extend(context_exchange("ctx1"));
    lines.push(lifecycle(uuid, "completed"));
    lines
}

fn wakeup_live(events: &[AdapterEvent], key: &str) -> (bool, BackgroundState) {
    let task = last_state(events, key);
    (task.live, task.state)
}

/// A one-shot wakeup fires as a command the CLI enqueued itself (`started` without `queued`,
/// recorded w1/w3/w4) and leaves the CLI's list. When the run it starts fails (or is
/// interrupted), no Stop hook comes and with it no list: the one-shot wakeup no longer keeps
/// the process (not live), without being ended; the recurring one stays live. An interrupted
/// run of the user's own leaves the list as it was (recorded w3: an interrupted turn keeps the
/// pending wakeup, which fires later).
#[tokio::test]
async fn a_failed_run_after_a_wakeup_fired_stops_trusting_the_one_shot_wakeups() {
    let mut script = init_exchange();
    script.extend(run_listing_wakeups("u1"));
    // The user's own run, interrupted: no Stop hook, but nothing of the CLI's own ran.
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u2", "message": {"role": "user", "content": "second"}}}),
        lifecycle("u2", "queued"),
        lifecycle("u2", "started"),
        init_with_lifecycle(),
        text_message("m2", "working"),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "i1", "request": {"subtype": "interrupt"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "i1", "response": {"still_queued": []}}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "error_during_execution", "is_error": true,
            "terminal_reason": "aborted_tools", "total_cost_usd": 0.2, "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ]);
    script.extend(context_exchange("ctx2"));
    script.push(lifecycle("u2", "cancelled"));
    // The wakeup comes due; its run fails (an API error or a usage limit: no Stop hook).
    script.extend([
        lifecycle("cli-1", "started"),
        init_with_lifecycle(),
        text_message("m3", "woke"),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "error_during_execution", "is_error": true,
            "errors": ["API Error: 529 overloaded"], "total_cost_usd": 0.3, "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ]);
    script.extend(context_exchange("ctx3"));
    script.push(lifecycle("cli-1", "completed"));
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("schedule")).await.unwrap();
    let mut events = Vec::new();
    next_matching(&mut h, &mut events, |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    let running = (true, BackgroundState::Running);
    assert_eq!(wakeup_live(&events, "cron:once"), running);
    assert_eq!(wakeup_live(&events, "cron:every"), running);

    h.session.send(TurnInput::text("second")).await.unwrap();
    next_matching(&mut h, &mut events, |e| {
        matches!(e, AdapterEvent::ItemCompleted { .. })
    })
    .await;
    h.session.interrupt().await.unwrap();
    next_matching(&mut h, &mut events, |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    assert_eq!(
        wakeup_live(&events, "cron:once"),
        running,
        "the user's interrupted run ran no wakeup"
    );

    events.extend(collect_until_exit(&mut h).await);
    assert_eq!(
        turn_statuses(&events),
        vec![
            TurnStatus::Completed,
            TurnStatus::Interrupted,
            TurnStatus::Failed
        ]
    );
    assert_eq!(
        wakeup_live(&events, "cron:once"),
        (false, BackgroundState::Running),
        "whether it fired is not known: not ended, but no longer keeping the process"
    );
    assert_eq!(wakeup_live(&events, "cron:every"), running);
    // Reported before the run's completion.
    let dropped = position(
        &events,
        |e| matches!(e, AdapterEvent::BackgroundTask { task } if task.key == "cron:once" && !task.live),
    );
    let failed = position(&events, |e| {
        matches!(
            e,
            AdapterEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            }
        )
    });
    assert!(dropped < failed);
}

/// After a run that ended without the list, the next list decides: a one-shot wakeup it names
/// is live again (it had not fired), one it does not name has ended.
#[tokio::test]
async fn the_next_list_confirms_or_ends_the_unconfirmed_wakeups() {
    let mut script = init_exchange();
    script.extend(run_listing_wakeups("u1"));
    script.extend([
        // The recurring wakeup fires (the CLI's own command) and that run is interrupted.
        lifecycle("cli-1", "started"),
        init_with_lifecycle(),
        text_message("m2", "tick"),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "i1", "request": {"subtype": "interrupt"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "i1", "response": {"still_queued": []}}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "error_during_execution", "is_error": true,
            "terminal_reason": "aborted_streaming", "total_cost_usd": 0.2, "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ]);
    script.extend(context_exchange("ctx2"));
    script.push(lifecycle("cli-1", "cancelled"));
    // The next run ends normally: the one-shot wakeup is still pending.
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u3", "message": {"role": "user", "content": "third"}}}),
        lifecycle("u3", "queued"),
        lifecycle("u3", "started"),
        init_with_lifecycle(),
        text_message("m3", "done"),
        json!({"dir": "out", "msg": {"type": "control_request", "request_id": "h2", "request": {
            "subtype": "hook_callback", "callback_id": crate::session::STOP_HOOK_ID, "input": {
                "hook_event_name": "Stop", "stop_hook_active": false, "background_tasks": [],
                "session_crons": [
                    {"id": "once", "schedule": "48 20 * * *", "recurring": false, "prompt": "Reply with exactly: woke-up"},
                    {"id": "every", "schedule": "* * * * *", "recurring": true, "prompt": "tick"}]}}}}),
        json!({"dir": "in", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "h2", "response": {}}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.3,
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ]);
    script.extend(context_exchange("ctx3"));
    script.push(lifecycle("u3", "completed"));
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("schedule")).await.unwrap();
    let mut events = Vec::new();
    next_matching(&mut h, &mut events, |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    // The CLI's own run: interrupt it once its message is out.
    next_matching(&mut h, &mut events, |e| {
        matches!(e, AdapterEvent::ItemCompleted { .. })
    })
    .await;
    h.session.interrupt().await.unwrap();
    next_matching(&mut h, &mut events, |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    assert_eq!(
        wakeup_live(&events, "cron:once"),
        (false, BackgroundState::Running)
    );
    assert_eq!(
        wakeup_live(&events, "cron:every"),
        (true, BackgroundState::Running)
    );
    h.session.send(TurnInput::text("third")).await.unwrap();
    events.extend(collect_until_exit(&mut h).await);
    assert_eq!(
        wakeup_live(&events, "cron:once"),
        (true, BackgroundState::Running),
        "the list names it again"
    );
    assert_eq!(last_state(&events, "cron:once").runs, 1, "the same run");
}

#[tokio::test]
async fn a_scheduled_wakeup_is_not_stopped_with_stop_task() {
    let mut h = start(synthetic(&init_exchange()));
    h.session.initialize().await.unwrap();
    assert!(matches!(
        h.session.stop_background("cron:abc").await,
        Err(AdapterError::Other(_))
    ));
    collect_until_exit(&mut h).await;
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
    assert_eq!(
        crate::session::user_message(json!("plain"), "u-1"),
        json!({"type": "user", "session_id": "", "message": {"role": "user", "content": "plain"},
            "parent_tool_use_id": null, "uuid": "u-1", "origin": {"kind": "human"}})
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
    let (session, _events) = ClaudeSession::start(
        adapter_in,
        adapter_out,
        ProcessLink::Manual(exit_rx),
        SessionParams {
            stop_grace: Duration::from_millis(200),
            ..params(tmp.path(), ThreadSettings::default())
        },
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
    // The CLI takes the message into a run and then answers nothing any more (a wedged event
    // loop).
    let (adapter_in, mut cli_out) = tokio::io::duplex(1 << 16);
    let (cli_in, adapter_out) = tokio::io::duplex(1 << 16);
    let cli = tokio::spawn(async move {
        let mut lines = BufReader::new(cli_in).lines();
        let mut seen = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["type"] == "user" {
                let started = json!({"type": "command_lifecycle", "command_uuid": msg["uuid"], "state": "started"});
                let mut bytes = serde_json::to_vec(&started).unwrap();
                bytes.push(b'\n');
                cli_out.write_all(&bytes).await.unwrap();
            }
            seen.push(msg);
        }
        seen
    });
    let (_exit_tx, exit_rx) = watch::channel(None);
    let tmp = tempfile::tempdir().unwrap();
    let stop_grace = Duration::from_millis(300);
    let (session, _events) = ClaudeSession::start(
        adapter_in,
        adapter_out,
        ProcessLink::Manual(exit_rx),
        SessionParams {
            stop_grace,
            ..params(tmp.path(), ThreadSettings::default())
        },
    );
    session.send(TurnInput::text("hi")).await.unwrap();
    let asked = tokio::time::Instant::now();
    let err = session.interrupt().await.unwrap_err();
    let waited = asked.elapsed();
    assert!(
        matches!(&err, AdapterError::Protocol(m) if m.contains("interrupt")),
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
