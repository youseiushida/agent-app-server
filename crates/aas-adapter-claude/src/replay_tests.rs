//! Replays recorded Claude Code sessions (`tests/fixtures/*.jsonl`) against the protocol
//! core over in-memory pipes.
//!
//! A fixture is the (sanitized) exchange with the real CLI: `{"dir":"in"|"out"|"exit",
//! "msg":…}`, and `"act":"steer"` on a user message the recorder sent while a turn ran. The fake CLI writes every `out` line and, at every `in` line, reads what the
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
    ThreadModes, ThreadSettings, ToolCategory, TurnStatus, TurnTrigger,
};
use aas_harness::{
    AdapterError, AdapterEvent, BackgroundState, BackgroundTaskInfo, BackgroundTaskKind, ExitInfo,
    SessionControl, SideAnswer, StatusSection, StopReason, TurnInput, TurnInputPart,
};
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, watch};

use crate::session::{ClaudeSession, ProcessLink, SessionParams};

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct Line {
    dir: String,
    msg: Value,
    /// How the driver reproduces an `in` line when its kind alone does not say (`steer`).
    act: Option<String>,
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
                act: v["act"].as_str().map(str::to_owned),
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
    for key in [
        "mode",
        "model",
        "settings",
        "detail",
        "task_id",
        "title",
        "source",
        "session_id",
        "question",
        "tool_use_id",
        "skip_behaviors",
    ] {
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
        command_cache: crate::commands::CommandCache::default(),
        agent_progress_summaries: None,
        max_output_file_bytes: aas_harness::AdapterPolicy::default().max_output_file_bytes,
    }
}

fn start(script: Vec<Line>) -> Harness {
    start_with(script, ThreadSettings::default())
}

/// The content the replays give the output file of task `task_id` (the recordings do not hold
/// the files the CLI wrote).
fn recorded_output(task_id: &str) -> String {
    format!("output of {task_id}\n[exited with code 0]\n")
}

/// Points every non-empty `task_notification.output_file` of the script at a file in `dir`
/// holding [`recorded_output`] (the recorded paths are the recording PC's).
fn with_output_files(script: Vec<Line>, dir: &std::path::Path) -> Vec<Line> {
    let files = dir.join("task-outputs");
    std::fs::create_dir_all(&files).unwrap();
    script
        .into_iter()
        .map(|mut line| {
            if line.dir == "out"
                && line.msg["subtype"] == "task_notification"
                && line.msg["output_file"]
                    .as_str()
                    .is_some_and(|f| !f.is_empty())
            {
                let task = line.msg["task_id"].as_str().unwrap_or("task").to_owned();
                let path = files.join(format!("{task}.output"));
                std::fs::write(&path, recorded_output(&task)).unwrap();
                line.msg["output_file"] = Value::String(path.display().to_string());
            }
            line
        })
        .collect()
}

/// Like [`start`], with the script's output files as they are (not rewritten).
fn start_raw(script: Vec<Line>) -> Harness {
    start_in(
        script,
        ThreadSettings::default(),
        tempfile::tempdir().unwrap(),
    )
}

fn start_with(script: Vec<Line>, settings: ThreadSettings) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let script = with_output_files(script, tmp.path());
    start_in(script, settings, tmp)
}

fn start_in(script: Vec<Line>, settings: ThreadSettings, tmp: tempfile::TempDir) -> Harness {
    let (adapter_in, fake_out) = tokio::io::duplex(1 << 20);
    let (fake_in, adapter_out) = tokio::io::duplex(1 << 20);
    let (exit_tx, exit_rx) = watch::channel(None);
    let (progress_tx, progress) = watch::channel(Progress::default());
    let fake = tokio::spawn(fake_cli(script, fake_in, fake_out, exit_tx, progress_tx));
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
    /// The engine's ids of the steered messages, and each `steer_message`'s outcome, in order.
    steers: Vec<(String, Result<(), AdapterError>)>,
    /// Every `status`, `rename`, `side_question` and `move_to_background`, in order.
    statuses: Vec<Result<Vec<StatusSection>, AdapterError>>,
    renames: Vec<Result<(), AdapterError>>,
    answers: Vec<Result<SideAnswer, AdapterError>>,
    moves: Vec<Result<(), AdapterError>>,
}

/// Pumps the adapter's events while the driver waits for a point of the recording.
struct Driver {
    h: Harness,
    events: Vec<AdapterEvent>,
    open_turns: i64,
    requests: HashSet<String>,
    tasks: HashSet<String>,
    /// Items the adapter reported backgroundable.
    backgroundable: HashSet<String>,
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
            AdapterEvent::ItemBackgroundable {
                key,
                backgroundable: true,
            } => {
                self.backgroundable.insert(key.clone());
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
        backgroundable: HashSet::new(),
    };
    assert_eq!(inputs[0].msg["request"]["subtype"], "initialize");
    d.h.session.initialize().await.unwrap();

