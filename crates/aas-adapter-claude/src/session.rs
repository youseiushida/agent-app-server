//! One Claude Code process speaking `stream-json` plus the Agent SDK control protocol.
//!
//! The protocol core is independent of process spawning: it runs over any
//! `AsyncRead`/`AsyncWrite` pair plus a [`ProcessLink`] that reports how the process ended,
//! so recorded transcripts can be replayed in tests over in-memory pipes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aas_harness::protocol::{
    Command, DeltaField, InteractionResolution, ItemBody, ItemStatus, NoticeLevel, ThreadSettings,
    TurnError, TurnStatus, Usage,
};
use aas_harness::{
    AdapterError, AdapterEvent, ExitInfo, SessionControl, SettingsApplied, StopReason, TurnInput,
    TurnInputPart,
};
use aas_stdio::{JsonLinesReader, LineError, ReadLine, SharedJsonLinesWriter, WriteError};
use aas_supervisor::ChildHandle;
use async_trait::async_trait;
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OnceCell, mpsc, oneshot, watch};

use crate::mapping::{self, PermissionAsk, TaskList, ToolClass, ToolResult};

/// How the session learns that its process ended.
pub(crate) enum ProcessLink {
    Child(ChildHandle),
    /// Used by tests: the exit is published on a watch channel.
    #[cfg_attr(not(test), allow(dead_code))]
    Manual(watch::Receiver<Option<ExitInfo>>),
}

impl ProcessLink {
    async fn wait(&self) -> ExitInfo {
        match self {
            ProcessLink::Child(h) => h.wait().await,
            ProcessLink::Manual(rx) => {
                let mut rx = rx.clone();
                loop {
                    if let Some(info) = rx.borrow_and_update().clone() {
                        return info;
                    }
                    if rx.changed().await.is_err() {
                        return ExitInfo {
                            code: None,
                            stopped: None,
                            stderr_tail: String::new(),
                            exited_at_ms: 0,
                        };
                    }
                }
            }
        }
    }

    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
        match self {
            ProcessLink::Child(h) => h.shutdown(grace, reason).await,
            ProcessLink::Manual(_) => match tokio::time::timeout(grace, self.wait()).await {
                Ok(info) => info,
                Err(_) => ExitInfo {
                    code: None,
                    stopped: Some(reason),
                    stderr_tail: String::new(),
                    exited_at_ms: 0,
                },
            },
        }
    }

    fn stderr_tail(&self) -> String {
        match self {
            ProcessLink::Child(h) => h.stderr_tail(),
            ProcessLink::Manual(_) => String::new(),
        }
    }
}

/// Commands per working directory, shared between the adapter and its sessions.
pub(crate) type CommandCache = Arc<Mutex<HashMap<PathBuf, Vec<Command>>>>;

/// Static parameters of a session.
pub(crate) struct SessionParams {
    pub label: String,
    pub native_session_id: String,
    pub cwd: PathBuf,
    pub settings: ThreadSettings,
    pub stop_grace: Duration,
    pub request_timeout: Duration,
    pub max_line_bytes: usize,
    pub command_cache: CommandCache,
}

/// What the `initialize` control request returned.
#[derive(Debug, Clone, Default)]
pub(crate) struct InitializeInfo {
    pub raw: Value,
}

/// Control requests we sent, waiting for their response body (or error message).
type PendingControl = HashMap<String, oneshot::Sender<Result<Value, String>>>;

pub(crate) struct ClaudeSession {
    inner: Arc<Inner>,
}

struct Inner {
    label: String,
    cwd: PathBuf,
    writer: SharedJsonLinesWriter,
    state: Mutex<State>,
    /// Taken (closing the channel) right after `Exited`.
    events: Mutex<Option<mpsc::UnboundedSender<AdapterEvent>>>,
    pending: Mutex<Option<PendingControl>>,
    next_request: AtomicU64,
    link: ProcessLink,
    stop_grace: Duration,
    request_timeout: Duration,
    shutdown: OnceCell<ExitInfo>,
    command_cache: CommandCache,
}

#[derive(Default)]
struct State {
    native_session_id: String,
    settings: ThreadSettings,
    turn: Option<Turn>,
    turn_seq: u64,
    /// `total_cost_usd` of the previous `result` (cumulative per process).
    last_total_cost: f64,
    tasks: TaskList,
    /// Commands with descriptions from `initialize`.
    commands: Vec<Command>,
    /// Names last reported through `CommandsChanged`.
    reported_command_names: Vec<String>,
    reported_model: Option<String>,
    reported_permission_mode: Option<String>,
    asks: HashMap<String, PermissionAsk>,
    denied_tool_ids: HashSet<String>,
    rate_limit_notices: HashSet<(String, String)>,
    /// A finished turn waiting for its context-window occupancy.
    completion: Option<PendingCompletion>,
}

