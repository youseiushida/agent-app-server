//! Protocol core of one `pi --mode rpc` process.
//!
//! Generic over the byte streams and the process link so recorded transcripts can be
//! replayed over `tokio::io::duplex` without spawning anything.
//!
//! # Turn lifecycle (explicit signals only)
//!
//! pi (0.85.1, `agent-session.js` / `rpc-mode.js`) runs one agent run at a time. A run starts
//! with `agent_start` and ends with `agent_settled` ("no retry, compaction retry or queued
//! continuation remains"); within a run, every agent loop (the first one and each retry or
//! continuation) goes from `agent_start` to `agent_end`. `agent_end` alone is not an end: a
//! retry or compaction may follow.
//!
//! * **The user's turn.** `send` writes `prompt` and marks a turn active. The `prompt` response
//!   (`success`) or the first `agent_start` → `TurnStarted`. A failed `prompt` response →
//!   `TurnCompleted { failed }` (pi refused it before starting). `agent_settled` →
//!   `TurnCompleted`.
//! * **Prompts that start no run** (extension commands, `input` handlers) never produce
//!   `agent_settled`. After the `prompt` response the session therefore asks `get_state`: pi
//!   sets `isStreaming` synchronously right after it answers a prompt that starts a run, so
//!   `isStreaming == false` in that answer means "no run" → the turn completes immediately.
//! * **Runs pi starts by itself** (an extension's `sendMessage(…, { triggerTurn })` or
//!   `sendUserMessage`, from a timer, a watcher or an event handler): an `agent_start` while
//!   no turn is active → `TurnStarted` of a turn without input (design.md §5.5), mapped like
//!   any turn until its `agent_settled` → `TurnCompleted`. `interrupt` sends `abort`. While
//!   such a run goes on, `send` returns [`AdapterError::TurnInProgress`] without writing
//!   anything (pi would refuse a prompt: it is busy).
//! * **A run started from a run's end.** An extension can start a run from its handler of
//!   `agent_settled`; pi then writes the new run's `agent_start` before the old run's
//!   `agent_settled` (recorded). An `agent_settled` that arrives while an agent loop is
//!   running (an `agent_start` after the last `agent_end`) therefore ends the turn at once and
//!   opens a turn for the run that goes on; an `agent_start` after the turn's run has settled
//!   (the turn only waits for its context) is a new run too.
//! * **The race with a busy agent.** pi answers a plain prompt only after its preflight, and
//!   refuses it (`success: false`) when a run of its own started first. `send` waits for pi's
//!   answer to a plain prompt; an `agent_start` before that answer is a run pi started by
//!   itself (for a plain prompt pi answers before its own run starts). The run gets its own
//!   turn at once; the answer decides the rest, by event order only (pi's error text is never
//!   read): refused → the run stays a turn of its own and `send` returns `TurnInProgress`
//!   (the engine sends the input again after that turn); taken → the run is part of the
//!   user's turn. `send` stops waiting at the first sign that pi is working on the prompt
//!   and needs the user or time: a dialog, or a compaction before the prompt runs. Extension
//!   commands (pi runs them even while busy, and answers when their handler returns) are not
//!   waited for.
//! * `/compact [instructions]` (see [`crate::commands`]) is sent as the RPC command `compact`
//!   and runs as a turn of its own: `compaction_start` or the response starts it, the
//!   `compact` response ends it.
//!
//! # Context-window occupancy
//!
//! After every finished assistant message the session asks `get_session_stats` and relays
//! its `contextUsage` (tokens and window, as pi computes them for its own footer and
//! compaction) with the turn's usage. `agent_settled` completes the turn only once the
//! answers to those requests are in, so the last one belongs to the turn — unless a new run
//! has already started (above), which ends the turn at once.
//!
//! # Dialogs
//!
//! Every dialog stays pending until it is answered ([`SessionControl::respond`], or
//! [`SessionControl::expire_request`] when the engine no longer waits for it: pi then gets
//! the dismissal, `cancelled: true`), until an explicit signal withdraws it (the gate's
//! `dialogClosed` report: pi closed the gate's dialog because the turn was aborted), or until
//! the process ends. The end of a turn does not close a dialog: pi would wait for its answer
//! forever. pi's RPC mode closes a dialog that carries a `timeout` by itself without telling
//! the client; the adapter does not time it (see `docs/adapters/pi.md` §6).
//!
//! Every event is emitted by the reader task, so `Exited` is always last.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aas_harness::{
    AdapterError, AdapterEvent, ExitInfo, InteractionResolution, SessionControl, SettingsApplied,
    StopReason, ThreadSettings, TurnError, TurnInput, TurnStatus,
};
use aas_stdio::{JsonLinesReader, LineError, ReadLine, SharedJsonLinesWriter};
use async_trait::async_trait;
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use crate::commands;
use crate::gate::{self, Dialog, PendingDialog};
use crate::mapping::{self, Mapper};
use crate::wire::{self, PiCommand, PiModel, PiState, Response};

/// The process behind a session.
#[async_trait]
pub trait ProcessLink: Send + Sync + 'static {
    /// Resolves when the process tree is gone.
    async fn wait(&self) -> ExitInfo;
    /// Staged stop (caller already closed stdin).
    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo;
}

/// A supervised child.
pub struct ChildLink(pub aas_supervisor::ChildHandle);

#[async_trait]
impl ProcessLink for ChildLink {
    async fn wait(&self) -> ExitInfo {
        self.0.wait().await
    }

    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
        self.0.shutdown(grace, reason).await
    }
}

/// Static configuration of a session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub label: String,
    /// Mode file of the approval gate (`None` when the gate is not loaded, e.g. probes).
    pub gate_file: Option<PathBuf>,
    pub stop_grace: Duration,
    pub request_timeout: Duration,
    pub max_line_bytes: usize,
}

enum Internal {
    Emit(AdapterEvent),
    /// A `get_session_stats` request got no answer within the request timeout.
    StatsTimeout(String),
}

/// What began the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnKind {
    /// The user's `prompt`.
    Prompt,
    /// `/compact`, sent as the RPC command `compact`.
    Compact,
    /// A run pi started by itself: an `agent_start` while no turn was active.
    Agent,
}

