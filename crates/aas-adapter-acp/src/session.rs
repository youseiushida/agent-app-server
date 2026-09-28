//! One ACP session over one agent process.
//!
//! The protocol core is generic over the byte streams and a [`ProcessLink`], so tests replay
//! recorded transcripts over in-memory pipes without spawning anything.
//!
//! A single *router* task owns all session state and is the only emitter of
//! [`AdapterEvent`]s. Control calls reach it through a command channel. Responses to our own
//! requests are resolved by the JSON-RPC peer directly, so before handling any command the
//! router drains every notification already queued: the reader queues a turn's
//! `session/update`s before it resolves the `session/prompt` response, therefore draining
//! guarantees that a turn's items are emitted before its `TurnCompleted` (and that updates
//! replayed by `session/load` are classified as history before the session goes live).
//!
//! Requests of the agent (permissions, elicitations) are never left unanswered: one that
//! arrives while no turn runs is shown to the user (it belongs to the thread, or to the
//! background task it names), and one the engine expires is answered `cancelled`
//! (`SessionControl::expire_request`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aas_harness::protocol::{
    ContextUsage, ExpireReason, InteractionResolution, NoticeLevel, TurnError, TurnStatus,
};
use aas_harness::{
    AdapterError, AdapterEvent, AdapterPolicy, ExitInfo, NativeHistory, SessionControl,
    SessionHandle, SettingsApplied, StartGuard, StopReason, ThreadSettings, TurnInput,
    TurnInputPart,
};
use aas_stdio::{Incoming, IncomingRequest, RpcCallError, RpcPeer, RpcPeerConfig, RpcWireError};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OnceCell, mpsc, oneshot};

use crate::cache::OptionsCache;
use crate::cognition::{self, Background, Routed};
use crate::elicitation::{self, Elicitation, Form};
use crate::history::HistoryBuilder;
use crate::mapping::{self, SessionOptions, SettingKind};
use crate::tracker::{Emit, Tracker};
use crate::wire::{
    self, ConfigOption, InitializeResponse, PromptCapabilities, RequestPermissionParams,
    SessionSetupResponse, SessionUpdate,
};

/// The process behind a session.
#[async_trait]
pub trait ProcessLink: Send + Sync + 'static {
    /// Resolves when the process tree is gone.
    async fn wait(&self) -> ExitInfo;
    /// Staged stop (the caller already closed stdin).
    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo;
}

#[async_trait]
impl ProcessLink for aas_supervisor::ChildHandle {
    async fn wait(&self) -> ExitInfo {
        aas_supervisor::ChildHandle::wait(self).await
    }
    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
        aas_supervisor::ChildHandle::shutdown(self, grace, reason).await
    }
}

/// Options from `config.toml` (`[harness.options]`), documented in docs/adapters/acp.md.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdapterOptions {
    /// Call `authenticate` with this method id right after `initialize`.
    pub auth_method: Option<String>,
    /// Text appended to "authentication required" errors (e.g. `run \`devin auth login\``).
    pub auth_hint: Option<String>,
    /// Forward `_`-prefixed extension notifications as `Native` events.
    pub forward_extension_notifications: bool,
    /// MCP servers passed verbatim to `session/new` / `load` / `resume` / `fork`.
    pub mcp_servers: Vec<Value>,
}

/// How to set up the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupMode {
    New,
    /// Continue a session (`session/resume`, else `session/load` with the replay discarded).
    Resume(String),
    /// Branch a session (`session/fork`, unstable).
    Fork(String),
    /// `session/load` only to collect the replayed history.
    History(String),
}

pub struct LaunchParams<R, W> {
    pub reader: R,
    pub writer: W,
    pub link: Arc<dyn ProcessLink>,
    pub label: String,
    pub agent_name: String,
    pub cwd: PathBuf,
    pub mode: SetupMode,
    pub settings: ThreadSettings,
    pub options: AdapterOptions,
    pub policy: AdapterPolicy,
    pub cache: OptionsCache,
}

pub struct Launched {
    pub handle: SessionHandle,
    pub history: Option<NativeHistory>,
}

pub fn client_init() -> wire::InitializeRequest {
    wire::InitializeRequest {
        protocol_version: wire::PROTOCOL_VERSION,
        client_capabilities: wire::ClientCapabilities {
            meta: Some(cognition::client_meta()),
            ..wire::ClientCapabilities::default()
        },
        client_info: wire::Implementation {
            name: "agent-app-server".into(),
            title: Some("agent-app-server".into()),
            version: env!("CARGO_PKG_VERSION").into(),
        },
    }
}

pub fn peer_config(label: &str, policy: &AdapterPolicy) -> RpcPeerConfig {
    RpcPeerConfig {
        emit_jsonrpc_field: true,
        max_line_bytes: policy.max_line_bytes,
        label: label.to_owned(),
    }
}

/// Maps a failed setup request to an adapter error.
pub fn setup_error(
    method: &str,
    err: RpcCallError,
    agent: &str,
    options: &AdapterOptions,
    init: Option<&InitializeResponse>,
) -> AdapterError {
    match err {
        RpcCallError::Rpc(e) if e.code == wire::AUTH_REQUIRED => {
            AdapterError::Unavailable(auth_message(agent, options, init))
        }
        RpcCallError::Rpc(e) => AdapterError::Harness(format!("{method}: {}", e.message)),
        RpcCallError::Timeout(t) => {
            AdapterError::Protocol(format!("{method} did not answer within {t:?}"))
        }
        RpcCallError::Closed => AdapterError::Spawn(format!("{agent} exited during {method}")),
        RpcCallError::Io(e) => AdapterError::Spawn(format!("{method}: {e}")),
        RpcCallError::Decode(e) => AdapterError::Protocol(format!("{method}: {e}")),
    }
}

/// "authentication required" message listing the agent's methods and the configured hint.
pub fn auth_message(
    agent: &str,
    options: &AdapterOptions,
    init: Option<&InitializeResponse>,
) -> String {
    let mut msg = format!("{agent} requires authentication");
    if let Some(init) = init {
        let methods: Vec<String> = init
            .auth_methods
            .iter()
            .map(|m| {
                let mut s = m.name.clone();
                if m.kind.as_deref() == Some("terminal") && !m.args.is_empty() {
                    s.push_str(&format!(" (run the agent with `{}`)", m.args.join(" ")));
                } else if let Some(d) = &m.description {
                    s.push_str(&format!(" — {d}"));
                }
                s
            })
            .collect();
        if !methods.is_empty() {
            msg.push_str(&format!("; available methods: {}", methods.join(", ")));
        }
    }
    if let Some(hint) = &options.auth_hint {
        msg.push_str(&format!(". {hint}"));
    }
    msg
}