/// A turn whose `result` arrived. Claude Code reports the context-window occupancy only when
/// asked (control request `get_context_usage`), so the adapter asks right after `result` and
/// emits `TurnCompleted` with the answer. The answer is bounded by the request timeout; a new
/// CLI-initiated turn or the end of the output publishes the completion without it.
struct PendingCompletion {
    request_id: String,
    status: TurnStatus,
    usage: Option<Usage>,
    error: Option<TurnError>,
    deadline: tokio::time::Instant,
}

#[derive(Default)]
struct Turn {
    acked: bool,
    interrupt_requested: bool,
    current_message: Option<String>,
    current_block: Option<(String, u64)>,
    blocks: HashMap<(String, u64), Block>,
    tools: HashMap<String, ToolItem>,
    plan_item: Option<String>,
    standalone: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

struct Block {
    kind: BlockKind,
    key: String,
    started: bool,
    completed: bool,
    accumulated: String,
    final_text: Option<String>,
}

struct ToolItem {
    /// `None` for plan tools, which have no item of their own.
    key: Option<String>,
    name: String,
    input: Value,
    started: Option<ItemBody>,
    done: bool,
}

fn text_body(kind: BlockKind, text: String) -> ItemBody {
    match kind {
        BlockKind::Text => ItemBody::AgentMessage { text },
        BlockKind::Thinking => ItemBody::Reasoning { text },
    }
}

impl ClaudeSession {
    /// Starts the reader over the process pipes. Returns the session and its event stream.
    pub(crate) fn start<R, W>(
        reader: R,
        writer: W,
        link: ProcessLink,
        params: SessionParams,
    ) -> (Arc<ClaudeSession>, mpsc::UnboundedReceiver<AdapterEvent>)
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            label: params.label,
            cwd: params.cwd,
            writer: SharedJsonLinesWriter::new(writer),
            state: Mutex::new(State {
                native_session_id: params.native_session_id,
                settings: params.settings,
                ..State::default()
            }),
            events: Mutex::new(Some(tx)),
            pending: Mutex::new(Some(HashMap::new())),
            next_request: AtomicU64::new(1),
            link,
            stop_grace: params.stop_grace,
            request_timeout: params.request_timeout,
            shutdown: OnceCell::new(),
            command_cache: params.command_cache,
        });
        let reader_inner = inner.clone();
        let max_line_bytes = params.max_line_bytes;
        tokio::spawn(async move {
            reader_inner
                .read_loop(JsonLinesReader::new(reader, max_line_bytes))
                .await;
        });
        (Arc::new(ClaudeSession { inner }), rx)
    }

    /// Sends the `initialize` control request and records models/commands/mode.
    pub(crate) async fn initialize(&self) -> Result<InitializeInfo, AdapterError> {
        let raw = self
            .inner
            .control(json!({ "subtype": "initialize", "hooks": null }))
            .await?;
        let commands = mapping::commands_from_initialize(&raw);
        let mode = raw
            .get("current_permission_mode")
            .and_then(Value::as_str)
            .map(str::to_owned);
        {
            let mut st = self.inner.state.lock();
            st.commands = commands.clone();
            st.reported_command_names = commands.iter().map(|c| c.name.clone()).collect();
            if st.settings.permission_mode.is_none() {
                st.settings.permission_mode = mode.clone();
            }
            st.reported_permission_mode = mode.clone();
        }
        self.inner
            .command_cache
            .lock()
            .insert(self.inner.cwd.clone(), commands.clone());
        self.inner.emit(AdapterEvent::CommandsChanged { commands });
        if mode.is_some() {
            self.inner.emit(AdapterEvent::SessionInfo {
                model: None,
                permission_mode: mode,
                effort: None,
            });
        }
        Ok(InitializeInfo { raw })
    }

    pub(crate) fn stderr_tail(&self) -> String {
        self.inner.link.stderr_tail()
    }
}

impl Inner {
    fn emit(&self, event: AdapterEvent) {
        if let Some(tx) = self.events.lock().as_ref() {
            let _ = tx.send(event);
        }
    }

    async fn write(&self, value: &Value) -> Result<(), AdapterError> {
        self.writer.send(value).await.map_err(|e| match e {
            WriteError::Closed => AdapterError::Closed,
            WriteError::Io(e) => AdapterError::Other(format!("writing to claude failed: {e}")),
        })
    }