/// Runs pi started by itself while a plain prompt waited for pi's answer (the race with a busy
/// agent, see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Foreign {
    #[default]
    None,
    /// Such a run goes on.
    Running,
    /// Such a run has settled (its `TurnCompleted` waits for pi's answer to the prompt).
    Settled,
}

/// What `send` returns once pi has decided about a plain prompt.
type Decision = oneshot::Sender<Result<(), AdapterError>>;

#[derive(Debug)]
struct ActiveTurn {
    kind: TurnKind,
    /// Id of the `prompt` (or `compact`) command that began the turn; `None` for a run pi
    /// started by itself.
    command_id: Option<String>,
    started: bool,
    accepted: bool,
    settled_early: bool,
    probe_id: Option<String>,
    abort_requested: bool,
    /// `agent_start` of this turn's run has arrived.
    run_started: bool,
    /// An abort was sent before the run started: it is sent again at the run's `agent_start`
    /// (see [`PiSession::interrupt`]).
    abort_on_run_start: bool,
    /// `get_session_stats` requests of this turn not answered yet.
    stats_pending: HashSet<String>,
    /// `agent_settled` arrived while stats requests were pending.
    settle_waiting: bool,
    /// `send` waits for pi's answer to this (plain) prompt.
    decision: Option<Decision>,
    /// Runs pi started by itself before it answered this prompt.
    foreign: Foreign,
    /// The user messages that open this turn's run are relayed as notices: the run's input
    /// was not typed by the user (a run pi started by itself, or one an extension command
    /// started), so the engine has no user message for it.
    relay_input: bool,
    /// Between the start of the turn's run and its first assistant message.
    input_phase: bool,
}

impl ActiveTurn {
    fn new(kind: TurnKind, command_id: Option<String>) -> Self {
        Self {
            kind,
            command_id,
            started: false,
            accepted: false,
            settled_early: false,
            probe_id: None,
            abort_requested: false,
            run_started: false,
            abort_on_run_start: false,
            stats_pending: HashSet::new(),
            settle_waiting: false,
            decision: None,
            foreign: Foreign::None,
            relay_input: false,
            input_phase: false,
        }
    }

    /// The turn of a run pi started by itself, from its `agent_start` on.
    fn agent() -> Self {
        Self {
            started: true,
            accepted: true,
            run_started: true,
            relay_input: true,
            input_phase: true,
            ..Self::new(TurnKind::Agent, None)
        }
    }

    /// Whether `send` still waits for pi's answer to this prompt (its caller may have given up).
    fn waiting(&self) -> bool {
        self.decision.as_ref().is_some_and(|tx| !tx.is_closed())
    }

    /// A plain prompt pi has not answered yet, with no run of pi's own seen before: pi is
    /// working on it (its preflight).
    fn in_preflight(&self) -> bool {
        self.kind == TurnKind::Prompt && !self.accepted && self.foreign == Foreign::None
    }
}

/// Hands pi's decision about a prompt to the waiting `send` (nothing when it gave up).
fn decide(decision: Option<Decision>, outcome: Result<(), AdapterError>) {
    if let Some(tx) = decision {
        let _ = tx.send(outcome);
    }
}

#[derive(Default)]
struct State {
    waiters: HashMap<String, oneshot::Sender<Response>>,
    turn: Option<ActiveTurn>,
    dialogs: HashMap<String, PendingDialog>,
    declined: HashSet<String>,
    steering: Vec<String>,
    closed: bool,
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    /// `/compact` is executed by the adapter (pi lists no command of that name).
    builtin_compact: bool,
    /// Invocation names of pi's extension commands (from the last `get_commands`).
    extension_commands: HashSet<String>,
    /// An agent loop is running: an `agent_start` arrived after the last `agent_end`.
    loop_running: bool,
}

impl State {
    /// Whether a new turn may start: the session is open and no turn is active. A run pi
    /// started by itself is going on → [`AdapterError::TurnInProgress`] (its `TurnStarted` has
    /// been emitted).
    fn idle(&self) -> Result<(), AdapterError> {
        if self.closed {
            return Err(AdapterError::Closed);
        }
        match &self.turn {
            None => Ok(()),
            Some(turn) if turn.kind == TurnKind::Agent => Err(AdapterError::TurnInProgress),
            Some(_) => Err(AdapterError::Other("a turn is already running".into())),
        }
    }
}

pub(crate) struct Shared {
    cfg: SessionConfig,
    writer: SharedJsonLinesWriter,
    state: Mutex<State>,
    next_id: AtomicU64,
    internal: mpsc::UnboundedSender<Internal>,
    link: Arc<dyn ProcessLink>,
    shutdown: tokio::sync::OnceCell<ExitInfo>,
}

/// One pi session.
#[derive(Clone)]
pub struct PiSession {
    shared: Arc<Shared>,
}