/// Starts the peer, runs the handshake and returns the live session.
pub async fn launch<R, W>(p: LaunchParams<R, W>) -> Result<Launched, AdapterError>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let LaunchParams {
        reader,
        writer,
        link,
        label,
        agent_name,
        cwd,
        mode,
        settings,
        options,
        policy,
        cache,
    } = p;
    let (peer, incoming) = RpcPeer::start(reader, writer, peer_config(&label, &policy));
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let collect = matches!(mode, SetupMode::History(_));
    let router = Router {
        peer: peer.clone(),
        link: link.clone(),
        tx: events_tx,
        self_tx: cmd_tx.clone(),
        tracker: Tracker::new(),
        background: None,
        history: collect.then(|| (Tracker::new(), HistoryBuilder::new())),
        live: false,
        session_id: None,
        options: SessionOptions::default(),
        last_info: None,
        last_session_info: None,
        pending: HashMap::new(),
        deferred: Vec::new(),
        perm_seq: 0,
        turn: None,
        turn_lost: false,
        cost_usd: None,
        cost_baseline_zero: matches!(mode, SetupMode::New | SetupMode::Fork(_)),
        forward_ext: options.forward_extension_notifications,
        request_timeout: policy.handshake_timeout,
        cache,
        label,
    };
    tokio::spawn(router.run(incoming, cmd_rx));
    // The router keeps the process handle and the writer alive: if this future is dropped
    // during the handshake, the guard stops the process.
    let guard = {
        let (peer, link, grace) = (peer.clone(), link.clone(), policy.stop_grace);
        StartGuard::new(move |reason| async move {
            peer.close_writer().await;
            link.shutdown(grace, reason).await
        })
    };

    let hs = Handshake {
        agent_name: &agent_name,
        cwd: &cwd,
        mode: &mode,
        settings: &settings,
        options: &options,
        policy: &policy,
    };
    let result = handshake(&peer, &cmd_tx, &hs).await;
    match result {
        Ok((init, session_id, history)) => {
            guard.disarm();
            let control = Arc::new(AcpControl {
                cmd_tx,
                peer,
                link,
                grace: policy.stop_grace,
                prompt_caps: init.agent_capabilities.prompt_capabilities.clone(),
                exit: OnceCell::new(),
            });
            Ok(Launched {
                handle: SessionHandle {
                    native_session_id: Some(session_id),
                    control,
                    events: events_rx,
                },
                history,
            })
        }
        Err(e) => {
            guard.stop(StopReason::Shutdown).await;
            Err(e)
        }
    }
}

struct Handshake<'a> {
    agent_name: &'a str,
    cwd: &'a Path,
    mode: &'a SetupMode,
    settings: &'a ThreadSettings,
    options: &'a AdapterOptions,
    policy: &'a AdapterPolicy,
}

async fn handshake(
    peer: &RpcPeer,
    cmd_tx: &mpsc::UnboundedSender<Cmd>,
    p: &Handshake<'_>,
) -> Result<(InitializeResponse, String, Option<NativeHistory>), AdapterError> {
    let timeout = p.policy.handshake_timeout;
    let init: InitializeResponse = peer
        .request_timeout("initialize", client_init(), timeout)
        .await
        .map_err(|e| setup_error("initialize", e, p.agent_name, p.options, None))?;
    if init.protocol_version != wire::PROTOCOL_VERSION {
        return Err(AdapterError::Unavailable(format!(
            "{} speaks ACP protocol version {}; this adapter supports version {}",
            p.agent_name,
            init.protocol_version,
            wire::PROTOCOL_VERSION
        )));
    }
    if let Some(method) = &p.options.auth_method {
        peer.request_timeout::<_, Value>("authenticate", json!({ "methodId": method }), timeout)
            .await
            .map_err(|e| setup_error("authenticate", e, p.agent_name, p.options, Some(&init)))?;
    }
    // The router must know the confirmed extensions before the session exists: `session/load`
    // replays updates right away.
    let (reply, rx) = oneshot::channel();
    cmd_tx
        .send(Cmd::Initialized {
            background: cognition::confirmed(&init),
            reply,
        })
        .map_err(|_| AdapterError::Closed)?;
    rx.await.map_err(|_| AdapterError::Closed)?;

    let caps = &init.agent_capabilities;
    let cwd = path_string(p.cwd);
    let mcp = p.options.mcp_servers.clone();
    let existing = |id: &str| wire::ExistingSessionRequest {
        session_id: id.to_owned(),
        cwd: cwd.clone(),
        mcp_servers: mcp.clone(),
    };
    let (method, params, known_id): (&str, Value, Option<String>) = match &p.mode {
        SetupMode::New => (
            "session/new",
            serde_json::to_value(wire::NewSessionRequest {
                cwd: cwd.clone(),
                mcp_servers: mcp.clone(),
            })
            .expect("serializes"),
            None,
        ),
        SetupMode::Resume(id) if caps.session_capabilities.resume() => (
            "session/resume",
            serde_json::to_value(existing(id)).expect("serializes"),
            Some(id.clone()),
        ),
        SetupMode::Resume(id) | SetupMode::History(id) if caps.load_session => (
            "session/load",
            serde_json::to_value(existing(id)).expect("serializes"),
            Some(id.clone()),
        ),
        SetupMode::Resume(_) => {
            return Err(AdapterError::Unsupported(
                "resume (agent supports neither session/resume nor session/load)",
            ));
        }
        SetupMode::History(_) => {
            return Err(AdapterError::Unsupported(
                "history (agent does not support session/load)",
            ));
        }
        SetupMode::Fork(id) if caps.session_capabilities.fork() => (
            "session/fork",
            serde_json::to_value(existing(id)).expect("serializes"),
            None,
        ),
        SetupMode::Fork(_) => return Err(AdapterError::Unsupported("fork")),
    };
    let setup: SessionSetupResponse = peer
        .request_timeout(method, params, timeout)
        .await
        .map_err(|e| setup_error(method, e, p.agent_name, p.options, Some(&init)))?;
    let session_id = match known_id.or_else(|| setup.session_id.clone()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            return Err(AdapterError::Protocol(format!(
                "{method} returned no sessionId"
            )));
        }
    };

    let (reply, rx) = oneshot::channel();
    cmd_tx
        .send(Cmd::SetupDone {
            session_id: session_id.clone(),
            setup,
            is_new: *p.mode == SetupMode::New,
            reply,
        })
        .map_err(|_| AdapterError::Closed)?;
    let history = rx.await.map_err(|_| AdapterError::Closed)?;

    if !matches!(p.mode, SetupMode::History(_)) {
        let (reply, rx) = oneshot::channel();
        cmd_tx
            .send(Cmd::ApplySettings {
                settings: p.settings.clone(),
                strict: false,
                reply,
            })
            .map_err(|_| AdapterError::Closed)?;
        rx.await.map_err(|_| AdapterError::Closed)??;
    }
    Ok((init, session_id, history))
}

