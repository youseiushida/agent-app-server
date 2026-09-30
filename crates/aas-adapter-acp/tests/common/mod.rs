//! A scripted ACP agent over in-memory pipes, driven by recorded or hand-written steps.

// Each test crate that includes this module uses a different subset of it, so helpers unused
// by one crate are expected here (not a blanket allow for production code).
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use aas_adapter_acp::testing::FakeLink;
use aas_harness::protocol::{
    InteractionRequest, ItemBody, ItemStatus, NoticeLevel, TurnError, TurnStatus, Usage,
};
use aas_harness::{AdapterEvent, BackgroundTaskInfo, ExitInfo};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// One step of the agent's script.
#[derive(Debug, Clone)]
pub enum Step {
    /// The agent writes this message. Responses to client requests carry the *recorded* id,
    /// which is replaced by the id the client actually used.
    Send(Value),
    /// The agent waits for the next client message and checks its shape.
    Expect(Expect),
    /// The agent waits until the test calls [`Agent::release`]. A replayed recording sends
    /// everything at once; a gate keeps what the agent sent after a point (e.g. after a
    /// turn's end) from arriving before the test has seen that point.
    Gate,
}

#[derive(Debug, Clone, Default)]
pub struct Expect {
    /// Method of a client request or notification.
    pub method: Option<String>,
    /// Recorded id of a client request (to map later responses).
    pub recorded_id: Option<Value>,
    /// The client message must be a response to this agent request id.
    pub response_to: Option<Value>,
}

impl Step {
    pub fn request(method: &str, recorded_id: i64) -> Step {
        Step::Expect(Expect {
            method: Some(method.into()),
            recorded_id: Some(Value::from(recorded_id)),
            response_to: None,
        })
    }
    pub fn notification(method: &str) -> Step {
        Step::Expect(Expect {
            method: Some(method.into()),
            recorded_id: None,
            response_to: None,
        })
    }
    pub fn response_to(id: Value) -> Step {
        Step::Expect(Expect {
            method: None,
            recorded_id: None,
            response_to: Some(id),
        })
    }
}

/// Loads a recorded transcript (`{"dir": "in"|"out", "msg": …}` per line).
pub fn load_fixture(name: &str) -> Vec<Step> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let entry: Value = serde_json::from_str(l).unwrap();
            let msg = entry["msg"].clone();
            match entry["dir"].as_str() {
                Some("in") => Step::Send(msg),
                Some("out") => {
                    if let Some(method) = msg.get("method").and_then(Value::as_str) {
                        Step::Expect(Expect {
                            method: Some(method.into()),
                            recorded_id: msg.get("id").cloned(),
                            response_to: None,
                        })
                    } else {
                        Step::response_to(msg["id"].clone())
                    }
                }
                other => panic!("bad dir {other:?}"),
            }
        })
        .collect()
}

/// Handle of a running scripted agent.
pub struct Agent {
    pub task: JoinHandle<Result<Vec<Value>, String>>,
    gates: mpsc::UnboundedSender<()>,
}

impl Agent {
    /// Lets the script go past its next [`Step::Gate`].
    pub fn release(&self) {
        self.gates.send(()).expect("the scripted agent is running");
    }

    /// Waits for the script to finish; returns every message the client sent.
    pub async fn finish(self) -> Vec<Value> {
        match tokio::time::timeout(STEP_TIMEOUT * 3, self.task).await {
            Ok(Ok(Ok(msgs))) => msgs,
            Ok(Ok(Err(e))) => panic!("scripted agent failed: {e}"),
            Ok(Err(e)) => panic!("scripted agent panicked: {e}"),
            Err(_) => panic!("scripted agent did not finish"),
        }
    }
}

/// What the agent does after its last step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// Wait for the client to close stdin, then exit with code 0.
    OnClientEof,
    /// Close stdout immediately and exit with the given code and stderr.
    Crash(i32, &'static str),
    /// Stop reading stdin (a wedged agent) until the process is terminated.
    Hang,
}