impl PiSession {
    /// Starts reading pi's output. The returned receiver yields every adapter event, ending
    /// with exactly one `Exited`.
    pub fn start<R, W>(
        reader: R,
        writer: W,
        link: Arc<dyn ProcessLink>,
        cfg: SessionConfig,
    ) -> (PiSession, mpsc::UnboundedReceiver<AdapterEvent>)
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let max_line = cfg.max_line_bytes;
        let shared = Arc::new(Shared {
            cfg,
            writer: SharedJsonLinesWriter::new(writer),
            // Until `get_commands` says otherwise, pi has no `compact` command of its own.
            state: Mutex::new(State {
                builtin_compact: true,
                ..State::default()
            }),
            next_id: AtomicU64::new(1),
            internal: internal_tx,
            link,
            shutdown: tokio::sync::OnceCell::new(),
        });
        let task = ReaderTask {
            shared: shared.clone(),
            events: events_tx,
            mapper: Mapper::new(),
        };
        tokio::spawn(task.run(JsonLinesReader::new(reader, max_line), internal_rx));
        (PiSession { shared }, events_rx)
    }

    pub(crate) fn downgrade(&self) -> std::sync::Weak<Shared> {
        Arc::downgrade(&self.shared)
    }

    pub(crate) fn upgrade(weak: &std::sync::Weak<Shared>) -> Option<PiSession> {
        weak.upgrade().map(|shared| PiSession { shared })
    }

    /// Emits an event through the reader task (keeps `Exited` last).
    pub fn emit(&self, event: AdapterEvent) {
        let _ = self.shared.internal.send(Internal::Emit(event));
    }

    /// Whether pi's output has ended.
    pub fn is_closed(&self) -> bool {
        self.shared.state.lock().closed
    }

    fn next_id(&self) -> String {
        next_id(&self.shared)
    }

    async fn write(&self, value: Value) -> Result<(), AdapterError> {
        write(&self.shared, value).await
    }

    /// Sends a command and waits for its response.
    pub async fn request(&self, kind: &str, fields: Value) -> Result<Response, AdapterError> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.shared.state.lock();
            if st.closed {
                return Err(AdapterError::Closed);
            }
            st.waiters.insert(id.clone(), tx);
        }
        if let Err(e) = self.write(wire::cmd_with(&id, kind, fields)).await {
            self.shared.state.lock().waiters.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(self.shared.cfg.request_timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err(AdapterError::Closed),
            Err(_) => {
                self.shared.state.lock().waiters.remove(&id);
                Err(AdapterError::Other(format!(
                    "pi did not answer `{kind}` within {:?}",
                    self.shared.cfg.request_timeout
                )))
            }
        }
    }

    /// Like [`request`](Self::request) but turns `success: false` into an error.
    pub async fn request_ok(
        &self,
        kind: &str,
        fields: Value,
    ) -> Result<Option<Value>, AdapterError> {
        let resp = self.request(kind, fields).await?;
        if resp.success {
            Ok(resp.data)
        } else {
            Err(AdapterError::Harness(resp.error_message()))
        }
    }

    pub async fn get_state(&self) -> Result<PiState, AdapterError> {
        let data = self
            .request_ok("get_state", json!({}))
            .await?
            .unwrap_or(Value::Null);
        serde_json::from_value(data).map_err(|e| AdapterError::Protocol(format!("get_state: {e}")))
    }

    pub async fn get_models(&self) -> Result<Vec<PiModel>, AdapterError> {
        let data = self
            .request_ok("get_available_models", json!({}))
            .await?
            .unwrap_or(Value::Null);
        serde_json::from_value(data.get("models").cloned().unwrap_or(Value::Null))
            .map_err(|e| AdapterError::Protocol(format!("get_available_models: {e}")))
    }

    pub async fn get_commands(&self) -> Result<Vec<PiCommand>, AdapterError> {
        let data = self
            .request_ok("get_commands", json!({}))
            .await?
            .unwrap_or(Value::Null);
        let commands: Vec<PiCommand> =
            serde_json::from_value(data.get("commands").cloned().unwrap_or(Value::Null))
                .map_err(|e| AdapterError::Protocol(format!("get_commands: {e}")))?;
        {
            let mut st = self.shared.state.lock();
            st.builtin_compact = commands::builtin_compact(&commands);
            st.extension_commands = commands::extension_command_names(&commands);
        }
        Ok(commands)
    }

    /// Records the model/effort/mode currently in effect (after the handshake).
    pub fn set_current(
        &self,
        model: Option<String>,
        effort: Option<String>,
        permission_mode: Option<String>,
    ) {
        let mut st = self.shared.state.lock();
        st.model = model;
        st.effort = effort;
        st.permission_mode = permission_mode;
    }

    async fn set_model(&self, model: &str) -> Result<(), AdapterError> {
        let (provider, model_id) = wire::split_model_id(model).ok_or_else(|| {
            AdapterError::Other(format!(
                "pi model ids have the form provider/id, got `{model}`"
            ))
        })?;
        self.request_ok(
            "set_model",
            json!({ "provider": provider, "modelId": model_id }),
        )
        .await?;
        self.shared.state.lock().model = Some(model.to_owned());
        Ok(())
    }

    async fn set_effort(&self, level: &str) -> Result<(), AdapterError> {
        if !wire::THINKING_LEVELS.contains(&level) {
            return Err(AdapterError::Other(format!(
                "unknown thinking level `{level}`"
            )));
        }
        self.request_ok("set_thinking_level", json!({ "level": level }))
            .await?;
        self.shared.state.lock().effort = Some(level.to_owned());
        Ok(())
    }

    /// Applies model/effort/mode that differ from what is in effect.
    pub async fn apply(&self, settings: &ThreadSettings) -> Result<(), AdapterError> {
        let (model, effort, mode) = {
            let st = self.shared.state.lock();
            (
                st.model.clone(),
                st.effort.clone(),
                st.permission_mode.clone(),
            )
        };
        if let Some(wanted) = &settings.permission_mode {
            if !gate::is_valid_mode(wanted) {
                return Err(AdapterError::Other(format!(
                    "unknown permission mode `{wanted}`"
                )));
            }
            if mode.as_deref() != Some(wanted.as_str()) {
                if let Some(file) = &self.shared.cfg.gate_file {
                    gate::write_mode(file, wanted)
                        .map_err(|e| AdapterError::Other(format!("writing gate mode: {e}")))?;
                }
                self.shared.state.lock().permission_mode = Some(wanted.clone());
            }
        }
        if let Some(wanted) = &settings.model
            && model.as_deref() != Some(wanted.as_str())
        {
            self.set_model(wanted).await?;
        }
        if let Some(wanted) = &settings.effort
            && effort.as_deref() != Some(wanted.as_str())
        {
            self.set_effort(wanted).await?;
        }
        let st = self.shared.state.lock();
        let info = AdapterEvent::SessionInfo {
            model: st.model.clone(),
            permission_mode: st.permission_mode.clone(),
            effort: st.effort.clone(),
        };
        drop(st);
        self.emit(info);
        Ok(())
    }
}

/// Completes the start of a session: checks that pi opened the expected session, applies
/// the thread's model/effort/mode (only what differs), and publishes commands and settings.
pub async fn handshake(
    session: &PiSession,
    expected_session_id: &str,
    settings: &ThreadSettings,
    permission_mode: &str,
) -> Result<(), AdapterError> {
    let state = session.get_state().await?;
    if state.session_id.as_deref() != Some(expected_session_id) {
        return Err(AdapterError::Protocol(format!(
            "pi opened session {:?}, expected {expected_session_id}",
            state.session_id
        )));
    }
    session.set_current(
        state.model.as_ref().map(PiModel::qualified_id),
        state.thinking_level.clone(),
        Some(permission_mode.to_owned()),
    );
    let wanted = ThreadSettings {
        model: settings.model.clone(),
        effort: settings.effort.clone(),
        permission_mode: Some(permission_mode.to_owned()),
    };
    session.apply(&wanted).await?;
    let commands = session.get_commands().await?;
    session.emit(AdapterEvent::CommandsChanged {
        commands: commands::commands(commands),
    });
    Ok(())
}