    let mut settings = ThreadSettings {
        permission_mode: Some("default".into()),
        ..start_settings
    };
    let mut modes = ThreadModes::default();
    let mut sends = Vec::new();
    let mut steers = Vec::new();
    let mut spawned_stops = Vec::new();
    let mut interrupts = Vec::new();
    let mut statuses = Vec::new();
    let mut renames = Vec::new();
    let mut answers = Vec::new();
    let mut moves = Vec::new();
    for (index, line) in inputs.iter().enumerate().skip(1) {
        let msg = &line.msg;
        match (
            msg["type"].as_str().unwrap(),
            msg["request"]["subtype"].as_str(),
        ) {
            ("user", _) if line.act.as_deref() == Some("steer") => {
                d.wait(index, "a steer", |d| d.open_turns > 0).await;
                let message_id = format!("steer-{index}");
                let steered =
                    d.h.session
                        .steer_message(
                            &message_id,
                            input_from(&msg["message"]["content"], d.h.tmp.path()),
                        )
                        .await;
                steers.push((message_id, steered));
            }
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
            // Written by the adapter by itself (`get_usage` and `get_plan` by `status`).
            (
                "control_request",
                Some(
                    "get_context_usage"
                    | "get_settings"
                    | "cancel_async_message"
                    | "get_usage"
                    | "get_plan",
                ),
            ) => {}
            ("control_request", Some("get_status")) => {
                d.wait(index, "a status", |_| true).await;
                let s = d.h.session.clone();
                statuses.push(tokio::spawn(async move { s.status().await }));
            }
            ("control_request", Some("rename_session")) => {
                d.wait(index, "a rename", |d| d.open_turns == 0).await;
                let title = msg["request"]["title"].as_str().unwrap().to_owned();
                renames.push(d.h.session.rename(&title).await);
            }
            ("control_request", Some("side_question")) => {
                d.wait(index, "a side question", |_| true).await;
                let question = msg["request"]["question"].as_str().unwrap().to_owned();
                let s = d.h.session.clone();
                answers.push(tokio::spawn(
                    async move { s.side_question(&question).await },
                ));
            }
            ("control_request", Some("background_tasks")) => {
                let key = format!("tool:{}", msg["request"]["tool_use_id"].as_str().unwrap());
                d.wait(index, "work to move to the background", |d| {
                    d.backgroundable.contains(&key)
                })
                .await;
                let s = d.h.session.clone();
                moves.push(tokio::spawn(
                    async move { s.move_to_background(&key).await },
                ));
            }
            ("control_request", Some("set_permission_mode"))
                if msg["request"]["mode"] == "plan" =>
            {
                d.wait(index, "plan mode", |d| d.open_turns == 0).await;
                modes.plan = true;
                d.h.session.apply_modes(&modes).await.unwrap();
            }
            ("control_request", Some("apply_flag_settings"))
                if msg["request"]["settings"]["fastMode"].is_boolean() =>
            {
                d.wait(index, "fast mode", |d| d.open_turns == 0).await;
                modes.fast = msg["request"]["settings"]["fastMode"] == true;
                d.h.session.apply_modes(&modes).await.unwrap();
            }
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
                // The engine takes over the permission mode the CLI reports (design.md 5.5).
                if let Some(mode) = reported_permission_mode(&d.events) {
                    settings.permission_mode = Some(mode);
                }
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
    let mut status_results = Vec::new();
    for s in statuses {
        status_results.push(s.await.unwrap());
    }
    let mut answer_results = Vec::new();
    for a in answers {
        answer_results.push(a.await.unwrap());
    }
    let mut move_results = Vec::new();
    for m in moves {
        move_results.push(m.await.unwrap());
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
        steers,
        statuses: status_results,
        renames,
        answers: answer_results,
        moves: move_results,
    }
}

/// The permission mode the CLI reported last (`SessionInfo`).
fn reported_permission_mode(events: &[AdapterEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        AdapterEvent::SessionInfo {
            permission_mode: Some(mode),
            ..
        } => Some(mode.clone()),
        _ => None,
    })
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
    // "Allow for this session" returned Claude Code's `setMode acceptEdits` suggestion: the
    // CLI switched and said so (`system/status`), and the switch is reported for the thread.
    // The recorder's own switch to acceptEdits afterwards was dropped from the fixture: the CLI
    // was in that mode already, so the adapter sends nothing (docs/adapters/claude.md §18).
    let modes: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::SessionInfo {
                permission_mode: Some(m),
                ..
            } => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(modes, ["default", "acceptEdits"]);
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
        ..
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
        ..
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

/// w1: `ScheduleWakeup` schedules a wakeup (its result says when it comes due, not its id). The
/// task is live from that result, the item goes on as it, the Stop hook's list names the same
/// wakeup (no second task for its entry), and when it came due the list at the end of the run
/// it started no longer holds it: it completed.
#[tokio::test]
async fn replays_a_schedule_wakeup_that_fires() {
    let Replayed { events, sends, .. } = replay("schedule_wakeup_fire.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let key = "wakeup:toolu_018MmP8CaLmXXaaJjuuYeA8i";
    let history = task_history(&events);
    let first = &history[key][0];
    assert_eq!(
        (
            first.kind,
            first.title.as_str(),
            first.live,
            first.stoppable,
            first.next_run_at
        ),
        (
            BackgroundTaskKind::Scheduled,
            "Reply with exactly: woke-up",
            true,
            false,
            Some(1_790_596_080_000)
        )
    );
    assert_eq!(
        first.progress.as_ref().and_then(|p| p.summary.as_deref()),
        Some("recording test"),
        "the reason the model gave, for people"
    );
    let item = first
        .origin_item_key
        .clone()
        .expect("the ScheduleWakeup item");
    let (_, body, status) = completed(&events)
        .into_iter()
        .find(|(k, _, _)| *k == item)
        .unwrap();
    assert!(matches!(&body, ItemBody::ToolCall { name, .. } if name == "ScheduleWakeup"));
    assert_eq!(status, ItemStatus::Backgrounded);
    let task_at = position(
        &events,
        |e| matches!(e, AdapterEvent::BackgroundTask { task } if task.key == key),
    );
    let item_at = position(
        &events,
        |e| matches!(e, AdapterEvent::ItemCompleted { key: k, .. } if *k == item),
    );
    assert!(
        task_at < item_at,
        "the task before the item that goes on as it"
    );
    assert!(
        !history.contains_key("cron:180719e1"),
        "the list's entry is the same wakeup: {:?}",
        history.keys()
    );
    // Live through the end of the turn that scheduled it (its list named it).
    let first_end = position(&events, |e| matches!(e, AdapterEvent::TurnCompleted { .. }));
    let at_first_end = events[..first_end]
        .iter()
        .rev()
        .find_map(|e| match e {
            AdapterEvent::BackgroundTask { task } if task.key == key => Some(task.clone()),
            _ => None,
        })
        .unwrap();
    assert!(at_first_end.live && at_first_end.state == BackgroundState::Running);
    let fired = last_state(&events, key);
    assert_eq!(
        (fired.state, fired.live),
        (BackgroundState::Completed, false)
    );
    // The run it started is the CLI's own, which the CLI marks with nothing explicit.
    assert_eq!(turn_triggers(&events), vec![None, None]);
}

/// w3: the wakeup a turn scheduled stays live while the user's next turn is interrupted (no
/// Stop hook, and nothing of the CLI's own ran): it still comes due, and the run it starts
/// ends it. The interrupted turn's foreground command is never a task.
#[tokio::test]
async fn replays_a_wakeup_that_outlives_an_interrupted_turn() {
    let Replayed { events, sends, .. } = replay("schedule_wakeup_interrupt.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let key = "wakeup:toolu_01WU7BWawSfQMEuioHd3Tw86";
    assert_eq!(
        turn_statuses(&events),
        vec![
            TurnStatus::Completed,
            TurnStatus::Interrupted,
            TurnStatus::Completed
        ]
    );
    let interrupted = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, AdapterEvent::TurnCompleted { .. }))
        .nth(1)
        .map(|(i, _)| i)
        .unwrap();
    let before = events[..interrupted]
        .iter()
        .rev()
        .find_map(|e| match e {
            AdapterEvent::BackgroundTask { task } if task.key == key => Some(task.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (before.live, before.state, before.next_run_at),
        (true, BackgroundState::Running, Some(1_790_596_260_000))
    );
    assert!(
        !events[interrupted..]
            .iter()
            .take_while(|e| !matches!(e, AdapterEvent::TurnStarted))
            .any(|e| matches!(e, AdapterEvent::BackgroundTask { task } if task.key == key)),
        "the interrupted turn changed nothing about it"
    );
    let history = task_history(&events);
    assert!(
        !history.contains_key("cron:925f09a0"),
        "{:?}",
        history.keys()
    );
    assert!(!history.contains_key("b74x16gn7"), "a foreground command");
    let fired = last_state(&events, key);
    assert_eq!(
        (fired.state, fired.live),
        (BackgroundState::Completed, false)
    );
}

/// A shell task's output is the file its `task_notification` names (recorded f1b); an
/// agent's file is its transcript, not output (recorded f2).
#[tokio::test]
async fn a_shell_tasks_output_is_the_file_its_end_names() {
    let Replayed { events, .. } = replay("background_bash.jsonl").await;
    let shell = last_state(&events, "b0psjkcj9");
    assert_eq!(shell.kind, BackgroundTaskKind::Shell);
    let result = shell.result.expect("the end's result");
    assert_eq!(
        result.output.as_deref(),
        Some(recorded_output("b0psjkcj9").as_str())
    );
    assert_eq!(result.output_omitted_bytes, None);
    let Replayed { events, .. } = replay("background_agent.jsonl").await;
    let agent = last_state(&events, "adddbb491bccbcff1");
    assert_eq!(agent.kind, BackgroundTaskKind::Agent);
    assert_eq!(agent.result.and_then(|r| r.output), None);
}

/// A shell task whose output file cannot be read ends without output, and the user is told.
#[tokio::test]
async fn an_unreadable_output_file_is_reported() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "out", "msg": {"type": "system", "subtype": "task_started", "task_id": "bx",
            "description": "npm run build", "task_type": "local_bash", "is_backgrounded": true}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "task_notification", "task_id": "bx",
            "status": "completed", "output_file": "", "summary": "done"}}),
    ]);
    let mut h = start(synthetic(&script));
    // An output file that is not there (the replay only writes the named ones).
    h.session.initialize().await.unwrap();
    let events = collect_until_exit(&mut h).await;
    let shell = last_state(&events, "bx");
    assert_eq!(shell.state, BackgroundState::Completed);
    assert_eq!(shell.result.and_then(|r| r.output), None, "no file named");

    let missing = h.tmp.path().join("missing.output");
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "out", "msg": {"type": "system", "subtype": "task_started", "task_id": "by",
            "description": "npm run build", "task_type": "local_bash", "is_backgrounded": true}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "task_notification", "task_id": "by",
            "status": "completed", "output_file": missing.display().to_string(), "summary": "done"}}),
    ]);
    // Not rewritten: the path is made to point at a file that does not exist.
    let script: Vec<Line> = synthetic(&script)
        .into_iter()
        .map(|mut l| {
            if l.msg["subtype"] == "task_notification" {
                l.msg["output_file"] = Value::String(missing.display().to_string());
            }
            l
        })
        .collect();
    let mut h2 = start_raw(script);
    h2.session.initialize().await.unwrap();
    let events = collect_until_exit(&mut h2).await;
    let shell = last_state(&events, "by");
    assert_eq!(shell.state, BackgroundState::Completed);
    assert_eq!(shell.result.and_then(|r| r.output), None);
    assert!(events.iter().any(|e| matches!(e,
        AdapterEvent::Notice { level: aas_harness::NoticeLevel::Warning, code: Some(c), message }
            if c == "backgroundOutputUnreadable" && message.contains("by"))));
}

