//! Replays transcripts recorded from pi 0.85.1 (`tests/fixtures/`) against the session core
//! over in-memory pipes. A small fake plays pi's side: it answers `get_state` (the adapter's
//! "did a run start?" probe and its question at the end of a turn; `isStreaming` and what the
//! scenario set in [`FakePi::state`]), `get_session_stats` (asked after every assistant message;
//! the answer has the shape recorded from pi 0.85.1, with `contextUsage.tokens` = 1000 × the
//! number of stats requests so far), `clear_queue`, `get_entries` and `get_fork_messages` (from
//! the session tree in [`FakePi::entries`], like pi 0.85.1's `rpc-mode.js`) and, once the
//! scenario set a listing, `get_commands` itself, and hands every other command to the
//! scenario.
//!
//! Fixture lines are pi output, except:
//! * `{"$await": "<command>", "respond"?: true}` — wait for the adapter to send `<command>`
//!   (and answer it with success when `respond` is set);
//! * `"id": "$prompt"` / `"id": "$abort"` — replaced by the id of that command.
//!
//! `agent_*.jsonl` and `dialog_outside_turn.jsonl` were recorded from pi 0.85.1 with the live
//! test's extension (`tests/extension/aas-live.ts`): runs pi starts by itself, the race of a
//! prompt with such a run, a run started from a run's end, an abort, the approval gate inside
//! such a run, and a dialog outside any turn. Response ids are replaced by `resp-<n>`.
//!
//! `rec2.json` holds pi 0.85.1's own output recorded on 2026-09-28 (the recording "rec2"), with
//! local paths replaced by `C:\rec`: the entries of a session after each of three turns and its
//! user messages (`get_entries`, `get_fork_messages`), names (`set_session_name`,
//! `session_info_changed`), a fork at an entry by an extension (`ctx.fork`), the editor text of
//! an extension (`set_editor_text`), an extension command while pi streams, a reload, and an
//! extension that starts a new session (`ctx.newSession`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use aas_adapter_pi::{ForkTarget, PiSession, ProcessLink, SessionConfig, handshake};
use aas_harness::{
    AdapterError, AdapterEvent, ContextUsage, DeltaField, ExitInfo, ExpireReason,
    InteractionRequest, InteractionResolution, ItemBody, ItemStatus, NoticeLevel, SessionControl,
    StopReason, ThreadSettings, TurnInput, TurnStatus,
};
use aas_protocol::Subject;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(10);
/// Context window of the recorded model (`contextWindow` in `handshake.json`).
const WINDOW: u64 = 262144;

/// `get_session_stats` data as recorded from pi 0.85.1, with the given context tokens.
fn session_stats(tokens: u64) -> Value {
    json!({
        "sessionId": "611067bd-f742-47e9-9db7-c2a49a25ef82", "userMessages": 1, "assistantMessages": 1,
        "toolCalls": 0, "toolResults": 0, "totalMessages": 2,
        "tokens": {"input": 120, "output": 12, "cacheRead": 0, "cacheWrite": 0, "total": 132},
        "cost": 0.0001,
        "contextUsage": {"tokens": tokens, "contextWindow": WINDOW, "percent": tokens as f64 * 100.0 / WINDOW as f64}
    })
}

struct FakeLink {
    exited: watch::Sender<Option<ExitInfo>>,
}

impl FakeLink {
    fn exit(&self, code: Option<i32>, stopped: Option<StopReason>) {
        self.exited.send_if_modified(|v| {
            if v.is_some() {
                return false;
            }
            *v = Some(ExitInfo {
                code,
                stopped,
                stderr_tail: String::new(),
                exited_at_ms: 0,
            });
            true
        });
    }
}

#[async_trait::async_trait]
impl ProcessLink for FakeLink {
    async fn wait(&self) -> ExitInfo {
        let mut rx = self.exited.subscribe();
        loop {
            if let Some(info) = rx.borrow_and_update().clone() {
                return info;
            }
            if rx.changed().await.is_err() {
                panic!("link dropped");
            }
        }
    }

    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
        match tokio::time::timeout(grace, self.wait()).await {
            Ok(info) => info,
            Err(_) => {
                self.exit(None, Some(reason));
                self.wait().await
            }
        }
    }
}

/// What the fake reports of its session.
#[derive(Default)]
struct FakePi {
    /// Fields of every `get_state` answer besides `isStreaming` (e.g. `sessionId`).
    state: serde_json::Map<String, Value>,
    /// The session's entries in file order (the leaf is the last one).
    entries: Vec<Value>,
    /// `get_commands`, once the fake answers it itself.
    commands: Option<Value>,
    /// The commands the fake answered itself, in order.
    answered: Vec<Value>,
}

impl FakePi {
    /// pi 0.85.1's `get_entries`: the entries after `since` (all without it), and the leaf.
    fn entries(&self, since: Option<&str>) -> Result<Value, String> {
        let after = match since {
            None => 0,
            Some(id) => {
                self.entries
                    .iter()
                    .position(|e| e["id"] == id)
                    .ok_or_else(|| format!("Entry not found: {id}"))?
                    + 1
            }
        };
        Ok(json!({
            "entries": self.entries[after..],
            "leafId": self.entries.last().map_or(Value::Null, |e| e["id"].clone()),
        }))
    }

    /// pi 0.85.1's `get_fork_messages`: the user messages with text, in file order.
    fn fork_messages(&self) -> Value {
        let messages: Vec<Value> = self
            .entries
            .iter()
            .filter(|e| e["type"] == "message" && e["message"]["role"] == "user")
            .filter_map(|e| {
                let content = &e["message"]["content"];
                let text = match content {
                    Value::String(s) => s.clone(),
                    Value::Array(blocks) => blocks
                        .iter()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<String>(),
                    _ => String::new(),
                };
                (!text.is_empty()).then(|| json!({"entryId": e["id"], "text": text}))
            })
            .collect();
        json!({ "messages": messages })
    }
}

struct Fake {
    out: Arc<Mutex<Option<DuplexStream>>>,
    commands: mpsc::UnboundedReceiver<Value>,
    streaming: Arc<AtomicBool>,
    link: Arc<FakeLink>,
    /// `get_session_stats` requests answered by the fake so far.
    stats: Arc<AtomicU64>,
    pi: Arc<std::sync::Mutex<FakePi>>,
}

impl Fake {
    async fn write(&self, value: &Value) {
        let mut guard = self.out.lock().await;
        let w = guard.as_mut().expect("fake output open");
        w.write_all(format!("{value}\n").as_bytes()).await.unwrap();
    }

    async fn expect(&mut self, kind: &str) -> Value {
        let cmd = tokio::time::timeout(WAIT, self.commands.recv())
            .await
            .expect("command in time")
            .expect("command");
        assert_eq!(cmd["type"], kind, "unexpected command {cmd}");
        cmd
    }

    async fn respond(&self, cmd: &Value, data: Option<Value>) {
        let mut resp =
            json!({"type":"response","command":cmd["type"],"success":true,"id":cmd["id"]});
        if let Some(d) = data {
            resp["data"] = d;
        }
        self.write(&resp).await;
    }

    /// pi's session from now on: the `get_state` fields and the entries.
    fn set_session(&self, state: &Value, entries: Vec<Value>) {
        let mut pi = self.pi.lock().unwrap();
        pi.state = state.as_object().cloned().unwrap_or_default();
        pi.state.remove("isStreaming");
        pi.entries = entries;
    }

    /// Appends entries pi wrote.
    fn append(&self, entries: &[Value]) {
        self.pi.lock().unwrap().entries.extend_from_slice(entries);
    }

    /// The commands of `kind` the fake answered itself (in order).
    fn answered(&self, kind: &str) -> Vec<Value> {
        self.pi
            .lock()
            .unwrap()
            .answered
            .iter()
            .filter(|c| c["type"] == kind)
            .cloned()
            .collect()
    }

    /// Plays a fixture. `prompt_id` replaces `$prompt`; returns the answers the adapter
    /// sent to `$await` points (in order).
    async fn play(&mut self, name: &str, prompt_id: &str) -> Vec<Value> {
        self.play_from(name, prompt_id, 0).await
    }

    /// Plays a fixture from its line `skip` on (the lines before were written by the test).
    async fn play_from(&mut self, name: &str, prompt_id: &str, skip: usize) -> Vec<Value> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let text = std::fs::read_to_string(path).unwrap();
        let mut awaited = Vec::new();
        let mut abort_id = String::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()).skip(skip) {
            let mut value: Value = serde_json::from_str(line).unwrap();
            if let Some(kind) = value.get("$await").and_then(Value::as_str) {
                let kind = kind.to_owned();
                let cmd = self.expect(&kind).await;
                if value.get("respond").and_then(Value::as_bool) == Some(true) {
                    self.respond(&cmd, None).await;
                }
                if kind == "abort" {
                    abort_id = cmd["id"].as_str().unwrap().to_owned();
                }
                awaited.push(cmd);
                continue;
            }
            match value.get("id").and_then(Value::as_str) {
                Some("$prompt") => value["id"] = json!(prompt_id),
                Some("$abort") => value["id"] = json!(abort_id),
                _ => {}
            }
            if value["type"] == "agent_settled" {
                self.streaming.store(false, Ordering::SeqCst);
            }
            if value["type"] == "agent_start" {
                self.streaming.store(true, Ordering::SeqCst);
            }
            self.write(&value).await;
        }
        awaited
    }

    /// Simulates pi exiting: closes stdout and marks the process gone.
    async fn exit(&self, code: i32) {
        if let Some(mut w) = self.out.lock().await.take() {
            let _ = w.shutdown().await;
        }
        self.link.exit(Some(code), None);
    }
}

struct Harness {
    session: PiSession,
    events: mpsc::UnboundedReceiver<AdapterEvent>,
    fake: Fake,
    /// The `send` of the last turn started with [`Harness::start_turn`]: it waits for pi's
    /// answer to a plain prompt.
    sending: Option<JoinHandle<Result<(), AdapterError>>>,
}

impl Harness {
    /// Turn scenarios: the fake answers `get_state` (the run probe) and `get_session_stats`
    /// by itself.
    fn new() -> Self {
        Self::build(true, true)
    }