/// Starts the agent. Returns the client's ends of the pipes.
pub fn spawn_agent(
    steps: Vec<Step>,
    link: FakeLink,
    end: End,
) -> (DuplexStream, DuplexStream, Agent) {
    let (client_writer, agent_reader) = tokio::io::duplex(1 << 20);
    let (agent_writer, client_reader) = tokio::io::duplex(1 << 20);
    let (gates, mut gate_rx) = mpsc::unbounded_channel::<()>();
    let task = tokio::spawn(async move {
        let mut lines = BufReader::new(agent_reader).lines();
        let mut out = agent_writer;
        let mut ids: HashMap<String, Value> = HashMap::new();
        let mut received = Vec::new();
        for (i, step) in steps.into_iter().enumerate() {
            match step {
                Step::Send(mut msg) => {
                    let is_response = msg.get("method").is_none()
                        && (msg.get("result").is_some() || msg.get("error").is_some());
                    if is_response {
                        let rec = msg["id"].to_string();
                        let actual = ids
                            .get(&rec)
                            .cloned()
                            .ok_or_else(|| format!("step {i}: response for unmapped id {rec}"))?;
                        msg["id"] = actual;
                    }
                    let mut line = serde_json::to_vec(&msg).unwrap();
                    line.push(b'\n');
                    out.write_all(&line)
                        .await
                        .map_err(|e| format!("step {i}: write: {e}"))?;
                }
                Step::Expect(e) => {
                    let line = tokio::time::timeout(STEP_TIMEOUT, lines.next_line())
                        .await
                        .map_err(|_| format!("step {i}: timed out waiting for {e:?}"))?
                        .map_err(|err| format!("step {i}: read: {err}"))?
                        .ok_or_else(|| {
                            format!("step {i}: client closed while waiting for {e:?}")
                        })?;
                    let got: Value = serde_json::from_str(&line)
                        .map_err(|err| format!("step {i}: bad json {line}: {err}"))?;
                    if got.get("jsonrpc") != Some(&Value::from("2.0")) {
                        return Err(format!(
                            "step {i}: client message without jsonrpc 2.0: {got}"
                        ));
                    }
                    if let Some(method) = &e.method {
                        if got.get("method").and_then(Value::as_str) != Some(method.as_str()) {
                            return Err(format!("step {i}: expected {method}, got {got}"));
                        }
                        if let Some(rec) = &e.recorded_id {
                            ids.insert(rec.to_string(), got["id"].clone());
                        }
                    }
                    if let Some(rid) = &e.response_to
                        && (got.get("id") != Some(rid)
                            || (got.get("result").is_none() && got.get("error").is_none()))
                    {
                        return Err(format!("step {i}: expected a response to {rid}, got {got}"));
                    }
                    received.push(got);
                }
                Step::Gate => {
                    tokio::time::timeout(STEP_TIMEOUT * 3, gate_rx.recv())
                        .await
                        .map_err(|_| format!("step {i}: the gate was never released"))?
                        .ok_or_else(|| format!("step {i}: the test ended before the gate"))?;
                }
            }
        }
        match end {
            End::OnClientEof => {
                loop {
                    match tokio::time::timeout(STEP_TIMEOUT, lines.next_line()).await {
                        Ok(Ok(Some(line))) => received
                            .push(serde_json::from_str(&line).unwrap_or(Value::String(line))),
                        Ok(Ok(None)) | Ok(Err(_)) => break,
                        Err(_) => return Err("client never closed stdin".into()),
                    }
                }
                drop(out);
                link.exit(0, "");
            }
            End::Crash(code, stderr) => {
                drop(out);
                link.exit(code, stderr);
            }
            End::Hang => {
                link.wait_exit().await;
                drop(lines);
                drop(out);
            }
        }
        Ok(received)
    });
    (client_reader, client_writer, Agent { task, gates })
}