    /// Writes a message within `request_timeout` (a CLI that stopped reading its stdin
    /// blocks the write on the full pipe).
    async fn write_bounded(&self, value: &Value) -> Result<(), AdapterError> {
        match tokio::time::timeout(self.request_timeout, self.write(value)).await {
            Ok(written) => written,
            Err(_) => Err(AdapterError::Protocol(format!(
                "claude did not read its input within {:?}",
                self.request_timeout
            ))),
        }
    }

    /// Sends a control request and waits for its response body, both within
    /// `request_timeout`.
    async fn control(&self, request: Value) -> Result<Value, AdapterError> {
        self.control_within(request, self.request_timeout).await
    }

    /// [`control`](Self::control) within `limit`, the write included (a CLI that stopped
    /// reading its stdin blocks it on the full pipe).
    async fn control_within(&self, request: Value, limit: Duration) -> Result<Value, AdapterError> {
        let id = format!("aas_{}", self.next_request.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            let map = pending.as_mut().ok_or(AdapterError::Closed)?;
            map.insert(id.clone(), tx);
        }
        let subtype = request
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let msg = json!({ "type": "control_request", "request_id": id, "request": request });
        let exchange = async {
            self.write(&msg).await?;
            match rx.await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(message)) => Err(AdapterError::Harness(format!("{subtype}: {message}"))),
                Err(_) => Err(AdapterError::Closed),
            }
        };
        let result = match tokio::time::timeout(limit, exchange).await {
            Ok(result) => result,
            Err(_) => Err(AdapterError::Protocol(format!(
                "no response to control request {subtype} within {limit:?}"
            ))),
        };
        if result.is_err()
            && let Some(map) = self.pending.lock().as_mut()
        {
            map.remove(&id);
        }
        result
    }

    async fn read_loop<R: AsyncRead + Unpin>(self: Arc<Self>, mut reader: JsonLinesReader<R>) {
        loop {
            let deadline = self.state.lock().completion.as_ref().map(|c| c.deadline);
            let line = tokio::select! {
                line = reader.next() => line,
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                    tracing::warn!(label = %self.label, timeout = ?self.request_timeout, "no answer to get_context_usage; the turn completes without it");
                    self.flush_completion();
                    continue;
                }
            };
            match line {
                Ok(Some(ReadLine::Json(value))) => self.handle(value).await,
                Ok(Some(ReadLine::NotJson(line))) => {
                    self.emit(AdapterEvent::Native {
                        payload: json!({ "nonJsonLine": line }),
                    });
                }
                Err(LineError::TooLong { max }) => {
                    tracing::warn!(label = %self.label, max, "claude wrote an oversized line; skipped");
                    self.emit(AdapterEvent::Notice {
                        level: NoticeLevel::Warning,
                        message: format!("Skipped a Claude Code message larger than {max} bytes"),
                        code: Some("lineTooLong".into()),
                    });
                }
                Ok(None) => break,
                Err(LineError::Io(e)) => {
                    tracing::debug!(label = %self.label, error = %e, "claude stdout failed");
                    break;
                }
            }
        }
        // Output ended: publish a completion still waiting for its context, fail outstanding
        // control requests, then report the exit.
        self.flush_completion();
        drop(self.pending.lock().take());
        let info = self.link.wait().await;
        self.emit(AdapterEvent::Exited { info });
        // Contract: the channel closes right after `Exited`.
        self.events.lock().take();
    }

    async fn handle(&self, msg: Value) {
        let kind = msg
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match kind.as_str() {
            "control_response" => self.on_control_response(&msg),
            "control_request" => self.on_control_request(msg).await,
            "control_cancel_request" => {
                if let Some(id) = msg.get("request_id").and_then(Value::as_str) {
                    let removed = self.state.lock().asks.remove(id);
                    if removed.is_some() {
                        self.emit(AdapterEvent::InteractionWithdrawn {
                            request_id: id.to_owned(),
                        });
                    }
                }
            }
            "system" => self.on_system(&msg),
            "stream_event" => {
                if is_main_thread(&msg) {
                    self.on_stream_event(&msg);
                }
            }
            "assistant" => {
                if is_main_thread(&msg) {
                    self.on_assistant(&msg);
                }
            }
            "user" => {
                if is_main_thread(&msg) {
                    self.on_user(&msg);
                }
            }
            "result" => self.on_result(&msg).await,
            "rate_limit_event" => self.on_rate_limit(&msg),
            "keep_alive" => {}
            _ => self.emit(AdapterEvent::Native { payload: msg }),
        }
    }

    fn on_control_response(&self, msg: &Value) {
        let Some(response) = msg.get("response") else {
            return;
        };
        let Some(id) = response.get("request_id").and_then(Value::as_str) else {
            return;
        };
        let completion = {
            let mut st = self.state.lock();
            match st.completion.as_ref() {
                Some(c) if c.request_id == id => st.completion.take(),
                _ => None,
            }
        };
        if let Some(mut completion) = completion {
            if response.get("subtype").and_then(Value::as_str) == Some("error") {
                let error = response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                tracing::warn!(label = %self.label, error, "get_context_usage failed; the turn completes without it");
            } else if let Some(context) = response
                .get("response")
                .and_then(mapping::context_from_usage_response)
            {
                completion.usage.get_or_insert_with(Usage::default).context = Some(context);
            }
            self.emit(AdapterEvent::TurnCompleted {
                status: completion.status,
                usage: completion.usage,
                error: completion.error,
            });
            return;
        }
        let sender = self.pending.lock().as_mut().and_then(|m| m.remove(id));
        let Some(sender) = sender else {
            tracing::debug!(label = %self.label, id, "control response for an unknown request");
            return;
        };
        let outcome = if response.get("subtype").and_then(Value::as_str) == Some("error") {
            Err(response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned())
        } else {
            Ok(response
                .get("response")
                .cloned()
                .unwrap_or_else(|| json!({})))
        };
        let _ = sender.send(outcome);
    }

    async fn on_control_request(&self, msg: Value) {
        let Some(id) = msg
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let request = msg.get("request").cloned().unwrap_or(Value::Null);
        let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or("");
        if subtype != "can_use_tool" {
            // No hooks and no SDK MCP servers are registered, so nothing else is expected.
            // Answer with an error so the CLI never waits forever, and surface the request.
            let reply = json!({
                "type": "control_response",
                "response": { "subtype": "error", "request_id": id, "error": format!("unsupported control request: {subtype}") }
            });
            let _ = self.write(&reply).await;
            self.emit(AdapterEvent::Native { payload: msg });
            return;
        }
        let ask = PermissionAsk::from_request(&request);
        let interaction = mapping::permission_interaction(&request, &ask);
        let item_key = {
            let mut st = self.state.lock();
            let key = ask
                .tool_use_id
                .as_ref()
                .and_then(|t| st.turn.as_ref().and_then(|turn| turn.tools.get(t)))
                .and_then(|tool| tool.key.clone());
            st.asks.insert(id.clone(), ask);
            key
        };
        self.emit(AdapterEvent::InteractionRequested {
            request_id: id,
            request: interaction,
            item_key,
        });
    }

    fn on_system(&self, msg: &Value) {
        let subtype = msg.get("subtype").and_then(Value::as_str).unwrap_or("");
        match subtype {
            "init" => {
                let mut st = self.state.lock();
                self.ack_turn(&mut st);
                if let Some(id) = msg.get("session_id").and_then(Value::as_str)
                    && !id.is_empty()
                    && id != st.native_session_id
                {
                    st.native_session_id = id.to_owned();
                    self.emit(AdapterEvent::SessionIdentified {
                        native_session_id: id.to_owned(),
                    });
                }
                let model = msg.get("model").and_then(Value::as_str).map(str::to_owned);
                let mode = msg
                    .get("permissionMode")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if model != st.reported_model || mode != st.reported_permission_mode {
                    st.reported_model = model.clone();
                    st.reported_permission_mode = mode.clone();
                    self.emit(AdapterEvent::SessionInfo {
                        model,
                        permission_mode: mode,
                        effort: None,
                    });
                }
                if let Some(names) = msg.get("slash_commands").and_then(Value::as_array) {
                    let names: Vec<String> = names
                        .iter()
                        .filter_map(|n| n.as_str().map(str::to_owned))
                        .collect();
                    if names != st.reported_command_names {
                        let commands = mapping::commands_from_names(&names, &st.commands);
                        st.reported_command_names = names;
                        self.command_cache
                            .lock()
                            .insert(self.cwd.clone(), commands.clone());
                        self.emit(AdapterEvent::CommandsChanged { commands });
                    }
                }
            }
            // Progress signals with no user-visible state: the result marks the turn end.
            "status" | "thinking_tokens" | "session_state_changed" => {}
            "compact_boundary" => self.emit(AdapterEvent::Notice {
                level: NoticeLevel::Info,
                message: "Conversation compacted".into(),
                code: Some("compacted".into()),
            }),
            _ => self.emit(AdapterEvent::Native {
                payload: msg.clone(),
            }),
        }
    }

    /// Marks the running turn as acknowledged, or opens a turn the CLI started by itself
    /// (e.g. a background task waking the session).
    fn ack_turn(&self, st: &mut State) {
        match st.turn.as_mut() {
            Some(turn) if turn.acked => {}
            Some(turn) => {
                turn.acked = true;
                self.emit(AdapterEvent::TurnStarted);
            }
            None => {
                // The previous turn is over before this one starts, context or not.
                if let Some(c) = st.completion.take() {
                    self.emit(AdapterEvent::TurnCompleted {
                        status: c.status,
                        usage: c.usage,
                        error: c.error,
                    });
                }
                st.turn_seq += 1;
                st.turn = Some(Turn {
                    acked: true,
                    ..Turn::default()
                });
                self.emit(AdapterEvent::TurnStarted);
            }
        }
    }

    fn on_stream_event(&self, msg: &Value) {
        let Some(event) = msg.get("event") else {
            return;
        };
        let mut st = self.state.lock();
        self.ack_turn(&mut st);
        let turn_seq = st.turn_seq;
        let turn = st.turn.as_mut().expect("turn exists after ack");
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                turn.current_message = event
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "content_block_start" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let msg_id = turn
                    .current_message
                    .clone()
                    .unwrap_or_else(|| format!("turn{turn_seq}"));
                let block_type = event
                    .pointer("/content_block/type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let kind = match block_type {
                    "text" => Some(BlockKind::Text),
                    "thinking" => Some(BlockKind::Thinking),
                    _ => None,
                };
                turn.current_block = Some((msg_id.clone(), index));
                if let Some(kind) = kind {
                    let prefix = if kind == BlockKind::Text {
                        "txt"
                    } else {
                        "rsn"
                    };
                    let key = format!("{prefix}:{msg_id}:{index}");
                    let started = kind == BlockKind::Text;
                    turn.blocks.insert(
                        (msg_id, index),
                        Block {
                            kind,
                            key: key.clone(),
                            started,
                            completed: false,
                            accumulated: String::new(),
                            final_text: None,
                        },
                    );
                    if started {
                        self.emit(AdapterEvent::ItemStarted {
                            key,
                            body: text_body(kind, String::new()),
                        });
                    }
                }
            }
            "content_block_delta" => {
                let Some(current) = turn.current_block.clone() else {
                    return;
                };
                let Some(block) = turn.blocks.get_mut(&current) else {
                    return;
                };
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                let text = match (block.kind, delta.get("type").and_then(Value::as_str)) {
                    (BlockKind::Text, Some("text_delta")) => {
                        delta.get("text").and_then(Value::as_str)
                    }
                    (BlockKind::Thinking, Some("thinking_delta")) => {
                        delta.get("thinking").and_then(Value::as_str)
                    }
                    _ => None,
                };
                let Some(text) = text.filter(|t| !t.is_empty()) else {
                    return;
                };
                block.accumulated.push_str(text);
                if !block.started {
                    block.started = true;
                    self.emit(AdapterEvent::ItemStarted {
                        key: block.key.clone(),
                        body: text_body(block.kind, String::new()),
                    });
                }
                self.emit(AdapterEvent::ItemDelta {
                    key: block.key.clone(),
                    field: DeltaField::Text,
                    text: text.to_owned(),
                });
            }
            "content_block_stop" => {
                let Some(current) = turn.current_block.take() else {
                    return;
                };
                if let Some(block) = turn.blocks.get_mut(&current) {
                    self.complete_block(block);
                }
            }
            _ => {}
        }
    }

    fn complete_block(&self, block: &mut Block) {
        if block.completed {
            return;
        }
        block.completed = true;
        if !block.started {
            // A thinking block whose text was redacted: nothing to show.
            if let Some(text) = block.final_text.clone().filter(|t| !t.is_empty()) {
                block.started = true;
                self.emit(AdapterEvent::ItemStarted {
                    key: block.key.clone(),
                    body: text_body(block.kind, text),
                });
                self.emit(AdapterEvent::ItemCompleted {
                    key: block.key.clone(),
                    body: None,
                    status: ItemStatus::Completed,
                });
            }
            return;
        }
        let text = block
            .final_text
            .clone()
            .unwrap_or_else(|| block.accumulated.clone());
        self.emit(AdapterEvent::ItemCompleted {
            key: block.key.clone(),
            body: Some(text_body(block.kind, text)),
            status: ItemStatus::Completed,
        });
    }

    fn on_assistant(&self, msg: &Value) {
        let Some(message) = msg.get("message") else {
            return;
        };
        let msg_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let blocks = message
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut st = self.state.lock();
        self.ack_turn(&mut st);
        let turn = st.turn.as_mut().expect("turn exists after ack");
        let single = blocks.len() == 1;
        for block in blocks {
            let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
            match block_type {
                "text" | "thinking" => {
                    let kind = if block_type == "text" {
                        BlockKind::Text
                    } else {
                        BlockKind::Thinking
                    };
                    let field = if kind == BlockKind::Text {
                        "text"
                    } else {
                        "thinking"
                    };
                    let text = block
                        .get(field)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    // With partial messages each assistant message carries the block that is
                    // currently streaming; match it by message id and kind.
                    let streaming = single
                        .then(|| turn.current_block.clone())
                        .flatten()
                        .filter(|(id, _)| *id == msg_id)
                        .and_then(|k| turn.blocks.get_mut(&k))
                        .filter(|b| b.kind == kind && b.final_text.is_none());
                    match streaming {
                        Some(b) if !b.completed => b.final_text = Some(text),
                        Some(b) => {
                            b.final_text = Some(text.clone());
                            if b.started && b.accumulated != text {
                                self.emit(AdapterEvent::ItemUpdated {
                                    key: b.key.clone(),
                                    body: text_body(kind, text),
                                });
                            }
                        }
                        None => {
                            if text.is_empty() {
                                continue;
                            }
                            turn.standalone += 1;
                            let prefix = if kind == BlockKind::Text {
                                "txt"
                            } else {
                                "rsn"
                            };
                            let key = format!("{prefix}:{msg_id}:a{}", turn.standalone);
                            self.emit(AdapterEvent::ItemStarted {
                                key: key.clone(),
                                body: text_body(kind, text),
                            });
                            self.emit(AdapterEvent::ItemCompleted {
                                key,
                                body: None,
                                status: ItemStatus::Completed,
                            });
                        }
                    }
                }
                "tool_use" => {
                    let Some(id) = block.get("id").and_then(Value::as_str).map(str::to_owned)
                    else {
                        continue;
                    };
                    if turn.tools.contains_key(&id) {
                        continue;
                    }
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    let started = mapping::tool_started_body(&name, &input);
                    let key = started.as_ref().map(|_| format!("tool:{id}"));
                    if let (Some(key), Some(body)) = (&key, &started) {
                        self.emit(AdapterEvent::ItemStarted {
                            key: key.clone(),
                            body: body.clone(),
                        });
                    }
                    turn.tools.insert(
                        id,
                        ToolItem {
                            key,
                            name,
                            input,
                            started,
                            done: false,
                        },
                    );
                }
                "redacted_thinking" => {}
                _ => self.emit(AdapterEvent::Native {
                    payload: json!({ "assistantBlock": block }),
                }),
            }
        }
    }

    fn on_user(&self, msg: &Value) {
        let Some(content) = msg.pointer("/message/content").and_then(Value::as_array) else {
            // A plain-string user message is the CLI replaying our own prompt.
            return;
        };
        let results: Vec<&Value> = content
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .collect();
        if results.is_empty() {
            return;
        }
        // `tool_use_result` describes the message's single tool result; with several results
        // in one message it is ambiguous and not used.
        let structured = if results.len() == 1 {
            msg.get("tool_use_result").cloned()
        } else {
            None
        };
        let mut st = self.state.lock();
        self.ack_turn(&mut st);
        for block in results {
            let Some(tool_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let result = ToolResult {
                text: mapping::tool_result_text(block.get("content").unwrap_or(&Value::Null)),
                is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
                structured: structured.clone(),
                denied_by_user: st.denied_tool_ids.remove(tool_id),
            };
            let turn_seq = st.turn_seq;
            let State { turn, tasks, .. } = &mut *st;
            let Some(turn) = turn.as_mut() else { continue };
            let Some(tool) = turn.tools.get_mut(tool_id) else {
                continue;
            };
            if tool.done {
                continue;
            }
            tool.done = true;
            if mapping::classify_tool(&tool.name) == ToolClass::Plan {
                if tasks.apply(&tool.name, &tool.input, &result) {
                    let body = ItemBody::Plan {
                        entries: tasks.entries(),
                    };
                    match &turn.plan_item {
                        Some(key) => self.emit(AdapterEvent::ItemUpdated {
                            key: key.clone(),
                            body,
                        }),
                        None => {
                            let key = format!("plan:{turn_seq}");
                            turn.plan_item = Some(key.clone());
                            self.emit(AdapterEvent::ItemStarted { key, body });
                        }
                    }
                }
                continue;
            }
            if let (Some(key), Some(started)) = (&tool.key, &tool.started) {
                let (body, status) =
                    mapping::tool_completed(&tool.name, &tool.input, started, &result);
                self.emit(AdapterEvent::ItemCompleted {
                    key: key.clone(),
                    body: Some(body),
                    status,
                });
            }
        }
    }

    async fn on_result(&self, msg: &Value) {
        let request_id = format!("aas_{}", self.next_request.fetch_add(1, Ordering::Relaxed));
        {
            let mut st = self.state.lock();
            if !self.finish_turn(&mut st, msg, &request_id) {
                return;
            }
        }
        let request = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "get_context_usage", "detail": "summary" }
        });
        if let Err(e) = self.write(&request).await {
            tracing::debug!(label = %self.label, error = %e, "could not ask for the context usage");
            self.flush_completion();
        }
    }

    /// Publishes a completion still waiting for its context, without the context.
    fn flush_completion(&self) {
        let completion = self.state.lock().completion.take();
        if let Some(c) = completion {
            self.emit(AdapterEvent::TurnCompleted {
                status: c.status,
                usage: c.usage,
                error: c.error,
            });
        }
    }

    /// Closes the running turn's items and records its completion (sent once the context
    /// usage is known). Returns `false` when there was no turn.
    fn finish_turn(&self, st: &mut State, msg: &Value, request_id: &str) -> bool {
        let Some(turn) = st.turn.take() else {
            // A result without any turn activity (should not happen); still report it.
            self.emit(AdapterEvent::Native {
                payload: msg.clone(),
            });
            return false;
        };
        if !turn.acked {
            self.emit(AdapterEvent::TurnStarted);
        }
        // Text blocks that never saw content_block_stop (e.g. aborted streams) are closed with
        // the text received so far.
        let status_for_open = if turn.interrupt_requested {
            ItemStatus::Interrupted
        } else {
            ItemStatus::Completed
        };
        let mut open: Vec<&Block> = turn
            .blocks
            .values()
            .filter(|b| b.started && !b.completed)
            .collect();
        open.sort_by(|a, b| a.key.cmp(&b.key));
        for block in open {
            let text = block
                .final_text
                .clone()
                .unwrap_or_else(|| block.accumulated.clone());
            self.emit(AdapterEvent::ItemCompleted {
                key: block.key.clone(),
                body: Some(text_body(block.kind, text)),
                status: status_for_open,
            });
        }
        if let Some(key) = &turn.plan_item {
            self.emit(AdapterEvent::ItemCompleted {
                key: key.clone(),
                body: None,
                status: ItemStatus::Completed,
            });
        }
        let total = msg.get("total_cost_usd").and_then(Value::as_f64);
        let delta = total.map(|t| {
            let d = (t - st.last_total_cost).max(0.0);
            st.last_total_cost = t;
            d
        });
        let (status, error) = mapping::turn_outcome(msg, turn.interrupt_requested);
        let usage = mapping::usage_from_result(msg, delta);
        st.completion = Some(PendingCompletion {
            request_id: request_id.to_owned(),
            status,
            usage,
            error,
            deadline: tokio::time::Instant::now() + self.request_timeout,
        });
        true
    }

    fn on_rate_limit(&self, msg: &Value) {
        let info = msg.get("rate_limit_info").cloned().unwrap_or(Value::Null);
        let status = info
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let limit = info
            .get("rateLimitType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let level = match status.as_str() {
            "allowed_warning" => NoticeLevel::Warning,
            "rejected" => NoticeLevel::Error,
            _ => return,
        };
        if !self
            .state
            .lock()
            .rate_limit_notices
            .insert((status.clone(), limit.clone()))
        {
            return;
        }
        let utilization = info.get("utilization").and_then(Value::as_f64);
        let message = match (status.as_str(), utilization) {
            ("rejected", _) => format!("Claude usage limit reached ({limit})"),
            (_, Some(u)) => format!("Claude usage at {:.0}% of the {limit} limit", u * 100.0),
            _ => format!("Claude usage is close to the {limit} limit"),
        };
        self.emit(AdapterEvent::Notice {
            level,
            message,
            code: Some(format!("rateLimit:{status}")),
        });
    }
}