    /// `auto_state = false`: every `get_state` goes to the scenario (handshake tests);
    /// `auto_stats = false`: every `get_session_stats` does.
    fn build(auto_state: bool, auto_stats: bool) -> Self {
        let (adapter_out, fake_read) = tokio::io::duplex(1 << 20);
        let (fake_write, adapter_in) = tokio::io::duplex(1 << 20);
        let (exited, _) = watch::channel(None);
        let link = Arc::new(FakeLink { exited });
        let cfg = SessionConfig {
            label: "pi[test]".into(),
            gate_file: None,
            stop_grace: Duration::from_millis(500),
            request_timeout: Duration::from_secs(5),
            max_line_bytes: 1 << 20,
        };
        let (session, events) = PiSession::start(adapter_in, adapter_out, link.clone(), cfg);
        let out = Arc::new(Mutex::new(Some(fake_write)));
        let streaming = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(AtomicU64::new(0));
        let pi = Arc::new(std::sync::Mutex::new(FakePi::default()));
        let (cmd_tx, commands) = mpsc::unbounded_channel();
        {
            let out = out.clone();
            let streaming = streaming.clone();
            let stats = stats.clone();
            let link = link.clone();
            let pi = pi.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(fake_read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let cmd: Value = serde_json::from_str(&line).unwrap();
                    let auto: Option<Result<Value, String>> = {
                        let pi = pi.lock().unwrap();
                        match cmd["type"].as_str() {
                            Some("get_state") if auto_state => {
                                let mut state = pi.state.clone();
                                state.insert(
                                    "isStreaming".into(),
                                    json!(streaming.load(Ordering::SeqCst)),
                                );
                                Some(Ok(Value::Object(state)))
                            }
                            Some("get_session_stats") if auto_stats => Some(Ok(session_stats(
                                1000 * (stats.fetch_add(1, Ordering::SeqCst) + 1),
                            ))),
                            Some("clear_queue") => {
                                Some(Ok(json!({"steering": [], "followUp": []})))
                            }
                            Some("get_entries") => Some(pi.entries(cmd["since"].as_str())),
                            Some("get_fork_messages") => Some(Ok(pi.fork_messages())),
                            Some("get_commands") => pi
                                .commands
                                .clone()
                                .map(|commands| Ok(json!({ "commands": commands }))),
                            _ => None,
                        }
                    };
                    if let Some(answer) = auto {
                        pi.lock().unwrap().answered.push(cmd.clone());
                        let resp = match answer {
                            Ok(data) => {
                                json!({"type":"response","command":cmd["type"],"success":true,"id":cmd["id"],"data":data})
                            }
                            Err(error) => {
                                json!({"type":"response","command":cmd["type"],"success":false,"id":cmd["id"],"error":error})
                            }
                        };
                        if let Some(w) = out.lock().await.as_mut() {
                            w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
                        }
                    } else {
                        let _ = cmd_tx.send(cmd);
                    }
                }
                // stdin closed by the adapter: pi exits cleanly.
                if let Some(mut w) = out.lock().await.take() {
                    let _ = w.shutdown().await;
                }
                link.exit(Some(0), None);
            });
        }
        Harness {
            session,
            events,
            fake: Fake {
                out,
                commands,
                streaming,
                link,
                stats,
                pi,
            },
            sending: None,
        }
    }

    async fn next(&mut self) -> AdapterEvent {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("event in time")
            .expect("event")
    }

    /// Collects events until (and including) the first one matching `stop`.
    async fn until(&mut self, stop: impl Fn(&AdapterEvent) -> bool) -> Vec<AdapterEvent> {
        let mut out = Vec::new();
        loop {
            let ev = self.next().await;
            let done = stop(&ev);
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    /// Sends `text` and returns the id of the `prompt` pi got. The `send` goes on in the
    /// background ([`Harness::sent`]): for a plain prompt it returns once pi answered it.
    async fn start_turn(&mut self, text: &str) -> String {
        let session = self.session.clone();
        let input = TurnInput::text(text);
        self.sending = Some(tokio::spawn(async move { session.send(input).await }));
        let cmd = self.fake.expect("prompt").await;
        assert_eq!(cmd["message"], text);
        cmd["id"].as_str().unwrap().to_owned()
    }

    /// What the `send` of the last [`Harness::start_turn`] returned.
    async fn sent(&mut self) -> Result<(), AdapterError> {
        let sending = self.sending.take().expect("a turn was started");
        tokio::time::timeout(WAIT, sending)
            .await
            .expect("send returned in time")
            .unwrap()
    }

    /// Lets the session learn pi's commands (`get_commands`), as the handshake does; the fake
    /// answers `get_commands` with them from now on.
    async fn load_commands(&mut self, commands: Value) {
        self.fake.pi.lock().unwrap().commands = Some(commands);
        self.session.get_commands().await.unwrap();
    }
}

fn completed(ev: &AdapterEvent) -> bool {
    matches!(ev, AdapterEvent::TurnCompleted { .. })
}

fn final_text(events: &[AdapterEvent], want: &str) -> bool {
    events.iter().any(|e| {
        matches!(e, AdapterEvent::ItemCompleted { body: Some(ItemBody::AgentMessage { text }), .. } if text == want)
    })
}

#[tokio::test]
async fn normal_turn_streams_items_and_completes() {
    let mut h = Harness::new();
    let prompt = h.start_turn("Reply with exactly: OK").await;
    h.fake.play("turn_ok.jsonl", &prompt).await;
    let events = h.until(completed).await;

    assert_eq!(events[0], AdapterEvent::TurnStarted);
    assert!(events.contains(&AdapterEvent::SessionInfo {
        model: None,
        permission_mode: None,
        effort: Some("high".into())
    }));
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemStarted {
            body: ItemBody::Reasoning { .. },
            ..
        }
    )));
    assert!(final_text(&events, "OK"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnUsage { .. }))
    );
    // User messages are created by the engine, never by the adapter.
    assert!(!events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemStarted {
            body: ItemBody::UserMessage { .. },
            ..
        }
    )));
    match events.last().unwrap() {
        AdapterEvent::TurnCompleted {
            status,
            usage,
            error,
            ..
        } => {
            assert_eq!(*status, TurnStatus::Completed);
            assert!(usage.is_some_and(|u| u.output_tokens > 0));
            assert!(error.is_none());
            // One assistant message → one `get_session_stats` → 1000 tokens of the window.
            assert_eq!(h.fake.stats.load(Ordering::SeqCst), 1);
            assert_eq!(
                usage.unwrap().context,
                Some(ContextUsage {
                    used_tokens: 1000,
                    window_tokens: WINDOW
                })
            );
        }
        other => panic!("{other:?}"),
    }
    // The context was also relayed while the turn ran.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnUsage { usage } if usage.context.is_some()))
    );
}

#[tokio::test]
async fn gate_approval_allows_the_command() {
    let mut h = Harness::new();
    let prompt = h.start_turn("Use your bash tool to run exactly `echo hello-aas` and then reply with the single word DONE.").await;

    // Play until the gate asks, answer, then let the rest play.
    let fixture = h.fake.play("turn_gate_allow.jsonl", &prompt);
    let session = h.session.clone();
    let mut events = h.events;
    let driver = tokio::spawn(async move {
        let mut seen = Vec::new();
        loop {
            let ev = tokio::time::timeout(WAIT, events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AdapterEvent::InteractionRequested {
                request_id,
                request,
                item_key,
                ..
            } = &ev
            {
                match request {
                    InteractionRequest::Approval {
                        subject: Subject::Command { command, .. },
                        options,
                        ..
                    } => {
                        assert_eq!(command, "echo hello-aas");
                        assert!(options.iter().any(|o| o.id == "allowSession"));
                    }
                    other => panic!("{other:?}"),
                }
                assert!(item_key.as_deref().is_some_and(|k| k.starts_with("tool:")));
                session
                    .respond(
                        request_id,
                        &InteractionResolution::Approval {
                            option_id: "allow".into(),
                            feedback: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            let done = completed(&ev);
            seen.push(ev);
            if done {
                return seen;
            }
        }
    });
    let awaited = fixture.await;
    assert_eq!(awaited[0]["type"], "extension_ui_response");
    assert_eq!(awaited[0]["value"], "{\"choice\":\"allow\"}");
    let events = driver.await.unwrap();
    let tool = events
        .iter()
        .find_map(|e| match e {
            AdapterEvent::ItemCompleted {
                body: Some(ItemBody::CommandExecution { output, .. }),
                status,
                ..
            } => Some((output.clone(), *status)),
            _ => None,
        })
        .unwrap();
    assert_eq!(tool, ("hello-aas\n".to_owned(), ItemStatus::Completed));
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemDelta {
            field: DeltaField::Output,
            ..
        }
    )));
    assert!(final_text(&events, "DONE"));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn gate_denial_with_feedback_declines_the_tool() {
    let mut h = Harness::new();
    let prompt = h.start_turn("Use your bash tool to run exactly `echo second-aas` and then reply with the single word DONE.").await;
    let fixture = h.fake.play("turn_gate_deny.jsonl", &prompt);
    let session = h.session.clone();
    let mut events = h.events;
    let driver = tokio::spawn(async move {
        let mut seen = Vec::new();
        loop {
            let ev = tokio::time::timeout(WAIT, events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AdapterEvent::InteractionRequested { request_id, .. } = &ev {
                session
                    .respond(
                        request_id,
                        &InteractionResolution::Approval {
                            option_id: "denyWithFeedback".into(),
                            feedback: Some("not now".into()),
                        },
                    )
                    .await
                    .unwrap();
                // A second answer to the same request is rejected.
                let again = session
                    .respond(request_id, &InteractionResolution::Dismissed)
                    .await;
                assert!(matches!(again, Err(AdapterError::UnknownRequest(_))));
            }
            let done = completed(&ev);
            seen.push(ev);
            if done {
                return seen;
            }
        }
    });
    let awaited = fixture.await;
    let value: Value = serde_json::from_str(awaited[0]["value"].as_str().unwrap()).unwrap();
    assert_eq!(value, json!({"choice": "deny", "feedback": "not now"}));
    let events = driver.await.unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted {
            body: Some(ItemBody::CommandExecution { .. }),
            status: ItemStatus::Declined,
            ..
        }
    )));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn steer_is_delivered_into_the_running_turn() {
    let mut h = Harness::new();
    let prompt = h.start_turn("Use your bash tool to run exactly `sleep 4; echo slept` and then reply with the single word READY.").await;
    let fixture = h.fake.play("turn_steer.jsonl", &prompt);
    let session = h.session.clone();
    let mut events = h.events;
    let driver = tokio::spawn(async move {
        let mut seen = Vec::new();
        loop {
            let ev = tokio::time::timeout(WAIT, events.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(
                &ev,
                AdapterEvent::ItemStarted {
                    body: ItemBody::CommandExecution { .. },
                    ..
                }
            ) {
                session
                    .steer(TurnInput::text(
                        "After that, also append the word BANANA to your reply.",
                    ))
                    .await
                    .unwrap();
            }
            let done = completed(&ev);
            seen.push(ev);
            if done {
                return seen;
            }
        }
    });
    let awaited = fixture.await;
    assert_eq!(awaited[0]["type"], "steer");
    assert_eq!(
        awaited[0]["message"],
        "After that, also append the word BANANA to your reply."
    );
    let events = driver.await.unwrap();
    assert!(final_text(&events, "READY BANANA"));
    assert!(!events.iter().any(
        |e| matches!(e, AdapterEvent::Notice { code: Some(c), .. } if c == "steerNotDelivered")
    ));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn interrupt_aborts_and_reports_interrupted() {
    let mut h = Harness::new();
    let prompt = h
        .start_turn("Count from 1 to 200, one number per line, nothing else.")
        .await;
    let fixture = h.fake.play("turn_abort.jsonl", &prompt);
    let session = h.session.clone();
    let mut events = h.events;
    let driver = tokio::spawn(async move {
        let mut seen = Vec::new();
        let mut interrupted = false;
        loop {
            let ev = tokio::time::timeout(WAIT, events.recv())
                .await
                .unwrap()
                .unwrap();
            if !interrupted
                && matches!(
                    &ev,
                    AdapterEvent::ItemStarted {
                        body: ItemBody::AgentMessage { .. },
                        ..
                    }
                )
            {
                interrupted = true;
                session.interrupt().await.unwrap();
            }
            let done = completed(&ev);
            seen.push(ev);
            if done {
                return seen;
            }
        }
    });
    let awaited = fixture.await;
    assert_eq!(awaited[0]["type"], "abort");
    let events = driver.await.unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted {
            status: ItemStatus::Interrupted,
            ..
        }
    )));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        }
    ));
}