/// The assistant message of a `ScheduleWakeup` call (the shape of recording w1).
fn wakeup_call(id: &str, input: Value) -> Value {
    json!({"dir": "out", "msg": {"type": "assistant", "parent_tool_use_id": null,
        "message": {"id": format!("m-{id}"), "content": [
            {"type": "tool_use", "id": id, "name": "ScheduleWakeup", "input": input}]}}})
}

/// The tool result of a `ScheduleWakeup` call, `result` its structured part (recording w1).
fn wakeup_result(id: &str, result: Value) -> Value {
    json!({"dir": "out", "msg": {"type": "user", "parent_tool_use_id": null,
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id,
            "content": "Next wakeup scheduled."}]},
        "tool_use_result": result}})
}

/// The Stop hook of a run that ends normally, with the CLI's list of pending wakeups.
fn stop_hook(request_id: &str, crons: Value) -> [Value; 2] {
    [
        json!({"dir": "out", "msg": {"type": "control_request", "request_id": request_id, "request": {
            "subtype": "hook_callback", "callback_id": crate::session::STOP_HOOK_ID, "input": {
                "hook_event_name": "Stop", "stop_hook_active": false, "background_tasks": [],
                "session_crons": crons}}}}),
        json!({"dir": "in", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": request_id, "response": {}}}}),
    ]
}

/// A `result` that ends a run normally.
fn success_result(cost: f64) -> Value {
    json!({"dir": "out", "msg": {"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": cost,
        "usage": {"input_tokens": 1, "output_tokens": 1}}})
}

/// A `result` of a run that failed on an API error (no Stop hook came before it).
fn api_error_result(cost: f64) -> Value {
    json!({"dir": "out", "msg": {"type": "result", "subtype": "error_during_execution", "is_error": true,
        "errors": ["API Error: 529 overloaded"], "total_cost_usd": cost, "usage": {"input_tokens": 1, "output_tokens": 1}}})
}

/// A `ScheduleWakeup` in a turn that is interrupted (no Stop hook): its wakeup is live from the
/// result on; the next list names the wakeup under its own id; `ScheduleWakeup {stop: true}`
/// cancels it before it came due.
#[tokio::test]
async fn a_schedule_wakeup_of_an_interrupted_turn_is_live_until_a_signal_ends_it() {
    let due = 4_102_444_800_000i64;
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u1", "message": {"role": "user", "content": "loop"}}}),
        lifecycle("u1", "queued"),
        lifecycle("u1", "started"),
        init_with_lifecycle(),
        wakeup_call("toolu_W", json!({"delaySeconds": 1200, "reason": "check the deploy",
            "prompt": "/loop check the deploy", "noop": false})),
        wakeup_result("toolu_W", json!({"scheduledFor": due, "clampedDelaySeconds": 1200, "wasClamped": false})),
        text_message("m1", "waiting"),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "i1", "request": {"subtype": "interrupt"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "i1", "response": {"still_queued": []}}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "error_during_execution", "is_error": true,
            "terminal_reason": "aborted_streaming", "total_cost_usd": 0.1, "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ]);
    script.extend(context_exchange("ctx1"));
    script.push(lifecycle("u1", "cancelled"));
    // The user's next turn ends normally: the list names the wakeup, and the model stops the
    // loop in the turn after.
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u3", "message": {"role": "user", "content": "status?"}}}),
        lifecycle("u3", "queued"),
        lifecycle("u3", "started"),
        init_with_lifecycle(),
        text_message("m3", "still waiting"),
    ]);
    script.extend(stop_hook(
        "h1",
        json!([{"id": "5f00ba11", "schedule": "0 9 1 1 *", "recurring": false,
            "prompt": "/loop check the deploy"}]),
    ));
    script.push(success_result(0.3));
    script.extend(context_exchange("ctx3"));
    script.push(lifecycle("u3", "completed"));
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u4", "message": {"role": "user", "content": "stop the loop"}}}),
        lifecycle("u4", "queued"),
        lifecycle("u4", "started"),
        init_with_lifecycle(),
        wakeup_call("toolu_S", json!({"stop": true})),
        wakeup_result("toolu_S", json!({"scheduledFor": 0, "clampedDelaySeconds": 0, "wasClamped": false,
            "stopped": true, "cancelledWakeups": 1})),
        text_message("m4", "stopped"),
        success_result(0.4),
    ]);
    script.extend(context_exchange("ctx4"));
    script.push(lifecycle("u4", "completed"));
    let key = "wakeup:toolu_W";
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("loop")).await.unwrap();
    let mut events = Vec::new();
    next_matching(
        &mut h,
        &mut events,
        |e| matches!(e, AdapterEvent::ItemCompleted { key, .. } if key == "tool:toolu_W"),
    )
    .await;
    let scheduled = last_state(&events, key);
    assert_eq!(
        (scheduled.live, scheduled.state, scheduled.next_run_at),
        (true, BackgroundState::Running, Some(due))
    );
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
        wakeup_live(&events, key),
        (true, BackgroundState::Running),
        "the interrupted turn's wakeup is pending"
    );
    h.session.send(TurnInput::text("status?")).await.unwrap();
    next_matching(&mut h, &mut events, |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    assert_eq!(wakeup_live(&events, key), (true, BackgroundState::Running));
    assert!(!task_history(&events).contains_key("cron:5f00ba11"));
    h.session
        .send(TurnInput::text("stop the loop"))
        .await
        .unwrap();
    events.extend(collect_until_exit(&mut h).await);
    assert_eq!(
        wakeup_live(&events, key),
        (false, BackgroundState::Stopped),
        "cancelled before it came due"
    );
    let stop_item = completed(&events)
        .into_iter()
        .find(|(k, _, _)| k == "tool:toolu_S")
        .unwrap();
    assert_eq!(
        stop_item.2,
        ItemStatus::Completed,
        "a call that schedules nothing is not background work"
    );
    assert!(!task_history(&events).contains_key("wakeup:toolu_S"));
}