pub fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Converts the input of a turn to ACP content blocks. Mentions become `resource_link`
/// blocks (every ACP agent must accept them); images require the `image` prompt capability.
pub async fn prompt_blocks(
    input: &TurnInput,
    caps: &PromptCapabilities,
) -> Result<Vec<Value>, AdapterError> {
    let mut blocks = Vec::new();
    for part in &input.parts {
        match part {
            TurnInputPart::Text(text) => {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            TurnInputPart::Image { path, mime } => {
                if !caps.image {
                    return Err(AdapterError::Unsupported(
                        "images (the agent does not accept image prompts)",
                    ));
                }
                let bytes = tokio::fs::read(path).await.map_err(|e| {
                    AdapterError::Other(format!("cannot read image {}: {e}", path.display()))
                })?;
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                blocks.push(json!({ "type": "image", "data": data, "mimeType": mime }));
            }
            TurnInputPart::Mention { relative, absolute } => {
                let uri = url::Url::from_file_path(absolute).map_err(|_| {
                    AdapterError::Other(format!(
                        "cannot build a file URI for {}",
                        absolute.display()
                    ))
                })?;
                blocks.push(
                    json!({ "type": "resource_link", "uri": uri.as_str(), "name": relative }),
                );
            }
        }
    }
    if blocks.is_empty() {
        return Err(AdapterError::Other("empty input".into()));
    }
    Ok(blocks)
}

// ----- router ---------------------------------------------------------------------------------

enum Cmd {
    /// `initialize` answered; `background`: the agent confirmed Cognition's background
    /// extension (`crate::cognition`).
    Initialized {
        background: bool,
        reply: oneshot::Sender<()>,
    },
    SetupDone {
        session_id: String,
        setup: SessionSetupResponse,
        is_new: bool,
        reply: oneshot::Sender<Option<NativeHistory>>,
    },
    Send {
        blocks: Vec<Value>,
        reply: oneshot::Sender<Result<(), AdapterError>>,
    },
    Interrupt {
        reply: oneshot::Sender<Result<(), AdapterError>>,
    },
    Respond {
        request_id: String,
        resolution: InteractionResolution,
        reply: oneshot::Sender<Result<(), AdapterError>>,
    },
    ApplySettings {
        settings: ThreadSettings,
        strict: bool,
        reply: oneshot::Sender<Result<SettingsApplied, AdapterError>>,
    },
    /// The settings requests started by `ApplySettings` finished.
    SettingsDone {
        applied: Vec<AppliedSetting>,
        error: Option<AdapterError>,
        reply: oneshot::Sender<Result<SettingsApplied, AdapterError>>,
    },
    PrepareShutdown {
        reply: oneshot::Sender<()>,
    },
    PromptDone {
        result: Result<Value, RpcCallError>,
    },
    /// `SessionControl::stop_background`.
    StopBackground {
        key: String,
        reply: oneshot::Sender<Result<(), AdapterError>>,
    },
    /// `SessionControl::expire_request`.
    Expire {
        request_id: String,
        reply: oneshot::Sender<Result<(), AdapterError>>,
    },
}

/// An agent request waiting for the user.
enum Pending {
    Permission {
        rpc_id: Value,
        options: Vec<wire::PermissionOption>,
    },
    Elicitation {
        rpc_id: Value,
        form: Form,
        /// URL mode: the id `elicitation/complete` refers to.
        elicitation_id: Option<String>,
    },
}

impl Pending {
    /// The answer that tells the agent the request was cancelled.
    fn cancelled(&self) -> Value {
        match self {
            Pending::Permission { .. } => wire::permission_cancelled(),
            Pending::Elicitation { .. } => elicitation::cancelled(),
        }
    }

    fn rpc_id(&self) -> &Value {
        match self {
            Pending::Permission { rpc_id, .. } | Pending::Elicitation { rpc_id, .. } => rpc_id,
        }
    }

    /// The method of the agent's request.
    fn method(&self) -> &'static str {
        match self {
            Pending::Permission { .. } => "session/request_permission",
            Pending::Elicitation { .. } => "elicitation/create",
        }
    }
}

/// One setting to change on the agent.
enum SettingRequest {
    ConfigOption {
        kind: SettingKind,
        value: String,
        config_id: String,
    },
    Mode {
        kind: SettingKind,
        value: String,
    },
}

/// A setting the agent accepted.
struct AppliedSetting {
    kind: SettingKind,
    value: String,
    /// The full option list returned by `session/set_config_option`, when the agent sent one.
    config_options: Option<Vec<ConfigOption>>,
}

struct TurnState {
    cancel_requested: bool,
    start_cost: Option<f64>,
    /// Context-window occupancy from the turn's last `usage_update` that carried a size.
    context: Option<ContextUsage>,
}

struct Router {
    peer: RpcPeer,
    link: Arc<dyn ProcessLink>,
    tx: mpsc::UnboundedSender<AdapterEvent>,
    self_tx: mpsc::UnboundedSender<Cmd>,
    tracker: Tracker,
    /// Background work, when the agent confirmed Cognition's extension (`crate::cognition`).
    background: Option<Background>,
    /// Present while collecting a replay for `read_native_history`.
    history: Option<(Tracker, HistoryBuilder)>,
    /// `false` until the setup response has been handled.
    live: bool,
    session_id: Option<String>,
    options: SessionOptions,
    last_info: Option<(Option<String>, Option<String>, Option<String>)>,
    last_session_info: Option<Value>,
    pending: HashMap<String, Pending>,
    /// Session-scoped requests of the agent that arrived before the session was set up; they
    /// are handled when it is (their session id and scope are known only then).
    deferred: Vec<IncomingRequest>,
    /// Sequence of interaction request ids (`perm-N`, `elic-N`).
    perm_seq: u64,
    turn: Option<TurnState>,
    /// The prompt request failed because the connection closed; finished after the exit.
    turn_lost: bool,
    cost_usd: Option<f64>,
    cost_baseline_zero: bool,
    forward_ext: bool,
    /// Deadline of the settings requests (`AdapterPolicy::handshake_timeout`).
    request_timeout: Duration,
    cache: OptionsCache,
    label: String,
}

impl Router {
    fn emit(&self, event: AdapterEvent) {
        let _ = self.tx.send(event);
    }

    fn notice(&self, level: NoticeLevel, message: String, code: &str) {
        self.emit(AdapterEvent::Notice {
            level,
            message,
            code: Some(code.to_owned()),
        });
    }

    fn native(&self, payload: Value) {
        self.emit(AdapterEvent::Native { payload });
    }

    fn emit_all(&mut self, out: Vec<Emit>) {
        for e in out {
            match e {
                Emit::Event(ev) => self.emit(ev),
                Emit::UserText(_) => {}
                Emit::Unrendered(content) => {
                    self.native(json!({ "sessionUpdate": "unrenderedContent", "content": content }))
                }
            }
        }
    }

    async fn run(
        mut self,
        mut incoming: mpsc::UnboundedReceiver<Incoming>,
        mut cmds: mpsc::UnboundedReceiver<Cmd>,
    ) {
        loop {
            tokio::select! {
                biased;
                msg = incoming.recv() => match msg {
                    Some(m) => self.on_incoming(m).await,
                    None => break,
                },
                cmd = cmds.recv() => {
                    let Some(cmd) = cmd else { break };
                    while let Ok(m) = incoming.try_recv() {
                        self.on_incoming(m).await;
                    }
                    self.on_cmd(cmd).await;
                }
            }
        }

        // The agent's output ended. Let an outstanding prompt settle (its request fails as
        // the connection is closed), answer late commands, then report the exit.
        while self.turn.is_some() && !self.turn_lost {
            match cmds.recv().await {
                Some(cmd) => self.on_cmd(cmd).await,
                None => break,
            }
        }
        let info = self.link.wait().await;
        match self.turn.as_ref().map(|t| t.cancel_requested) {
            Some(true) => self.finish_turn(
                TurnStatus::Interrupted,
                None,
                Some(TurnError {
                    message: format!("the agent {}", info.describe()),
                    kind: "forced".to_owned(),
                }),
                None,
            ),
            // Died on its own: only `Exited` ends the turn (harness contract). The engine fails
            // it with the exit and the stderr lines `policy.exit_message_stderr_lines` quotes,
            // as for every other harness.
            Some(false) => self.abandon_turn(),
            None => {}
        }
        self.withdraw_all();
        // Answer anything still waiting so callers do not hang.
        cmds.close();
        while let Ok(cmd) = cmds.try_recv() {
            reply_closed(cmd);
        }
        self.emit(AdapterEvent::Exited { info });
    }