#[tokio::test]
async fn an_abort_sent_before_the_run_starts_is_sent_again_when_it_starts() {
    let mut h = Harness::new();
    let prompt = h.start_turn("hello").await;
    // pi accepted the prompt (and is streaming from then on); the run itself has not started:
    // the prompt is still in its preflight (auth check, auto-compaction, extension hooks).
    h.fake.streaming.store(true, Ordering::SeqCst);
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    h.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    h.session.interrupt().await.unwrap();
    let first = h.fake.expect("abort").await;
    h.fake.respond(&first, None).await;
    // pi 0.85.1's abort does not cancel the preflight: the run starts anyway.
    h.fake.write(&json!({"type":"agent_start"})).await;
    let second = h.fake.expect("abort").await;
    assert_ne!(first["id"], second["id"]);
    h.fake.respond(&second, None).await;
    h.fake.streaming.store(false, Ordering::SeqCst);
    h.fake
        .write(&json!({"type":"agent_end","messages":[]}))
        .await;
    h.fake.write(&json!({"type":"agent_settled"})).await;
    let events = h.until(completed).await;
    assert!(
        matches!(
            events.last().unwrap(),
            AdapterEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        ),
        "{events:?}"
    );
    // An abort during the run itself is sent once.
    let prompt = h.start_turn("again").await;
    h.fake.streaming.store(true, Ordering::SeqCst);
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    h.fake.write(&json!({"type":"agent_start"})).await;
    h.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    h.session.interrupt().await.unwrap();
    let only = h.fake.expect("abort").await;
    h.fake.respond(&only, None).await;
    h.fake.streaming.store(false, Ordering::SeqCst);
    h.fake
        .write(&json!({"type":"agent_end","messages":[]}))
        .await;
    h.fake.write(&json!({"type":"agent_settled"})).await;
    let events = h.until(completed).await;
    assert!(
        matches!(
            events.last().unwrap(),
            AdapterEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        ),
        "{events:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), h.fake.commands.recv())
            .await
            .is_err(),
        "no second abort"
    );
}

#[tokio::test]
async fn prompt_handled_without_a_run_completes_through_the_state_probe() {
    let mut h = Harness::new();
    let prompt = h.start_turn("/session-name hello").await;
    // An extension command: pi answers the prompt but never starts a run.
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    let events = h.until(completed).await;
    assert_eq!(events[0], AdapterEvent::TurnStarted);
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: None,
            ..
        }
    ));
    assert_eq!(h.sent().await, Ok(()));
    // The session is idle again.
    h.start_turn("next").await;
}

#[tokio::test]
async fn rejected_prompt_fails_the_turn() {
    let mut h = Harness::new();
    let prompt = h.start_turn("hello").await;
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":false,"id":prompt,"error":"No API key found for orcarouter"}))
        .await;
    match h.next().await {
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some(err),
            ..
        } => {
            assert!(err.message.contains("No API key"));
            assert_eq!(err.kind, "harnessError");
        }
        other => panic!("{other:?}"),
    }
    // The refusal is reported by the turn's `TurnCompleted`; `send` itself succeeded.
    assert_eq!(h.sent().await, Ok(()));
    // The session is idle again.
    h.start_turn("again").await;
}

#[tokio::test]
async fn second_send_while_running_is_refused() {
    let mut h = Harness::new();
    h.start_turn("one").await;
    let err = h.session.send(TurnInput::text("two")).await.unwrap_err();
    assert!(matches!(err, AdapterError::Other(_)));
}

#[tokio::test]
async fn process_exit_mid_turn_ends_with_exited_only() {
    let mut h = Harness::new();
    let prompt = h.start_turn("hello").await;
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    h.fake.write(&json!({"type":"agent_start"})).await;
    h.fake
        .write(&json!({"type":"message_start","message":{"role":"assistant","content":[]}}))
        .await;
    h.fake
        .write(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}))
        .await;
    h.fake.exit(1).await;
    let events = h.until(|e| matches!(e, AdapterEvent::Exited { .. })).await;
    assert!(!events.iter().any(completed));
    match events.last().unwrap() {
        AdapterEvent::Exited { info } => assert_eq!(info.code, Some(1)),
        other => panic!("{other:?}"),
    }
    assert!(
        h.events.recv().await.is_none(),
        "channel closes after Exited"
    );
    assert!(matches!(
        h.session.send(TurnInput::text("x")).await,
        Err(AdapterError::Closed | AdapterError::Other(_))
    ));
}

#[tokio::test]
async fn shutdown_closes_stdin_and_exited_is_last() {
    let mut h = Harness::new();
    let info = h.session.shutdown(StopReason::User).await;
    assert_eq!(info.code, Some(0));
    assert_eq!(info.stopped, None);
    // Idempotent.
    assert_eq!(h.session.shutdown(StopReason::Idle).await, info);
    let events = h.until(|e| matches!(e, AdapterEvent::Exited { .. })).await;
    assert_eq!(events.len(), 1);
}

/// Starts a turn and lets pi accept it and start its run.
async fn running_turn(h: &mut Harness, text: &str) {
    // pi is streaming from the prompt response on (the adapter's run probe sees it).
    h.fake.streaming.store(true, Ordering::SeqCst);
    let prompt = h.start_turn(text).await;
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    h.fake.write(&json!({"type":"agent_start"})).await;
    assert_eq!(h.next().await, AdapterEvent::TurnStarted);
}

#[tokio::test]
async fn timed_dialogs_are_not_timed_by_the_adapter() {
    // pi closes a dialog with a `timeout` by itself and does not tell the client. The adapter
    // does not guess when that happened: the interaction stays open until it is answered (here
    // by the engine's expiry at the end of the turn), and the prompt says that pi may answer by
    // itself.
    let mut h = Harness::new();
    running_turn(&mut h, "hello").await;
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"u9","method":"select","title":"Pick","options":["A","B"],"timeout":50}))
        .await;
    match h.next().await {
        AdapterEvent::InteractionRequested {
            request_id,
            request: InteractionRequest::Question { questions, .. },
            ..
        } => {
            assert_eq!(request_id, "u9");
            assert!(
                questions[0].prompt.ends_with(
                    "pi answers this dialog with its default after 1 s without a reply."
                )
            );
        }
        other => panic!("{other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.events.try_recv().is_err(),
        "nothing happens when the dialog's time is up"
    );
    // pi ends the run's agent loop, then settles it.
    h.fake
        .write(&json!({"type":"agent_end","messages":[]}))
        .await;
    h.fake.write(&json!({"type":"agent_settled"})).await;
    let events = h.until(completed).await;
    assert_eq!(
        events.len(),
        1,
        "the end of the turn withdraws nothing: {events:?}"
    );
    // The engine expired the turn's request and answers pi: the dismissal (pi ignores an
    // answer for a dialog it closed itself).
    h.session
        .expire_request("u9", ExpireReason::TurnEnded)
        .await
        .unwrap();
    let answer = h.fake.expect("extension_ui_response").await;
    assert_eq!(
        answer,
        json!({"type":"extension_ui_response","id":"u9","cancelled":true})
    );
    let err = h
        .session
        .expire_request("u9", ExpireReason::TurnEnded)
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::UnknownRequest(_)));
}