fn next_id(shared: &Shared) -> String {
    format!("aas-{}", shared.next_id.fetch_add(1, Ordering::Relaxed))
}

async fn write(shared: &Shared, value: Value) -> Result<(), AdapterError> {
    shared
        .writer
        .send(&value)
        .await
        .map_err(|_| AdapterError::Closed)
}

async fn prompt_payload(input: &TurnInput) -> Result<(String, Vec<Value>), AdapterError> {
    let message = input.to_plain_text();
    let mut images = Vec::new();
    for (path, mime) in input.images() {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| AdapterError::Other(format!("reading image {}: {e}", path.display())))?;
        images.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            "mimeType": mime,
        }));
    }
    Ok((message, images))
}

#[async_trait]
impl SessionControl for PiSession {
    /// Starts a turn: `compact` for `/compact`, else `prompt`. Returns
    /// [`AdapterError::TurnInProgress`] without writing anything while a run pi started by
    /// itself goes on. For a plain prompt (not an extension command) it waits for pi's
    /// decision (see the module docs, "The race with a busy agent"): taken → `Ok`, refused
    /// because a run of pi's own started first → `TurnInProgress`, refused otherwise → `Ok`
    /// with the turn's `TurnCompleted { failed }` emitted.
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        let (compact, extension_command) = {
            let st = self.shared.state.lock();
            st.idle()?;
            let compact = if st.builtin_compact {
                commands::parse_compact(&input)
            } else {
                None
            };
            let command = compact.is_none()
                && commands::is_extension_command(&input.to_plain_text(), &st.extension_commands);
            (compact, command)
        };
        let id = self.next_id();
        let (cmd, kind) = match compact {
            Some(instructions) => {
                let mut cmd = wire::cmd(&id, "compact");
                if let Some(text) = instructions {
                    cmd["customInstructions"] = json!(text);
                }
                (cmd, TurnKind::Compact)
            }
            None => {
                let (message, images) = prompt_payload(&input).await?;
                let mut cmd = json!({ "id": id, "type": "prompt", "message": message });
                if !images.is_empty() {
                    cmd["images"] = Value::Array(images);
                }
                (cmd, TurnKind::Prompt)
            }
        };
        let decision = (kind == TurnKind::Prompt && !extension_command).then(oneshot::channel);
        let decision = {
            let mut st = self.shared.state.lock();
            // pi may have started a run of its own while the input was prepared.
            st.idle()?;
            let mut turn = ActiveTurn::new(kind, Some(id.clone()));
            turn.relay_input = extension_command;
            let rx = decision.map(|(tx, rx)| {
                turn.decision = Some(tx);
                rx
            });
            st.turn = Some(turn);
            rx
        };
        if let Err(e) = self.write(cmd).await {
            // pi's input is closed: the process is ending (its `Exited` follows).
            let mut st = self.shared.state.lock();
            if st
                .turn
                .as_ref()
                .is_some_and(|t| t.command_id.as_deref() == Some(id.as_str()) && !t.started)
            {
                st.turn = None;
            }
            return Err(e);
        }
        match decision {
            None => Ok(()),
            // The reader decides; it drops the decision only when pi's output ends.
            Some(rx) => rx.await.unwrap_or(Err(AdapterError::Closed)),
        }
    }

    async fn steer(&self, input: TurnInput) -> Result<(), AdapterError> {
        if self.shared.state.lock().turn.is_none() {
            return Err(AdapterError::Other("no turn is running".into()));
        }
        let (message, images) = prompt_payload(&input).await?;
        let mut fields = json!({ "message": message });
        if !images.is_empty() {
            fields["images"] = Value::Array(images);
        }
        self.request_ok("steer", fields).await.map(|_| ())
    }

    /// Sends `abort`, for the user's turn and for a run pi started by itself alike. pi's abort
    /// stops what runs at that moment (the agent run, a retry, a compaction); a `prompt` still
    /// in its preflight (auth check, auto-compaction, the extensions' `before_agent_start`)
    /// goes on and starts its run afterwards. An abort sent before the turn's `agent_start` is
    /// therefore sent again when that run starts.
    async fn interrupt(&self) -> Result<(), AdapterError> {
        {
            let mut st = self.shared.state.lock();
            match st.turn.as_mut() {
                None => return Ok(()),
                Some(turn) => {
                    turn.abort_requested = true;
                    if turn.kind == TurnKind::Prompt && !turn.run_started {
                        turn.abort_on_run_start = true;
                    }
                }
            }
        }
        // pi closes the gate's open dialogs with the turn's abort signal; the gate reports each
        // closure (`dialogClosed`) and the reader withdraws the interaction then.
        // Bounded: a pi that no longer reads its stdin must not hold the engine's forced stop.
        let id = self.next_id();
        let grace = self.shared.cfg.stop_grace;
        match tokio::time::timeout(grace, self.write(wire::cmd(&id, "abort"))).await {
            Ok(written) => written,
            Err(_) => Err(AdapterError::Protocol(format!(
                "pi did not read the abort within {grace:?}"
            ))),
        }
    }

    async fn respond(
        &self,
        request_id: &str,
        resolution: &InteractionResolution,
    ) -> Result<(), AdapterError> {
        let pending = self
            .shared
            .state
            .lock()
            .dialogs
            .get(request_id)
            .cloned()
            .ok_or_else(|| AdapterError::UnknownRequest(request_id.to_owned()))?;
        let answer = gate::answer(&pending, resolution).map_err(AdapterError::Other)?;
        {
            let mut st = self.shared.state.lock();
            if st.dialogs.remove(request_id).is_none() {
                // Answered or withdrawn (the gate reported its closure) meanwhile.
                return Err(AdapterError::UnknownRequest(request_id.to_owned()));
            }
            if let PendingDialog::Gate { tool_call_id, .. } = &pending
                && answer.declined
            {
                st.declined.insert(tool_call_id.clone());
            }
        }
        let mut msg = json!({ "type": "extension_ui_response", "id": request_id });
        if let (Value::Object(target), Value::Object(fields)) = (&mut msg, answer.fields) {
            target.extend(fields);
        }
        self.write(msg).await
    }

    async fn apply_settings(
        &self,
        settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError> {
        self.apply(settings).await?;
        Ok(SettingsApplied::Live)
    }

    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        let shared = self.shared.clone();
        self.shared
            .shutdown
            .get_or_init(|| async move {
                let started = tokio::time::Instant::now();
                let running = shared.state.lock().turn.is_some();
                if running {
                    // Bounded: a pi that no longer reads its stdin must not keep the stop from
                    // reaching the termination stage.
                    let id = next_id(&shared);
                    let _ = tokio::time::timeout(
                        shared.cfg.stop_grace,
                        write(&shared, wire::cmd(&id, "abort")),
                    )
                    .await;
                }
                // pi's RPC mode shuts down cleanly when stdin ends. Returns promptly even while
                // another write is stuck on a full pipe.
                shared.writer.close().await;
                let grace = shared.cfg.stop_grace.saturating_sub(started.elapsed());
                shared.link.shutdown(grace, reason).await
            })
            .await
            .clone()
    }
}