    // ----- incoming -----

    async fn on_incoming(&mut self, msg: Incoming) {
        match msg {
            Incoming::Notification { method, params } => self.on_notification(method, params).await,
            Incoming::Request(req) => self.on_request(req).await,
            Incoming::Malformed(line) => self.native(json!({ "malformed": line })),
        }
    }

    async fn on_notification(&mut self, method: String, params: Value) {
        if method == "elicitation/complete" {
            match serde_json::from_value::<wire::CompleteElicitationParams>(params.clone()) {
                Ok(p) => self.on_elicitation_complete(&p.elicitation_id).await,
                Err(_) => self.native(json!({ "method": method, "params": params })),
            }
        } else if method == "session/update" {
            match serde_json::from_value::<wire::SessionNotification>(params.clone()) {
                Ok(n) => self.on_update(n),
                Err(_) => self.native(json!({ "method": method, "params": params })),
            }
        } else if method.starts_with('_') {
            if self.forward_ext {
                self.native(json!({ "method": method, "params": params }));
            } else {
                tracing::trace!(label = %self.label, %method, "extension notification ignored");
            }
        } else {
            self.native(json!({ "method": method, "params": params }));
        }
    }

    fn on_update(&mut self, n: wire::SessionNotification) {
        if let (Some(ours), true) = (&self.session_id, !n.session_id.is_empty())
            && *ours != n.session_id
        {
            self.native(json!({ "foreignSession": n.session_id, "update": n.update }));
            return;
        }
        let raw = n.update;
        let meta = raw.get("_meta").cloned();
        let update = SessionUpdate::parse(raw.clone());

        // Item-producing updates.
        if !self.live {
            if let Some((tracker, builder)) = &mut self.history {
                if self.background.is_some()
                    && is_item_update(&update)
                    && cognition::is_sub_agent_update(meta.as_ref())
                {
                    // A sub-agent's own work is not part of the conversation's history.
                    return;
                }
                let mut out = Vec::new();
                if tracker.on_update(&update, &mut out) {
                    for e in out {
                        builder.push(e);
                    }
                    return;
                }
            } else if is_item_update(&update) {
                // Replay of `session/load` used to resume: already in our history.
                return;
            }
        } else if is_item_update(&update) {
            if matches!(update, SessionUpdate::UserMessageChunk(_)) {
                return;
            }
            if let Some(background) = self.background.as_mut() {
                let mut out = Vec::new();
                let routed = background.on_update(
                    &update,
                    meta.as_ref(),
                    &mut self.tracker,
                    self.turn.is_some(),
                    &mut out,
                );
                self.emit_all(out);
                match routed {
                    Routed::Root => {}
                    Routed::Handled => return,
                    Routed::Hidden => {
                        tracing::trace!(label = %self.label, "a sub-agent's own text or plan; not an item of the turn");
                        return;
                    }
                    Routed::Unmapped => {
                        self.native(json!({ "subAgentUpdate": raw }));
                        return;
                    }
                }
            }
            if self.turn.is_none() {
                self.native(json!({ "outsideTurn": raw }));
                return;
            }
            let mut out = Vec::new();
            self.tracker.on_update(&update, &mut out);
            self.emit_all(out);
            return;
        }

        // Session state updates.
        match update {
            SessionUpdate::AvailableCommands(c) => {
                self.cache.record_commands(&c.available_commands);
                let commands = c.available_commands.iter().map(mapping::command).collect();
                self.emit(AdapterEvent::CommandsChanged { commands });
            }
            SessionUpdate::ConfigOptions(c) => {
                self.options.config_options = c.config_options;
                if self.cache.record_options(&self.options) {
                    self.emit(AdapterEvent::HarnessInfoChanged);
                }
                self.publish_info();
            }
            SessionUpdate::CurrentMode(m) => {
                self.options
                    .set_current(SettingKind::Mode, &m.current_mode_id);
                self.publish_info();
            }
            SessionUpdate::SessionInfo(info) => {
                if let Some((_, builder)) = &mut self.history {
                    builder.set_title(info.title.clone());
                }
                let payload = json!({ "sessionUpdate": "session_info_update", "title": info.title, "updatedAt": info.updated_at });
                if self.last_session_info.as_ref() != Some(&payload) {
                    self.last_session_info = Some(payload.clone());
                    match info.title.clone().filter(|t| !t.trim().is_empty()) {
                        Some(title) => self.emit(AdapterEvent::SessionTitle { title }),
                        None => self.native(payload),
                    }
                }
            }
            SessionUpdate::Usage(u) => {
                if let Some(background) = self.background.as_mut() {
                    let mut out = Vec::new();
                    let sub_agent = background.on_usage(meta.as_ref(), &mut out);
                    self.emit_all(out);
                    if sub_agent {
                        // A sub-agent's usage: never the root agent's context or cost.
                        return;
                    }
                }
                if let Some(turn) = self.turn.as_mut()
                    && let Some(context) = mapping::context_usage(&u)
                {
                    turn.context = Some(context);
                }
                if let Some(cost) = u.cost
                    && cost.currency.eq_ignore_ascii_case("USD")
                {
                    self.cost_usd = Some(cost.amount);
                }
            }
            SessionUpdate::Unknown(raw) => self.native(raw),
            // Item updates were handled above.
            _ => {}
        }
    }

    fn publish_info(&mut self) {
        let info = (
            self.options.current(SettingKind::Model),
            self.options.current(SettingKind::Mode),
            self.options.current(SettingKind::Effort),
        );
        if self.last_info.as_ref() != Some(&info) {
            self.last_info = Some(info.clone());
            self.emit(AdapterEvent::SessionInfo {
                model: info.0,
                permission_mode: info.1,
                effort: info.2,
            });
        }
    }

    async fn on_request(&mut self, req: IncomingRequest) {
        if req.method == "session/request_permission" {
            self.on_permission(req).await;
            return;
        }
        if req.method == "elicitation/create" {
            self.on_elicitation(req).await;
            return;
        }
        let err = RpcWireError::method_not_found(&req.method);
        let written = self.peer.respond_error(req.id, err).await;
        self.log_unanswered(&req.method, "method not found", written);
        if !req.method.starts_with('_') {
            self.notice(
                NoticeLevel::Warning,
                format!("The agent called `{}`, which agent-app-server does not provide; the call was rejected.", req.method),
                "unsupportedClientMethod",
            );
        }
    }