#[tokio::test]
async fn the_gate_reports_a_dialog_closed_by_the_abort() {
    let mut h = Harness::new();
    running_turn(&mut h, "run it").await;
    h.fake
        .write(&json!({"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{"command":"sleep 100"}}))
        .await;
    let gate =
        json!({"v":1,"toolCallId":"call_1","toolName":"bash","input":{"command":"sleep 100"}});
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"g1","method":"select","title":format!("aas-gate:{gate}"),
            "options":["allow","allowSession","deny"]}))
        .await;
    let events = h
        .until(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        .await;
    assert!(
        matches!(events.last().unwrap(), AdapterEvent::InteractionRequested { request_id, .. } if request_id == "g1")
    );

    h.session.interrupt().await.unwrap();
    h.fake.expect("abort").await;
    // Asking pi to abort is not a withdrawal by itself: pi closes the dialog, the gate says so.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        h.events.try_recv().is_err(),
        "the interaction stays until the gate reports"
    );
    let report = json!({"v":1,"event":"dialogClosed","toolCallId":"call_1","reason":"aborted"});
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"n1","method":"notify","notifyType":"info","message":format!("aas-gate:{report}")}))
        .await;
    assert_eq!(
        h.next().await,
        AdapterEvent::InteractionWithdrawn {
            request_id: "g1".into()
        }
    );
    // The gate blocked the tool: pi reports it as an error, the adapter as declined.
    h.fake
        .write(&json!({"type":"tool_execution_end","toolCallId":"call_1","toolName":"bash",
            "result":{"content":[{"type":"text","text":"The approval request was cancelled."}]},"isError":true}))
        .await;
    // pi ends the run's agent loop, then settles it.
    h.fake
        .write(&json!({"type":"agent_end","messages":[]}))
        .await;
    h.fake.write(&json!({"type":"agent_settled"})).await;
    let events = h.until(completed).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            AdapterEvent::ItemCompleted {
                status: ItemStatus::Declined,
                ..
            }
        )),
        "{events:?}"
    );
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        }
    ));
    // A report for a dialog that was answered (or is unknown) changes nothing.
    let late = json!({"v":1,"event":"dialogClosed","toolCallId":"call_1","reason":"answered"});
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"n2","method":"notify","message":format!("aas-gate:{late}")}))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.events.try_recv().is_err());
}

#[tokio::test]
async fn settling_waits_for_the_context_of_the_last_message() {
    let mut h = Harness::build(true, false);
    running_turn(&mut h, "hello").await;
    h.fake
        .write(&json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Hi"}],
            "stopReason":"stop","usage":{"input":100,"output":2,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.001}}}}))
        .await;
    let stats = h.fake.expect("get_session_stats").await;
    // pi settles before it answers the stats request: the turn waits for the answer.
    // pi ends the run's agent loop, then settles it.
    h.fake
        .write(&json!({"type":"agent_end","messages":[]}))
        .await;
    h.fake.write(&json!({"type":"agent_settled"})).await;
    let early = h
        .until(|e| matches!(e, AdapterEvent::TurnUsage { .. }))
        .await;
    assert!(early.iter().all(|e| !completed(e)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        h.events.try_recv().is_err(),
        "the turn is not completed before the stats answer"
    );
    h.fake.respond(&stats, Some(session_stats(4200))).await;
    let events = h.until(completed).await;
    let context = Some(ContextUsage {
        used_tokens: 4200,
        window_tokens: WINDOW,
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::TurnUsage { usage } if usage.context == context))
    );
    match events.last().unwrap() {
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Some(usage),
            ..
        } => {
            assert_eq!(usage.context, context);
            assert_eq!(usage.input_tokens, 100);
        }
        other => panic!("{other:?}"),
    }

    // Right after a compaction pi knows no context (`tokens: null`): nothing is reported.
    running_turn(&mut h, "again").await;
    h.fake
        .write(&json!({"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"stop",
            "usage":{"input":10,"output":1}}}))
        .await;
    let stats = h.fake.expect("get_session_stats").await;
    let mut unknown = session_stats(0);
    unknown["contextUsage"] = json!({"tokens": null, "contextWindow": WINDOW, "percent": null});
    h.fake.respond(&stats, Some(unknown)).await;
    // pi ends the run's agent loop, then settles it.
    h.fake
        .write(&json!({"type":"agent_end","messages":[]}))
        .await;
    h.fake.write(&json!({"type":"agent_settled"})).await;
    let events = h.until(completed).await;
    assert!(
        matches!(events.last().unwrap(), AdapterEvent::TurnCompleted { usage: Some(u), .. } if u.context.is_none())
    );
}

#[tokio::test]
async fn compact_runs_the_rpc_compaction_as_a_turn() {
    let mut h = Harness::new();
    h.session
        .send(TurnInput::text("/compact keep the API decisions"))
        .await
        .unwrap();
    let cmd = h.fake.expect("compact").await;
    assert_eq!(cmd["customInstructions"], "keep the API decisions");
    h.fake
        .write(&json!({"type":"compaction_start","reason":"manual"}))
        .await;
    assert_eq!(h.next().await, AdapterEvent::TurnStarted);
    let result = json!({
        "summary": "Summary of conversation...", "firstKeptEntryId": "abc123", "tokensBefore": 150000,
        "estimatedTokensAfter": 32000,
        "usage": {"input": 32000, "output": 1200, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 33200,
            "cost": {"input": 0.01, "output": 0.02, "cacheRead": 0, "cacheWrite": 0, "total": 0.03}},
        "details": {}
    });
    h.fake
        .write(&json!({"type":"compaction_end","reason":"manual","result":result,"aborted":false,"willRetry":false}))
        .await;
    h.fake.respond(&cmd, Some(result)).await;
    let events = h.until(completed).await;
    assert!(events.iter().any(|e| matches!(e, AdapterEvent::Notice { message, .. } if message == "Conversation compacted (150000 tokens before)")));
    match events.last().unwrap() {
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Some(usage),
            error: None,
            ..
        } => {
            assert_eq!(
                (usage.input_tokens, usage.output_tokens, usage.cost_usd),
                (32000, 1200, Some(0.03))
            );
        }
        other => panic!("{other:?}"),
    }
    // The session is idle again.
    h.start_turn("next").await;
}

#[tokio::test]
async fn a_compaction_pi_refuses_fails_the_turn() {
    // Recorded from pi 0.85.1 with an empty session.
    let mut h = Harness::new();
    h.session.send(TurnInput::text("/compact")).await.unwrap();
    let cmd = h.fake.expect("compact").await;
    assert!(cmd.get("customInstructions").is_none());
    h.fake
        .write(&json!({"type":"compaction_start","reason":"manual"}))
        .await;
    h.fake
        .write(
            &json!({"type":"compaction_end","reason":"manual","aborted":false,"willRetry":false,
            "errorMessage":"Compaction failed: Nothing to compact (session too small)"}),
        )
        .await;
    h.fake
        .write(
            &json!({"id":cmd["id"],"type":"response","command":"compact","success":false,
            "error":"Nothing to compact (session too small)"}),
        )
        .await;
    let events = h.until(completed).await;
    assert_eq!(events[0], AdapterEvent::TurnStarted);
    assert!(events.iter().any(
        |e| matches!(e, AdapterEvent::Notice { level: NoticeLevel::Error, message, .. }
        if message == "Compaction failed: Nothing to compact (session too small)")
    ));
    match events.last().unwrap() {
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some(error),
            ..
        } => {
            assert_eq!(error.message, "Nothing to compact (session too small)");
            assert_eq!(error.kind, "harnessError");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_interrupted_compaction_is_interrupted() {
    let mut h = Harness::new();
    h.session.send(TurnInput::text("/compact")).await.unwrap();
    let cmd = h.fake.expect("compact").await;
    h.fake
        .write(&json!({"type":"compaction_start","reason":"manual"}))
        .await;
    assert_eq!(h.next().await, AdapterEvent::TurnStarted);
    h.session.interrupt().await.unwrap();
    h.fake.expect("abort").await;
    h.fake.write(&json!({"type":"compaction_end","reason":"manual","result":null,"aborted":true,"willRetry":false})).await;
    h.fake
        .write(&json!({"id":cmd["id"],"type":"response","command":"compact","success":false,"error":"Compaction cancelled"}))
        .await;
    let events = h.until(completed).await;
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            ..
        }
    ));
}

#[tokio::test]
async fn a_compact_command_of_pi_itself_is_left_to_pi() {
    let mut h = Harness::new();
    let session = h.session.clone();
    let listing = tokio::spawn(async move { session.get_commands().await });
    let cmd = h.fake.expect("get_commands").await;
    h.fake
        .respond(&cmd, Some(json!({"commands": [{"name": "compact", "description": "My own", "source": "extension"}]})))
        .await;
    assert_eq!(listing.await.unwrap().unwrap().len(), 1);
    // An extension owns `/compact`: the text goes to pi as a prompt.
    h.session.send(TurnInput::text("/compact")).await.unwrap();
    let prompt = h.fake.expect("prompt").await;
    assert_eq!(prompt["message"], "/compact");
}

#[tokio::test]
async fn handshake_checks_session_and_applies_settings() {
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/handshake.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let mut h = Harness::build(false, true);
    let session = h.session.clone();
    let settings = ThreadSettings {
        model: Some("orcarouter/deepseek/deepseek-v4.1-flash".into()),
        effort: Some("low".into()),
        permission_mode: None,
    };
    let task = tokio::spawn(async move {
        handshake(
            &session,
            "611067bd-f742-47e9-9db7-c2a49a25ef82",
            &settings,
            "askCommands",
        )
        .await
    });
    let cmd = h.fake.expect("get_state").await;
    h.fake.respond(&cmd, Some(fixture["state"].clone())).await;
    // state reports obsidian/Qwen3.8-27B + "medium": both differ → set_model, set_thinking_level
    let cmd = h.fake.expect("set_model").await;
    assert_eq!(cmd["provider"], "orcarouter");
    assert_eq!(cmd["modelId"], "deepseek/deepseek-v4.1-flash");
    h.fake
        .respond(&cmd, Some(fixture["setModel"].clone()))
        .await;
    let cmd = h.fake.expect("set_thinking_level").await;
    assert_eq!(cmd["level"], "low");
    h.fake.respond(&cmd, None).await;
    let cmd = h.fake.expect("get_commands").await;
    h.fake
        .respond(&cmd, Some(fixture["commands"].clone()))
        .await;
    task.await.unwrap().unwrap();

    let info = h.next().await;
    assert_eq!(
        info,
        AdapterEvent::SessionInfo {
            model: Some("orcarouter/deepseek/deepseek-v4.1-flash".into()),
            permission_mode: Some("askCommands".into()),
            effort: Some("low".into())
        }
    );
    match h.next().await {
        AdapterEvent::CommandsChanged { commands } => {
            // pi 0.85.1's bundled llama.cpp extension (`<inline:llama.cpp>`) only works in pi's
            // interactive mode: not offered.
            assert!(!commands.iter().any(|c| c.name == "llama"));
            // The RPC `compact` command is offered as `/compact`.
            assert_eq!(commands.last().map(|c| c.name.as_str()), Some("compact"));
        }
        other => panic!("{other:?}"),
    }

    // A different session id is a protocol error.
    let session = h.session.clone();
    let task = tokio::spawn(async move {
        handshake(&session, "other-id", &ThreadSettings::default(), "ask").await
    });
    let cmd = h.fake.expect("get_state").await;
    h.fake.respond(&cmd, Some(fixture["state"].clone())).await;
    assert!(matches!(
        task.await.unwrap(),
        Err(AdapterError::Protocol(_))
    ));
}

