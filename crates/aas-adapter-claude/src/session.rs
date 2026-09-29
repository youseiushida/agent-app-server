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
    Command, DeltaField, ExpireReason, InteractionResolution, ItemBody, ItemStatus, NoticeLevel,
    ThreadModes, ThreadSettings, TurnError, TurnStatus, TurnTrigger, Usage,
};
use aas_harness::{
    AdapterError, AdapterEvent, BackgroundTaskInfo, ExitInfo, SessionControl, SettingsApplied,
    SideAnswer, StatusSection, StopReason, TurnInput, TurnInputPart,
};
use aas_stdio::{JsonLinesReader, LineError, ReadLine, SharedJsonLinesWriter, WriteError};
use aas_supervisor::ChildHandle;
use async_trait::async_trait;
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, OnceCell, mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::background::{CRON_KEY_PREFIX, Cron, Requester, Tracker};
use crate::commands::{CommandCache, CommandView};
use crate::mapping::{
    self, BackgroundEffect, PLAN_MODE, PermissionAsk, TaskList, ToolClass, ToolResult,
};

/// The callback id of the Stop hook the adapter registers at `initialize`: its input lists the
/// pending scheduled wakeups (`session_crons`). The id is ours to choose (the SDK's
/// `hookCallbackIds`).
pub(crate) const STOP_HOOK_ID: &str = "aas_stop";

/// `system/init.capabilities` entry of a CLI that reports `command_lifecycle` frames for user
/// messages that carry a `uuid`.
const LIFECYCLE_CAPABILITY: &str = "msg_lifecycle_v1";

/// Prefix of the item key of a tool call (`tool:<tool_use_id>`).
const TOOL_KEY_PREFIX: &str = "tool:";

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
    /// `initialize.agentProgressSummaries` (options `agentProgressSummaries`); `None` leaves
    /// the CLI's own default.
    pub agent_progress_summaries: Option<bool>,
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
    agent_progress_summaries: Option<bool>,
    /// Woken when the fate of the message `send` wrote changes ([`Outgoing`]) or the output
    /// ends. One waiter at a time (the engine serializes `send`); a wake-up before the wait
    /// is kept as a permit.
    admission: Notify,
}

#[derive(Default)]
struct State {
    native_session_id: String,
    settings: ThreadSettings,
    turn: Option<Turn>,
    turn_seq: u64,
    /// `total_cost_usd` of the previous `result` (cumulative per process).
    last_total_cost: f64,
    plan: TaskList,
    /// What the CLI said about its commands (docs/adapters/claude.md §8).
    commands: CommandView,
    /// The menu last reported through `CommandsChanged`.
    reported_commands: Vec<Command>,
    reported_model: Option<String>,
    /// The permission mode other than plan last reported through `SessionInfo`.
    reported_permission_mode: Option<String>,
    /// Plan mode last reported through `ModesReported`.
    reported_plan: Option<bool>,
    /// `fast_mode_state` last reported through `ModesReported`.
    reported_fast_state: Option<String>,
    /// The CLI's permission mode is `plan` (as it last reported, or as we set it). The thread's
    /// own permission mode is `settings.permission_mode` meanwhile.
    plan_on: bool,
    /// Fast mode is requested (`apply_flag_settings {fastMode}`).
    fast_on: bool,
    /// Steers written into the running turn, by their uuid, until the CLI ends them.
    steers: HashMap<String, Steer>,
    /// `cancel_async_message` requests for steers the turn did not take, by request id.
    withdrawals: HashMap<String, Steer>,
    /// The `initialize` handshake completed.
    initialized: bool,
    /// A failed `result` before the handshake completed: why the CLI refused to start.
    startup_error: Option<String>,
    asks: HashMap<String, PermissionAsk>,
    denied_tool_ids: HashSet<String>,
    rate_limit_notices: HashSet<(String, String)>,
    /// A finished turn waiting for its context-window occupancy.
    completion: Option<PendingCompletion>,
    /// The user message `send` wrote whose run has not started yet.
    outgoing: Option<Outgoing>,
    /// Whether the CLI reports `command_lifecycle` frames (the last `system/init` lists
    /// `msg_lifecycle_v1` in `capabilities`); `None` before the first `init`.
    lifecycle: Option<bool>,
    /// Commands the CLI reported `queued` that have not started or ended yet. A command that
    /// starts without having been queued is one the CLI enqueued itself (its schema: "cron
    /// triggers, teammate shutdown prompts, deferred-turn resume … emit started/terminal
    /// without 'queued'").
    queued_commands: HashSet<String>,
    /// Background tasks and scheduled wakeups.
    background: Tracker,
    /// The CLI's output ended.
    closed: bool,
}

/// A message steered into a running turn ([`SessionControl::steer_message`]).
#[derive(Debug, Clone)]
struct Steer {
    /// The message's `uuid` (its `command_lifecycle` frames name it).
    uuid: String,
    /// The engine's id of the message.
    message_id: String,
    /// The run it was written into (`turn_seq`).
    turn_seq: u64,
    /// The CLI took it into a run (`command_lifecycle started`).
    started: bool,
}

/// A user message on its way into a run (see [`SessionControl::send`]).
struct Outgoing {
    uuid: String,
    /// The CLI took it into a run: `command_lifecycle started` (or, from a CLI that does not
    /// report the lifecycle, the next run's start).
    started: bool,
    /// The CLI started a run of its own first (its `TurnStarted` was emitted) and the message
    /// waits in the CLI's queue.
    behind: bool,
    /// `started` arrived while `behind`: the `turn_seq` of the run open then (`None`: none was).
    started_in_turn: Option<u64>,
    /// The CLI will not run it (`command_lifecycle` `refused`, `discarded` or `cancelled`
    /// before it started).
    refused: Option<String>,
}

/// A turn whose `result` arrived. Claude Code reports the context-window occupancy only when
/// asked (control request `get_context_usage`), so the adapter asks right after `result` and
/// emits `TurnCompleted` with the answer. The answer is bounded by the request timeout; a new
/// CLI-initiated turn or the end of the output publishes the completion without it.
struct PendingCompletion {
    /// The run that ended (`turn_seq`).
    turn_seq: u64,
    /// The `get_context_usage` request still waiting for its answer.
    context_request: Option<String>,
    /// The `cancel_async_message` requests for the run's steers still waiting for their answer:
    /// a returned steer is reported before the run's `TurnCompleted`.
    withdrawals: HashSet<String>,
    status: TurnStatus,
    usage: Option<Usage>,
    error: Option<TurnError>,
    trigger: Option<TurnTrigger>,
    deadline: tokio::time::Instant,
}

impl PendingCompletion {
    fn answered(&self) -> bool {
        self.context_request.is_none() && self.withdrawals.is_empty()
    }

    fn into_event(self) -> AdapterEvent {
        AdapterEvent::TurnCompleted {
            status: self.status,
            usage: self.usage,
            error: self.error,
            trigger: self.trigger,
        }
    }
}