/// Claude Code's scheduler runs a one-shot wakeup that lands on :00 or :30 up to 90 s before the
/// `scheduledFor` its `ScheduleWakeup` result stated (its `CronCreate` description: "one-shot
/// tasks landing on :00 or :30 fire up to 90 s early"). Here every command of the CLI's own
/// starts before that time (`scheduledFor` is in 2100): the run that reschedules ends the
/// wakeup it ran as fired (not cancelled), the next wakeup's run fails without a list and
/// leaves it unconfirmed (no longer keeping the process), and the next list, which does not
/// hold it, ends it as fired.
#[tokio::test]
async fn a_wakeup_the_cli_runs_before_its_scheduled_time_ends_as_fired() {
    // 2100-01-01T00:00:00Z and 00:30:00Z.
    let due1 = 4_102_444_800_000i64;
    let due2 = due1 + 30 * 60_000;
    let prompt = "/loop check the deploy";
    let call = |id: &str| {
        wakeup_call(
            id,
            json!({"delaySeconds": 1200, "reason": "check the deploy", "prompt": prompt, "noop": false}),
        )
    };
    let mut script = init_exchange();
    // The user's turn schedules the first wakeup; the list at its end names it.
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u1", "message": {"role": "user", "content": "loop"}}}),
        lifecycle("u1", "queued"),
        lifecycle("u1", "started"),
        init_with_lifecycle(),
        call("toolu_1"),
        wakeup_result("toolu_1", json!({"scheduledFor": due1, "clampedDelaySeconds": 1200, "wasClamped": false})),
        text_message("m1", "waiting"),
    ]);
    script.extend(stop_hook(
        "h1",
        json!([{"id": "c1", "schedule": "0 9 1 1 *", "recurring": false, "prompt": prompt}]),
    ));
    script.push(success_result(0.1));
    script.extend(context_exchange("ctx1"));
    script.push(lifecycle("u1", "completed"));
    // The CLI runs it as a command of its own, and the model schedules the next one.
    script.extend([
        lifecycle("cli-1", "started"),
        init_with_lifecycle(),
        call("toolu_2"),
        wakeup_result(
            "toolu_2",
            json!({"scheduledFor": due2, "clampedDelaySeconds": 1200, "wasClamped": false}),
        ),
        text_message("m2", "the deploy is still running"),
    ]);
    script.extend(stop_hook(
        "h2",
        json!([{"id": "c2", "schedule": "30 9 1 1 *", "recurring": false, "prompt": prompt}]),
    ));
    script.push(success_result(0.2));
    script.extend(context_exchange("ctx2"));
    script.push(lifecycle("cli-1", "completed"));
    // The CLI runs the next one; that run fails (no Stop hook, no list).
    script.extend([
        lifecycle("cli-2", "started"),
        init_with_lifecycle(),
        text_message("m3", "checking"),
        api_error_result(0.3),
    ]);
    script.extend(context_exchange("ctx3"));
    script.push(lifecycle("cli-2", "completed"));
    // The user's next turn ends normally; its list holds no wakeup.
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u4", "message": {"role": "user", "content": "status?"}}}),
        lifecycle("u4", "queued"),
        lifecycle("u4", "started"),
        init_with_lifecycle(),
        text_message("m4", "the loop is over"),
    ]);
    script.extend(stop_hook("h4", json!([])));
    script.push(success_result(0.4));
    script.extend(context_exchange("ctx4"));
    script.push(lifecycle("u4", "completed"));

    let (first, second) = ("wakeup:toolu_1", "wakeup:toolu_2");
    let running = (true, BackgroundState::Running);
    let turn_end = |e: &AdapterEvent| matches!(e, AdapterEvent::TurnCompleted { .. });
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("loop")).await.unwrap();
    let mut events = Vec::new();
    next_matching(&mut h, &mut events, turn_end).await;
    assert_eq!(wakeup_live(&events, first), running);
    // The run of the CLI's own that rescheduled.
    next_matching(&mut h, &mut events, turn_end).await;
    assert_eq!(
        wakeup_live(&events, first),
        (false, BackgroundState::Completed),
        "the CLI ran it (before its scheduledFor): fired, not cancelled"
    );
    assert_eq!(wakeup_live(&events, second), running);
    assert_eq!(last_state(&events, second).next_run_at, Some(due2));
    // The failed run.
    next_matching(&mut h, &mut events, turn_end).await;
    assert_eq!(
        wakeup_live(&events, second),
        (false, BackgroundState::Running),
        "the CLI may have run it: it no longer keeps the process until a list says"
    );
    h.session.send(TurnInput::text("status?")).await.unwrap();
    events.extend(collect_until_exit(&mut h).await);
    assert_eq!(
        wakeup_live(&events, second),
        (false, BackgroundState::Completed)
    );
    let history = task_history(&events);
    assert!(
        !history.contains_key("cron:c1") && !history.contains_key("cron:c2"),
        "each list entry is the wakeup its call scheduled: {:?}",
        history.keys()
    );
    assert_eq!(
        turn_statuses(&events),
        vec![
            TurnStatus::Completed,
            TurnStatus::Completed,
            TurnStatus::Failed,
            TurnStatus::Completed
        ]
    );
}

/// A CLI without lifecycle frames does not say which commands it enqueued itself: a run the
/// adapter did not start is one (the CLI runs a wakeup only while no run is going, as a run of
/// its own). A wakeup scheduled in an interrupted turn stays live through the user's own
/// interrupted turns; the CLI runs it (before its `scheduledFor`, which is in 2100), the model
/// does not reschedule, and the list at the end of that run holds only the next wakeup the CLI
/// armed by itself with the same prompt: the wakeup fired, and the entry is another wakeup.
#[tokio::test]
async fn without_lifecycle_frames_a_run_of_the_clis_own_may_have_run_a_wakeup() {
    let due = 4_102_444_800_000i64;
    let prompt = "/loop check the deploy";
    let init = json!({"dir": "out", "msg": {"type": "system", "subtype": "init", "session_id": "replay",
        "capabilities": ["interrupt_receipt_v1"]}});
    let interrupted = |request_id: &str| {
        [
            json!({"dir": "in", "msg": {"type": "control_request", "request_id": request_id, "request": {"subtype": "interrupt"}}}),
            json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": request_id, "response": {"still_queued": []}}}}),
            json!({"dir": "out", "msg": {"type": "result", "subtype": "error_during_execution", "is_error": true,
                "terminal_reason": "aborted_streaming", "total_cost_usd": 0.1, "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ]
    };
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u1", "message": {"role": "user", "content": "loop"}}}),
        init.clone(),
        wakeup_call("toolu_W", json!({"delaySeconds": 1200, "reason": "check the deploy",
            "prompt": prompt, "noop": false})),
        wakeup_result("toolu_W", json!({"scheduledFor": due, "clampedDelaySeconds": 1200, "wasClamped": false})),
        text_message("m1", "waiting"),
    ]);
    script.extend(interrupted("i1"));
    script.extend(context_exchange("ctx1"));
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "u2", "message": {"role": "user", "content": "anything new?"}}}),
        init.clone(),
        text_message("m2", "looking"),
    ]);
    script.extend(interrupted("i2"));
    script.extend(context_exchange("ctx2"));
    // A run the adapter did not start.
    script.extend([init.clone(), text_message("m3", "the deploy is done")]);
    script.extend(stop_hook(
        "h3",
        json!([{"id": "k1", "schedule": "30 9 1 1 *", "recurring": false, "prompt": prompt}]),
    ));
    script.push(success_result(0.3));
    script.extend(context_exchange("ctx3"));

    let key = "wakeup:toolu_W";
    let running = (true, BackgroundState::Running);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    let mut events = Vec::new();
    for (message, answer) in [("loop", "waiting"), ("anything new?", "looking")] {
        h.session.send(TurnInput::text(message)).await.unwrap();
        next_matching(&mut h, &mut events, |e| {
            matches!(e, AdapterEvent::ItemStarted { body: ItemBody::AgentMessage { text }, .. } if text == answer)
        })
        .await;
        h.session.interrupt().await.unwrap();
        next_matching(&mut h, &mut events, |e| {
            matches!(e, AdapterEvent::TurnCompleted { .. })
        })
        .await;
        assert_eq!(
            wakeup_live(&events, key),
            running,
            "the user's own interrupted turn ran no wakeup"
        );
    }
    events.extend(collect_until_exit(&mut h).await);
    assert_eq!(
        wakeup_live(&events, key),
        (false, BackgroundState::Completed),
        "the CLI's own run may have been it, and the list does not hold it"
    );
    assert_eq!(wakeup_live(&events, "cron:k1"), running);
    assert_eq!(
        turn_statuses(&events),
        vec![
            TurnStatus::Interrupted,
            TurnStatus::Interrupted,
            TurnStatus::Completed
        ]
    );
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
            act: l["act"].as_str().map(str::to_owned),
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
    // Leaving ultracode for the CLI's default reports nothing: the level the default resolves
    // to (`medium` here) is not the thread's choice.
    assert_eq!(
        efforts,
        vec![Some("ultracode".into()), None, Some("ultracode".into())]
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

// -------------------------------------------------------------------------------------------
// Recordings of Claude Code 2.1.284 (rec2): steers, rename, status, side questions, fast mode,
// moving work to the background, plan mode, turn anchors (docs/adapters/claude.md §18).
// -------------------------------------------------------------------------------------------

/// The anchors the adapter reported, in order.
fn anchors(events: &[AdapterEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::TurnAnchor { anchor } => {
                Some(crate::mapping::anchor_uuid(anchor).unwrap().to_owned())
            }
            _ => None,
        })
        .collect()
}

fn agent_messages(events: &[AdapterEvent]) -> Vec<String> {
    completed(events)
        .into_iter()
        .filter_map(|(_, b, _)| match b {
            ItemBody::AgentMessage { text } => Some(text),
            _ => None,
        })
        .collect()
}