/// Inserts a [`Step::Gate`] right after the agent's response to the client request recorded
/// with id `recorded_id` (e.g. the `session/prompt` that ends a turn).
pub fn gate_after_response(mut steps: Vec<Step>, recorded_id: i64) -> Vec<Step> {
    let at = steps
        .iter()
        .position(|s| {
            matches!(s, Step::Send(m) if m.get("method").is_none()
                && m["id"] == recorded_id
                && (m.get("result").is_some() || m.get("error").is_some()))
        })
        .unwrap_or_else(|| panic!("no response to request {recorded_id}"));
    steps.insert(at + 1, Step::Gate);
    steps
}

/// Everything a session emitted, folded like the engine would.
#[derive(Debug, Default)]
pub struct Folded {
    pub events: Vec<AdapterEvent>,
    pub items: Vec<(String, ItemBody, ItemStatus)>,
    pub index: HashMap<String, usize>,
    pub turns: Vec<(TurnStatus, Option<Usage>, Option<TurnError>)>,
    pub interactions: Vec<(String, InteractionRequest, Option<String>)>,
    /// `background_key` of each interaction, by request id.
    pub interaction_tasks: HashMap<String, Option<String>>,
    pub withdrawn: Vec<String>,
    /// Every background task report, in order.
    pub task_events: Vec<BackgroundTaskInfo>,
    /// The latest report of each background task, by key.
    pub tasks: HashMap<String, BackgroundTaskInfo>,
    /// What each background task streamed as its output (appends applied, replacements
    /// taking over), by key.
    pub outputs: HashMap<String, String>,
    pub notices: Vec<(NoticeLevel, String, Option<String>)>,
    pub natives: Vec<Value>,
    pub infos: Vec<(Option<String>, Option<String>, Option<String>)>,
    pub commands: Vec<Vec<String>>,
    pub exited: Option<ExitInfo>,
}

impl Folded {
    pub fn apply(&mut self, ev: AdapterEvent) {
        assert!(self.exited.is_none(), "event after Exited: {ev:?}");
        match &ev {
            AdapterEvent::ItemStarted { key, body } => {
                assert!(!self.index.contains_key(key), "item {key} started twice");
                self.index.insert(key.clone(), self.items.len());
                self.items
                    .push((key.clone(), body.clone(), ItemStatus::InProgress));
            }
            AdapterEvent::ItemDelta { key, field, text } => {
                let i = self.index[key];
                assert_eq!(
                    self.items[i].2,
                    ItemStatus::InProgress,
                    "delta for closed item {key}"
                );
                assert!(
                    self.items[i].1.append(*field, text),
                    "delta field mismatch for {key}"
                );
            }
            AdapterEvent::ItemUpdated { key, body } => {
                let i = self.index[key];
                self.items[i].1 = body.clone();
            }
            AdapterEvent::ItemCompleted { key, body, status } => {
                if *status == ItemStatus::Backgrounded {
                    assert!(
                        self.tasks
                            .values()
                            .any(|t| t.origin_item_key.as_deref() == Some(key.as_str())),
                        "item {key} backgrounded before a task named it as its origin"
                    );
                }
                let i = self.index[key];
                assert_eq!(
                    self.items[i].2,
                    ItemStatus::InProgress,
                    "item {key} completed twice"
                );
                if let Some(b) = body {
                    self.items[i].1 = b.clone();
                }
                self.items[i].2 = *status;
            }
            AdapterEvent::TurnCompleted {
                status,
                usage,
                error,
                ..
            } => {
                let open: Vec<_> = self
                    .items
                    .iter()
                    .filter(|i| i.2 == ItemStatus::InProgress)
                    .map(|i| i.0.clone())
                    .collect();
                assert!(open.is_empty(), "items open at turn end: {open:?}");
                self.turns.push((*status, *usage, error.clone()));
            }
            AdapterEvent::InteractionRequested {
                request_id,
                request,
                item_key,
                background_key,
            } => {
                self.interactions
                    .push((request_id.clone(), request.clone(), item_key.clone()));
                self.interaction_tasks
                    .insert(request_id.clone(), background_key.clone());
            }
            AdapterEvent::BackgroundTask { task } => {
                // The port's contract: an ended task is not live, and only a new run (one
                // more `runs`) makes an ended task run again.
                assert!(
                    !(task.state.is_ended() && task.live),
                    "ended task reported live: {task:?}"
                );
                if let Some(before) = self.tasks.get(&task.key)
                    && before.state.is_ended()
                    && !task.state.is_ended()
                {
                    assert_eq!(
                        task.runs,
                        before.runs + 1,
                        "restart without a new run: {task:?}"
                    );
                }
                self.task_events.push((**task).clone());
                self.tasks.insert(task.key.clone(), (**task).clone());
            }
            AdapterEvent::BackgroundOutput { key, output } => {
                // The port's contract: output of a task that was reported and still runs.
                let task = self.tasks.get(key);
                assert!(
                    task.is_some_and(|t| !t.state.is_ended()),
                    "output of a task that does not run: {key} {task:?}"
                );
                let text = self.outputs.entry(key.clone()).or_default();
                match output {
                    aas_harness::OutputUpdate::Append(t) => text.push_str(t),
                    aas_harness::OutputUpdate::Replace(t) => *text = t.clone(),
                }
            }
            AdapterEvent::InteractionWithdrawn { request_id } => {
                self.withdrawn.push(request_id.clone())
            }
            AdapterEvent::Notice {
                level,
                message,
                code,
            } => self.notices.push((*level, message.clone(), code.clone())),
            AdapterEvent::Native { payload } => self.natives.push(payload.clone()),
            AdapterEvent::SessionInfo {
                model,
                permission_mode,
                effort,
            } => self
                .infos
                .push((model.clone(), permission_mode.clone(), effort.clone())),
            AdapterEvent::CommandsChanged { commands } => self
                .commands
                .push(commands.iter().map(|c| c.name.clone()).collect()),
            AdapterEvent::Exited { info } => self.exited = Some(info.clone()),
            _ => {}
        }
        self.events.push(ev);
    }