#[derive(Default)]
struct Turn {
    acked: bool,
    /// The uuid of our user message this run consumes; `None` for a run the CLI started by
    /// itself.
    own: Option<String>,
    interrupt_requested: bool,
    /// The Stop hook listed the pending wakeups in this run (the run ends normally).
    wakeups_listed: bool,
    current_message: Option<String>,
    current_block: Option<(String, u64)>,
    blocks: HashMap<(String, u64), Block>,
    tools: HashMap<String, ToolItem>,
    plan_item: Option<String>,
    standalone: u64,
    /// The `uuid` of the run's last main-thread transcript entry the CLI streamed: its prompt
    /// (our message's `uuid`), then every `assistant` message and `user` tool result. It is the
    /// run's anchor (`--resume-session-at`, docs/adapters/claude.md §19).
    anchor: Option<String>,
    /// Tool uses whose work the CLI reported running in the foreground
    /// (`task_started {is_backgrounded: false}`), which `background_tasks` can move.
    backgroundable: HashSet<String>,
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

/// Where the message `send` wrote stands.
enum Admission {
    Started,
    Behind,
    Refused(String),
}

/// Clears [`State::outgoing`] for `uuid` when `send` returns or is dropped: from then on the
/// CLI's frames for it describe a run the adapter reports as the CLI's own.
struct OutgoingGuard<'a> {
    inner: &'a Inner,
    uuid: &'a str,
}

impl Drop for OutgoingGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.inner.state.lock();
        if st.outgoing.as_ref().is_some_and(|o| o.uuid == self.uuid) {
            st.outgoing = None;
        }
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
            agent_progress_summaries: params.agent_progress_summaries,
            admission: Notify::new(),
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

    /// Sends the `initialize` control request and records models/commands/mode. Then, when
    /// the session starts with the ultracode effort, confirms it through `get_settings`. A CLI
    /// that refuses to start says why in a failed `result` before it ends; that text is the
    /// error then ([`mapping::startup_error`]).
    pub(crate) async fn initialize(&self) -> Result<InitializeInfo, AdapterError> {
        let raw = match self
            .inner
            .control(initialize_request(self.inner.agent_progress_summaries))
            .await
        {
            Ok(raw) => raw,
            Err(e) => {
                return Err(match self.startup_error() {
                    Some(refusal) => AdapterError::Harness(refusal),
                    None => e,
                });
            }
        };
        let mode = raw
            .get("current_permission_mode")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let effort = {
            let mut guard = self.inner.state.lock();
            let st = &mut *guard;
            st.initialized = true;
            st.commands.terminal = self
                .inner
                .command_cache
                .lock()
                .terminal_for(&self.inner.cwd);
            st.commands.set_listed(raw.get("commands"));
            let commands = self.inner.publish_menu(st);
            self.inner.emit(AdapterEvent::CommandsChanged { commands });
            // Plan mode is never the mode to return to (a thread's permission mode `plan` of
            // earlier versions is plan mode over the CLI's default).
            if st
                .settings
                .permission_mode
                .as_deref()
                .is_none_or(|m| m == PLAN_MODE)
            {
                st.settings.permission_mode = mode.clone().filter(|m| m != PLAN_MODE);
            }
            // A report the reader handled meanwhile (`system/status` right after the handshake)
            // is newer than the handshake's answer.
            if let Some(mode) = mode.as_deref().filter(|_| st.reported_plan.is_none()) {
                self.inner.report_permission_mode(st, mode);
            }
            if st.reported_fast_state.is_none() {
                self.inner.report_fast_state(st, &raw);
            }
            st.settings.effort.clone()
        };
        if effort.as_deref() == Some(mapping::ULTRACODE) {
            self.inner.confirm_effort(effort.as_deref()).await?;
        }
        Ok(InitializeInfo { raw })
    }

    pub(crate) fn stderr_tail(&self) -> String {
        self.inner.link.stderr_tail()
    }

    /// Why the CLI refused to start (a failed `result` before the handshake completed).
    pub(crate) fn startup_error(&self) -> Option<String> {
        self.inner.state.lock().startup_error.clone()
    }

    /// `get_usage {skip_behaviors: true}` (the status without a session, from a probe process).
    pub(crate) async fn usage(&self) -> Result<Value, AdapterError> {
        self.inner.control(usage_request()).await
    }
}

/// The `get_usage` request: without the scan of seven days of transcripts that fills
/// `behaviors` (24.5 s instead of 0.36 s in recording b1; the status does not show it).
fn usage_request() -> Value {
    json!({ "subtype": "get_usage", "skip_behaviors": true })
}

/// The `initialize` control request.
/// * `hooks.Stop`: the Stop hook input carries the pending scheduled wakeups
///   (`session_crons`), the only list of them the CLI gives.
/// * `perTaskStopAffordance: true`: the phone stops single tasks with `stop_task`, so an
///   interrupt only aborts the turn and spares background agents and workflows. The CLI takes
///   the first `initialize`'s value for the whole process.
/// * `agentProgressSummaries` only when configured (the CLI's default otherwise).
pub(crate) fn initialize_request(agent_progress_summaries: Option<bool>) -> Value {
    let mut request = json!({
        "subtype": "initialize",
        "hooks": { "Stop": [{ "hookCallbackIds": [STOP_HOOK_ID] }] },
        "perTaskStopAffordance": true
    });
    if let Some(on) = agent_progress_summaries {
        request["agentProgressSummaries"] = json!(on);
    }
    request
}

impl Inner {
    fn emit(&self, event: AdapterEvent) {
        if let Some(tx) = self.events.lock().as_ref() {
            let _ = tx.send(event);
        }
    }

    fn emit_tasks(&self, tasks: Vec<BackgroundTaskInfo>) {
        for task in tasks {
            self.emit(AdapterEvent::BackgroundTask {
                task: Box::new(task),
            });
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

    /// Writes a `control_response` to a request of the CLI (bounded like every write).
    async fn answer(&self, request_id: &str, response: Value) -> Result<(), AdapterError> {
        let msg = json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": request_id, "response": response }
        });
        self.write_bounded(&msg).await
    }

    /// `set_permission_mode {mode}`.
    async fn set_permission_mode(&self, mode: &str) -> Result<(), AdapterError> {
        self.control(json!({ "subtype": "set_permission_mode", "mode": mode }))
            .await
            .map(|_| ())
    }