/// Every turn's anchor comes before its completion.
fn assert_anchored_before_completion(events: &[AdapterEvent]) {
    let mut anchored = false;
    for e in events {
        match e {
            AdapterEvent::TurnStarted => anchored = false,
            AdapterEvent::TurnAnchor { .. } => anchored = true,
            AdapterEvent::TurnCompleted { .. } => assert!(anchored, "a turn completed unanchored"),
            _ => {}
        }
    }
}

/// a1: a message sent while the turn's first Bash ran was taken at the tool boundary
/// (`command_lifecycle started` before the turn's `result`, no new `init`): no second turn, no
/// returned steer, and the answer honours it.
#[tokio::test]
async fn replays_a_steer_the_turn_takes_at_a_tool_boundary() {
    let Replayed {
        events,
        sends,
        steers,
        ..
    } = replay("steer_absorbed.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_eq!(steers.len(), 1);
    assert!(steers[0].1.is_ok(), "{steers:?}");
    assert_well_formed(&events);
    assert_eq!(turn_statuses(&events), vec![TurnStatus::Completed]);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AdapterEvent::SteerReturned { .. }))
    );
    assert!(
        agent_messages(&events)
            .last()
            .unwrap()
            .ends_with("PINEAPPLE")
    );
    // The anchor is the turn's last assistant message.
    assert_eq!(anchors(&events), ["bb06a71c-a942-4712-867f-e4d881fd6a1b"]);
    assert_anchored_before_completion(&events);
    // Both Bash calls ran in the foreground; the first one long enough to be reported.
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::ItemBackgroundable { key, backgroundable: true } if key == "tool:toolu_017Ke8Sye5hwGKpBwMNP6wt3")));
}

/// a2 as recorded: no tool boundary was left, and the CLI had dequeued the steer when the
/// withdrawal came (`{cancelled: false}`): it runs as the CLI's next run, which answers it. The
/// first turn completes before that run starts; nothing is handed back.
#[tokio::test]
async fn replays_a_steer_the_cli_runs_next() {
    let Replayed { events, steers, .. } = replay("steer_next_run.jsonl").await;
    assert!(steers[0].1.is_ok(), "{steers:?}");
    assert_well_formed(&events);
    assert_eq!(
        turn_statuses(&events),
        vec![TurnStatus::Completed, TurnStatus::Completed]
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AdapterEvent::SteerReturned { .. }))
    );
    assert_eq!(
        agent_messages(&events).last().map(String::as_str),
        Some("MANGO")
    );
    // The run answers a person's message: no trigger.
    assert_eq!(turn_triggers(&events), vec![None, None]);
    // Its anchor starts with the message itself (the steer's uuid is its transcript entry).
    assert_eq!(anchors(&events).len(), 2);
    assert_anchored_before_completion(&events);
}

/// a2 with the withdrawal first (`{cancelled: true}`, the shape of recording a3): the steer is
/// handed back before the turn completes, and the engine's resend is an ordinary turn.
#[tokio::test]
async fn replays_a_returned_steer() {
    let Replayed {
        events,
        sends,
        steers,
        ..
    } = replay("steer_returned.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let (message_id, steered) = &steers[0];
    assert!(steered.is_ok());
    let returned = position(
        &events,
        |e| matches!(e, AdapterEvent::SteerReturned { message_id: m } if m == message_id),
    );
    let first_end = position(&events, |e| matches!(e, AdapterEvent::TurnCompleted { .. }));
    assert!(
        returned < first_end,
        "handed back before the turn completed"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AdapterEvent::SteerReturned { .. }))
            .count(),
        1
    );
    assert_eq!(turn_statuses(&events).len(), 2);
    assert_eq!(
        agent_messages(&events).last().map(String::as_str),
        Some("MANGO")
    );
}

/// b1: the status (`get_status` and `get_usage`), a rename as the host, side questions while
/// idle and while a Bash runs (answered beside the turn), and the Bash reported backgroundable.
#[tokio::test]
async fn replays_status_rename_and_side_questions() {
    let Replayed {
        events,
        sends,
        statuses,
        renames,
        answers,
        ..
    } = replay("rename_status_btw.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    assert_eq!(turn_statuses(&events).len(), 4);
    assert_eq!(renames.len(), 1);
    assert!(renames[0].is_ok(), "{renames:?}");
    // The status: the CLI's own sections, then the plan's usage and the session's usage.
    assert_eq!(statuses.len(), 2);
    let first = statuses[0].as_ref().unwrap();
    let titles: Vec<&str> = first.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Session", "Environment", "Plan usage", "Session usage"]
    );
    let row = |sections: &[StatusSection], title: &str, label: &str| -> Option<String> {
        sections
            .iter()
            .find(|s| s.title == title)?
            .rows
            .iter()
            .find(|r| r.label == label)
            .map(|r| r.value.clone())
    };
    assert_eq!(row(first, "Session", "Version").as_deref(), Some("2.1.284"));
    assert_eq!(row(first, "Plan usage", "Plan").as_deref(), Some("max"));
    assert_eq!(
        row(first, "Plan usage", "Current session").as_deref(),
        Some("43% used, resets 2026-09-28T20:20:00.469143+00:00")
    );
    assert!(row(first, "Plan usage", "Current week (all models)").is_some());
    assert!(row(first, "Plan usage", "Current week (Fable)").is_some());
    assert_eq!(
        row(first, "Plan usage", "Usage credits").as_deref(),
        Some("off (out_of_credits)")
    );
    // After the rename the CLI shows the session's name.
    let second = statuses[1].as_ref().unwrap();
    assert_eq!(
        row(second, "Session", "Session name").as_deref(),
        Some("Rec2 host title")
    );
    assert_eq!(
        row(second, "Session usage", "Cost").as_deref(),
        Some("$0.0124")
    );
    // Side questions: the CLI's answers, not in the history.
    assert_eq!(answers.len(), 2);
    let during = answers[1].as_ref().unwrap();
    assert!(!during.synthetic);
    assert!(during.answer.as_deref().unwrap().contains("side question"));
    assert!(
        !agent_messages(&events)
            .iter()
            .any(|m| m.contains("side question"))
    );
    // `control_request_progress` is progress of our own request, not an unknown message.
    assert!(!events.iter().any(|e| matches!(e, AdapterEvent::Native { payload } if payload["subtype"] == "control_request_progress")));
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::ItemBackgroundable { key, backgroundable: true } if key == "tool:toolu_01191VJsg4ezTSESwopzv9bE")));
    assert_eq!(anchors(&events).len(), 4);
}

/// e1: fast mode on (the flag setting), the CLI's state for it, the server's refusal as a
/// notice, and off again.
#[tokio::test]
async fn replays_fast_mode() {
    let Replayed { events, sends, .. } = replay("fast_mode.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let states: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ModesReported {
                fast_state: Some(s),
                ..
            } => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(states, ["off", "on", "off"]);
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::Notice { level: aas_harness::NoticeLevel::Error, message, code: Some(c) } if c == "fast-mode-overage-rejected" && message == "Fast mode disabled · usage credits exhausted")));
    // Fast mode was on as asked: no reason to tell.
    assert!(!events.iter().any(|e| matches!(e, AdapterEvent::Notice { code: Some(c), .. } if c == crate::session::FAST_MODE_DISABLED_NOTICE)));
}