    pub fn item(&self, key: &str) -> &(String, ItemBody, ItemStatus) {
        &self.items[self.index[key]]
    }

    /// The latest report of background task `key`.
    pub fn task(&self, key: &str) -> &BackgroundTaskInfo {
        self.tasks
            .get(key)
            .unwrap_or_else(|| panic!("no background task {key}: {:?}", self.tasks.keys()))
    }

    /// Position of the first event matching `pred`.
    pub fn position(&self, pred: impl Fn(&AdapterEvent) -> bool) -> usize {
        self.events.iter().position(pred).expect("no event matches")
    }
}

/// Pumps events until `stop` returns true for one of them (inclusive).
pub async fn pump_until(
    rx: &mut mpsc::UnboundedReceiver<AdapterEvent>,
    folded: &mut Folded,
    mut stop: impl FnMut(&AdapterEvent) -> bool,
) -> AdapterEvent {
    loop {
        let ev = tokio::time::timeout(STEP_TIMEOUT, rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed");
        folded.apply(ev.clone());
        if stop(&ev) {
            return ev;
        }
    }
}

pub fn is_turn_completed(ev: &AdapterEvent) -> bool {
    matches!(ev, AdapterEvent::TurnCompleted { .. })
}

pub fn is_exited(ev: &AdapterEvent) -> bool {
    matches!(ev, AdapterEvent::Exited { .. })
}

/// A report of background task `key` in which it has ended.
pub fn task_ended(key: &'static str) -> impl FnMut(&AdapterEvent) -> bool {
    move |ev| matches!(ev, AdapterEvent::BackgroundTask { task } if task.key == key && task.state.is_ended())
}

/// Drains remaining events until `Exited` and asserts the channel then closes.
pub async fn drain_to_exit(rx: &mut mpsc::UnboundedReceiver<AdapterEvent>, folded: &mut Folded) {
    pump_until(rx, folded, is_exited).await;
    let next = tokio::time::timeout(STEP_TIMEOUT, rx.recv())
        .await
        .expect("channel did not close after Exited");
    assert!(next.is_none(), "event after Exited: {next:?}");
}