    /// Reads back what the CLI applied (`get_settings`) and checks that ultracode is on
    /// exactly when `target` is ultracode. The CLI answers `apply_flag_settings` with success
    /// even when it does not turn ultracode on (a model without xhigh effort, workflows
    /// disabled), so only `applied.ultracode` confirms it.
    async fn confirm_effort(&self, target: Option<&str>) -> Result<(), AdapterError> {
        let settings = self.control(json!({ "subtype": "get_settings" })).await?;
        let applied = settings.get("applied");
        let on = applied
            .and_then(|a| a.get("ultracode"))
            .and_then(Value::as_bool)
            == Some(true);
        let effort = applied
            .and_then(|a| a.get("effort"))
            .and_then(Value::as_str);
        let model = applied
            .and_then(|a| a.get("model"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        // What the CLI applied is its explicit report of the effort (reflected into the thread),
        // except when the thread asked for the CLI's default: the level the default resolves
        // to is the CLI's business, and reporting it would replace the user's "default".
        if target.is_some() || on {
            self.emit(AdapterEvent::SessionInfo {
                model: None,
                permission_mode: None,
                effort: if on {
                    Some(mapping::ULTRACODE.to_owned())
                } else {
                    effort.map(str::to_owned)
                },
            });
        }
        let want = target == Some(mapping::ULTRACODE);
        match (want, on) {
            (true, false) => Err(AdapterError::Harness(format!(
                "Claude Code did not turn ultracode on for {model} (get_settings: applied.ultracode is false); ultracode needs a model with xhigh effort and dynamic workflows enabled"
            ))),
            (false, true) => Err(AdapterError::Harness(format!(
                "Claude Code kept ultracode on for {model} (get_settings: applied.ultracode is true)"
            ))),
            _ => Ok(()),
        }
    }

    /// Makes the menu from what the CLI said ([`CommandView::visible`]), shares it (the
    /// adapter's `commands`, the session-switching aliases) and records it as reported.
    fn publish_menu(&self, st: &mut State) -> Vec<Command> {
        let menu = st.commands.visible();
        {
            let mut cache = self.command_cache.lock();
            cache.learn(&st.commands);
            cache.set_menu(&self.cwd, menu.clone());
        }
        st.reported_commands = menu.clone();
        menu
    }

    /// Emits `CommandsChanged` when the menu differs from the one last reported.
    fn update_menu(&self, st: &mut State) {
        if st.commands.visible() != st.reported_commands {
            let commands = self.publish_menu(st);
            self.emit(AdapterEvent::CommandsChanged { commands });
        }
    }

    /// The CLI's permission mode (`initialize.current_permission_mode`, every `system/init`,
    /// and `system/status` whenever it changes): `plan` is plan mode (`ModesReported`), any
    /// other mode is the thread's permission mode (`SessionInfo`, and plan mode off). Both are
    /// reflected into the thread by the engine; nothing is reported twice.
    fn report_permission_mode(&self, st: &mut State, mode: &str) {
        let plan = mode == PLAN_MODE;
        st.plan_on = plan;
        if !plan {
            // What the CLI now runs with, so that `apply_settings` compares with it.
            st.settings.permission_mode = Some(mode.to_owned());
            if st.reported_permission_mode.as_deref() != Some(mode) {
                st.reported_permission_mode = Some(mode.to_owned());
                self.emit(AdapterEvent::SessionInfo {
                    model: None,
                    permission_mode: Some(mode.to_owned()),
                    effort: None,
                });
            }
        }
        if st.reported_plan != Some(plan) {
            st.reported_plan = Some(plan);
            self.emit(AdapterEvent::ModesReported {
                plan: Some(plan),
                fast_state: None,
            });
        }
    }

    /// `fast_mode_state` (`off` / `cooldown` / `on`) of `initialize`, `system/init` or `result`:
    /// what the CLI intends for fast mode, shown as it is (`Thread.fastModeState`). Whether a
    /// request was served fast is the API's business (`usage.speed`); a refusal comes as a
    /// `system/notification` (a notice).
    fn report_fast_state(&self, st: &mut State, msg: &Value) {
        let Some(state) = msg.get("fast_mode_state").and_then(Value::as_str) else {
            return;
        };
        if st.reported_fast_state.as_deref() != Some(state) {
            st.reported_fast_state = Some(state.to_owned());
            self.emit(AdapterEvent::ModesReported {
                plan: None,
                fast_state: Some(state.to_owned()),
            });
        }
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
        // control requests and a waiting `send`, then report the exit.
        self.flush_completion();
        drop(self.pending.lock().take());
        self.state.lock().closed = true;
        self.admission.notify_one();
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
            "command_lifecycle" => self.on_lifecycle(&msg),
            "stream_event" => {
                if is_main_thread(&msg) {
                    self.on_stream_event(&msg);
                }
            }
            "assistant" => {
                if is_main_thread(&msg) {
                    self.on_assistant(&msg);
                } else {
                    self.on_subagent_assistant(&msg);
                }
            }
            "user" => {
                if is_main_thread(&msg) {
                    self.on_user(&msg);
                } else {
                    self.on_subagent_user(&msg);
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
        let failed = response.get("subtype").and_then(Value::as_str) == Some("error");
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        {
            let mut guard = self.state.lock();
            let st = &mut *guard;
            if let Some(completion) = st
                .completion
                .as_mut()
                .filter(|c| c.context_request.as_deref() == Some(id))
            {
                completion.context_request = None;
                if failed {
                    tracing::warn!(label = %self.label, error, "get_context_usage failed; the turn completes without it");
                } else if let Some(context) = response
                    .get("response")
                    .and_then(mapping::context_from_usage_response)
                {
                    completion.usage.get_or_insert_with(Usage::default).context = Some(context);
                }
                self.complete_if_answered(st);
                return;
            }
            if let Some(steer) = st.withdrawals.remove(id) {
                let cancelled = !failed
                    && response
                        .pointer("/response/cancelled")
                        .and_then(Value::as_bool)
                        == Some(true);
                if cancelled {
                    // Claude Code dropped it from its queue: it goes back to the engine's.
                    self.return_steer(st, &steer);
                } else if failed {
                    tracing::warn!(label = %self.label, error, "withdrawing a steer that the turn did not take failed; the CLI runs it as its next run");
                } else {
                    tracing::info!(label = %self.label, "a steer the turn did not take had been dequeued already; the CLI runs it as its next run");
                }
                if let Some(completion) = st.completion.as_mut() {
                    completion.withdrawals.remove(id);
                }
                self.complete_if_answered(st);
                return;
            }
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

    /// Publishes the pending completion once every answer it waits for has come.
    fn complete_if_answered(&self, st: &mut State) {
        if st
            .completion
            .as_ref()
            .is_some_and(PendingCompletion::answered)
            && let Some(c) = st.completion.take()
        {
            self.emit(c.into_event());
        }
    }

    /// Hands a steer the CLI did not take back to the engine (`SteerReturned`), which queues it
    /// again — while its turn is still open for the engine (running, or its completion still
    /// pending). Once that turn completed, the engine has no steer to return it to: the user is
    /// told that the message was not delivered.
    fn return_steer(&self, st: &mut State, steer: &Steer) {
        st.steers.retain(|_, s| s.message_id != steer.message_id);
        let open = st.turn.as_ref().map(|_| st.turn_seq) == Some(steer.turn_seq)
            || st
                .completion
                .as_ref()
                .is_some_and(|c| c.turn_seq == steer.turn_seq);
        if open {
            self.emit(AdapterEvent::SteerReturned {
                message_id: steer.message_id.clone(),
            });
        } else {
            tracing::warn!(label = %self.label, message = %steer.message_id, "a steer came back after its turn completed");
            self.emit(AdapterEvent::Notice {
                level: NoticeLevel::Warning,
                message: "A message sent while the previous turn was running was not delivered to Claude Code; send it again".into(),
                code: Some("steerNotDelivered".into()),
            });
        }
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
        match subtype {
            "can_use_tool" => self.on_can_use_tool(id, &request),
            "hook_callback"
                if request.get("callback_id").and_then(Value::as_str) == Some(STOP_HOOK_ID) =>
            {
                self.on_stop_hook(&id, &request).await;
            }
            _ => {
                // No other hooks and no SDK MCP servers are registered, so nothing else is
                // expected. Answer with an error so the CLI never waits forever, and surface
                // the request.
                let reply = json!({
                    "type": "control_response",
                    "response": { "subtype": "error", "request_id": id, "error": format!("unsupported control request: {subtype}") }
                });
                if let Err(e) = self.write_bounded(&reply).await {
                    // The CLI's stdin is closed, broken or not read: the process is ending or
                    // stuck, which its exit (or the next request's deadline) reports.
                    tracing::warn!(label = %self.label, subtype, error = %e, "could not refuse a control request");
                }
                self.emit(AdapterEvent::Native { payload: msg });
            }
        }
    }

    fn on_can_use_tool(&self, id: String, request: &Value) {
        let ask = PermissionAsk::from_request(request);
        let interaction = mapping::permission_interaction(request, &ask);
        let (item_key, background_key) = {
            let mut st = self.state.lock();
            // `agent_id`: the request comes from a subagent (a background agent, an agent of a
            // workflow, or a subagent the turn waits for).
            let background_key =
                request
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .and_then(|agent| match st.background.requester(agent) {
                        Requester::Task(key) => Some(key),
                        Requester::Foreground => None,
                        Requester::Unknown => Some(agent.to_owned()),
                    });
            let item_key = if background_key.is_none() {
                ask.tool_use_id
                    .as_ref()
                    .and_then(|t| st.turn.as_ref().and_then(|turn| turn.tools.get(t)))
                    .and_then(|tool| tool.key.clone())
            } else {
                None
            };
            st.asks.insert(id.clone(), ask);
            (item_key, background_key)
        };
        self.emit(AdapterEvent::InteractionRequested {
            request_id: id,
            request: interaction,
            item_key,
            background_key,
        });
    }

    /// The Stop hook fired (a turn is ending normally): let the CLI go on, then take the list
    /// of pending wakeups from its input.
    async fn on_stop_hook(&self, id: &str, request: &Value) {
        if let Err(e) = self.answer(id, json!({})).await {
            tracing::warn!(label = %self.label, error = %e, "could not answer the Stop hook");
        }
        let Some(crons) = request
            .pointer("/input/session_crons")
            .and_then(Value::as_array)
        else {
            // A CLI that does not list the wakeups there: nothing is known about them.
            return;
        };
        let crons: Vec<Cron> = crons.iter().filter_map(Cron::from_entry).collect();
        let mut st = self.state.lock();
        if let Some(turn) = st.turn.as_mut() {
            turn.wakeups_listed = true;
        }
        let changes = st.background.crons_listed(crons);
        self.emit_tasks(changes);
    }

    fn on_system(&self, msg: &Value) {
        let subtype = msg.get("subtype").and_then(Value::as_str).unwrap_or("");
        match subtype {
            "init" => {
                let mut guard = self.state.lock();
                let st = &mut *guard;
                self.ack_turn(st, Some(msg));
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
                if model.is_some() && model != st.reported_model {
                    st.reported_model = model.clone();
                    self.emit(AdapterEvent::SessionInfo {
                        model,
                        permission_mode: None,
                        effort: None,
                    });
                }
                if let Some(mode) = msg.get("permissionMode").and_then(Value::as_str) {
                    self.report_permission_mode(st, mode);
                }
                self.report_fast_state(st, msg);
                st.commands.set_init(msg);
                if msg.get("terminal_slash_commands").is_some() {
                    self.command_cache
                        .lock()
                        .remember_terminal(&self.cwd, &st.commands.terminal);
                }
                self.update_menu(st);
            }
            // The whole command list again, after it changed (skills, MCP prompts).
            "commands_changed" => {
                let mut guard = self.state.lock();
                let st = &mut *guard;
                st.commands.set_listed(msg.get("commands"));
                self.update_menu(st);
            }
            // The permission mode changed (`set_permission_mode`, an approval's `setMode`,
            // leaving plan mode after its approval). Other `status` messages are progress.
            "status" => {
                if let Some(mode) = msg.get("permissionMode").and_then(Value::as_str) {
                    let mut st = self.state.lock();
                    self.report_permission_mode(&mut st, mode);
                }
            }
            // A notification the CLI shows its user (e.g. `fast-mode-overage-rejected`, "Fast
            // mode disabled · usage credits exhausted"), in its words.
            "notification" => {
                let level = match msg.get("color").and_then(Value::as_str) {
                    Some("error") => NoticeLevel::Error,
                    Some("warning") => NoticeLevel::Warning,
                    _ => NoticeLevel::Info,
                };
                match msg.get("text").and_then(Value::as_str) {
                    Some(text) => self.emit(AdapterEvent::Notice {
                        level,
                        message: aas_harness::sanitize_terminal_text(text),
                        code: msg.get("key").and_then(Value::as_str).map(str::to_owned),
                    }),
                    None => self.emit(AdapterEvent::Native {
                        payload: msg.clone(),
                    }),
                }
            }
            "background_tasks_changed" => {
                let mut st = self.state.lock();
                let changes = st.background.live_changed(msg);
                self.emit_tasks(changes);
            }
            "task_started" => {
                let mut st = self.state.lock();
                let tool_use_id = msg.get("tool_use_id").and_then(Value::as_str);
                let origin = tool_use_id
                    .and_then(|t| st.turn.as_ref().and_then(|turn| turn.tools.get(t)))
                    .and_then(|tool| tool.key.clone());
                let changes = st.background.started(msg, origin.clone());
                self.emit_tasks(changes);
                // Work the turn's tool waits for in the foreground: from now on
                // `background_tasks {tool_use_id}` can move it (Ctrl+B; recording f3: before
                // this message the CLI answers `backgrounded: false`).
                if msg.get("is_backgrounded").and_then(Value::as_bool) == Some(false)
                    && let (Some(tool_use_id), Some(key)) = (tool_use_id, origin)
                    && let Some(turn) = st.turn.as_mut()
                    && turn.backgroundable.insert(tool_use_id.to_owned())
                {
                    self.emit(AdapterEvent::ItemBackgroundable {
                        key,
                        backgroundable: true,
                    });
                }
            }
            "task_progress" => {
                let mut st = self.state.lock();
                let changes = st.background.progress(msg);
                self.emit_tasks(changes);
            }
            "task_updated" => {
                let mut st = self.state.lock();
                let changes = st.background.updated(msg);
                self.emit_tasks(changes);
            }
            "task_notification" => {
                let mut st = self.state.lock();
                let (changes, known) = st.background.notification(msg);
                self.emit_tasks(changes);
                if !known {
                    self.emit(AdapterEvent::Native {
                        payload: msg.clone(),
                    });
                }
            }
            // Progress signals with no user-visible state: the result marks the turn end;
            // `control_request_progress` tells that one of our requests (a side question) is
            // being worked on, which its answer ends.
            "thinking_tokens" | "session_state_changed" | "control_request_progress" => {}
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

    /// `command_lifecycle {command_uuid, state}`: the fate of a queued command. The message
    /// `send` is waiting for is followed here; other commands (a wakeup coming due, a
    /// continuation) start runs the adapter reports as the CLI's own at their `init`. A
    /// command that starts without having been queued is one the CLI enqueued itself: a
    /// wakeup may have fired (`Tracker::cli_command_started`).
    fn on_lifecycle(&self, msg: &Value) {
        let (Some(uuid), Some(state)) = (
            msg.get("command_uuid").and_then(Value::as_str),
            msg.get("state").and_then(Value::as_str),
        ) else {
            return;
        };
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let ours = st.outgoing.as_ref().is_some_and(|o| o.uuid == uuid);
        match state {
            "queued" => {
                st.queued_commands.insert(uuid.to_owned());
            }
            "started" => {
                if !st.queued_commands.remove(uuid) && !ours {
                    st.background.cli_command_started();
                }
            }
            // completed, cancelled, discarded, refused: the command is over.
            _ => {
                st.queued_commands.remove(uuid);
            }
        }
        if st.steers.contains_key(uuid) {
            self.steer_lifecycle(st, uuid, state);
            return;
        }
        let open_run = st.turn.as_ref().map(|_| st.turn_seq);
        let Some(out) = st.outgoing.as_mut().filter(|o| o.uuid == uuid) else {
            return;
        };
        match state {
            "started" if !out.started => {
                out.started = true;
                if out.behind || open_run.is_some() {
                    out.started_in_turn = open_run;
                    if let Some(turn) = st.turn.as_mut() {
                        // The CLI folded the message into the run it is running (at a tool
                        // boundary): that run answers the user.
                        turn.own = Some(uuid.to_owned());
                    }
                } else {
                    // Our message starts the next run; its uuid is the run's first transcript
                    // entry.
                    st.turn_seq += 1;
                    st.turn = Some(Turn {
                        own: Some(uuid.to_owned()),
                        anchor: Some(uuid.to_owned()),
                        ..Turn::default()
                    });
                }
                self.admission.notify_one();
            }
            "refused" | "discarded" | "cancelled" if !out.started => {
                out.refused = Some(state.to_owned());
                self.admission.notify_one();
            }
            // queued, completed; a cancel after the start is the aborted run's end, which its
            // result reports.
            _ => {}
        }
    }

    /// The fate of a steered message (docs/adapters/claude.md §3, "steer"):
    /// * `started` while its turn runs: the CLI took it into the turn at a tool boundary
    ///   (recording a1: `started` before the turn's `result`, no new `system/init`);
    /// * `started` after the turn's `result`: the CLI runs it as its next run (recording a2),
    ///   which answers the message; the previous turn completes first;
    /// * `refused` / `discarded`, or `cancelled` that we did not ask for, before it started: the
    ///   CLI will not run it, and it goes back to the engine;
    /// * `completed`, or `cancelled` by our withdrawal (whose answer returns it): over.
    fn steer_lifecycle(&self, st: &mut State, uuid: &str, state: &str) {
        let Some(steer) = st.steers.get_mut(uuid) else {
            return;
        };
        match state {
            "started" if !steer.started => {
                steer.started = true;
                if st.turn.is_none() {
                    if let Some(c) = st.completion.take() {
                        self.emit(c.into_event());
                    }
                    st.turn_seq += 1;
                    st.turn = Some(Turn {
                        own: Some(uuid.to_owned()),
                        anchor: Some(uuid.to_owned()),
                        ..Turn::default()
                    });
                }
            }
            "refused" | "discarded" | "cancelled" if !steer.started => {
                let withdrawing = st.withdrawals.values().any(|w| w.uuid == uuid);
                let steer = st.steers.remove(uuid).expect("the steer is known");
                if !withdrawing {
                    tracing::info!(label = %self.label, state, "Claude Code did not take a steered message");
                    self.return_steer(st, &steer);
                }
            }
            "completed" | "refused" | "discarded" | "cancelled" => {
                st.steers.remove(uuid);
            }
            _ => {}
        }
    }

    /// Marks the running turn as acknowledged, or opens a turn the CLI started by itself
    /// (e.g. a background task waking the session). `init` is the `system/init` that starts
    /// it, when that is the message.
    fn ack_turn(&self, st: &mut State, init: Option<&Value>) {
        if let Some(init) = init {
            st.lifecycle = Some(
                init.get("capabilities")
                    .and_then(Value::as_array)
                    .is_some_and(|c| c.iter().any(|v| v.as_str() == Some(LIFECYCLE_CAPABILITY))),
            );
        }
        match st.turn.as_mut() {
            Some(turn) if turn.acked => {}
            Some(turn) => {
                turn.acked = true;
                self.emit(AdapterEvent::TurnStarted);
            }
            None => {
                // The previous turn is over before this one starts, context or not.
                if let Some(c) = st.completion.take() {
                    self.emit(c.into_event());
                }
                let lifecycle = st.lifecycle == Some(true);
                let own = match st.outgoing.as_mut() {
                    Some(out) if !out.started && !out.behind => {
                        if lifecycle {
                            // The CLI reports when it takes our message into a run and has not
                            // done so: this run is its own, and the message waits behind it.
                            out.behind = true;
                            None
                        } else {
                            // A CLI without lifecycle frames: the next run takes the message.
                            out.started = true;
                            Some(out.uuid.clone())
                        }
                    }
                    _ => None,
                };
                st.turn_seq += 1;
                st.turn = Some(Turn {
                    acked: true,
                    anchor: own.clone(),
                    own,
                    ..Turn::default()
                });
                self.emit(AdapterEvent::TurnStarted);
                if st.outgoing.is_some() {
                    self.admission.notify_one();
                }
            }
        }
    }

    fn on_stream_event(&self, msg: &Value) {
        let Some(event) = msg.get("event") else {
            return;
        };
        let mut st = self.state.lock();
        self.ack_turn(&mut st, None);
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
        self.ack_turn(&mut st, None);
        let turn = st.turn.as_mut().expect("turn exists after ack");
        if let Some(uuid) = msg.get("uuid").and_then(Value::as_str) {
            turn.anchor = Some(uuid.to_owned());
        }
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

    /// A subagent's assistant message. Subagent internals are not shown (their tool item and
    /// background task carry them); the tool uses are recorded only to find the owner of a
    /// task the subagent starts.
    fn on_subagent_assistant(&self, msg: &Value) {
        let Some(parent) = msg.get("parent_tool_use_id").and_then(Value::as_str) else {
            return;
        };
        let ids: Vec<&str> = msg
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(|b| b.get("id").and_then(Value::as_str))
            .collect();
        if ids.is_empty() {
            return;
        }
        let mut st = self.state.lock();
        let changes = st.background.subagent_tool_uses(parent, ids);
        self.emit_tasks(changes);
    }

    /// A subagent's user message (its tool results): the answered tool uses are no longer
    /// needed for finding owners.
    fn on_subagent_user(&self, msg: &Value) {
        let Some(content) = msg.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        let mut st = self.state.lock();
        for id in content
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
        {
            st.background.subagent_tool_result(id);
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
        self.ack_turn(&mut st, None);
        if let (Some(uuid), Some(turn)) =
            (msg.get("uuid").and_then(Value::as_str), st.turn.as_mut())
        {
            turn.anchor = Some(uuid.to_owned());
        }
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
            let State {
                turn,
                plan,
                background,
                ..
            } = &mut *st;
            let Some(turn) = turn.as_mut() else { continue };
            let Some(tool) = turn.tools.get_mut(tool_id) else {
                continue;
            };
            if tool.done {
                continue;
            }
            tool.done = true;
            if mapping::classify_tool(&tool.name) == ToolClass::Plan {
                if plan.apply(&tool.name, &tool.input, &result) {
                    let body = ItemBody::Plan {
                        entries: plan.entries(),
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
            let (Some(key), Some(started)) = (&tool.key, &tool.started) else {
                continue;
            };
            let (body, mut status) =
                mapping::tool_completed(&tool.name, &tool.input, started, &result);
            // Work the tool left running in the background: the task first, then the item
            // that goes on as it (the port's order).
            match mapping::background_effect(&tool.name, &tool.input, &result) {
                Some(BackgroundEffect::Task(launch)) => {
                    self.emit_tasks(background.launched(launch, key.clone()));
                    status = ItemStatus::Backgrounded;
                }
                Some(BackgroundEffect::Cron(cron)) => {
                    self.emit_tasks(background.cron_created(cron, key.clone()));
                    status = ItemStatus::Backgrounded;
                }
                Some(BackgroundEffect::CronDeleted(id)) => {
                    self.emit_tasks(background.cron_deleted(&id));
                }
                Some(BackgroundEffect::CronList(crons)) => {
                    self.emit_tasks(background.crons_listed(crons));
                }
                None => {}
            }
            self.emit(AdapterEvent::ItemCompleted {
                key: key.clone(),
                body: Some(body),
                status,
            });
        }
    }

    fn next_request_id(&self) -> String {
        format!("aas_{}", self.next_request.fetch_add(1, Ordering::Relaxed))
    }

    /// A run ended: its completion waits for the context usage (`get_context_usage`) and for
    /// the withdrawal of the steers the run did not take (`cancel_async_message`; a steer the
    /// CLI takes only at a tool boundary, and one that came after the last is still queued).
    /// Both requests are written here and answered in [`Self::on_control_response`].
    async fn on_result(&self, msg: &Value) {
        let request_id = self.next_request_id();
        let withdrawals: Vec<(String, String)> = {
            let mut guard = self.state.lock();
            let st = &mut *guard;
            self.report_fast_state(st, msg);
            if !self.finish_turn(st, msg, &request_id) {
                return;
            }
            let ended = st.turn_seq;
            // Steers the CLI took into this run or an earlier one are over with it (their
            // `completed` frame normally said so already).
            st.steers.retain(|_, s| !s.started || s.turn_seq > ended);
            let unstarted: Vec<Steer> = st
                .steers
                .values()
                .filter(|s| !s.started && s.turn_seq == ended)
                .cloned()
                .collect();
            let mut requests = Vec::new();
            for steer in unstarted {
                let id = self.next_request_id();
                requests.push((id.clone(), steer.uuid.clone()));
                if let Some(c) = st.completion.as_mut() {
                    c.withdrawals.insert(id.clone());
                }
                st.withdrawals.insert(id, steer);
            }
            requests
        };
        let request = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "get_context_usage", "detail": "summary" }
        });
        if let Err(e) = self.write(&request).await {
            tracing::debug!(label = %self.label, error = %e, "could not ask for the context usage");
            self.flush_completion();
        }
        for (id, uuid) in withdrawals {
            if let Err(e) = self.write(&withdraw_request(&id, &uuid)).await {
                // The CLI's input is gone (the process ends): nothing runs the message.
                tracing::warn!(label = %self.label, error = %e, "could not withdraw a steer the turn did not take");
                let mut guard = self.state.lock();
                let st = &mut *guard;
                st.withdrawals.remove(&id);
                if let Some(c) = st.completion.as_mut() {
                    c.withdrawals.remove(&id);
                }
                self.complete_if_answered(st);
            }
        }
    }

    /// Publishes a completion still waiting for its context, without the context.
    fn flush_completion(&self) {
        let completion = self.state.lock().completion.take();
        if let Some(c) = completion {
            self.emit(c.into_event());
        }
    }

    /// Closes the running turn's items and records its completion (sent once the context
    /// usage is known). Returns `false` when there was no turn.
    fn finish_turn(&self, st: &mut State, msg: &Value, request_id: &str) -> bool {
        let Some(turn) = st.turn.take() else {
            if !st.initialized
                && let Some(refusal) = mapping::startup_error(msg)
            {
                // The CLI refused to start the session (e.g. an unknown resume anchor) and
                // ends: `initialize` reports it.
                tracing::info!(label = %self.label, refusal, "Claude Code refused to start the session");
                st.startup_error = Some(refusal);
                return false;
            }
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
        if !turn.wakeups_listed {
            // Interrupted or failed: no list of the pending wakeups came with this end.
            let lifecycle = st.lifecycle == Some(true);
            let changes = st.background.turn_ended_without_list(lifecycle);
            self.emit_tasks(changes);
        }
        let total = msg.get("total_cost_usd").and_then(Value::as_f64);
        let delta = total.map(|t| {
            let d = (t - st.last_total_cost).max(0.0);
            st.last_total_cost = t;
            d
        });
        let (status, error) = mapping::turn_outcome(msg, turn.interrupt_requested);
        let usage = mapping::usage_from_result(msg, delta);
        // Why the CLI started a run by itself, when its result says so (`origin`). A run that
        // took our message is the user's turn.
        let trigger = match turn.own {
            Some(_) => None,
            None => mapping::turn_trigger(msg),
        };
        // The run's anchor, before its completion (the engine records it with the turn).
        if let Some(uuid) = &turn.anchor {
            self.emit(AdapterEvent::TurnAnchor {
                anchor: mapping::turn_anchor(uuid),
            });
        }
        st.completion = Some(PendingCompletion {
            turn_seq: st.turn_seq,
            context_request: Some(request_id.to_owned()),
            withdrawals: HashSet::new(),
            status,
            usage,
            error,
            trigger,
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

    /// Waits until the CLI decided about the message `uuid` that `send` wrote.
    async fn admission(&self, uuid: &str, deadline: Instant) -> Result<Admission, AdapterError> {
        loop {
            {
                let st = self.state.lock();
                match st.outgoing.as_ref().filter(|o| o.uuid == uuid) {
                    Some(o) if o.behind => return Ok(Admission::Behind),
                    Some(o) if o.started => return Ok(Admission::Started),
                    Some(o) if o.refused.is_some() => {
                        return Ok(Admission::Refused(o.refused.clone().unwrap_or_default()));
                    }
                    Some(_) if !st.closed => {}
                    _ => return Err(AdapterError::Closed),
                }
            }
            if tokio::time::timeout_at(deadline, self.admission.notified())
                .await
                .is_err()
            {
                return Err(AdapterError::Protocol(format!(
                    "Claude Code did not take the message into a run within {:?}",
                    self.request_timeout
                )));
            }
        }
    }

    /// The CLI started a run of its own before it took our message `uuid`, which waits in its
    /// queue. The message is withdrawn (`cancel_async_message`) so that the engine sends it
    /// again once that run is over (`TurnInProgress`); its `TurnStarted` has been emitted.
    /// When the CLI had already taken the message (`cancelled: false`), it runs in the CLI's
    /// run (folded in at a tool boundary): that run is then the user's turn.
    async fn withdraw(&self, uuid: &str, deadline: Instant) -> Result<(), AdapterError> {
        let limit = deadline.saturating_duration_since(Instant::now());
        let reply = self
            .control_within(
                json!({ "subtype": "cancel_async_message", "message_uuid": uuid }),
                limit,
            )
            .await?;
        if reply.get("cancelled").and_then(Value::as_bool) == Some(true) {
            tracing::info!(label = %self.label, "claude started a run of its own first; the message was withdrawn and follows that run");
            return Err(AdapterError::TurnInProgress);
        }
        loop {
            {
                let st = self.state.lock();
                match st.outgoing.as_ref().filter(|o| o.uuid == uuid) {
                    Some(o) if o.started => {
                        if o.started_in_turn.is_some() {
                            // Folded into the CLI's run, which `on_lifecycle` bound to it.
                            return Ok(());
                        }
                        return Err(AdapterError::Harness(
                            "Claude Code had taken the message for its next run before it could be withdrawn; that run is reported as the agent's own".into(),
                        ));
                    }
                    Some(o) if o.refused.is_some() => {
                        return Err(AdapterError::Harness(format!(
                            "Claude Code did not take the message (command_lifecycle {})",
                            o.refused.as_deref().unwrap_or_default()
                        )));
                    }
                    Some(_) if !st.closed => {}
                    _ => return Err(AdapterError::Closed),
                }
            }
            if tokio::time::timeout_at(deadline, self.admission.notified())
                .await
                .is_err()
            {
                return Err(AdapterError::Protocol(format!(
                    "Claude Code did not start the message it could not withdraw within {:?}",
                    self.request_timeout
                )));
            }
        }
    }
}

/// `cancel_async_message` for the steer `uuid` ("Drops a pending async user message from the
/// command queue by uuid. No-op if already dequeued for execution."): `{cancelled: true}` when it
/// was dropped (recording a3).
fn withdraw_request(request_id: &str, uuid: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "cancel_async_message", "message_uuid": uuid }
    })
}

/// A status section in place of one whose request failed, with the error's text.
fn failed_section(title: &str, error: &AdapterError) -> StatusSection {
    StatusSection {
        title: title.to_owned(),
        rows: vec![aas_harness::StatusRow {
            label: "Error".into(),
            value: error.detail(),
        }],
    }
}

/// Main-thread messages have `parent_tool_use_id: null`; subagent internals are not shown
/// (the subagent's `Task`/`Agent` tool item and its background task carry its result).
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

/// The user message for `content`. The `uuid` makes the CLI report the message's fate
/// (`command_lifecycle`); `origin {kind: "human"}` says a person typed it (the CLI's trust gates,
/// e.g. the ultracode keyword, fail closed without it).
pub(crate) fn user_message(content: Value, uuid: &str) -> Value {
    json!({
        "type": "user",
        "session_id": "",
        "message": { "role": "user", "content": content },
        "parent_tool_use_id": null,
        "uuid": uuid,
        "origin": { "kind": "human" }
    })
}

#[async_trait]
impl SessionControl for ClaudeSession {
    /// Writes the user message and waits (within `request_timeout`) until the CLI takes it
    /// into a run (`command_lifecycle started`). When the CLI starts a run of its own first,
    /// the message is withdrawn and the error is `TurnInProgress` (see [`Inner::withdraw`]).
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        let content = user_content(&input).await?;
        let uuid = uuid::Uuid::new_v4().to_string();
        {
            let mut st = self.inner.state.lock();
            if st.closed {
                return Err(AdapterError::Closed);
            }
            if st.turn.is_some() {
                // A run the CLI started by itself (its `TurnStarted` was emitted at its start).
                return Err(AdapterError::TurnInProgress);
            }
            // A turn waiting for its context answer ends before the new one begins.
            if let Some(c) = st.completion.take() {
                self.inner.emit(c.into_event());
            }
            st.outgoing = Some(Outgoing {
                uuid: uuid.clone(),
                started: false,
                behind: false,
                started_in_turn: None,
                refused: None,
            });
        }
        let _guard = OutgoingGuard {
            inner: &self.inner,
            uuid: &uuid,
        };
        self.inner
            .write_bounded(&user_message(content, &uuid))
            .await?;
        let deadline = Instant::now() + self.inner.request_timeout;
        match self.inner.admission(&uuid, deadline).await? {
            Admission::Started => Ok(()),
            Admission::Behind => self.inner.withdraw(&uuid, deadline).await,
            Admission::Refused(state) => Err(AdapterError::Harness(format!(
                "Claude Code did not take the message (command_lifecycle {state})"
            ))),
        }
    }

    /// A steer without the engine's id of the message (the engine always names it, see
    /// [`Self::steer_message`]).
    async fn steer(&self, input: TurnInput) -> Result<(), AdapterError> {
        let message_id = uuid::Uuid::new_v4().to_string();
        self.steer_message(&message_id, input).await
    }

    /// Writes the message into the running turn as an ordinary user message (no `priority`;
    /// `priority: "now"` would abort the turn, recording a4). Claude Code takes it at the turn's
    /// next tool boundary: `command_lifecycle started` before the turn's `result`, with no new
    /// `system/init` (recording a1). A message the turn did not take by its `result` is
    /// withdrawn (`cancel_async_message`) and handed back with `SteerReturned` before the
    /// turn's `TurnCompleted` (docs/adapters/claude.md §3).
    async fn steer_message(&self, message_id: &str, input: TurnInput) -> Result<(), AdapterError> {
        let content = user_content(&input).await?;
        let uuid = uuid::Uuid::new_v4().to_string();
        {
            let mut guard = self.inner.state.lock();
            let st = &mut *guard;
            if st.closed {
                return Err(AdapterError::Closed);
            }
            if st.lifecycle != Some(true) {
                return Err(AdapterError::Harness(
                    "this Claude Code does not report when it takes a message into a turn (no msg_lifecycle_v1), so a steer could not be followed".into(),
                ));
            }
            match (&st.turn, &st.completion) {
                (Some(_), _) => {
                    let steer = Steer {
                        uuid: uuid.clone(),
                        message_id: message_id.to_owned(),
                        turn_seq: st.turn_seq,
                        started: false,
                    };
                    st.steers.insert(uuid.clone(), steer);
                }
                // The turn's `result` came already; its completion waits for its answers, so the
                // engine still has the turn: the message goes back to its queue right away.
                (None, Some(_)) => {
                    self.inner.emit(AdapterEvent::SteerReturned {
                        message_id: message_id.to_owned(),
                    });
                    return Ok(());
                }
                (None, None) => {
                    return Err(AdapterError::Other(
                        "the turn ended before the message could be added to it; send it as a new message".into(),
                    ));
                }
            }
        }
        if let Err(e) = self
            .inner
            .write_bounded(&user_message(content, &uuid))
            .await
        {
            self.inner.state.lock().steers.remove(&uuid);
            return Err(e);
        }
        Ok(())
    }

    /// Sends the `interrupt` control request. Claude Code acknowledges it right away and ends
    /// the turn with its `result`; a CLI that does not acknowledge it within `stop_grace` gets
    /// an error back, so the engine's forced stop (`interrupt_grace`) is never held up here by
    /// the much longer `request_timeout`. With `perTaskStopAffordance` the CLI aborts only the
    /// turn: background agents and workflows go on.
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
        self.inner.answer(request_id, body).await?;
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
        let (current, plan_on) = {
            let st = self.inner.state.lock();
            (st.settings.clone(), st.plan_on)
        };
        // The permission mode `plan` of earlier versions is plan mode, which `apply_modes`
        // handles: the mode plan mode returns to stays.
        let wanted_mode = match settings.permission_mode.as_deref() {
            Some(PLAN_MODE) => current.permission_mode.clone(),
            _ => settings.permission_mode.clone(),
        };
        if wanted_mode != current.permission_mode {
            let mode = wanted_mode.clone().unwrap_or_else(|| "default".to_owned());
            self.inner.set_permission_mode(&mode).await?;
            self.inner.state.lock().settings.permission_mode = wanted_mode;
            if plan_on {
                // In plan mode the thread's permission mode is the one Claude Code returns to
                // when plan mode ends (after the plan's approval), which it remembers when plan
                // mode begins (recordings h1, h2): plan mode begins again from the new mode.
                self.inner.set_permission_mode(PLAN_MODE).await?;
            }
        }
        let model_changed = settings.model != current.model;
        if model_changed {
            self.inner
                .control(json!({ "subtype": "set_model", "model": settings.model }))
                .await?;
            self.inner.state.lock().settings.model = settings.model.clone();
        }
        let effort_changed = settings.effort != current.effort;
        let was_ultracode = current.effort.as_deref() == Some(mapping::ULTRACODE);
        let ultracode = settings.effort.as_deref() == Some(mapping::ULTRACODE);
        if effort_changed {
            self.inner
                .control(json!({
                    "subtype": "apply_flag_settings",
                    "settings": mapping::effort_flag_settings(settings.effort.as_deref(), was_ultracode)
                }))
                .await?;
            self.inner.state.lock().settings.effort = settings.effort.clone();
        }
        // Ultracode is confirmed by reading back what the CLI applied, after it was asked for,
        // left, or the model changed under it (the CLI turns it off for a model without xhigh
        // effort and on again for one with it, and reports neither).
        if (ultracode && (effort_changed || model_changed)) || (was_ultracode && effort_changed) {
            self.inner
                .confirm_effort(settings.effort.as_deref())
                .await?;
        }
        Ok(SettingsApplied::Live)
    }

    /// Plan mode is Claude Code's permission mode `plan` (`set_permission_mode`), entered from
    /// the thread's permission mode, to which the CLI returns when the plan is approved; leaving
    /// it by request sets the thread's permission mode again (`default` when there is none:
    /// `plan` is never a mode to return to). Fast mode is the flag setting `fastMode`
    /// (`apply_flag_settings`), the SDK's opt-in and switch in one (recording e1).
    ///
    /// After a change the CLI's next word on plan mode always goes to the engine (which asked
    /// for the change), also when it says the CLI kept or left plan mode after all.
    async fn apply_modes(&self, modes: &ThreadModes) -> Result<SettingsApplied, AdapterError> {
        let (plan_on, fast_on, base) = {
            let st = self.inner.state.lock();
            (st.plan_on, st.fast_on, st.settings.permission_mode.clone())
        };
        if modes.plan != plan_on {
            let mode = if modes.plan {
                PLAN_MODE.to_owned()
            } else {
                base.filter(|m| m != PLAN_MODE)
                    .unwrap_or_else(|| "default".to_owned())
            };
            self.inner.set_permission_mode(&mode).await?;
            let mut st = self.inner.state.lock();
            st.plan_on = modes.plan;
            st.reported_plan = None;
        }
        if modes.fast != fast_on {
            self.inner
                .control(json!({
                    "subtype": "apply_flag_settings",
                    "settings": mapping::fast_mode_flag_settings(modes.fast)
                }))
                .await?;
            self.inner.state.lock().fast_on = modes.fast;
        }
        Ok(SettingsApplied::Live)
    }

    /// `rename_session` for the session this process runs (`session_id`: the CLI refuses the
    /// request when it moved to another one) as a rename the user made in the hosting
    /// application (`source: "host"`, which the CLI counts as a user rename). The CLI answers
    /// with no body and reports nothing on stdout; the name goes to the transcript
    /// (`custom-title`) (recording b1).
    async fn rename(&self, title: &str) -> Result<(), AdapterError> {
        let session_id = self.inner.state.lock().native_session_id.clone();
        self.inner
            .control(json!({
                "subtype": "rename_session",
                "title": title,
                "source": "host",
                "session_id": session_id
            }))
            .await
            .map(|_| ())
    }

    /// The CLI's own `/status` sections (`get_status`, `@internal`), its usage (`get_usage`
    /// with `skip_behaviors`, experimental) and, in plan mode, the current plan (`get_plan`,
    /// `@internal`); see [`mapping::status_sections`]. A failed `get_usage` or `get_plan`
    /// becomes a section that says so; a failed `get_status` fails the request.
    async fn status(&self) -> Result<Vec<StatusSection>, AdapterError> {
        let status = self
            .inner
            .control(json!({ "subtype": "get_status" }))
            .await?;
        let mut sections = mapping::status_sections(&status);
        match self.inner.control(usage_request()).await {
            Ok(usage) => sections.extend(mapping::usage_sections(&usage, true)),
            Err(e) => sections.push(failed_section("Plan usage", &e)),
        }
        if self.inner.state.lock().plan_on {
            match self.inner.control(json!({ "subtype": "get_plan" })).await {
                Ok(plan) => sections.extend(mapping::plan_section(&plan)),
                Err(e) => sections.push(failed_section("Plan", &e)),
            }
        }
        Ok(sections)
    }

    /// `/btw`: `side_question {question}` -> `{response, synthetic}`. The CLI answers beside the
    /// conversation, also while a turn runs, and records nothing in the transcript (recording
    /// b1).
    async fn side_question(&self, question: &str) -> Result<SideAnswer, AdapterError> {
        let reply = self
            .inner
            .control(json!({ "subtype": "side_question", "question": question }))
            .await?;
        Ok(SideAnswer {
            answer: reply
                .get("response")
                .and_then(Value::as_str)
                .map(str::to_owned),
            synthetic: reply.get("synthetic").and_then(Value::as_bool) == Some(true),
        })
    }

    /// Ctrl+B for one tool: `background_tasks {tool_use_id}` -> `{backgrounded: true}`. The
    /// tool's result then says the work goes on in the background, which closes the item as
    /// `backgrounded` with its task (recordings f1b, f2).
    async fn move_to_background(&self, item_key: &str) -> Result<(), AdapterError> {
        let tool_use_id = {
            let st = self.inner.state.lock();
            item_key
                .strip_prefix(TOOL_KEY_PREFIX)
                .filter(|id| {
                    st.turn
                        .as_ref()
                        .is_some_and(|turn| turn.backgroundable.contains(*id))
                })
                .map(str::to_owned)
        };
        let Some(tool_use_id) = tool_use_id else {
            return Err(AdapterError::Other(format!(
                "{item_key} is not work Claude Code runs in the foreground of the current turn"
            )));
        };
        let reply = self
            .inner
            .control(json!({ "subtype": "background_tasks", "tool_use_id": tool_use_id }))
            .await?;
        match reply.get("backgrounded").and_then(Value::as_bool) {
            Some(true) => Ok(()),
            _ => Err(AdapterError::Harness(format!(
                "Claude Code did not move the work to the background (background_tasks answered {reply})"
            ))),
        }
    }

    /// Stops background task `key` with `stop_task`. The CLI answers `{}` also for a task
    /// that is unknown or already ended, so the answer only says the request was taken; the
    /// end arrives as `task_notification`. Scheduled wakeups cannot be stopped this way.
    async fn stop_background(&self, key: &str) -> Result<(), AdapterError> {
        if key.starts_with(CRON_KEY_PREFIX) {
            return Err(AdapterError::Other(
                "Claude Code has no request that cancels a scheduled wakeup".into(),
            ));
        }
        self.inner
            .control(json!({ "subtype": "stop_task", "task_id": key }))
            .await
            .map(|_| ())
    }

    /// Answers a permission request or question the engine no longer waits for with a denial
    /// that says why, so that the CLI (or its background agent) does not wait forever.
    async fn expire_request(
        &self,
        request_id: &str,
        reason: ExpireReason,
    ) -> Result<(), AdapterError> {
        let ask = self
            .inner
            .state
            .lock()
            .asks
            .get(request_id)
            .cloned()
            .ok_or_else(|| AdapterError::UnknownRequest(request_id.to_owned()))?;
        self.inner
            .answer(request_id, mapping::expiry_response(reason))
            .await?;
        let mut st = self.inner.state.lock();
        st.asks.remove(request_id);
        if let Some(tool_id) = ask.tool_use_id {
            st.denied_tool_ids.insert(tool_id);
        }
        Ok(())
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