/// Main-thread messages have `parent_tool_use_id: null`; subagent internals are not shown
/// (the subagent's `Task`/`Agent` tool item carries its result).
fn is_main_thread(msg: &Value) -> bool {
    msg.get("parent_tool_use_id").is_none_or(Value::is_null)
}

/// The `message.content` of a user message for `input`: a plain string when the input is
/// only text, otherwise text and image blocks in input order. Mentions are sent as `@path`
/// text (stream-json mode does not expand them; the model reads files with its tools).
pub(crate) async fn user_content(input: &TurnInput) -> Result<Value, AdapterError> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    for part in &input.parts {
        match part {
            TurnInputPart::Text(t) => text.push_str(t),
            TurnInputPart::Mention { relative, .. } => {
                if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                    text.push(' ');
                }
                text.push('@');
                text.push_str(relative);
            }
            TurnInputPart::Image { path, mime } => {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": std::mem::take(&mut text) }));
                }
                let bytes = tokio::fs::read(path).await.map_err(|e| {
                    AdapterError::Other(format!("cannot read image {}: {e}", path.display()))
                })?;
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                blocks.push(json!({ "type": "image", "source": { "type": "base64", "media_type": mime, "data": data } }));
            }
        }
    }
    if blocks.is_empty() {
        return Ok(Value::String(text));
    }
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    Ok(Value::Array(blocks))
}