struct ReaderTask {
    shared: Arc<Shared>,
    events: mpsc::UnboundedSender<AdapterEvent>,
    mapper: Mapper,
}

enum SettleAction {
    None,
    Complete,
}

impl ReaderTask {
    fn emit(&self, event: AdapterEvent) {
        let _ = self.events.send(event);
    }

    async fn run<R: AsyncRead + Unpin>(
        mut self,
        mut reader: JsonLinesReader<R>,
        mut internal: mpsc::UnboundedReceiver<Internal>,
    ) {
        let label = self.shared.cfg.label.clone();
        loop {
            tokio::select! {
                line = reader.next() => match line {
                    Ok(Some(ReadLine::Json(value))) => self.handle(value).await,
                    Ok(Some(ReadLine::NotJson(text))) => self.emit(AdapterEvent::Native { payload: mapping::non_json_payload(&text) }),
                    Err(LineError::TooLong { max }) => {
                        tracing::warn!(session = %label, max, "pi wrote an oversized line; skipped");
                        self.emit(AdapterEvent::Notice {
                            level: aas_harness::NoticeLevel::Warning,
                            message: format!("pi sent a message larger than {max} bytes; it was skipped"),
                            code: Some("oversizedLine".into()),
                        });
                    }
                    Ok(None) | Err(LineError::Io(_)) => break,
                },
                Some(msg) = internal.recv() => self.handle_internal(msg).await,
            }
        }
        let decision = {
            let mut st = self.shared.state.lock();
            st.closed = true;
            st.waiters.clear();
            st.turn.as_mut().and_then(|t| t.decision.take())
        };
        // pi ended before it decided about the prompt.
        decide(decision, Err(AdapterError::Closed));
        // Drain events queued by control methods before the process ended.
        while let Ok(msg) = internal.try_recv() {
            if let Internal::Emit(ev) = msg {
                self.emit(ev);
            }
        }
        let info = self.shared.link.wait().await;
        self.emit(AdapterEvent::Exited { info });
    }