/// e1 against an account whose extra usage is off (probe of 2026-09-30, Claude Code 2.1.284):
/// the CLI keeps the requested fast mode off and says why (`fast_mode_disabled_reason:
/// extra_usage_disabled` in `init` and `result`), which reaches the user once as a notice; off
/// by request, the opt-in reason is not news.
#[tokio::test]
async fn a_fast_mode_the_cli_keeps_off_is_explained_once() {
    let Replayed { events, sends, .. } = replay("fast_mode_unavailable.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let states: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ModesReported {
                fast_state: Some(s),
                ..
            } => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(states, ["off"]);
    let notices: Vec<(&aas_harness::NoticeLevel, &str)> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Notice {
                level,
                message,
                code: Some(c),
            } if c == crate::session::FAST_MODE_DISABLED_NOTICE => Some((level, message.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        [(
            &aas_harness::NoticeLevel::Warning,
            "Claude Code keeps fast mode off: extra_usage_disabled"
        )]
    );
}

/// `fast_mode_unavailable.jsonl` with the first turn's `result` reporting `on` without a reason
/// (live run of 2026-09-30, Claude Code 2.1.285: `init` said off with `extra_usage_disabled`, the
/// same turn's `result` said on). The reason in `init` alone is no notice; the second turn is
/// off by request, without a reason. A `result` off with the reason is the notice of
/// `a_fast_mode_the_cli_keeps_off_is_explained_once`.
#[tokio::test]
async fn a_fast_mode_off_only_at_the_start_of_a_turn_is_no_notice() {
    let Replayed { events, sends, .. } = replay("fast_mode_on_within_turn.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let states: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ModesReported {
                fast_state: Some(s),
                ..
            } => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(states, ["off", "on", "off"]);
    assert!(
        !events.iter().any(|e| matches!(e, AdapterEvent::Notice { code: Some(c), .. } if c == crate::session::FAST_MODE_DISABLED_NOTICE)),
        "{events:?}"
    );
}

/// f1b: a foreground Bash, backgroundable once the CLI registered its task, moved to the
/// background: the item closes as `backgrounded` with its shell task, the turn goes on.
#[tokio::test]
async fn replays_moving_a_bash_to_the_background() {
    let Replayed {
        events,
        sends,
        moves,
        ..
    } = replay("background_bash.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_eq!(moves.len(), 1);
    assert!(moves[0].is_ok(), "{moves:?}");
    assert_well_formed(&events);
    let key = "tool:toolu_01MBWgr8oNAwmCQLyBTdcHt4";
    let backgroundable = position(
        &events,
        |e| matches!(e, AdapterEvent::ItemBackgroundable { key: k, .. } if k == key),
    );
    let closed = position(
        &events,
        |e| matches!(e, AdapterEvent::ItemCompleted { key: k, status: ItemStatus::Backgrounded, .. } if k == key),
    );
    assert!(backgroundable < closed);
    let task = last_state(&events, "b0psjkcj9");
    assert_eq!(task.kind, BackgroundTaskKind::Shell);
    assert_eq!(task.origin_item_key.as_deref(), Some(key));
    assert_eq!(task.state, BackgroundState::Completed);
    assert_eq!(turn_statuses(&events), vec![TurnStatus::Completed]);
}

/// f2: a foreground Agent moved to the background ends the turn; its end starts a run the
/// result marks.
#[tokio::test]
async fn replays_moving_an_agent_to_the_background() {
    let Replayed {
        events,
        sends,
        moves,
        ..
    } = replay("background_agent.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert!(moves[0].is_ok(), "{moves:?}");
    assert_well_formed(&events);
    let key = "tool:toolu_01QQSCQKj2ES3fi8GEYiNT5F";
    assert!(
        completed(&events)
            .iter()
            .any(|(k, _, s)| k == key && *s == ItemStatus::Backgrounded)
    );
    let agent = last_state(&events, "adddbb491bccbcff1");
    assert_eq!(agent.kind, BackgroundTaskKind::Agent);
    assert_eq!(agent.origin_item_key.as_deref(), Some(key));
    assert_eq!(
        turn_triggers(&events),
        vec![None, Some(TurnTrigger::BackgroundTask)]
    );
}

/// h1: the permission modes set (acceptEdits, default), plan mode, the plan presented for
/// approval (a proposed plan), plan mode left after the approval (back to default), and the
/// approval that switched to acceptEdits — each reported for the thread.
#[tokio::test]
async fn replays_plan_mode_and_the_permission_modes_the_cli_reports() {
    let Replayed { events, sends, .. } = replay("plan_mode.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    let modes: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::SessionInfo {
                permission_mode: Some(m),
                ..
            } => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(modes, ["default", "acceptEdits", "default", "acceptEdits"]);
    let plan: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ModesReported { plan: Some(p), .. } => Some(*p),
            _ => None,
        })
        .collect();
    assert_eq!(plan, [false, true, false]);
    // No permission mode `plan` is ever reported.
    assert!(!modes.contains(&"plan"));
    let proposed: Vec<(String, ItemStatus)> = completed(&events)
        .into_iter()
        .filter_map(|(_, b, s)| match b {
            ItemBody::ProposedPlan { text } => Some((text, s)),
            _ => None,
        })
        .collect();
    assert_eq!(proposed.len(), 1);
    assert!(proposed[0].0.contains("hello.txt"), "{proposed:?}");
    assert_eq!(proposed[0].1, ItemStatus::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::InteractionRequested {
            request: InteractionRequest::Approval {
                subject: aas_harness::protocol::Subject::Plan { .. },
                ..
            },
            ..
        }
    )));
}

/// g1: each turn's anchor is its last assistant message — the uuids the fork recordings resumed
/// at (`--resume-session-at`; g2: "1. APPLE 2. BANANA" for the second).
#[tokio::test]
async fn replays_three_turns_with_their_anchors() {
    let Replayed { events, sends, .. } = replay("three_turns.jsonl").await;
    assert!(sends.iter().all(Result::is_ok), "{sends:?}");
    assert_well_formed(&events);
    assert_eq!(
        anchors(&events),
        [
            "cffd151b-1a78-4509-882b-5c8460ad0617",
            "018cc139-cfac-4f9a-bb3c-d5959641a34c",
            "e53b7015-438e-44db-be34-f68e6484399e"
        ]
    );
    assert_anchored_before_completion(&events);
}

// -------------------------------------------------------------------------------------------
// Scripted exchanges for the paths the rec2 recordings do not reach.
// -------------------------------------------------------------------------------------------

/// A user message the adapter writes, and the CLI taking it into a run.
fn own_run(uuid: &str, text: &str) -> Vec<Value> {
    vec![
        json!({"dir": "in", "msg": {"type": "user", "uuid": uuid, "message": {"role": "user", "content": text}}}),
        lifecycle(uuid, "queued"),
        lifecycle(uuid, "started"),
        init_with_lifecycle(),
    ]
}

fn result_line() -> Value {
    json!({"dir": "out", "msg": {"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.1,
        "usage": {"input_tokens": 1, "output_tokens": 1}}})
}

/// Waits until the fake CLI consumed `n` input lines.
async fn consumed(h: &mut Harness, n: usize, seen: &mut Vec<AdapterEvent>) {
    let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
    while h.progress.borrow().consumed < n {
        tokio::select! {
            ev = h.events.recv() => seen.push(ev.expect("events")),
            changed = h.progress.changed() => changed.expect("the fake CLI ended early"),
            _ = tokio::time::sleep_until(deadline) => panic!("the fake CLI did not consume {n} lines"),
        }
    }
}

/// The CLI refused a steer (`command_lifecycle refused` before it started): it goes back to the
/// engine at once, and the turn's end withdraws nothing.
#[tokio::test]
async fn a_refused_steer_is_handed_back() {
    let mut script = init_exchange();
    script.extend(own_run("U1", "work"));
    script.push(json!({"dir": "in", "act": "steer", "msg": {"type": "user", "uuid": "S1", "message": {"role": "user", "content": "also this"}}}));
    script.push(lifecycle("S1", "queued"));
    script.push(lifecycle("S1", "refused"));
    script.push(text_message("m1", "done"));
    script.push(result_line());
    script.extend(context_exchange("ctx1"));
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("work")).await.unwrap();
    h.session
        .steer_message("M1", TurnInput::text("also this"))
        .await
        .unwrap();
    let events = collect_until_exit(&mut h).await;
    let returned = position(
        &events,
        |e| matches!(e, AdapterEvent::SteerReturned { message_id } if message_id == "M1"),
    );
    let end = position(&events, |e| matches!(e, AdapterEvent::TurnCompleted { .. }));
    assert!(returned < end);
}