#[tokio::test]
async fn shutdown_is_not_blocked_by_a_write_pi_never_reads() {
    // pi reads nothing any more: a large prompt fills the pipe and its write never completes,
    // holding the writer.
    let (adapter_in, _pi_out) = tokio::io::duplex(1024);
    let (_pi_in, adapter_out) = tokio::io::duplex(1024);
    let link = Arc::new(FakeLink {
        exited: watch::channel(None).0,
    });
    let cfg = SessionConfig {
        label: "pi[test]".into(),
        gate_file: None,
        stop_grace: Duration::from_millis(200),
        request_timeout: WAIT,
        max_line_bytes: 1 << 20,
    };
    let (session, _events) = PiSession::start(adapter_in, adapter_out, link, cfg);
    let stuck = {
        let session = session.clone();
        tokio::spawn(async move { session.send(TurnInput::text("x".repeat(64 * 1024))).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!stuck.is_finished(), "the write is stuck on the full pipe");
    let info = tokio::time::timeout(WAIT, session.shutdown(StopReason::User))
        .await
        .expect("the staged stop reached the termination stage");
    assert_eq!(info.stopped, Some(StopReason::User));
    let sent = tokio::time::timeout(WAIT, stuck)
        .await
        .expect("the stuck write was abandoned")
        .unwrap();
    assert!(sent.is_err());
}

// ----- runs pi starts by itself, the race with a busy agent, dialogs outside a turn -----------

/// The messages of the notices with `code`, in order.
fn notices(events: &[AdapterEvent], code: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Notice {
                message,
                code: Some(c),
                ..
            } if c == code => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn count(events: &[AdapterEvent], pred: impl Fn(&AdapterEvent) -> bool) -> usize {
    events.iter().filter(|e| pred(e)).count()
}

/// Commands of the live test's extension, as `get_commands` lists them.
fn live_commands() -> Value {
    json!([
        {"name": "aas-later", "description": "aas live test: start a run by itself later (custom message)", "source": "extension"},
        {"name": "skill:review", "description": "A skill", "source": "skill"}
    ])
}

/// Nothing more reaches pi (the fake answers `get_state` and `get_session_stats` itself).
async fn nothing_written(h: &mut Harness) {
    assert!(
        tokio::time::timeout(Duration::from_millis(200), h.fake.commands.recv())
            .await
            .is_err(),
        "nothing more was written to pi"
    );
}

/// The assistant message `text` of a run, reported whole (as pi does for a provider that
/// does not stream).
async fn assistant(h: &mut Harness, text: &str) {
    h.fake
        .write(&json!({"type":"message_start","message":{"role":"assistant","content":[]}}))
        .await;
    h.fake
        .write(&json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":text}],
            "stopReason":"stop","usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0}}}))
        .await;
}

/// The end of a run: its (last) agent loop ends, then the run settles.
async fn run_ends(h: &mut Harness) {
    h.fake
        .write(&json!({"type":"agent_end","messages":[]}))
        .await;
    h.fake.write(&json!({"type":"agent_settled"})).await;
}

// ----- anchors, forks, names, status, commands (recorded in rec2, pi 0.85.1) ------------------

/// `tests/fixtures/rec2.json`.
fn rec2() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rec2.json"),
        )
        .unwrap(),
    )
    .unwrap()
}

fn array(v: &Value) -> Vec<Value> {
    v.as_array().expect("an array").clone()
}

/// A recorded response to the adapter's own request `cmd` (the recorded id replaced).
async fn reply(h: &Harness, cmd: &Value, recorded: &Value) {
    let mut resp = recorded.clone();
    resp["id"] = cmd["id"].clone();
    h.fake.write(&resp).await;
}

/// The anchor reported right before the turn's `TurnCompleted` (the events of one turn).
fn anchor_of(events: &[AdapterEvent]) -> Option<Value> {
    let completed_at = events.iter().position(completed).expect("a completed turn");
    match completed_at.checked_sub(1).map(|i| &events[i]) {
        Some(AdapterEvent::TurnAnchor { anchor }) => Some(anchor.clone()),
        _ => {
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, AdapterEvent::TurnAnchor { .. })),
                "an anchor comes right before TurnCompleted: {events:?}"
            );
            None
        }
    }
}

/// The recorded entries of the three turns (session dfa5c5b7): the entries before the first
/// user message, then the entries each turn added.
fn recorded_turn_entries(rec: &Value) -> (Vec<Value>, [Vec<Value>; 3]) {
    let t = &rec["threeTurns"];
    let all = array(&t["entriesAfterOne"]["entries"]);
    let first_user = all
        .iter()
        .position(|e| e["message"]["role"] == "user")
        .unwrap();
    (
        all[..first_user].to_vec(),
        [
            all[first_user..].to_vec(),
            array(&t["entriesAfterTwo"]["entries"]),
            array(&t["entriesAfterThree"]["entries"]),
        ],
    )
}

/// Runs a turn whose run adds `entries` to the session tree; returns its events.
async fn turn_adding(h: &mut Harness, text: &str, entries: &[Value]) -> Vec<AdapterEvent> {
    running_turn(h, text).await;
    h.fake.append(entries);
    run_ends(h).await;
    h.until(completed).await
}

#[tokio::test]
async fn recorded_turns_are_anchored_by_the_entries_they_added() {
    let rec = rec2();
    let t = &rec["threeTurns"];
    let (before, turns) = recorded_turn_entries(&rec);
    let mut h = Harness::new();
    h.fake.set_session(&t["stateStart"], before);
    h.fake.pi.lock().unwrap().commands = Some(t["commands"].clone());
    handshake(
        &h.session,
        "dfa5c5b7-269c-44a2-ba1b-9992ea29c424",
        &ThreadSettings::default(),
        "ask",
    )
    .await
    .unwrap();
    h.until(|e| matches!(e, AdapterEvent::CommandsChanged { .. }))
        .await;
    // The handshake learnt the leaf: the session had no user message yet, so every entry was
    // asked for.
    assert_eq!(
        h.fake.answered("get_entries"),
        [json!({"id": h.fake.answered("get_entries")[0]["id"], "type": "get_entries"})]
    );

    let expected = [
        ("Reply with exactly: ONE", "6229f6ef", "41c51822"),
        ("Reply with exactly: TWO", "46f74d5b", "343ba349"),
        ("Reply with exactly: THREE", "20e3f58d", "eb03ba61"),
    ];
    for ((text, leaf, user), entries) in expected.iter().zip(turns.iter()) {
        let events = turn_adding(&mut h, text, entries).await;
        assert_eq!(
            anchor_of(&events),
            Some(json!({"leafId": leaf, "userEntryId": user})),
            "{events:?}"
        );
    }
    // Each turn asked for the entries after the previous turn's leaf (the requests recorded from
    // pi after TWO and THREE were the same).
    let since: Vec<Value> = h
        .fake
        .answered("get_entries")
        .iter()
        .map(|c| c["since"].clone())
        .collect();
    assert_eq!(
        since,
        [
            Value::Null,
            json!("da36193f"),
            json!("6229f6ef"),
            json!("46f74d5b")
        ]
    );
    // The recorded user messages are what the fake lists (the shape of `get_fork_messages`).
    assert_eq!(h.fake.pi.lock().unwrap().fork_messages(), t["forkMessages"]);
}

#[tokio::test]
async fn a_turn_after_one_completed_for_a_new_run_is_anchored_without_its_user_entry() {
    let rec = rec2();
    let (before, turns) = recorded_turn_entries(&rec);
    let mut h = Harness::new();
    h.fake.set_session(&rec["threeTurns"]["stateStart"], before);
    // No handshake: where the first turn begins is not known; its anchor has the leaf only.
    let events = turn_adding(&mut h, "Reply with exactly: ONE", &turns[0]).await;
    assert_eq!(anchor_of(&events), Some(json!({"leafId": "6229f6ef"})));
    // A run started from the end of the next turn (pi writes its `agent_start` before the
    // turn's `agent_settled`): that turn completes at once, without an anchor.
    running_turn(&mut h, "Reply with exactly: TWO").await;
    h.fake.append(&turns[1]);
    h.fake
        .write(&json!({"type":"agent_end","messages":[]}))
        .await;
    h.fake.write(&json!({"type":"agent_start"})).await;
    h.fake.write(&json!({"type":"agent_settled"})).await;
    let events = h.until(completed).await;
    assert_eq!(anchor_of(&events), None);
    assert_eq!(h.next().await, AdapterEvent::TurnStarted);
    // The run's turn: its entries follow the earlier turn's, so only its leaf is known.
    h.fake.append(&turns[2]);
    run_ends(&mut h).await;
    let events = h.until(completed).await;
    assert_eq!(anchor_of(&events), Some(json!({"leafId": "20e3f58d"})));
    // From then on turns are anchored whole again.
    let next = [
        json!({"type":"message","id":"u4","parentId":"20e3f58d","message":{"role":"user","content":[{"type":"text","text":"four"}]}}),
        json!({"type":"message","id":"a4","parentId":"u4","message":{"role":"assistant","content":[]}}),
    ];
    let events = turn_adding(&mut h, "four", &next).await;
    assert_eq!(
        anchor_of(&events),
        Some(json!({"leafId": "a4", "userEntryId": "u4"}))
    );
}

/// The commands of the three turns' session with the gate's fork command.
fn with_gate_commands(rec: &Value) -> Value {
    let mut commands = array(&rec["threeTurns"]["commands"]);
    commands.push(json!({"name": "aas-gate-fork", "description": "agent-app-server: branch this session at an entry (internal)", "source": "extension", "sourceInfo": {"path": "C:\\rec\\pi-state\\aas-gate-v3.ts", "source": "local"}}));
    Value::Array(commands)
}