    async fn on_permission(&mut self, req: IncomingRequest) {
        let params = match serde_json::from_value::<RequestPermissionParams>(req.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                let written = self
                    .peer
                    .respond_error(
                        req.id,
                        RpcWireError::new(-32602, format!("invalid params: {e}")),
                    )
                    .await;
                self.log_unanswered(&req.method, "invalid params", written);
                self.native(json!({ "method": req.method, "params": req.params }));
                return;
            }
        };
        if !self.live && self.history.is_none() {
            // The session is being set up: handled once it is (`Cmd::SetupDone`).
            self.deferred.push(req);
            return;
        }
        let foreign = !params.session_id.is_empty()
            && self
                .session_id
                .as_ref()
                .is_some_and(|ours| *ours != params.session_id);
        if !self.live || foreign {
            // Nothing can show it: the process only replays a session's history (nobody
            // answers there), or it is about another session.
            let written = self
                .peer
                .respond(req.id, wire::permission_cancelled())
                .await;
            self.log_unanswered(&req.method, "cancelled", written);
            let (reason, code) = if foreign {
                ("for another session", "permissionOutsideSession")
            } else {
                (
                    "while its history was being read",
                    "permissionDuringHistoryRead",
                )
            };
            self.notice(
                NoticeLevel::Warning,
                format!("The agent asked for permission {reason}; the request was cancelled."),
                code,
            );
            return;
        }
        if self.turn.as_ref().is_some_and(|t| t.cancel_requested) {
            // ACP: after `session/cancel` the client answers every permission request
            // `cancelled`. Recorded, as the agent's request is not shown.
            let written = self
                .peer
                .respond(req.id, wire::permission_cancelled())
                .await;
            self.log_unanswered(&req.method, "cancelled", written);
            self.native(json!({ "method": req.method, "params": req.params, "answered": "cancelled", "reason": "turnCancelled" }));
            return;
        }
        // Whom it belongs to: the background task it names explicitly (a background shell's
        // or a background sub-agent's tool call), else the running turn (as an item of it),
        // else the thread.
        let background_key = self
            .background
            .as_mut()
            .and_then(|b| b.task_of_tool(&params.tool_call));
        let (item_key, tool) = if self.turn.is_some() && background_key.is_none() {
            let mut out = Vec::new();
            let key = self.tracker.tool_item(&params.tool_call, &mut out);
            self.emit_all(out);
            let tool = self
                .tracker
                .tool(&params.tool_call.tool_call_id)
                .cloned()
                .unwrap_or_default();
            (Some(key), tool)
        } else {
            let tool = self
                .background
                .as_ref()
                .and_then(|b| b.tool_view(&params.tool_call))
                .unwrap_or_else(|| self.tracker.peek_tool(&params.tool_call));
            (None, tool)
        };
        let (request, dropped) = mapping::permission_request(&tool, &params.options);
        if !dropped.is_empty() {
            self.notice(
                NoticeLevel::Warning,
                format!(
                    "The agent offered permission options of an unknown kind; they are hidden: {}",
                    dropped.join(", ")
                ),
                "unknownPermissionOption",
            );
        }
        let offered = matches!(&request, aas_harness::InteractionRequest::Approval { options, .. } if !options.is_empty());
        if !offered {
            let body = match mapping::dismiss_option(&params.options) {
                Some(id) => wire::permission_selected(&id),
                None => wire::permission_cancelled(),
            };
            let written = self.peer.respond(req.id, body).await;
            self.log_unanswered(&req.method, "no option to show", written);
            return;
        }
        self.perm_seq += 1;
        let request_id = format!("perm-{}", self.perm_seq);
        self.pending.insert(
            request_id.clone(),
            Pending::Permission {
                rpc_id: req.id,
                options: params.options,
            },
        );
        self.emit(AdapterEvent::InteractionRequested {
            request_id,
            request,
            item_key,
            background_key,
        });
    }

    /// `elicitation/create`: shown as a question when it is about this session (it belongs to
    /// the background task of its tool call, the running turn, or the thread; one that arrives
    /// while the session is being set up waits until it is). Answered with `cancel` when
    /// nothing can show it (request scope, another session, a process that only reads a
    /// session's history, a mode that cannot be shown) or the turn is being cancelled.
    async fn on_elicitation(&mut self, req: IncomingRequest) {
        let params =
            match serde_json::from_value::<wire::CreateElicitationParams>(req.params.clone()) {
                Ok(p) => p,
                Err(e) => {
                    let written = self
                        .peer
                        .respond_error(
                            req.id,
                            RpcWireError::new(-32602, format!("invalid params: {e}")),
                        )
                        .await;
                    self.log_unanswered(&req.method, "invalid params", written);
                    self.native(json!({ "method": req.method, "params": req.params }));
                    return;
                }
            };
        // Nothing can show a request outside this session: request scope (before a session
        // exists, e.g. during authentication, while the start waits for the answer), another
        // session, or a process that only replays a session's history.
        let outside = match (&params.session_id, &self.session_id) {
            (None, _) => Some(("outside of a session", "elicitationOutsideSession")),
            (Some(theirs), Some(ours)) if theirs != ours => {
                Some(("for another session", "elicitationOutsideSession"))
            }
            _ if !self.live && self.history.is_some() => Some((
                "while its history was being read",
                "elicitationDuringHistoryRead",
            )),
            _ if !self.live => {
                // The session is being set up: handled once it is (`Cmd::SetupDone`).
                self.deferred.push(req);
                return;
            }
            _ => None,
        };
        if let Some((reason, code)) = outside {
            let written = self.peer.respond(req.id, elicitation::cancelled()).await;
            self.log_unanswered(&req.method, "cancelled", written);
            self.notice(
                NoticeLevel::Warning,
                format!("The agent asked for input {reason}; the request was cancelled."),
                code,
            );
            return;
        }
        if self.turn.as_ref().is_some_and(|t| t.cancel_requested) {
            // ACP: after `session/cancel`, requests are answered as cancelled.
            let written = self.peer.respond(req.id, elicitation::cancelled()).await;
            self.log_unanswered(&req.method, "cancelled", written);
            self.native(json!({ "method": req.method, "params": req.params, "answered": "cancel", "reason": "turnCancelled" }));
            return;
        }
        match elicitation::request(&params) {
            Elicitation::Ask { request, form } => {
                self.perm_seq += 1;
                let request_id = format!("elic-{}", self.perm_seq);
                // Whom it belongs to, as for permissions: the background task of its tool
                // call, else the running turn, else the thread.
                let background_key = match (&params.tool_call_id, self.background.as_mut()) {
                    (Some(id), Some(b)) => b.task_of_tool(&wire::ToolCallFields {
                        tool_call_id: id.clone(),
                        ..wire::ToolCallFields::default()
                    }),
                    _ => None,
                };
                let item_key = params
                    .tool_call_id
                    .as_deref()
                    .filter(|_| self.turn.is_some() && background_key.is_none())
                    .and_then(|id| self.tracker.tool_key(id));
                let elicitation_id = (form == Form::Url)
                    .then(|| params.elicitation_id.clone())
                    .flatten();
                self.pending.insert(
                    request_id.clone(),
                    Pending::Elicitation {
                        rpc_id: req.id,
                        form,
                        elicitation_id,
                    },
                );
                self.emit(AdapterEvent::InteractionRequested {
                    request_id,
                    request,
                    item_key,
                    background_key,
                });
            }
            Elicitation::UnknownMode(mode) => {
                let written = self.peer.respond(req.id, elicitation::cancelled()).await;
                self.log_unanswered(&req.method, "cancelled (unknown mode)", written);
                self.notice(
                    NoticeLevel::Warning,
                    format!("The agent asked for input in a form agent-app-server cannot show (mode `{mode}`); it was cancelled."),
                    "unsupportedElicitation",
                );
                self.native(json!({ "method": req.method, "params": req.params }));
            }
        }
    }

    /// `elicitation/complete`: the agent reports that the URL-mode interaction finished. A
    /// request still waiting for the user is answered `accept` (the flow the user was asked to
    /// complete is complete) and withdrawn.
    async fn on_elicitation_complete(&mut self, elicitation_id: &str) {
        let waiting = self.pending.iter().find_map(|(request_id, p)| match p {
            Pending::Elicitation {
                elicitation_id: Some(id),
                ..
            } if id == elicitation_id => Some(request_id.clone()),
            _ => None,
        });
        let Some(request_id) = waiting else {
            tracing::debug!(label = %self.label, elicitation_id, "elicitation/complete for a request that is already answered");
            return;
        };
        if let Some(p) = self.pending.remove(&request_id) {
            let written = self
                .peer
                .respond(p.rpc_id().clone(), elicitation::accepted())
                .await;
            self.log_unanswered(p.method(), "accepted on elicitation/complete", written);
            self.emit(AdapterEvent::InteractionWithdrawn { request_id });
        }
    }

    /// Answers every pending request with `cancelled` and withdraws the interactions.
    async fn cancel_pending(&mut self) {
        let pending: Vec<(String, Pending)> = self.pending.drain().collect();
        for (request_id, p) in pending {
            let body = p.cancelled();
            let written = self.peer.respond(p.rpc_id().clone(), body).await;
            self.log_unanswered(p.method(), "cancelled", written);
            self.emit(AdapterEvent::InteractionWithdrawn { request_id });
        }
    }

    /// Logs an answer to an agent request that could not be written: the agent's stdin is
    /// closed or broken, so the process is ending (its exit is reported on its own). Such a
    /// failure is never dropped silently.
    fn log_unanswered(&self, method: &str, answer: &str, written: Result<(), RpcCallError>) {
        if let Err(e) = written {
            tracing::warn!(label = %self.label, method, answer, error = %e, "could not answer an agent request");
        }
    }

    fn withdraw_all(&mut self) {
        let ids: Vec<String> = self.pending.drain().map(|(id, _)| id).collect();
        for request_id in ids {
            self.emit(AdapterEvent::InteractionWithdrawn { request_id });
        }
    }

    // ----- commands -----

    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Initialized { background, reply } => {
                if background {
                    self.background = Some(Background::new());
                }
                let _ = reply.send(());
            }
            Cmd::StopBackground { key, reply } => self.stop_background(key, reply),
            Cmd::Expire { request_id, reply } => {
                let result = self.expire(&request_id).await;
                let _ = reply.send(result);
            }
            Cmd::SetupDone {
                session_id,
                setup,
                is_new,
                reply,
            } => {
                self.session_id = Some(session_id);
                if let Some(opts) = setup.config_options {
                    self.options.config_options = opts;
                }
                if setup.modes.is_some() {
                    self.options.modes = setup.modes;
                }
                let mut changed = self.cache.record_options(&self.options);
                if is_new {
                    changed |= self.cache.record_defaults(&self.options);
                }
                if changed {
                    self.emit(AdapterEvent::HarnessInfoChanged);
                }
                let history = self.history.take().map(|(mut tracker, mut builder)| {
                    let mut out = Vec::new();
                    tracker.end_turn(TurnStatus::Completed, &mut out);
                    for e in out {
                        builder.push(e);
                    }
                    builder.finish()
                });
                self.live = true;
                self.publish_info();
                // Requests the agent made while the session was being set up.
                for req in std::mem::take(&mut self.deferred) {
                    self.on_request(req).await;
                }
                let _ = reply.send(history);
            }
            Cmd::Send { blocks, reply } => {
                if self.turn.is_some() {
                    let _ =
                        reply.send(Err(AdapterError::Other("a turn is already running".into())));
                    return;
                }
                let Some(session_id) = self.session_id.clone() else {
                    let _ = reply.send(Err(AdapterError::Closed));
                    return;
                };
                let start_cost = self.cost_usd.or(if self.cost_baseline_zero {
                    Some(0.0)
                } else {
                    None
                });
                self.turn = Some(TurnState {
                    cancel_requested: false,
                    start_cost,
                    context: None,
                });
                self.emit(AdapterEvent::TurnStarted);
                let peer = self.peer.clone();
                let tx = self.self_tx.clone();
                let params = serde_json::to_value(wire::PromptRequest {
                    session_id,
                    prompt: blocks,
                })
                .expect("serializes");
                tokio::spawn(async move {
                    let result = peer.request_value("session/prompt", params).await;
                    let _ = tx.send(Cmd::PromptDone { result });
                });
                let _ = reply.send(Ok(()));
            }
            Cmd::PromptDone { result } => self.on_prompt_done(result),
            Cmd::Interrupt { reply } => {
                let result = self.request_cancel().await;
                let _ = reply.send(result);
            }
            Cmd::Respond {
                request_id,
                resolution,
                reply,
            } => {
                let result = self.respond(&request_id, resolution).await;
                let _ = reply.send(result);
            }
            Cmd::ApplySettings {
                settings,
                strict,
                reply,
            } => self.apply_settings(&settings, strict, reply),
            Cmd::SettingsDone {
                applied,
                error,
                reply,
            } => self.settings_done(applied, error, reply),
            Cmd::PrepareShutdown { reply } => {
                if self.turn.is_some() {
                    let _ = self.request_cancel().await;
                }
                let _ = reply.send(());
            }
        }
    }

    /// Asks the agent to stop background task `key` (Cognition's extension). The request is
    /// sent from its own task so the router keeps serving the agent meanwhile; `{}` means the
    /// agent took it, the end arrives as the task's end signal.
    fn stop_background(&mut self, key: String, reply: oneshot::Sender<Result<(), AdapterError>>) {
        let Some(background) = &self.background else {
            let _ = reply.send(Err(AdapterError::Unsupported("backgroundStop")));
            return;
        };
        let target = match background.stop_target(&key) {
            Ok(target) => target,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let Some(session_id) = self.session_id.clone() else {
            let _ = reply.send(Err(AdapterError::Closed));
            return;
        };
        let (method, params) = target.request(&session_id);
        let peer = self.peer.clone();
        let timeout = self.request_timeout;
        tokio::spawn(async move {
            let result = peer
                .request_timeout::<_, Value>(method, params, timeout)
                .await
                .map(|_| ())
                .map_err(|e| match e {
                    RpcCallError::Closed => AdapterError::Closed,
                    other => AdapterError::Harness(format!("{method}: {other}")),
                });
            let _ = reply.send(result);
        });
    }

    /// The engine expired a request (its turn or background task ended) and no longer waits
    /// for the user: answer the agent `cancelled` (the ACP answer for a request the client
    /// abandons), so that it does not wait either.
    async fn expire(&mut self, request_id: &str) -> Result<(), AdapterError> {
        let Some(p) = self.pending.remove(request_id) else {
            return Err(AdapterError::UnknownRequest(request_id.to_owned()));
        };
        self.peer
            .respond(p.rpc_id().clone(), p.cancelled())
            .await
            .map_err(|e| match e {
                RpcCallError::Closed => AdapterError::Closed,
                other => AdapterError::Other(other.to_string()),
            })
    }

    async fn request_cancel(&mut self) -> Result<(), AdapterError> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let first = !turn.cancel_requested;
        turn.cancel_requested = true;
        if first && let Some(id) = &self.session_id {
            self.peer
                .notify(
                    "session/cancel",
                    wire::SessionIdParams {
                        session_id: id.clone(),
                    },
                )
                .await
                .map_err(|e| AdapterError::Other(format!("session/cancel: {e}")))?;
        }
        self.cancel_pending().await;
        Ok(())
    }

    fn on_prompt_done(&mut self, result: Result<Value, RpcCallError>) {
        if self.turn.is_none() {
            return;
        }
        match result {
            Ok(value) => {
                // A response that does not parse carries no stop reason: it is forwarded as
                // `Native` below and the turn ends by the empty stop reason's rule.
                let resp: wire::PromptResponse = match serde_json::from_value(value.clone()) {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!(error = %e, "session/prompt response does not parse");
                        wire::PromptResponse::default()
                    }
                };
                if resp.stop_reason.is_empty() {
                    self.native(json!({ "promptResponseWithoutStopReason": value }));
                }
                let outcome = mapping::stop_reason(&resp.stop_reason);
                let cost = match (self.turn.as_ref().and_then(|t| t.start_cost), self.cost_usd) {
                    (Some(start), Some(now)) if now >= start => Some(now - start),
                    _ => None,
                };
                let context = self.turn.as_ref().and_then(|t| t.context);
                let usage = mapping::turn_usage(resp.usage.as_ref(), cost, context);
                self.finish_turn(outcome.status, usage, outcome.error, outcome.notice);
            }
            Err(RpcCallError::Rpc(e)) => {
                let cancelled = e.code == wire::REQUEST_CANCELLED;
                let (status, kind) = if cancelled {
                    (TurnStatus::Interrupted, "cancelled")
                } else if e.code == wire::AUTH_REQUIRED {
                    (TurnStatus::Failed, "authRequired")
                } else {
                    (TurnStatus::Failed, "harnessError")
                };
                let error = (!cancelled).then(|| TurnError {
                    message: e.message.clone(),
                    kind: kind.to_owned(),
                });
                self.finish_turn(status, None, error, None);
            }
            Err(RpcCallError::Closed) => {
                // Connection gone: completed after the process exit is known.
                self.turn_lost = true;
            }
            Err(e) => {
                let error = TurnError {
                    message: format!("session/prompt failed: {e}"),
                    kind: "adapterError".to_owned(),
                };
                self.finish_turn(TurnStatus::Failed, None, Some(error), None);
            }
        }
    }

    /// Closes what the adapter tracks of a turn whose process died, without ending the turn:
    /// the open items fail, the pending requests are withdrawn, and the context the turn last
    /// saw is reported (a turn ended by `Exited` keeps it).
    fn abandon_turn(&mut self) {
        if let Some(usage) =
            mapping::turn_usage(None, None, self.turn.as_ref().and_then(|t| t.context))
        {
            self.emit(AdapterEvent::TurnUsage { usage });
        }
        let mut out = Vec::new();
        self.tracker.end_turn(TurnStatus::Failed, &mut out);
        self.emit_all(out);
        self.withdraw_all();
        self.turn = None;
        self.turn_lost = false;
    }

    fn finish_turn(
        &mut self,
        status: TurnStatus,
        usage: Option<aas_harness::Usage>,
        error: Option<TurnError>,
        notice: Option<(NoticeLevel, String, String)>,
    ) {
        // A turn that ended without a prompt response still reports the context it saw.
        let usage = usage.or_else(|| {
            mapping::turn_usage(None, None, self.turn.as_ref().and_then(|t| t.context))
        });
        let mut out = Vec::new();
        self.tracker.end_turn(status, &mut out);
        self.emit_all(out);
        if let Some((level, message, code)) = notice {
            self.notice(level, message, &code);
        }
        // Requests still pending stay pending: the engine expires those of this turn and has
        // them answered (`expire_request`); those of the thread or of a background task
        // outlive the turn.
        self.turn = None;
        self.turn_lost = false;
        self.emit(AdapterEvent::TurnCompleted {
            trigger: None,
            status,
            usage,
            error,
        });
    }

    async fn respond(
        &mut self,
        request_id: &str,
        resolution: InteractionResolution,
    ) -> Result<(), AdapterError> {
        let Some(p) = self.pending.remove(request_id) else {
            return Err(AdapterError::UnknownRequest(request_id.to_owned()));
        };
        let body = match (&p, &resolution) {
            (
                Pending::Permission { options, .. },
                InteractionResolution::Approval { option_id, .. },
            ) => {
                let known = options
                    .iter()
                    .any(|o| o.option_id == *option_id && mapping::option_kind(&o.kind).is_some());
                if !known {
                    self.pending.insert(request_id.to_owned(), p);
                    return Err(AdapterError::Other(format!("unknown option `{option_id}`")));
                }
                wire::permission_selected(option_id)
            }
            (Pending::Permission { options, .. }, InteractionResolution::Dismissed) => {
                match mapping::dismiss_option(options) {
                    Some(id) => wire::permission_selected(&id),
                    None => wire::permission_cancelled(),
                }
            }
            (Pending::Permission { .. }, InteractionResolution::Question { .. }) => {
                self.pending.insert(request_id.to_owned(), p);
                return Err(AdapterError::Other(
                    "a permission request needs an approval answer".into(),
                ));
            }
            (Pending::Elicitation { form, .. }, resolution) => {
                match elicitation::response(form, resolution) {
                    Ok(body) => body,
                    Err(message) => {
                        // Invalid answer: keep the request open so the user can answer again.
                        self.pending.insert(request_id.to_owned(), p);
                        return Err(AdapterError::Other(message));
                    }
                }
            }
        };
        self.peer
            .respond(p.rpc_id().clone(), body)
            .await
            .map_err(|e| match e {
                RpcCallError::Closed => AdapterError::Closed,
                other => AdapterError::Other(other.to_string()),
            })
    }

    /// Validates the wanted settings and sends the requests from a separate task, so the
    /// router keeps serving the agent (e.g. a permission request the agent raises before it
    /// answers) and other commands meanwhile. The result arrives as `Cmd::SettingsDone`.
    fn apply_settings(
        &mut self,
        settings: &ThreadSettings,
        strict: bool,
        reply: oneshot::Sender<Result<SettingsApplied, AdapterError>>,
    ) {
        let Some(session_id) = self.session_id.clone() else {
            let _ = reply.send(Err(AdapterError::Closed));
            return;
        };
        let wanted = [
            (SettingKind::Model, settings.model.as_deref()),
            (SettingKind::Mode, settings.permission_mode.as_deref()),
            (SettingKind::Effort, settings.effort.as_deref()),
        ];
        let mut requests = Vec::new();
        for (kind, value) in wanted {
            let Some(value) = value else { continue };
            if self.options.current(kind).as_deref() == Some(value) {
                continue;
            }
            let problem = if !self.options.has(kind) {
                Some(format!("the agent exposes no {} selector", kind.category()))
            } else if !self.options.accepts(kind, value) {
                Some(format!(
                    "`{value}` is not a {} the agent offers",
                    kind.category()
                ))
            } else {
                None
            };
            if let Some(problem) = problem {
                if strict {
                    let _ = reply.send(Err(AdapterError::Other(problem)));
                    return;
                }
                self.notice(
                    NoticeLevel::Warning,
                    format!("Setting ignored: {problem}."),
                    "settingIgnored",
                );
                continue;
            }
            requests.push(match self.options.option(kind) {
                Some(opt) => SettingRequest::ConfigOption {
                    kind,
                    value: value.to_owned(),
                    config_id: opt.id.clone(),
                },
                None => SettingRequest::Mode {
                    kind,
                    value: value.to_owned(),
                },
            });
        }
        if requests.is_empty() {
            self.settings_done(Vec::new(), None, reply);
            return;
        }
        let peer = self.peer.clone();
        let tx = self.self_tx.clone();
        let timeout = self.request_timeout;
        tokio::spawn(async move {
            let mut applied = Vec::new();
            let mut error = None;
            for request in requests {
                match request {
                    SettingRequest::ConfigOption {
                        kind,
                        value,
                        config_id,
                    } => {
                        let req = wire::SetConfigOptionRequest {
                            session_id: session_id.clone(),
                            config_id,
                            value: value.clone(),
                        };
                        match peer
                            .request_timeout::<_, wire::SetConfigOptionResponse>(
                                "session/set_config_option",
                                req,
                                timeout,
                            )
                            .await
                        {
                            Ok(resp) => applied.push(AppliedSetting {
                                kind,
                                value,
                                config_options: resp.config_options,
                            }),
                            Err(e) => {
                                error = Some(AdapterError::Harness(format!(
                                    "session/set_config_option: {e}"
                                )));
                                break;
                            }
                        }
                    }
                    SettingRequest::Mode { kind, value } => {
                        let req = wire::SetModeRequest {
                            session_id: session_id.clone(),
                            mode_id: value.clone(),
                        };
                        match peer
                            .request_timeout::<_, Value>("session/set_mode", req, timeout)
                            .await
                        {
                            Ok(_) => applied.push(AppliedSetting {
                                kind,
                                value,
                                config_options: None,
                            }),
                            Err(e) => {
                                error =
                                    Some(AdapterError::Harness(format!("session/set_mode: {e}")));
                                break;
                            }
                        }
                    }
                }
            }
            let _ = tx.send(Cmd::SettingsDone {
                applied,
                error,
                reply,
            });
        });
    }

    /// Records what the agent accepted and answers the `ApplySettings` caller.
    fn settings_done(
        &mut self,
        applied: Vec<AppliedSetting>,
        error: Option<AdapterError>,
        reply: oneshot::Sender<Result<SettingsApplied, AdapterError>>,
    ) {
        for setting in applied {
            match setting.config_options {
                Some(opts) => self.options.config_options = opts,
                None => self.options.set_current(setting.kind, &setting.value),
            }
        }
        if self.cache.record_options(&self.options) {
            self.emit(AdapterEvent::HarnessInfoChanged);
        }
        self.publish_info();
        let _ = reply.send(match error {
            Some(e) => Err(e),
            None => Ok(SettingsApplied::Live),
        });
    }
}