/// A steer that comes after the turn's `result` (its completion still waits for the context
/// answer) goes back to the engine without being written; one that comes after the completion
/// fails, since the engine's turn is over.
#[tokio::test]
async fn a_steer_after_the_result_is_handed_back_or_refused() {
    let status = |id: &str| {
        [
            json!({"dir": "in", "msg": {"type": "control_request", "request_id": id, "request": {"subtype": "get_status"}}}),
            json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": id,
                "response": {"sections": []}}}}),
            json!({"dir": "in", "msg": {"type": "control_request", "request_id": format!("{id}u"),
                "request": {"subtype": "get_usage", "skip_behaviors": true}}}),
            json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success",
                "request_id": format!("{id}u"), "response": {}}}}),
        ]
    };
    let mut script = init_exchange();
    script.extend(own_run("U1", "work"));
    script.push(text_message("m1", "done"));
    script.push(result_line());
    // The context request is read; its answer comes after the status the test asks for.
    script.push(
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "ctx1",
        "request": {"subtype": "get_context_usage", "detail": "summary"}}}),
    );
    let [ask, answer, ask_usage, usage] = status("s1");
    script.push(ask);
    script.extend(context_exchange("ctx1").into_iter().skip(1));
    script.extend([answer, ask_usage, usage]);
    // Keeps the CLI's output open for the last steer.
    let [ask, ..] = status("s2");
    script.push(ask);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("work")).await.unwrap();
    let mut seen = Vec::new();
    // The fake CLI read the context request: the result has been handled.
    consumed(&mut h, 3, &mut seen).await;
    h.session
        .steer_message("M1", TurnInput::text("late"))
        .await
        .unwrap();
    next_matching(
        &mut h,
        &mut seen,
        |e| matches!(e, AdapterEvent::SteerReturned { message_id } if message_id == "M1"),
    )
    .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnCompleted { .. })),
        "handed back before the turn completed"
    );
    // The status request lets the CLI answer the context: the turn completes.
    h.session.status().await.unwrap();
    next_matching(&mut h, &mut seen, |e| {
        matches!(e, AdapterEvent::TurnCompleted { .. })
    })
    .await;
    assert!(matches!(
        h.session
            .steer_message("M2", TurnInput::text("too late"))
            .await,
        Err(AdapterError::Other(_))
    ));
    let s = h.session.clone();
    let last = tokio::spawn(async move { s.status().await });
    collect_until_exit(&mut h).await;
    assert!(
        last.await.unwrap().is_err(),
        "the CLI ended without an answer"
    );
}

/// A CLI without `msg_lifecycle_v1` would never say whether it took a steer: steering fails.
#[tokio::test]
async fn a_steer_needs_the_lifecycle_frames() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "user", "uuid": "U1", "message": {"role": "user", "content": "work"}}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "init", "session_id": "replay"}}),
        // Keeps the CLI's output open until the steer was refused.
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "i1", "request": {"subtype": "interrupt"}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("work")).await.unwrap();
    let mut seen = Vec::new();
    next_matching(&mut h, &mut seen, |e| {
        matches!(e, AdapterEvent::TurnStarted)
    })
    .await;
    match h.session.steer_message("M1", TurnInput::text("x")).await {
        Err(AdapterError::Harness(m)) => assert!(m.contains("msg_lifecycle_v1"), "{m}"),
        other => panic!("{other:?}"),
    }
    let s = h.session.clone();
    let interrupt = tokio::spawn(async move { s.interrupt().await });
    collect_until_exit(&mut h).await;
    let _ = interrupt.await.unwrap();
}

/// The CLI started a run of its own (a task notification) before it answered the withdrawal of
/// the previous turn's steer: that turn completed already, so the user is told that the message
/// was not delivered instead.
#[tokio::test]
async fn a_steer_withdrawn_after_its_turn_completed_is_reported() {
    let mut script = init_exchange();
    script.extend(own_run("U1", "work"));
    script.push(json!({"dir": "in", "act": "steer", "msg": {"type": "user", "uuid": "S1", "message": {"role": "user", "content": "also"}}}));
    script.push(lifecycle("S1", "queued"));
    script.push(text_message("m1", "done"));
    script.push(result_line());
    script.push(
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "ctx1",
        "request": {"subtype": "get_context_usage", "detail": "summary"}}}),
    );
    script.push(
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "wd1",
        "request": {"subtype": "cancel_async_message", "message_uuid": "S1"}}}),
    );
    // A run of the CLI's own begins: the previous turn completes without its answers.
    script.push(init_with_lifecycle());
    script.push(
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success",
        "request_id": "wd1", "response": {"cancelled": true}}}}),
    );
    script.push(lifecycle("S1", "cancelled"));
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("work")).await.unwrap();
    h.session
        .steer_message("M1", TurnInput::text("also"))
        .await
        .unwrap();
    let mut seen = Vec::new();
    next_matching(
        &mut h,
        &mut seen,
        |e| matches!(e, AdapterEvent::Notice { code: Some(c), .. } if c == "steerNotDelivered"),
    )
    .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, AdapterEvent::SteerReturned { .. }))
    );
    h.session.shutdown(StopReason::Shutdown).await;
}

/// Plan mode over the thread's permission mode: on (`plan`), a new permission mode while in plan
/// mode (set, then plan again, so that the CLI returns to it after the approval), off (the
/// permission mode again). Fast mode is the flag setting.
#[tokio::test]
async fn modes_are_the_plan_permission_mode_and_the_fast_flag() {
    let mut script = init_exchange();
    let set = |id: &str, mode: &str| {
        [
            json!({"dir": "in", "msg": {"type": "control_request", "request_id": id,
                "request": {"subtype": "set_permission_mode", "mode": mode}}}),
            json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success",
                "request_id": id, "response": {"mode": mode}}}}),
            json!({"dir": "out", "msg": {"type": "system", "subtype": "status", "status": null, "permissionMode": mode}}),
        ]
    };
    script.extend(set("p1", "plan"));
    script.extend(set("p2", "acceptEdits"));
    script.extend(set("p3", "plan"));
    script.extend(ctl(
        "f1",
        json!({"subtype": "apply_flag_settings", "settings": {"fastMode": true}}),
    ));
    script.extend(set("p4", "acceptEdits"));
    script.extend(ctl(
        "f2",
        json!({"subtype": "apply_flag_settings", "settings": {"fastMode": false}}),
    ));
    let mut settings = ThreadSettings {
        permission_mode: Some("default".into()),
        ..ThreadSettings::default()
    };
    let mut h = start_with(synthetic(&script), settings.clone());
    h.session.initialize().await.unwrap();
    let mut modes = ThreadModes {
        plan: true,
        fast: false,
    };
    h.session.apply_modes(&modes).await.unwrap();
    settings.permission_mode = Some("acceptEdits".into());
    h.session.apply_settings(&settings).await.unwrap();
    modes.fast = true;
    h.session.apply_modes(&modes).await.unwrap();
    modes = ThreadModes::default();
    h.session.apply_modes(&modes).await.unwrap();
    let events = collect_until_exit(&mut h).await;
    let plan: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ModesReported { plan: Some(p), .. } => Some(*p),
            _ => None,
        })
        .collect();
    // The CLI's reports as they came: plan, acceptEdits for a moment, plan, acceptEdits.
    assert_eq!(plan, [false, true, false, true, false]);
}

/// A thread stored with the permission mode `plan` of earlier versions, on a CLI in plan mode:
/// plan mode is never the mode to return to, so leaving it sets `default`. After the change,
/// the CLI's next word on plan mode reaches the engine even when it says plan mode again.
#[tokio::test]
async fn leaving_plan_mode_never_returns_to_plan() {
    let mut script = vec![
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "r0", "request": {"subtype": "initialize"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "r0",
            "response": {"commands": [], "models": [], "current_permission_mode": "plan"}}}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "p1",
            "request": {"subtype": "set_permission_mode", "mode": "default"}}}),
        // No `system/status` follows this time.
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success",
            "request_id": "p1", "response": {"mode": "default"}}}}),
    ];
    // Something the adapter sends after `apply_modes` has returned, so that the report below
    // comes after it.
    script.extend(ctl(
        "f1",
        json!({"subtype": "apply_flag_settings", "settings": {"fastMode": true}}),
    ));
    script.push(json!({"dir": "out", "msg": {"type": "system", "subtype": "status", "status": null, "permissionMode": "plan"}}));
    let settings = ThreadSettings {
        permission_mode: Some("plan".into()),
        ..ThreadSettings::default()
    };
    let mut h = start_with(synthetic(&script), settings);
    h.session.initialize().await.unwrap();
    h.session
        .apply_modes(&ThreadModes::default())
        .await
        .unwrap();
    h.session
        .apply_modes(&ThreadModes {
            plan: false,
            fast: true,
        })
        .await
        .unwrap();
    let events = collect_until_exit(&mut h).await;
    let plan: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ModesReported { plan: Some(p), .. } => Some(*p),
            _ => None,
        })
        .collect();
    assert_eq!(plan, [true, true]);
    assert_eq!(reported_permission_mode(&events), None);
}