#[tokio::test]
async fn a_fork_at_a_turn_runs_the_gate_command_and_takes_the_branch_pi_made() {
    let rec = rec2();
    let (before, turns) = recorded_turn_entries(&rec);
    let mut h = Harness::new();
    let mut entries = before;
    entries.extend(turns.iter().flatten().cloned());
    h.fake
        .set_session(&rec["threeTurns"]["stateAfterTurns"], entries.clone());
    h.fake.pi.lock().unwrap().commands = Some(with_gate_commands(&rec));
    let session = h.session.clone();
    let fork = tokio::spawn(async move {
        session
            .fork_to(&ForkTarget {
                entry_id: "6229f6ef".into(),
                position: "at",
            })
            .await
    });
    let prompt = h.fake.expect("prompt").await;
    assert_eq!(prompt["message"], "/aas-gate-fork 6229f6ef at");
    assert!(prompt.get("streamingBehavior").is_none());
    // Recorded: an extension's `ctx.fork("6229f6ef", {position: "at"})` in a second process.
    let fork_at = &rec["forkAt"];
    for event in array(&fork_at["events"]) {
        h.fake.write(&event).await;
    }
    let through = entries.iter().position(|e| e["id"] == "6229f6ef").unwrap();
    h.fake
        .set_session(&fork_at["stateAfter"], entries[..=through].to_vec());
    reply(&h, &prompt, &fork_at["response"]).await;
    assert_eq!(
        fork.await.unwrap(),
        Ok("01a0e93f-94c2-72cb-ba2c-cd9a788c132f".to_owned())
    );
    // The other extensions' notifications are relayed as they are.
    let notified = h
        .until(|e| matches!(e, AdapterEvent::Notice { message, .. } if message.contains("fork-at:withSession")))
        .await;
    assert_eq!(notices(&notified, "extensionNotify").len(), 4);
}

#[tokio::test]
async fn a_fork_pi_does_not_make_fails_with_pi_reason() {
    let rec = rec2();
    let mut h = Harness::new();
    h.fake
        .set_session(&rec["threeTurns"]["stateAfterTurns"], Vec::new());
    h.fake.pi.lock().unwrap().commands = Some(with_gate_commands(&rec));
    let session = h.session.clone();
    let fork = tokio::spawn(async move {
        session
            .fork_to(&ForkTarget {
                entry_id: "nonexistent".into(),
                position: "before",
            })
            .await
    });
    let prompt = h.fake.expect("prompt").await;
    assert_eq!(prompt["message"], "/aas-gate-fork nonexistent before");
    // The gate reports pi's error (recorded as the RPC `fork`'s answer to an unknown entry).
    let error = rec["forkRefused"]["error"].as_str().unwrap();
    let report = json!({"v":1,"event":"forkFailed","error":error});
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"n1","method":"notify","notifyType":"info","message":format!("aas-gate:{report}")}))
        .await;
    h.fake.respond(&prompt, None).await;
    assert_eq!(
        fork.await.unwrap(),
        Err(AdapterError::Harness("Invalid entry ID for forking".into()))
    );
    // The report is the adapter's, not the user's.
    assert!(h.events.try_recv().is_err());
}

#[tokio::test]
async fn without_the_gate_command_no_fork_is_attempted() {
    let rec = rec2();
    let mut h = Harness::new();
    h.fake
        .set_session(&rec["threeTurns"]["stateAfterTurns"], Vec::new());
    h.fake.pi.lock().unwrap().commands = Some(rec["threeTurns"]["commands"].clone());
    let result = h
        .session
        .fork_to(&ForkTarget {
            entry_id: "6229f6ef".into(),
            position: "at",
        })
        .await;
    assert!(
        matches!(&result, Err(AdapterError::Other(m)) if m.contains("aas-gate-fork")),
        "{result:?}"
    );
    // pi would take the text for a prompt to the model: nothing was sent.
    nothing_written(&mut h).await;
    let result = h
        .session
        .fork_to(&ForkTarget {
            entry_id: "two words".into(),
            position: "at",
        })
        .await;
    assert!(matches!(result, Err(AdapterError::Other(_))));
}

#[tokio::test]
async fn names_go_to_pi_and_pi_names_come_back_as_titles() {
    let rec = rec2();
    let t = &rec["threeTurns"];
    let mut h = Harness::new();
    let session = h.session.clone();
    let rename = tokio::spawn(async move { session.rename("rec2 initial name").await });
    let cmd = h.fake.expect("set_session_name").await;
    assert_eq!(cmd["name"], "rec2 initial name");
    // Recorded: pi reports the name before it answers.
    h.fake.write(&t["nameEvent"]).await;
    reply(&h, &cmd, &t["nameResponse"]).await;
    assert_eq!(rename.await.unwrap(), Ok(()));
    assert_eq!(
        h.next().await,
        AdapterEvent::SessionTitle {
            title: "rec2 initial name".into()
        }
    );
    // pi refuses a blank name with its own words.
    let session = h.session.clone();
    let rename = tokio::spawn(async move { session.rename("   ").await });
    let cmd = h.fake.expect("set_session_name").await;
    reply(&h, &cmd, &t["blankNameResponse"]).await;
    assert_eq!(
        rename.await.unwrap(),
        Err(AdapterError::Harness("Session name cannot be empty".into()))
    );
    // An extension names the session (`pi.setSessionName`): the same event.
    h.fake.write(&t["extensionNameEvent"]).await;
    assert_eq!(
        h.next().await,
        AdapterEvent::SessionTitle {
            title: "ext renamed".into()
        }
    );
}

#[tokio::test]
async fn the_status_is_pi_session_screen() {
    let rec = rec2();
    let h = Harness::new();
    h.fake
        .set_session(&rec["threeTurns"]["stateAfterTurns"], Vec::new());
    let sections = h.session.status().await.unwrap();
    let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Session Info", "State", "Messages", "Tokens", "Cost"]
    );
    assert_eq!(sections[0].rows[0].label, "Name");
    assert_eq!(sections[0].rows[0].value, "rec2 named after turns");
    assert!(
        sections[1]
            .rows
            .iter()
            .any(|r| r.label == "Model" && r.value == "orcarouter/deepseek/deepseek-v4.1-flash")
    );
}

#[tokio::test]
async fn an_extension_editor_text_goes_to_the_composer() {
    let rec = rec2();
    let mut h = Harness::new();
    for request in array(&rec["editorText"]) {
        h.fake.write(&request).await;
    }
    assert_eq!(
        h.next().await,
        AdapterEvent::ComposerText {
            text: "Hello from the extension".into()
        }
    );
    assert_eq!(
        h.next().await,
        AdapterEvent::ComposerText {
            text: "pasted text".into()
        }
    );
}

#[tokio::test]
async fn an_extension_command_steered_while_pi_streams_goes_through_prompt() {
    let rec = rec2();
    let mut h = Harness::new();
    h.load_commands(rec["reload"]["commandsBefore"].clone())
        .await;
    running_turn(
        &mut h,
        "Count from 1 to 300, one number per line, nothing else.",
    )
    .await;
    let session = h.session.clone();
    let steer = tokio::spawn(async move {
        session
            .steer_message(
                "itm_1",
                TurnInput::text("/rec-ping c1-streamingBehavior-steer"),
            )
            .await
    });
    let cmd = h.fake.expect("prompt").await;
    assert_eq!(cmd["message"], "/rec-ping c1-streamingBehavior-steer");
    assert_eq!(cmd["streamingBehavior"], "steer");
    // Recorded: pi ran the command at once and answered when its handler returned.
    h.fake.write(&rec["streamingCommand"]["notify"]).await;
    reply(&h, &cmd, &rec["streamingCommand"]["response"]).await;
    assert_eq!(steer.await.unwrap(), Ok(()));
    // Other text is a steer (pi refuses extension commands there: `steerRefused`).
    let session = h.session.clone();
    let steer = tokio::spawn(async move {
        session
            .steer_message("itm_2", TurnInput::text("also say DONE"))
            .await
    });
    let cmd = h.fake.expect("steer").await;
    assert_eq!(cmd["message"], "also say DONE");
    h.fake.respond(&cmd, None).await;
    assert_eq!(steer.await.unwrap(), Ok(()));
    // The turn ran an extension command: its end asks for pi's commands again (unchanged here,
    // so nothing is reported).
    run_ends(&mut h).await;
    let events = h.until(completed).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AdapterEvent::CommandsChanged { .. })),
        "{events:?}"
    );
    assert_eq!(h.fake.answered("get_commands").len(), 2);
}

#[tokio::test]
async fn a_reload_reports_pi_new_commands_before_the_turn_completes() {
    let rec = rec2();
    let reload = &rec["reload"];
    let mut h = Harness::new();
    h.load_commands(reload["commandsBefore"].clone()).await;
    let prompt = h.start_turn("/rec-reload").await;
    assert_eq!(h.sent().await, Ok(()));
    for event in array(&reload["events"]) {
        h.fake.write(&event).await;
    }
    // Recorded: the extension files were re-read (`rec-v1` became `rec-v2`).
    h.fake.pi.lock().unwrap().commands = Some(reload["commandsAfter"].clone());
    reply(&h, &json!({"id": prompt}), &reload["response"]).await;
    let events = h.until(completed).await;
    let commands = events
        .iter()
        .find_map(|e| match e {
            AdapterEvent::CommandsChanged { commands } => Some(commands.clone()),
            _ => None,
        })
        .expect("the new commands");
    let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"rec-v2") && !names.contains(&"rec-v1"),
        "{names:?}"
    );
    assert!(!names.contains(&"llama"), "{names:?}");
    // The reload's notifications stay visible.
    assert_eq!(notices(&events, "extensionNotify").len(), 3);
}