fn is_item_update(u: &SessionUpdate) -> bool {
    matches!(
        u,
        SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_)
            | SessionUpdate::Plan(_)
    )
}

fn reply_closed(cmd: Cmd) {
    match cmd {
        Cmd::SetupDone { reply, .. } => drop(reply),
        Cmd::Initialized { reply, .. } => drop(reply),
        Cmd::Send { reply, .. }
        | Cmd::Interrupt { reply }
        | Cmd::Respond { reply, .. }
        | Cmd::StopBackground { reply, .. }
        | Cmd::Expire { reply, .. } => {
            let _ = reply.send(Err(AdapterError::Closed));
        }
        Cmd::ApplySettings { reply, .. } | Cmd::SettingsDone { reply, .. } => {
            let _ = reply.send(Err(AdapterError::Closed));
        }
        Cmd::PrepareShutdown { reply } => {
            let _ = reply.send(());
        }
        Cmd::PromptDone { .. } => {}
    }
}

// ----- control ----------------------------------------------------------------------------

struct AcpControl {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    peer: RpcPeer,
    link: Arc<dyn ProcessLink>,
    grace: Duration,
    prompt_caps: PromptCapabilities,
    exit: OnceCell<ExitInfo>,
}

impl AcpControl {
    async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Cmd,
    ) -> Result<T, AdapterError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(make(tx))
            .map_err(|_| AdapterError::Closed)?;
        rx.await.map_err(|_| AdapterError::Closed)
    }
}

