//! Protocol core of one `pi --mode rpc` process.
//!
//! Generic over the byte streams and the process link so recorded transcripts can be
//! replayed over `tokio::io::duplex` without spawning anything.
//!
//! # Turn lifecycle (explicit signals only)
//!
//! * `send` writes `prompt` and marks a turn active.
//! * The `prompt` response (`success`) or the first `agent_start` → `TurnStarted`.
//!   A failed `prompt` response → `TurnCompleted { failed }` (pi rejected it before starting).
//! * `agent_settled` (pi: "no retry, compaction retry or queued continuation remains") →
//!   `TurnCompleted`. `agent_end` is not used: it may be followed by retries/compaction.
//! * Prompts that pi handles without an agent run (extension commands, `input` handlers)
//!   never produce `agent_settled`. After the `prompt` response the session therefore asks
//!   `get_state`: pi sets `isStreaming` synchronously right after it answers a prompt that
//!   starts a run, so `isStreaming == false` in that answer means "no run" → the turn
//!   completes immediately. (Derived from `agent-session.js`/`rpc-mode.js` of pi 0.85.1.)
//! * `/compact [instructions]` (see [`crate::commands`]) is sent as the RPC command `compact`
//!   and runs as a turn of its own: `compaction_start` or the response starts it, the
//!   `compact` response ends it.
//!
//! # Context-window occupancy
//!
//! After every finished assistant message the session asks `get_session_stats` and relays
//! its `contextUsage` (tokens and window, as pi computes them for its own footer and
//! compaction) with the turn's usage. `agent_settled` completes the turn only once the
//! answers to those requests are in, so the last one belongs to the turn.
//!
//! # Dialogs
//!
//! Dialogs are withdrawn only on explicit signals: the gate's `dialogClosed` report (pi closed
//! the gate's dialog because the turn was aborted), the end of the turn, or the end of the
//! process. pi's RPC mode closes a dialog that carries a `timeout` by itself without telling
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

/// What the turn's command was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnKind {
    Prompt,
    /// `/compact`, sent as the RPC command `compact`.
    Compact,
}

#[derive(Debug)]
struct ActiveTurn {
    /// Id of the `prompt` (or `compact`) command that began the turn.
    prompt_id: String,
    kind: TurnKind,
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
}