#[tokio::test]
async fn a_new_session_an_extension_starts_is_reported_and_the_thread_settings_applied_again() {
    let rec = rec2();
    let new = &rec["newSession"];
    let mut h = Harness::new();
    h.fake.set_session(&new["stateBefore"], Vec::new());
    h.fake.pi.lock().unwrap().commands = Some(rec["reload"]["commandsAfter"].clone());
    let settings = ThreadSettings {
        model: Some("orcarouter/deepseek/deepseek-v4.1-flash".into()),
        effort: Some("low".into()),
        permission_mode: None,
    };
    // The thread's settings are in effect already: the handshake changes nothing.
    handshake(
        &h.session,
        "01a0e941-3ffa-714d-93b4-b647fa566116",
        &settings,
        "ask",
    )
    .await
    .unwrap();
    h.until(|e| matches!(e, AdapterEvent::CommandsChanged { .. }))
        .await;
    let prompt = h.start_turn("/rec-new").await;
    for event in array(&new["events"]) {
        h.fake.write(&event).await;
    }
    // Recorded: a new session with pi's defaults (another model, thinking `medium`).
    h.fake.set_session(&new["stateAfter"], Vec::new());
    reply(&h, &json!({"id": prompt}), &new["response"]).await;
    // The thread's model and thinking level are applied again.
    let cmd = h.fake.expect("set_model").await;
    assert_eq!(
        (&cmd["provider"], &cmd["modelId"]),
        (&json!("orcarouter"), &json!("deepseek/deepseek-v4.1-flash"))
    );
    h.fake.respond(&cmd, None).await;
    let cmd = h.fake.expect("set_thinking_level").await;
    assert_eq!(cmd["level"], "low");
    h.fake.respond(&cmd, None).await;
    let events = h.until(completed).await;
    let switched = events
        .iter()
        .position(|e| {
            *e == AdapterEvent::SessionIdentified {
                native_session_id: "01a0e941-5cbd-714d-93b4-b649075cd762".into(),
            }
        })
        .expect("the switch is reported");
    let settings_again = events
        .iter()
        .position(|e| {
            matches!(e, AdapterEvent::SessionInfo { model: Some(m), effort: Some(l), .. }
                if m == "orcarouter/deepseek/deepseek-v4.1-flash" && l == "low")
        })
        .expect("the settings are reported");
    assert!(switched < settings_again);
    // The turn belongs to neither session as a whole: no anchor.
    assert_eq!(anchor_of(&events), None);
    assert_eq!(
        h.session.native_session_id().as_deref(),
        Some("01a0e941-5cbd-714d-93b4-b649075cd762")
    );
    // The next turn is anchored in the new session (its leaf learnt at the switch).
    let entries = [
        json!({"type":"message","id":"93345645","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"Reply with exactly: NEW-SESSION"}]}}),
        json!({"type":"message","id":"d15e731c","parentId":"93345645","message":{"role":"assistant","content":[]}}),
    ];
    let events = turn_adding(&mut h, "Reply with exactly: NEW-SESSION", &entries).await;
    assert_eq!(
        anchor_of(&events),
        Some(json!({"leafId": "d15e731c", "userEntryId": "93345645"}))
    );
}

#[tokio::test]
async fn settings_pi_refuses_after_a_switch_are_reported() {
    let mut h = Harness::new();
    let model = |id: &str| json!({"id": id, "provider": "p", "reasoning": true});
    h.fake.set_session(
        &json!({"sessionId": "s1", "model": model("m"), "thinkingLevel": "low"}),
        Vec::new(),
    );
    h.fake.pi.lock().unwrap().commands =
        Some(json!([{"name": "switch", "description": "d", "source": "extension"}]));
    let settings = ThreadSettings {
        model: Some("p/m".into()),
        effort: Some("low".into()),
        permission_mode: None,
    };
    handshake(&h.session, "s1", &settings, "ask").await.unwrap();
    h.until(|e| matches!(e, AdapterEvent::CommandsChanged { .. }))
        .await;
    // An extension command whose name says nothing moves pi to another session.
    let prompt = h.start_turn("/switch").await;
    h.fake.set_session(
        &json!({"sessionId": "s2", "model": model("d"), "thinkingLevel": "medium"}),
        Vec::new(),
    );
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    let cmd = h.fake.expect("set_model").await;
    h.fake
        .write(&json!({"type":"response","command":"set_model","success":false,"id":cmd["id"],"error":"Model not found: p/m"}))
        .await;
    let events = h.until(completed).await;
    assert!(events.contains(&AdapterEvent::SessionIdentified {
        native_session_id: "s2".into()
    }));
    assert_eq!(
        notices(&events, "settingsNotApplied"),
        [
            "pi now runs another session, and the thread's model and thinking level could not be applied to it: Model not found: p/m"
        ]
    );
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn steers_that_come_after_the_run_are_returned_before_the_turn_completes() {
    let mut h = Harness::build(true, false);
    running_turn(&mut h, "hello").await;
    h.fake
        .write(&json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Hi"}],
            "stopReason":"stop","usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0}}}))
        .await;
    let stats = h.fake.expect("get_session_stats").await;
    run_ends(&mut h).await;
    // A notification after the run's end: once it arrives, the end has been read.
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"n1","method":"notify","message":"settled","notifyType":"info"}))
        .await;
    h.until(|e| matches!(e, AdapterEvent::Notice { message, .. } if message == "settled"))
        .await;
    // The run is over; the turn waits for its context. A steer now would reach an idle pi.
    h.session
        .steer_message("itm_late", TurnInput::text("one more thing"))
        .await
        .unwrap();
    assert!(matches!(
        h.session.steer(TurnInput::text("and another")).await,
        Err(AdapterError::Other(_))
    ));
    nothing_written(&mut h).await;
    h.fake.respond(&stats, Some(session_stats(2000))).await;
    let events = h.until(completed).await;
    let returned = events
        .iter()
        .position(|e| {
            *e == AdapterEvent::SteerReturned {
                message_id: "itm_late".into(),
            }
        })
        .expect("the steer is returned");
    assert!(returned < events.len() - 1, "{events:?}");
    // The session is idle: the returned input can start the next turn.
    h.start_turn("one more thing").await;
}

const BUSY: &str = "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.";

#[tokio::test]
async fn a_run_pi_starts_by_itself_is_a_turn_without_input() {
    let mut h = Harness::new();
    h.load_commands(live_commands()).await;
    // The extension command's own turn: pi runs the command, answers, and starts no run then.
    // `send` does not wait for pi's answer to an extension command.
    let prompt = h.start_turn("/aas-later 1500").await;
    assert_eq!(h.sent().await, Ok(()));
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    let events = h.until(completed).await;
    assert_eq!(events[0], AdapterEvent::TurnStarted);
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: None,
            ..
        }
    ));

    // 1.5 s later the extension starts a run by itself (recorded).
    h.fake.write(&json!({"type":"agent_start"})).await;
    assert_eq!(h.next().await, AdapterEvent::TurnStarted);
    // While that run goes on, the user's input is not written to pi (pi would refuse it): the
    // engine sends it again once the run's turn has completed.
    assert_eq!(
        h.session.send(TurnInput::text("hello")).await,
        Err(AdapterError::TurnInProgress)
    );
    h.fake.play_from("agent_custom.jsonl", "", 1).await;
    let events = h.until(completed).await;
    assert_eq!(
        notices(&events, "extensionMessage"),
        ["Reply with exactly: WOKE"],
        "the extension's message opens the turn, once"
    );
    assert!(final_text(&events, "WOKE"), "{events:?}");
    assert!(!events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemStarted {
            body: ItemBody::UserMessage { .. },
            ..
        }
    )));
    match events.last().unwrap() {
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Some(usage),
            error: None,
            trigger: None,
        } => {
            assert_eq!((usage.input_tokens, usage.output_tokens), (1613, 4));
            assert!(usage.context.is_some(), "get_session_stats was asked");
        }
        other => panic!("{other:?}"),
    }
    nothing_written(&mut h).await;

    // Idle again: the input goes to pi.
    let prompt = h.start_turn("hello").await;
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    assert_eq!(h.sent().await, Ok(()));
    assert_eq!(h.next().await, AdapterEvent::TurnStarted);
}

#[tokio::test]
async fn a_user_message_an_extension_sends_opens_its_run_as_a_notice() {
    let mut h = Harness::new();
    h.fake.play("agent_user.jsonl", "").await;
    let events = h.until(completed).await;
    assert_eq!(events[0], AdapterEvent::TurnStarted);
    assert_eq!(
        notices(&events, "extensionPrompt"),
        ["Reply with exactly: WOKE-USER"]
    );
    assert!(final_text(&events, "WOKE-USER"));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn a_prompt_refused_because_of_a_run_of_pi_own_waits_for_that_run() {
    // Recorded: the extension started a run while pi checked the prompt; pi wrote that run's
    // `agent_start`, then refused the prompt (`success: false`).
    let mut h = Harness::new();
    let prompt = h.start_turn("aas-race Reply with exactly: MINE").await;
    h.fake.play("agent_race.jsonl", &prompt).await;
    assert_eq!(h.sent().await, Err(AdapterError::TurnInProgress));
    let events = h.until(completed).await;
    // The run is a turn of its own; the refusal fails nothing.
    assert_eq!(events[0], AdapterEvent::TurnStarted);
    assert_eq!(count(&events, completed), 1);
    assert_eq!(
        notices(&events, "extensionMessage"),
        ["Reply with exactly: RACED"]
    );
    assert!(final_text(&events, "RACED"));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            ..
        }
    ));
    // The engine sends the input again after that turn: pi takes it now.
    let prompt = h.start_turn("aas-race Reply with exactly: MINE").await;
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    assert_eq!(h.sent().await, Ok(()));
    assert_eq!(h.next().await, AdapterEvent::TurnStarted);
}

#[tokio::test]
async fn a_run_of_pi_own_that_ended_before_the_refusal_is_a_turn_of_its_own() {
    let mut h = Harness::new();
    let prompt = h.start_turn("hello").await;
    h.fake.write(&json!({"type":"agent_start"})).await;
    assistant(&mut h, "OWN").await;
    run_ends(&mut h).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !h.sending.as_ref().unwrap().is_finished(),
        "send waits for pi's answer"
    );
    h.fake
        .write(
            &json!({"type":"response","command":"prompt","success":false,"id":prompt,"error":BUSY}),
        )
        .await;
    assert_eq!(h.sent().await, Err(AdapterError::TurnInProgress));
    let events = h.until(completed).await;
    assert_eq!(events[0], AdapterEvent::TurnStarted);
    assert!(final_text(&events, "OWN"));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
    h.start_turn("hello").await;
}