/// The status in plan mode adds the plan (`get_plan`); a failed `get_usage` becomes a section
/// that says so.
#[tokio::test]
async fn the_status_in_plan_mode_shows_the_plan() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "out", "msg": {"type": "system", "subtype": "status", "status": null, "permissionMode": "plan"}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "s1", "request": {"subtype": "get_status"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "s1",
            "response": {"sections": [{"title": "Session", "rows": [{"label": "Version", "value": "2.1.284"}, {"label": "Session name", "value": null}]}]}}}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "u1", "request": {"subtype": "get_usage", "skip_behaviors": true}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "error", "request_id": "u1",
            "error": "\u{1b}[31mnot signed in\u{1b}[0m"}}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "g1", "request": {"subtype": "get_plan"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "g1",
            "response": {"exists": true, "content": "# Plan\n\n1. Do it", "path": "C:\\Users\\user\\.claude\\plans\\p.md"}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    let mut seen = Vec::new();
    next_matching(&mut h, &mut seen, |e| {
        matches!(
            e,
            AdapterEvent::ModesReported {
                plan: Some(true),
                ..
            }
        )
    })
    .await;
    let sections = h.session.status().await.unwrap();
    let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Session", "Plan usage", "Plan"]);
    assert_eq!(sections[0].rows[1].value, "");
    assert_eq!(sections[1].rows[0].label, "Error");
    assert_eq!(sections[1].rows[0].value, "get_usage: not signed in");
    assert_eq!(sections[2].rows[1].value, "# Plan\n\n1. Do it");
    h.session.shutdown(StopReason::Shutdown).await;
}

/// Moving work that is not reported backgroundable fails without a request; a `background_tasks`
/// the CLI answers with `backgrounded: false` (recording f3: too early) fails too.
#[tokio::test]
async fn moving_to_the_background_needs_foreground_work_the_cli_moves() {
    let mut script = init_exchange();
    script.extend(own_run("U1", "work"));
    script.extend([
        json!({"dir": "out", "msg": {"type": "assistant", "parent_tool_use_id": null, "uuid": "a1", "message": {"id": "m1",
            "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "sleep 30"}}]}}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "task_started", "task_id": "b1", "tool_use_id": "t1",
            "description": "sleep", "is_backgrounded": false, "task_type": "local_bash"}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "bg1",
            "request": {"subtype": "background_tasks", "tool_use_id": "t1"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success",
            "request_id": "bg1", "response": {"backgrounded": false}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("work")).await.unwrap();
    let mut seen = Vec::new();
    next_matching(&mut h, &mut seen, |e| {
        matches!(e, AdapterEvent::ItemBackgroundable { key, backgroundable: true } if key == "tool:t1")
    })
    .await;
    assert!(matches!(
        h.session.move_to_background("tool:t2").await,
        Err(AdapterError::Other(_))
    ));
    match h.session.move_to_background("tool:t1").await {
        Err(AdapterError::Harness(m)) => assert!(m.contains("backgrounded"), "{m}"),
        other => panic!("{other:?}"),
    }
    h.session.shutdown(StopReason::Shutdown).await;
}

/// `system/commands_changed` replaces the list; `system/init` leaves out the terminal-only
/// commands and adds what the list lacks; the menu is reported only when it changed, and the
/// adapter's per-directory menu follows.
#[tokio::test]
async fn the_menu_follows_the_cli_lists() {
    let mut script = vec![
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "r0", "request": {"subtype": "initialize"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "r0",
            "response": {"current_permission_mode": "default", "models": [], "commands": [
                {"name": "clear", "description": "Start a new session", "aliases": ["reset", "new"]},
                {"name": "code-review", "description": "Review", "aliases": ["review"]},
                {"name": "color", "description": "Set the prompt bar color"}]}}}}),
    ];
    script.extend(own_run("U1", "hi"));
    script.extend([
        json!({"dir": "out", "msg": {"type": "system", "subtype": "init", "session_id": "replay",
            "capabilities": ["msg_lifecycle_v1"], "slash_commands": ["clear", "code-review", "color", "mcp__docs__search"],
            "terminal_slash_commands": ["color"]}}),
        json!({"dir": "out", "msg": {"type": "system", "subtype": "commands_changed", "commands": [
            {"name": "clear", "description": "Start a new session", "aliases": ["reset", "new"]},
            {"name": "code-review", "description": "Review", "aliases": ["review"]},
            {"name": "color", "description": "Set the prompt bar color"},
            {"name": "my-skill", "description": "A skill"}]}}),
        // The same list again: nothing to report.
        json!({"dir": "out", "msg": {"type": "system", "subtype": "commands_changed", "commands": [
            {"name": "clear", "description": "Start a new session", "aliases": ["reset", "new"]},
            {"name": "code-review", "description": "Review", "aliases": ["review"]},
            {"name": "color", "description": "Set the prompt bar color"},
            {"name": "my-skill", "description": "A skill"}]}}),
        text_message("m1", "hello"),
        result_line(),
    ]);
    script.extend(context_exchange("ctx1"));
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    h.session.send(TurnInput::text("hi")).await.unwrap();
    let events = collect_until_exit(&mut h).await;
    let menus: Vec<Vec<String>> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::CommandsChanged { commands } => {
                Some(commands.iter().map(|c| c.name.clone()).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        menus,
        vec![
            vec!["clear", "code-review", "color", "reset", "new", "review"],
            vec![
                "clear",
                "code-review",
                "reset",
                "new",
                "review",
                "mcp__docs__search"
            ],
            vec![
                "clear",
                "code-review",
                "my-skill",
                "reset",
                "new",
                "review",
                "mcp__docs__search"
            ],
        ]
    );
}

/// An unknown `--resume-session-at` anchor: Claude Code answers the handshake with a failed
/// `result` and ends (recording g6). Its text is the start's error.
#[tokio::test]
async fn a_refused_start_reports_the_cli_text() {
    let script = vec![
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "r0", "request": {"subtype": "initialize"}}}),
        json!({"dir": "out", "msg": {"type": "result", "subtype": "error_during_execution", "is_error": true, "num_turns": 0,
            "errors": ["No message found with message.uuid of: 11111111-2222-4333-8444-555555555555"]}}),
        // The process ends by itself (the script's end closes its output).
    ];
    let mut h = start(synthetic(&script));
    match h.session.initialize().await {
        Err(e @ AdapterError::Harness(_)) => assert_eq!(
            e.detail(),
            "No message found with message.uuid of: 11111111-2222-4333-8444-555555555555"
        ),
        other => panic!("{other:?}"),
    }
    let events = collect_until_exit(&mut h).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AdapterEvent::Native { .. })),
        "{events:?}"
    );
}

/// A rename names the session the process runs; the CLI's refusal is the error. A side question
/// the CLI answers with no text has no answer.
#[tokio::test]
async fn rename_and_side_question_answers() {
    let mut script = init_exchange();
    script.extend([
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "n1", "request": {"subtype": "rename_session",
            "title": "Mine", "source": "host", "session_id": "replay"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "error", "request_id": "n1",
            "error": "session_id is not the current session"}}}),
        json!({"dir": "in", "msg": {"type": "control_request", "request_id": "q1", "request": {"subtype": "side_question", "question": "why?"}}}),
        json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "q1",
            "response": {"response": null, "synthetic": true}}}}),
    ]);
    let mut h = start(synthetic(&script));
    h.session.initialize().await.unwrap();
    match h.session.rename("Mine").await {
        Err(e @ AdapterError::Harness(_)) => assert_eq!(
            e.detail(),
            "rename_session: session_id is not the current session"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        h.session.side_question("why?").await.unwrap(),
        SideAnswer {
            answer: None,
            synthetic: true
        }
    );
    collect_until_exit(&mut h).await;
}