impl ActiveTurn {
    fn new(prompt_id: String, kind: TurnKind) -> Self {
        Self {
            prompt_id,
            kind,
            started: false,
            accepted: false,
            settled_early: false,
            probe_id: None,
            abort_requested: false,
            run_started: false,
            abort_on_run_start: false,
            stats_pending: HashSet::new(),
            settle_waiting: false,
        }
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
        self.shared.state.lock().builtin_compact = commands::builtin_compact(&commands);
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
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        let compact = {
            let st = self.shared.state.lock();
            if st.closed {
                return Err(AdapterError::Closed);
            }
            if st.turn.is_some() {
                return Err(AdapterError::Other("a turn is already running".into()));
            }
            if st.builtin_compact {
                commands::parse_compact(&input)
            } else {
                None
            }
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
        self.shared.state.lock().turn = Some(ActiveTurn::new(id, kind));
        if let Err(e) = self.write(cmd).await {
            self.shared.state.lock().turn = None;
            return Err(e);
        }
        Ok(())
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

    /// Sends `abort`. pi's abort stops what runs at that moment (the agent run, a retry, a
    /// compaction); a `prompt` still in its preflight (auth check, auto-compaction, the
    /// extensions' `before_agent_start`) goes on and starts its run afterwards. An abort sent
    /// before the turn's `agent_start` is therefore sent again when that run starts.
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
                // Withdrawn (closed by pi, turn ended) between the lookup and now.
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
        {
            let mut st = self.shared.state.lock();
            st.closed = true;
            st.waiters.clear();
        }
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
    /// complete it.
    async fn settle(&mut self) {
        let wait = match self.shared.state.lock().turn.as_mut() {
            Some(turn) if !turn.stats_pending.is_empty() => {
                turn.settle_waiting = true;
                true
            }
            _ => false,
        };
        if !wait {
            self.complete_turn(None).await;
        }
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
                let (start_now, abort_again) = {
                    let mut st = self.shared.state.lock();
                    match st.turn.as_mut() {
                        Some(turn) => {
                            let start_now = !turn.started;
                            turn.started = true;
                            turn.run_started = true;
                            (start_now, std::mem::take(&mut turn.abort_on_run_start))
                        }
                        None => {
                            drop(st);
                            // A run we did not start (e.g. triggered by an extension later).
                            self.emit(AdapterEvent::Native { payload: value });
                            return;
                        }
                    }
                };
                if start_now {
                    self.mapper.begin_turn();
                    self.emit(AdapterEvent::TurnStarted);
                }
                if abort_again {
                    // The abort arrived during the prompt's preflight and did not stop this run.
                    let id = next_id(&self.shared);
                    if write(&self.shared, wire::cmd(&id, "abort")).await.is_err() {
                        // pi is gone; the end of its output settles the turn.
                        tracing::debug!(session = %self.shared.cfg.label, "could not repeat the abort: pi's input is closed");
                    }
                }
            }
            "agent_settled" => {
                let action = {
                    let mut st = self.shared.state.lock();
                    match st.turn.as_mut() {
                        // A compaction ends with the `compact` response, not with a run.
                        Some(turn) if turn.kind == TurnKind::Compact => SettleAction::None,
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
                }
                if let Some(notice) = mapping::custom_message_notice(&value) {
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
            PromptRejected,
            PromptAccepted {
                start_now: bool,
                settled_early: bool,
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
        let route = {
            let mut st = self.shared.state.lock();
            if let Some(tx) = id.as_ref().and_then(|id| st.waiters.remove(id)) {
                Route::Waiter(tx)
            } else if let Some(turn) = st
                .turn
                .as_mut()
                .filter(|t| t.kind == TurnKind::Compact && id.as_ref() == Some(&t.prompt_id))
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
            } else if let Some(turn) = st
                .turn
                .as_mut()
                .filter(|t| id.as_ref() == Some(&t.prompt_id))
            {
                if resp.success {
                    turn.accepted = true;
                    let start_now = !turn.started;
                    turn.started = true;
                    Route::PromptAccepted {
                        start_now,
                        settled_early: turn.settled_early,
                    }
                } else {
                    st.turn = None;
                    Route::PromptRejected
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
            Route::PromptRejected => self.emit(AdapterEvent::TurnCompleted {
                status: TurnStatus::Failed,
                usage: None,
                error: Some(TurnError {
                    message: resp.error_message(),
                    kind: "harnessError".into(),
                }),
            }),
            Route::PromptAccepted {
                start_now,
                settled_early,
            } => {
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
                let _ = write(&self.shared, wire::cmd(&probe_id, "get_state")).await;
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
                self.shared.state.lock().dialogs.insert(id.clone(), pending);
                self.emit(AdapterEvent::InteractionRequested {
                    request_id: id,
                    request,
                    item_key: tool_call_id.map(|t| mapping::tool_key(&t)),
                });
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
        let taken = {
            let mut st = self.shared.state.lock();
            st.turn.take().map(|turn| {
                let dialogs: Vec<String> = st.dialogs.drain().map(|(k, _)| k).collect();
                (
                    turn.abort_requested,
                    dialogs,
                    std::mem::take(&mut st.steering),
                )
            })
        };
        let Some((abort_requested, dialogs, steering)) = taken else {
            return;
        };
        let mut dialogs = dialogs;
        dialogs.sort();
        for request_id in dialogs {
            self.emit(AdapterEvent::InteractionWithdrawn { request_id });
        }
        if !steering.is_empty() {
            // pi would deliver these with the next prompt; drop them so they do not leak into
            // an unrelated turn, and tell the user.
            let id = next_id(&self.shared);
            let _ = write(&self.shared, wire::cmd(&id, "clear_queue")).await;
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
            status,
            usage,
            error,
        });
    }
}