#[tokio::test]
async fn a_prompt_pi_takes_after_a_run_of_its_own_joins_that_run_in_one_turn() {
    // pi took the prompt: what ran before its answer was part of what pi did then.
    let mut h = Harness::new();
    let prompt = h.start_turn("hello").await;
    h.fake.write(&json!({"type":"agent_start"})).await;
    assistant(&mut h, "OWN").await;
    run_ends(&mut h).await;
    // pi streams from its answer on (the adapter's run probe sees it).
    h.fake.streaming.store(true, Ordering::SeqCst);
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    assert_eq!(h.sent().await, Ok(()));
    h.fake.write(&json!({"type":"agent_start"})).await;
    assistant(&mut h, "MINE").await;
    run_ends(&mut h).await;
    let events = h.until(completed).await;
    assert_eq!(count(&events, |e| *e == AdapterEvent::TurnStarted), 1);
    assert!(final_text(&events, "OWN") && final_text(&events, "MINE"));
    match events.last().unwrap() {
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Some(usage),
            ..
        } => assert_eq!(usage.output_tokens, 2, "both runs count"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_run_started_at_the_end_of_a_run_gets_a_turn_of_its_own() {
    // Recorded: an extension starts a run from its `agent_settled` handler; pi writes the new
    // run's `agent_start` before the old run's `agent_settled`.
    let mut h = Harness::new();
    let prompt = h.start_turn("Reply with exactly: FIRST").await;
    h.fake.play("agent_chain.jsonl", &prompt).await;
    assert_eq!(h.sent().await, Ok(()));
    let first = h.until(completed).await;
    assert_eq!(first[0], AdapterEvent::TurnStarted);
    assert!(final_text(&first, "FIRST"));
    assert!(notices(&first, "extensionMessage").is_empty());
    let second = h.until(completed).await;
    assert_eq!(second[0], AdapterEvent::TurnStarted);
    assert_eq!(
        notices(&second, "extensionMessage"),
        ["Reply with exactly: CHAINED"]
    );
    assert!(final_text(&second, "CHAINED"));
    assert!(matches!(
        second.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
    h.start_turn("next").await;
}

#[tokio::test]
async fn a_run_that_starts_while_the_turn_waits_for_its_context_is_a_new_turn() {
    let mut h = Harness::build(true, false);
    running_turn(&mut h, "hello").await;
    assistant(&mut h, "Hi").await;
    let stats = h.fake.expect("get_session_stats").await;
    run_ends(&mut h).await;
    // The turn waits for its context; a new run starts meanwhile.
    h.fake.write(&json!({"type":"agent_start"})).await;
    let events = h.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    assert!(completed(&events[events.len() - 2]), "{events:?}");
    // The late answer belongs to the old turn: it is not reported for the new one.
    h.fake.respond(&stats, Some(session_stats(4200))).await;
    assistant(&mut h, "AGAIN").await;
    let stats = h.fake.expect("get_session_stats").await;
    h.fake.respond(&stats, Some(session_stats(5000))).await;
    run_ends(&mut h).await;
    let events = h.until(completed).await;
    assert!(final_text(&events, "AGAIN"));
    let contexts: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::TurnUsage { usage } => usage.context.map(|c| c.used_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(contexts, [5000]);
}

#[tokio::test]
async fn interrupting_a_run_of_pi_own_aborts_it() {
    // Recorded: an extension-started run, aborted while the model wrote.
    let mut h = Harness::new();
    let fixture = h.fake.play("agent_abort.jsonl", "");
    let session = h.session.clone();
    let mut events = h.events;
    let driver = tokio::spawn(async move {
        let mut seen = Vec::new();
        let mut interrupted = false;
        loop {
            let ev = tokio::time::timeout(WAIT, events.recv())
                .await
                .unwrap()
                .unwrap();
            if !interrupted
                && matches!(
                    &ev,
                    AdapterEvent::ItemStarted {
                        body: ItemBody::AgentMessage { .. },
                        ..
                    }
                )
            {
                interrupted = true;
                session.interrupt().await.unwrap();
            }
            let done = completed(&ev);
            seen.push(ev);
            if done {
                return seen;
            }
        }
    });
    let awaited = fixture.await;
    assert_eq!(awaited[0]["type"], "abort");
    let events = driver.await.unwrap();
    assert_eq!(events[0], AdapterEvent::TurnStarted);
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        }
    ));
}

#[tokio::test]
async fn the_gate_asks_inside_a_run_of_pi_own() {
    // Recorded with the gate in `ask` mode.
    let mut h = Harness::new();
    let fixture = h.fake.play("agent_gate.jsonl", "");
    let session = h.session.clone();
    let mut events = h.events;
    let driver = tokio::spawn(async move {
        let mut seen = Vec::new();
        loop {
            let ev = tokio::time::timeout(WAIT, events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AdapterEvent::InteractionRequested {
                request_id,
                request,
                background_key,
                ..
            } = &ev
            {
                assert!(matches!(
                    request,
                    InteractionRequest::Approval { subject: Subject::Command { command, .. }, .. }
                        if command == "echo agent-aas"
                ));
                assert_eq!(*background_key, None);
                session
                    .respond(
                        request_id,
                        &InteractionResolution::Approval {
                            option_id: "allow".into(),
                            feedback: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            let done = completed(&ev);
            seen.push(ev);
            if done {
                return seen;
            }
        }
    });
    let awaited = fixture.await;
    assert_eq!(awaited[0]["value"], "{\"choice\":\"allow\"}");
    let events = driver.await.unwrap();
    assert_eq!(events[0], AdapterEvent::TurnStarted);
    // The approval came inside the turn, before its end.
    let asked = events
        .iter()
        .position(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        .unwrap();
    assert!(asked < events.len() - 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AdapterEvent::ItemCompleted {
            body: Some(ItemBody::CommandExecution { output, .. }),
            status: ItemStatus::Completed,
            ..
        } if output == "agent-aas\n"
    )));
    assert!(final_text(&events, "DONE"));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn a_dialog_outside_a_turn_is_relayed_and_answered() {
    // Recorded: an extension asks from a timer while no run goes on; the answer reaches pi.
    let mut h = Harness::new();
    let fixture = h.fake.play("dialog_outside_turn.jsonl", "");
    let session = h.session.clone();
    let mut events = h.events;
    let driver = tokio::spawn(async move {
        let first = tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        let AdapterEvent::InteractionRequested {
            request_id,
            request,
            item_key,
            background_key,
        } = &first
        else {
            panic!("{first:?}")
        };
        assert!(
            matches!(request, InteractionRequest::Approval { title, .. } if title == "aas-live")
        );
        assert_eq!((item_key, background_key), (&None, &None));
        session
            .respond(request_id, &InteractionResolution::Dismissed)
            .await
            .unwrap();
        tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap()
    });
    let awaited = fixture.await;
    assert_eq!(
        awaited[0],
        json!({"type":"extension_ui_response","id":"4fb6e6cc-187a-4918-9e27-1bf526c3d9ff","cancelled":true})
    );
    // The extension got pi's answer (`confirm` resolves to false when dismissed).
    assert_eq!(
        driver.await.unwrap(),
        AdapterEvent::Notice {
            level: NoticeLevel::Info,
            message: "aas-live answered: false".into(),
            code: Some("extensionNotify".into()),
        }
    );
}

#[tokio::test]
async fn a_dialog_asked_outside_a_turn_outlives_the_next_turn() {
    let mut h = Harness::new();
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"d1","method":"confirm","title":"aas-live","message":"A dialog outside a turn"}))
        .await;
    assert!(matches!(
        h.next().await,
        AdapterEvent::InteractionRequested { request_id, .. } if request_id == "d1"
    ));
    running_turn(&mut h, "hello").await;
    run_ends(&mut h).await;
    let events = h.until(completed).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AdapterEvent::InteractionWithdrawn { .. })),
        "{events:?}"
    );
    // pi still waits for the answer.
    h.session
        .respond(
            "d1",
            &InteractionResolution::Approval {
                option_id: "yes".into(),
                feedback: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        h.fake.expect("extension_ui_response").await,
        json!({"type":"extension_ui_response","id":"d1","confirmed":true})
    );
}

#[tokio::test]
async fn send_waits_for_pi_answer_to_a_plain_prompt_until_pi_needs_the_user() {
    let mut h = Harness::new();
    h.start_turn("hello").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!h.sending.as_ref().unwrap().is_finished());

    // A dialog of pi's preflight (an `input` handler asks): the engine must relay it, so
    // `send` returns.
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"q1","method":"input","title":"Why?"}))
        .await;
    assert!(matches!(
        h.next().await,
        AdapterEvent::InteractionRequested { .. }
    ));
    assert_eq!(h.sent().await, Ok(()));
}

#[tokio::test]
async fn a_compaction_before_the_prompt_runs_ends_the_wait() {
    let mut h = Harness::new();
    let prompt = h.start_turn("hello").await;
    h.fake
        .write(&json!({"type":"compaction_start","reason":"threshold"}))
        .await;
    assert_eq!(h.sent().await, Ok(()));
    h.fake
        .write(&json!({"type":"compaction_end","reason":"threshold","result":{"tokensBefore":900000},"aborted":false,"willRetry":false}))
        .await;
    h.fake
        .write(&json!({"type":"response","command":"prompt","success":true,"id":prompt}))
        .await;
    let events = h.until(|e| matches!(e, AdapterEvent::TurnStarted)).await;
    assert_eq!(
        notices(&events, "compaction"),
        [
            "Compacting the conversation (threshold)",
            "Conversation compacted (900000 tokens before)"
        ]
    );
}

#[tokio::test]
async fn pi_ending_before_it_answers_the_prompt_closes_the_send() {
    let mut h = Harness::new();
    h.start_turn("hello").await;
    h.fake.exit(1).await;
    assert_eq!(h.sent().await, Err(AdapterError::Closed));
}

#[tokio::test]
async fn a_refusal_after_the_wait_ended_keeps_the_run_in_the_turn() {
    // `send` stopped waiting (a dialog of the preflight); a run of pi's own started, and pi
    // refused the prompt: the run is shown in the user's turn already and stays there.
    let mut h = Harness::new();
    let prompt = h.start_turn("hello").await;
    h.fake
        .write(&json!({"type":"extension_ui_request","id":"q1","method":"confirm","title":"Go on?","message":""}))
        .await;
    assert!(matches!(
        h.next().await,
        AdapterEvent::InteractionRequested { .. }
    ));
    assert_eq!(h.sent().await, Ok(()));
    h.fake.write(&json!({"type":"agent_start"})).await;
    assert_eq!(h.next().await, AdapterEvent::TurnStarted);
    h.fake
        .write(
            &json!({"type":"response","command":"prompt","success":false,"id":prompt,"error":BUSY}),
        )
        .await;
    assistant(&mut h, "OWN").await;
    run_ends(&mut h).await;
    let events = h.until(completed).await;
    assert_eq!(
        notices(&events, "promptNotTaken"),
        [format!("pi did not take this message: {BUSY}")]
    );
    assert!(final_text(&events, "OWN"));
    assert!(matches!(
        events.last().unwrap(),
        AdapterEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
}