#[async_trait]
impl SessionControl for ClaudeSession {
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        let content = user_content(&input).await?;
        {
            let mut st = self.inner.state.lock();
            if st.turn.is_some() {
                return Err(AdapterError::Other("a turn is already running".into()));
            }
            // A turn waiting for its context answer ends before the new one begins.
            if let Some(c) = st.completion.take() {
                self.inner.emit(AdapterEvent::TurnCompleted {
                    status: c.status,
                    usage: c.usage,
                    error: c.error,
                });
            }
            st.turn_seq += 1;
            st.turn = Some(Turn::default());
        }
        let msg = json!({
            "type": "user",
            "session_id": "",
            "message": { "role": "user", "content": content },
            "parent_tool_use_id": null
        });
        if let Err(e) = self.inner.write_bounded(&msg).await {
            self.inner.state.lock().turn = None;
            return Err(e);
        }
        Ok(())
    }

    async fn steer(&self, _input: TurnInput) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("steer"))
    }

    /// Sends the `interrupt` control request. Claude Code acknowledges it right away and ends
    /// the turn with its `result`; a CLI that does not acknowledge it within `stop_grace` gets
    /// an error back, so the engine's forced stop (`interrupt_grace`) is never held up here by
    /// the much longer `request_timeout`.
    async fn interrupt(&self) -> Result<(), AdapterError> {
        {
            let mut st = self.inner.state.lock();
            match st.turn.as_mut() {
                Some(turn) => turn.interrupt_requested = true,
                None => return Ok(()),
            }
        }
        self.inner
            .control_within(json!({ "subtype": "interrupt" }), self.inner.stop_grace)
            .await
            .map(|_| ())
    }

    async fn respond(
        &self,
        request_id: &str,
        resolution: &InteractionResolution,
    ) -> Result<(), AdapterError> {
        let ask = self
            .inner
            .state
            .lock()
            .asks
            .get(request_id)
            .cloned()
            .ok_or_else(|| AdapterError::UnknownRequest(request_id.to_owned()))?;
        let (body, denied) =
            mapping::permission_response(&ask, resolution).map_err(AdapterError::Other)?;
        let msg = json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": request_id, "response": body }
        });
        self.inner.write_bounded(&msg).await?;
        let mut st = self.inner.state.lock();
        st.asks.remove(request_id);
        if denied && let Some(tool_id) = ask.tool_use_id {
            st.denied_tool_ids.insert(tool_id);
        }
        Ok(())
    }

    async fn apply_settings(
        &self,
        settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError> {
        let current = self.inner.state.lock().settings.clone();
        if settings.permission_mode != current.permission_mode {
            let mode = settings
                .permission_mode
                .clone()
                .unwrap_or_else(|| "default".to_owned());
            self.inner
                .control(json!({ "subtype": "set_permission_mode", "mode": mode }))
                .await?;
            self.inner.state.lock().settings.permission_mode = settings.permission_mode.clone();
        }
        if settings.model != current.model {
            self.inner
                .control(json!({ "subtype": "set_model", "model": settings.model }))
                .await?;
            self.inner.state.lock().settings.model = settings.model.clone();
        }
        if settings.effort != current.effort {
            self.inner
                .control(json!({ "subtype": "apply_flag_settings", "settings": { "effortLevel": settings.effort } }))
                .await?;
            self.inner.state.lock().settings.effort = settings.effort.clone();
        }
        Ok(SettingsApplied::Live)
    }

    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        let inner = self.inner.clone();
        self.inner
            .shutdown
            .get_or_init(|| async move {
                let started = tokio::time::Instant::now();
                let running = inner.state.lock().turn.is_some();
                if running {
                    // Protocol-level cancel first; the response is not awaited (the process
                    // is about to stop anyway). Bounded: a CLI that no longer reads its stdin
                    // must not keep the stop from reaching the termination stage.
                    let msg = json!({
                        "type": "control_request",
                        "request_id": format!("aas_{}", inner.next_request.fetch_add(1, Ordering::Relaxed)),
                        "request": { "subtype": "interrupt" }
                    });
                    let _ = tokio::time::timeout(inner.stop_grace, inner.write(&msg)).await;
                }
                // Returns promptly even while another write is stuck on a full pipe.
                inner.writer.close().await;
                let grace = inner.stop_grace.saturating_sub(started.elapsed());
                inner.link.shutdown(grace, reason).await
            })
            .await
            .clone()
    }
}