#[async_trait]
impl SessionControl for AcpControl {
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        let blocks = prompt_blocks(&input, &self.prompt_caps).await?;
        self.call(|reply| Cmd::Send { blocks, reply }).await?
    }

    async fn steer(&self, _input: TurnInput) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("steer"))
    }

    async fn interrupt(&self) -> Result<(), AdapterError> {
        // Bounded: `session/cancel` waits for the agent's stdin, which a wedged agent may
        // never drain; the engine then stops the process after its interrupt grace.
        match tokio::time::timeout(self.grace, self.call(|reply| Cmd::Interrupt { reply })).await {
            Ok(result) => result?,
            Err(_) => Err(AdapterError::Other(format!(
                "the agent did not accept the cancel request within {:?}",
                self.grace
            ))),
        }
    }

    async fn respond(
        &self,
        request_id: &str,
        resolution: &InteractionResolution,
    ) -> Result<(), AdapterError> {
        let request_id = request_id.to_owned();
        let resolution = resolution.clone();
        self.call(|reply| Cmd::Respond {
            request_id,
            resolution,
            reply,
        })
        .await?
    }

    async fn apply_settings(
        &self,
        settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError> {
        let settings = settings.clone();
        self.call(|reply| Cmd::ApplySettings {
            settings,
            strict: true,
            reply,
        })
        .await?
    }

    async fn stop_background(&self, key: &str) -> Result<(), AdapterError> {
        let key = key.to_owned();
        self.call(|reply| Cmd::StopBackground { key, reply })
            .await?
    }

    /// Answers the agent `cancelled` (permission) or `{action: "cancel"}` (elicitation): the
    /// ACP answers for a request the client abandons, whatever ended it.
    async fn expire_request(
        &self,
        request_id: &str,
        _reason: ExpireReason,
    ) -> Result<(), AdapterError> {
        let request_id = request_id.to_owned();
        self.call(|reply| Cmd::Expire { request_id, reply }).await?
    }

    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        self.exit
            .get_or_init(|| async {
                let started = tokio::time::Instant::now();
                // Cancel a running prompt first (best effort: the router may be gone already).
                // Bounded: a wedged agent that no longer reads its stdin must not keep the stop
                // from reaching the termination stage.
                let _ = tokio::time::timeout(
                    self.grace,
                    self.call(|reply| Cmd::PrepareShutdown { reply }),
                )
                .await;
                // Returns promptly even while a write (e.g. a large prompt) is stuck on a full
                // pipe; that write fails, which also unblocks the router.
                self.peer.close_writer().await;
                self.link
                    .shutdown(self.grace.saturating_sub(started.elapsed()), reason)
                    .await
            })
            .await
            .clone()
    }
}