    async fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Emit(ev) => self.emit(ev),
            Internal::StatsTimeout(id) => {
                let settle_now = {
                    let mut st = self.shared.state.lock();
                    match st.turn.as_mut() {
                        Some(turn) if turn.stats_pending.contains(&id) => {
                            turn.stats_pending.remove(&id);
                            tracing::warn!(session = %self.shared.cfg.label, timeout = ?self.shared.cfg.request_timeout, "pi did not answer get_session_stats; the context is left out");
                            turn.settle_waiting && turn.stats_pending.is_empty()
                        }
                        _ => false,
                    }
                };
                if settle_now {
                    self.complete_turn(None).await;
                }
            }
        }
    }

    /// Asks pi for the session stats (context-window occupancy) of the running turn. The
    /// answer is routed back by id; `agent_settled` waits for it.
    async fn request_stats(&mut self) {
        let id = next_id(&self.shared);
        match self.shared.state.lock().turn.as_mut() {
            Some(turn) => turn.stats_pending.insert(id.clone()),
            None => return,
        };
        if write(&self.shared, wire::cmd(&id, "get_session_stats"))
            .await
            .is_err()
        {
            // pi is gone; the end of its output settles everything.
            if let Some(turn) = self.shared.state.lock().turn.as_mut() {
                turn.stats_pending.remove(&id);
            }
            return;
        }
        let (tx, timeout) = (
            self.shared.internal.clone(),
            self.shared.cfg.request_timeout,
        );
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let _ = tx.send(Internal::StatsTimeout(id));
        });
    }

    /// The run is over (`agent_settled`, or pi said no run started): completes the turn, or,
    /// while `get_session_stats` answers of the turn are outstanding, lets the last of them
    /// complete it. When an agent loop is already running again (a run started from the old
    /// run's end, see the module docs), the turn completes at once and the running run gets a
    /// turn of its own.
    async fn settle(&mut self) {
        enum Settle {
            Now,
            WaitForStats,
            NowThenNewRun,
        }
        let settle = {
            let mut st = self.shared.state.lock();
            let loop_running = st.loop_running;
            match st.turn.as_mut() {
                Some(_) if loop_running => Settle::NowThenNewRun,
                Some(turn) if !turn.stats_pending.is_empty() => {
                    turn.settle_waiting = true;
                    Settle::WaitForStats
                }
                _ => Settle::Now,
            }
        };
        match settle {
            Settle::Now => self.complete_turn(None).await,
            Settle::WaitForStats => {}
            Settle::NowThenNewRun => {
                self.complete_turn(None).await;
                self.open_agent_turn();
            }
        }
    }

    /// Opens the turn of a run pi started by itself (its `agent_start` has arrived).
    fn open_agent_turn(&mut self) {
        self.shared.state.lock().turn = Some(ActiveTurn::agent());
        self.mapper.begin_turn();
        self.emit(AdapterEvent::TurnStarted);
    }

    /// pi works on the prompt that `send` waits for and needs the user or time first (a
    /// dialog of an `input` or `before_agent_start` handler, a compaction before the prompt
    /// runs): `send` returns so that the engine can relay the dialog, and the turn goes on. A
    /// refusal that still follows is handled by `on_response` (`PromptNotTaken`).
    fn stop_waiting_in_preflight(&mut self) {
        let decision = self
            .shared
            .state
            .lock()
            .turn
            .as_mut()
            .filter(|t| t.in_preflight())
            .and_then(|t| t.decision.take());
        decide(decision, Ok(()));
    }

    /// The notice for a user message that opens a run the user did not type (see
    /// [`ActiveTurn::relay_input`]); the run's first assistant message ends its input.
    fn run_input(&mut self, kind: &str, value: &Value) -> Option<AdapterEvent> {
        let mut st = self.shared.state.lock();
        let turn = st.turn.as_mut()?;
        if kind == "message_start"
            && value.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
        {
            turn.input_phase = false;
            return None;
        }
        (turn.relay_input && turn.input_phase)
            .then(|| mapping::run_input_notice(value))
            .flatten()
    }

    /// Starts a `/compact` turn at its first explicit sign of life.
    fn start_compact_turn(&mut self) {
        let start_now = {
            let mut st = self.shared.state.lock();
            match st.turn.as_mut() {
                Some(turn) if turn.kind == TurnKind::Compact && !turn.started => {
                    turn.started = true;
                    true
                }
                _ => false,
            }
        };
        if start_now {
            self.mapper.begin_turn();
            self.emit(AdapterEvent::TurnStarted);
        }
    }

    async fn handle(&mut self, value: Value) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match kind.as_str() {
            "response" => self.on_response(value).await,
            "extension_ui_request" => self.on_ui_request(value),
            "agent_start" => {
                enum Start {
                    /// No turn: a run pi started by itself.
                    AgentRun,
                    /// The turn's run has settled (the turn waits for its context only): this
                    /// is a new run.
                    AfterSettled,
                    /// A run of pi's own before pi answered the waiting prompt.
                    Foreign { first: bool },
                    /// The turn's run (or a retry / continuation of it).
                    TurnRun { start_now: bool, abort_again: bool },
                }
                let start = {
                    let mut st = self.shared.state.lock();
                    st.loop_running = true;
                    match st.turn.as_mut() {
                        None => Start::AgentRun,
                        Some(turn) if turn.settle_waiting => Start::AfterSettled,
                        // pi answers a plain prompt before its run starts: whatever starts
                        // before the answer is not this prompt's run.
                        Some(turn)
                            if turn.kind == TurnKind::Prompt
                                && !turn.accepted
                                && turn.waiting() =>
                        {
                            let first = !turn.started;
                            if turn.foreign != Foreign::Running {
                                turn.input_phase = true;
                            }
                            turn.started = true;
                            turn.foreign = Foreign::Running;
                            turn.relay_input = true;
                            Start::Foreign { first }
                        }
                        Some(turn) => {
                            let start_now = !turn.started;
                            if !turn.run_started {
                                turn.input_phase = true;
                            }
                            turn.started = true;
                            turn.run_started = true;
                            Start::TurnRun {
                                start_now,
                                abort_again: std::mem::take(&mut turn.abort_on_run_start),
                            }
                        }
                    }
                };
                match start {
                    Start::AgentRun => self.open_agent_turn(),
                    Start::AfterSettled => {
                        self.complete_turn(None).await;
                        self.open_agent_turn();
                    }
                    Start::Foreign { first } => {
                        if first {
                            self.mapper.begin_turn();
                            self.emit(AdapterEvent::TurnStarted);
                        }
                    }
                    Start::TurnRun {
                        start_now,
                        abort_again,
                    } => {
                        if start_now {
                            self.mapper.begin_turn();
                            self.emit(AdapterEvent::TurnStarted);
                        }
                        if abort_again {
                            // The abort arrived during the prompt's preflight and did not stop
                            // this run.
                            let id = next_id(&self.shared);
                            if write(&self.shared, wire::cmd(&id, "abort")).await.is_err() {
                                // pi is gone; the end of its output settles the turn.
                                tracing::debug!(session = %self.shared.cfg.label, "could not repeat the abort: pi's input is closed");
                            }
                        }
                    }
                }
            }
            "agent_end" => {
                self.shared.state.lock().loop_running = false;
            }
            "agent_settled" => {
                let action = {
                    let mut st = self.shared.state.lock();
                    let loop_running = st.loop_running;
                    match st.turn.as_mut() {
                        // A compaction ends with the `compact` response, not with a run.
                        Some(turn) if turn.kind == TurnKind::Compact => SettleAction::None,
                        // A run of pi's own settled before pi answered the prompt: its end
                        // waits for the answer (see `on_response`).
                        Some(turn) if !turn.accepted && turn.foreign != Foreign::None => {
                            turn.foreign = if loop_running {
                                Foreign::Running
                            } else {
                                Foreign::Settled
                            };
                            SettleAction::None
                        }
                        Some(turn) if turn.accepted => SettleAction::Complete,
                        Some(turn) => {
                            turn.settled_early = true;
                            SettleAction::None
                        }
                        None => SettleAction::None,
                    }
                };
                if let SettleAction::Complete = action {
                    self.settle().await;
                }
            }
            "queue_update" => {
                let steering = value
                    .get("steering")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                self.shared.state.lock().steering = steering;
            }
            _ => {
                let in_turn = self
                    .shared
                    .state
                    .lock()
                    .turn
                    .as_ref()
                    .is_some_and(|t| t.started);
                if mapping::is_turn_scoped(&kind) && !in_turn {
                    self.emit(AdapterEvent::Native { payload: value });
                    return;
                }
                if kind == "thinking_level_changed" {
                    let level = value
                        .get("level")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.shared.state.lock().effort = level;
                }
                if kind == "compaction_start" {
                    self.start_compact_turn();
                    // A compaction before the waiting prompt runs (pi's preflight).
                    self.stop_waiting_in_preflight();
                }
                if let Some(notice) = mapping::custom_message_notice(&value) {
                    self.emit(notice);
                }
                if let Some(notice) = self.run_input(&kind, &value) {
                    self.emit(notice);
                }
                let shared = self.shared.clone();
                let declined = move |id: &str| shared.state.lock().declined.contains(id);
                for event in self.mapper.map(&value, &declined) {
                    self.emit(event);
                }
                let assistant_done = kind == "message_end"
                    && value.pointer("/message/role").and_then(Value::as_str) == Some("assistant");
                if assistant_done && in_turn {
                    self.request_stats().await;
                }
            }
        }
    }

    async fn on_response(&mut self, value: Value) {
        let resp: Response = match serde_json::from_value(value.clone()) {
            Ok(r) => r,
            Err(_) => {
                self.emit(AdapterEvent::Native { payload: value });
                return;
            }
        };
        let id = resp.id.clone();

        enum Route {
            Waiter(oneshot::Sender<Response>),
            PromptRejected {
                decision: Option<Decision>,
            },
            PromptAccepted {
                start_now: bool,
                settled_early: bool,
                decision: Option<Decision>,
            },
            /// pi refused the waiting prompt because a run of its own had started: that run
            /// is a turn of its own (`settled`: it has ended already).
            RefusedForOwnRun {
                settled: bool,
                decision: Option<Decision>,
            },
            /// pi refused the prompt after a run had started that the turn shows already
            /// (`send` no longer waited): the run stays the turn's, which ends with it.
            PromptNotTaken {
                settled: bool,
            },
            Probe,
            CompactDone {
                start_now: bool,
                abort_requested: bool,
            },
            Stats {
                settle_now: bool,
            },
            Other,
        }
        let is_command = |t: &ActiveTurn, kind: TurnKind| {
            t.kind == kind && id.is_some() && t.command_id.as_ref() == id.as_ref()
        };
        let route = {
            let mut st = self.shared.state.lock();
            if let Some(tx) = id.as_ref().and_then(|id| st.waiters.remove(id)) {
                Route::Waiter(tx)
            } else if let Some(turn) = st
                .turn
                .as_mut()
                .filter(|t| is_command(t, TurnKind::Compact))
            {
                let start_now = !turn.started;
                turn.started = true;
                Route::CompactDone {
                    start_now,
                    abort_requested: turn.abort_requested,
                }
            } else if let Some(turn) = st
                .turn
                .as_mut()
                .filter(|t| id.as_ref().is_some_and(|id| t.stats_pending.contains(id)))
            {
                if let Some(id) = &id {
                    turn.stats_pending.remove(id);
                }
                Route::Stats {
                    settle_now: turn.settle_waiting && turn.stats_pending.is_empty(),
                }
            } else if let Some(turn) = st.turn.as_mut().filter(|t| is_command(t, TurnKind::Prompt))
            {
                if resp.success {
                    turn.accepted = true;
                    let start_now = !turn.started;
                    turn.started = true;
                    if turn.foreign != Foreign::None {
                        // pi took the prompt although a run of its own had started first: the
                        // runs seen so far are part of this turn, and a run still going on is
                        // the one the prompt joined.
                        turn.run_started = turn.foreign == Foreign::Running;
                        turn.foreign = Foreign::None;
                        turn.relay_input = false;
                    }
                    Route::PromptAccepted {
                        start_now,
                        settled_early: turn.settled_early,
                        decision: turn.decision.take(),
                    }
                } else if turn.foreign != Foreign::None && turn.waiting() {
                    let settled = turn.foreign == Foreign::Settled;
                    let decision = turn.decision.take();
                    let mut own = ActiveTurn::agent();
                    own.stats_pending = std::mem::take(&mut turn.stats_pending);
                    own.input_phase = turn.input_phase;
                    *turn = own;
                    Route::RefusedForOwnRun { settled, decision }
                } else if turn.started {
                    let settled = turn.settled_early || turn.foreign == Foreign::Settled;
                    turn.accepted = true;
                    turn.foreign = Foreign::None;
                    turn.relay_input = true;
                    // The waiting `send` gave up (its caller's deadline): nothing to decide.
                    turn.decision = None;
                    Route::PromptNotTaken { settled }
                } else {
                    let decision = st.turn.take().and_then(|t| t.decision);
                    Route::PromptRejected { decision }
                }
            } else if let Some(turn) = st
                .turn
                .as_mut()
                .filter(|t| id.is_some() && t.probe_id == id)
            {
                turn.probe_id = None;
                Route::Probe
            } else {
                Route::Other
            }
        };

        match route {
            Route::Waiter(tx) => {
                let _ = tx.send(resp);
            }
            Route::PromptRejected { decision } => {
                self.emit(AdapterEvent::TurnCompleted {
                    trigger: None,
                    status: TurnStatus::Failed,
                    usage: None,
                    error: Some(TurnError {
                        message: resp.error_message(),
                        kind: "harnessError".into(),
                    }),
                });
                // The refusal is the turn's outcome, reported by the event above.
                decide(decision, Ok(()));
            }
            Route::RefusedForOwnRun { settled, decision } => {
                // Its `TurnStarted` went out at its `agent_start`.
                tracing::info!(session = %self.shared.cfg.label, "pi refused the prompt: a run of its own had started; the input waits for that run");
                if settled {
                    self.settle().await;
                }
                decide(decision, Err(AdapterError::TurnInProgress));
            }
            Route::PromptNotTaken { settled } => {
                self.emit(AdapterEvent::Notice {
                    level: aas_harness::NoticeLevel::Warning,
                    message: format!("pi did not take this message: {}", resp.error_message()),
                    code: Some("promptNotTaken".into()),
                });
                if settled {
                    self.settle().await;
                }
            }
            Route::PromptAccepted {
                start_now,
                settled_early,
                decision,
            } => {
                decide(decision, Ok(()));
                if start_now {
                    self.mapper.begin_turn();
                    self.emit(AdapterEvent::TurnStarted);
                }
                if settled_early {
                    self.settle().await;
                    return;
                }
                let probe_id = next_id(&self.shared);
                if let Some(turn) = self.shared.state.lock().turn.as_mut() {
                    turn.probe_id = Some(probe_id.clone());
                }
                if let Err(e) = write(&self.shared, wire::cmd(&probe_id, "get_state")).await {
                    // pi's stdin is closed: the process is ending, and its exit ends the turn.
                    tracing::warn!(session = %self.shared.cfg.label, error = %e, "could not ask pi whether the accepted prompt runs");
                }
            }
            Route::Probe => {
                // `isStreaming` missing means we cannot tell: keep waiting for agent_settled.
                let streaming = resp
                    .data
                    .as_ref()
                    .and_then(|d| d.get("isStreaming"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if resp.success && !streaming {
                    self.settle().await;
                }
            }
            Route::CompactDone {
                start_now,
                abort_requested,
            } => {
                if start_now {
                    self.mapper.begin_turn();
                    self.emit(AdapterEvent::TurnStarted);
                }
                let outcome = if resp.success {
                    let usage = resp
                        .data
                        .as_ref()
                        .and_then(|d| d.get("usage"))
                        .and_then(|u| serde_json::from_value::<wire::PiUsage>(u.clone()).ok())
                        .map(wire::PiUsage::to_protocol);
                    (TurnStatus::Completed, usage, None)
                } else if abort_requested {
                    (TurnStatus::Interrupted, None, None)
                } else {
                    let error = TurnError {
                        message: resp.error_message(),
                        kind: "harnessError".into(),
                    };
                    (TurnStatus::Failed, None, Some(error))
                };
                self.complete_turn(Some(outcome)).await;
            }
            Route::Stats { settle_now } => {
                match resp
                    .data
                    .as_ref()
                    .filter(|_| resp.success)
                    .and_then(wire::context_usage)
                {
                    Some(context) => {
                        self.mapper.set_context(context);
                        if let Some(usage) = self.mapper.turn_usage() {
                            self.emit(AdapterEvent::TurnUsage { usage });
                        }
                    }
                    None if !resp.success => {
                        tracing::warn!(session = %self.shared.cfg.label, error = %resp.error_message(), "get_session_stats failed");
                    }
                    // pi knows no context usage right now (no model, or just compacted).
                    None => {}
                }
                if settle_now {
                    self.complete_turn(None).await;
                }
            }
            Route::Other => {
                if !resp.success {
                    tracing::warn!(session = %self.shared.cfg.label, command = %resp.command, error = %resp.error_message(), "pi rejected a command");
                    if resp.command == "abort" {
                        self.emit(AdapterEvent::Notice {
                            level: aas_harness::NoticeLevel::Warning,
                            message: format!(
                                "pi could not abort the turn: {}",
                                resp.error_message()
                            ),
                            code: Some("abortFailed".into()),
                        });
                    }
                }
            }
        }
    }

    fn on_ui_request(&mut self, value: Value) {
        let Some(id) = value.get("id").and_then(Value::as_str).map(str::to_owned) else {
            self.emit(AdapterEvent::Native { payload: value });
            return;
        };
        match gate::interpret(&value) {
            Dialog::Ask {
                request,
                pending,
                tool_call_id,
            } => {
                // Pending until answered or expired (also across turns: it belongs to the
                // running turn, or to the thread when none runs), withdrawn only by the
                // gate's report or the end of the process.
                self.shared.state.lock().dialogs.insert(id.clone(), pending);
                self.emit(AdapterEvent::InteractionRequested {
                    background_key: None,
                    request_id: id,
                    request,
                    item_key: tool_call_id.map(|t| mapping::tool_key(&t)),
                });
                // A dialog of the waiting prompt's preflight (an `input` or
                // `before_agent_start` handler) needs the user: `send` returns.
                self.stop_waiting_in_preflight();
            }
            Dialog::GateClosed {
                tool_call_id,
                aborted,
            } => {
                // The gate's dialog closed without our answer being used (pi aborted it), or
                // after we answered (then it is gone already).
                let withdrawn = {
                    let mut st = self.shared.state.lock();
                    let found = st
                        .dialogs
                        .iter()
                        .find(|(_, d)| matches!(d, PendingDialog::Gate { tool_call_id: t, .. } if *t == tool_call_id))
                        .map(|(k, _)| k.clone());
                    if let Some(key) = &found {
                        st.dialogs.remove(key);
                        // The gate blocks a tool whose dialog closed without an answer.
                        st.declined.insert(tool_call_id.clone());
                    }
                    found
                };
                if let Some(request_id) = withdrawn {
                    tracing::debug!(session = %self.shared.cfg.label, %tool_call_id, aborted, "the gate's dialog closed without an answer");
                    self.emit(AdapterEvent::InteractionWithdrawn { request_id });
                }
            }
            Dialog::Notify { level, message } => self.emit(AdapterEvent::Notice {
                level,
                message,
                code: Some("extensionNotify".into()),
            }),
            Dialog::Ignored => {}
            Dialog::Unknown => self.emit(AdapterEvent::Native { payload: value }),
        }
    }

    /// Ends the running turn. `outcome` overrides the outcome derived from the run (used by
    /// `/compact`, whose result is its response).
    async fn complete_turn(
        &mut self,
        outcome: Option<(TurnStatus, Option<aas_harness::Usage>, Option<TurnError>)>,
    ) {
        // Open dialogs stay: pi still waits for their answers. The engine expires those that
        // belonged to this turn and answers them through `expire_request` (a dismissal).
        let taken = {
            let mut st = self.shared.state.lock();
            st.turn
                .take()
                .map(|turn| (turn.abort_requested, std::mem::take(&mut st.steering)))
        };
        let Some((abort_requested, steering)) = taken else {
            return;
        };
        if !steering.is_empty() {
            // pi would deliver these with the next prompt; drop them so they do not leak into
            // an unrelated turn, and tell the user.
            let id = next_id(&self.shared);
            if let Err(e) = write(&self.shared, wire::cmd(&id, "clear_queue")).await {
                // pi's stdin is closed: the process is ending, so nothing it queued can leak
                // into a later turn.
                tracing::warn!(session = %self.shared.cfg.label, error = %e, "could not clear pi's queue of undelivered messages");
            }
            self.emit(AdapterEvent::Notice {
                level: aas_harness::NoticeLevel::Warning,
                message: format!(
                    "The turn ended before this message could be delivered: {}",
                    steering.join(" / ")
                ),
                code: Some("steerNotDelivered".into()),
            });
        }
        let (status, usage, error) = match outcome {
            Some(outcome) => outcome,
            None => {
                let (status, error) = self.mapper.outcome(abort_requested);
                (status, self.mapper.turn_usage(), error)
            }
        };
        self.emit(AdapterEvent::TurnCompleted {
            trigger: None,
            status,
            usage,
            error,
        });
    }
}
