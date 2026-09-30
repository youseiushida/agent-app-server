//! Fake harness: a deterministic agent driven by scenario directives in the prompt
//! (see [`agent`]). Used by the engine/server test suites and, configured as
//! `kind = "fake"`, for developing the client without spending tokens.
//!
//! Two modes:
//! * `process` (default): the agent runs as a real child process
//!   (`aas-dummy-agent agent`) under the supervisor — exercises the full process path.
//! * `inProcess` (`options.mode = "inProcess"`): the agent runs as a task connected through
//!   in-memory pipes — fast unit tests without binaries.
//!
//! With `options.sessionsDir` (an absolute path) the agent keeps its sessions in that folder
//! like a real CLI ([`store`]), and the harness offers `fork` and `nativeSessions`: the
//! folder's sessions can be listed and imported, resumed, and branched off.
//!
//! Background work (`@bg`, [`background`]) is reported as background tasks (capabilities
//! `backgroundTasks` and `backgroundStop`), with the agent's own turns when a task ends.
//!
//! It offers every extended feature of the port ([`FakeAdapter::features`]): turn anchors and
//! forks at a turn (with a store; a fork before a turn ends right after the turn before it, as
//! Claude Code cuts), forks of sessions another process holds, renames both ways,
//! the harness status, side questions, moving a running item to the background, returned
//! steers, composer text, plan mode with proposed plans, fast mode with the model `fake-fast`,
//! the project trust decision, and a session-switching command with an alias (`fake-clear`,
//! `fake-reset`).

pub mod agent;
pub mod background;
pub mod store;
pub mod wire;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use std::collections::HashMap;

use aas_harness::*;
use aas_protocol::types::{
    EffortLevel, HarnessCapabilities, HarnessFeatures, Model, PermissionMode, PlanModeFeature,
    StatusRow, StatusSection,
};
use aas_stdio::{JsonLinesReader, JsonLinesWriter, ReadLine, SharedJsonLinesWriter};
use aas_supervisor::{ChildHandle, SpawnSpec};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::agent::AgentOptions;
use crate::store::{SessionStore, StoreError};
use crate::wire::{Ev, ForkAt, Modes, Op, Query};

/// Text the fake harness sends (with plan mode off) to implement a proposed plan in the same
/// thread: its own words, shaped like Codex's.
pub const IMPLEMENT_PROMPT: &str = "Implement the plan.";
/// Preamble of a new thread that implements a proposed plan (then a blank line and the plan).
pub const NEW_THREAD_PREAMBLE: &str =
    "Implement the plan below, which an earlier session proposed, in this new session.";

/// A permission mode the fake does not offer and takes as plan mode
/// ([`HarnessAdapter::upgrade_settings`]), like Claude Code's `plan`.
pub const LEGACY_PLAN_MODE: &str = "plan";

/// The fake model that runs in the permission mode `ask` only (`Model.permissionModes`), like
/// Claude Code's Haiku, which the CLI does not offer auto mode for.
pub const LIMITED_MODEL: &str = "fake-lite";

/// Buffer of each in-memory pipe between the adapter and an in-process agent. A pure buffer
/// size: both ends are read continuously.
const IN_PROCESS_PIPE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone)]
enum Mode {
    InProcess,
    Process { program: PathBuf },
}

/// How the agent runs (`options.mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ModeOption {
    #[default]
    Process,
    InProcess,
}

/// Adapter options (`[[harness]] options = { … }`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FakeOptions {
    #[serde(default)]
    mode: ModeOption,
    /// The agent's session store (absolute). Without it nothing is stored and the harness
    /// offers neither `fork` nor `nativeSessions`.
    #[serde(default)]
    sessions_dir: Option<PathBuf>,
}

impl FakeOptions {
    fn parse(value: &serde_json::Value) -> Result<Self, AdapterError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let options: Self = serde_json::from_value(value.clone())
            .map_err(|e| AdapterError::Unavailable(format!("invalid fake harness options: {e}")))?;
        if let Some(dir) = &options.sessions_dir
            && !dir.is_absolute()
        {
            return Err(AdapterError::Unavailable(format!(
                "options.sessionsDir must be an absolute path, not {}",
                dir.display()
            )));
        }
        Ok(options)
    }
}

fn store_error(e: StoreError) -> AdapterError {
    match e {
        StoreError::InvalidId(_)
        | StoreError::NotFound(_)
        | StoreError::Exists(_)
        | StoreError::OtherFolder { .. }
        | StoreError::NoSuchTurn { .. } => AdapterError::Harness(e.to_string()),
        StoreError::Io { .. } | StoreError::Corrupt { .. } => AdapterError::Other(e.to_string()),
    }
}

/// The fake harness adapter.
pub struct FakeAdapter {
    config: HarnessConfig,
    ctx: AdapterContext,
    display_name: String,
}

impl FakeAdapter {
    pub fn new(config: HarnessConfig, ctx: AdapterContext) -> Self {
        let display_name = config
            .display_name
            .clone()
            .unwrap_or_else(|| "Fake agent".to_owned());
        Self {
            config,
            ctx,
            display_name,
        }
    }

    /// Convenience constructor for an in-process fake with id `id`.
    pub fn in_process(id: &str, ctx: AdapterContext) -> Self {
        Self::new(
            HarnessConfig {
                id: id.to_owned(),
                kind: HarnessKind::Fake,
                display_name: None,
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
                options: serde_json::json!({ "mode": "inProcess" }),
            },
            ctx,
        )
    }

    fn options(&self) -> Result<FakeOptions, AdapterError> {
        FakeOptions::parse(&self.config.options)
    }

    fn mode(&self) -> Result<Mode, AdapterError> {
        match self.options()?.mode {
            ModeOption::InProcess => Ok(Mode::InProcess),
            ModeOption::Process => {
                let program = aas_supervisor::resolve_program(&self.config.command)
                    .map_err(|e| AdapterError::Unavailable(e.to_string()))?;
                Ok(Mode::Process { program })
            }
        }
    }

    /// The session store, required by `capability` (`fork`, `nativeSessions`).
    fn store(&self, capability: &'static str) -> Result<SessionStore, AdapterError> {
        self.options()?
            .sessions_dir
            .map(SessionStore::new)
            .ok_or(AdapterError::Unsupported(capability))
    }

    fn info(&self, options: &FakeOptions) -> HarnessInfo {
        let stored = options.sessions_dir.is_some();
        HarnessInfo {
            available: true,
            unavailable_reason: None,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            executable: None,
            capabilities: HarnessCapabilities {
                interrupt: true,
                steer: true,
                approvals: true,
                questions: true,
                resume: true,
                fork: stored,
                images: true,
                model_switch_live: true,
                native_sessions: stored,
                background_tasks: true,
                background_stop: true,
            },
            models: vec![
                Model {
                    id: "fake-fast".into(),
                    display_name: "Fake fast".into(),
                    description: None,
                    is_default: true,
                    effort_levels: None,
                    permission_modes: None,
                },
                Model {
                    id: "fake-slow".into(),
                    display_name: "Fake slow".into(),
                    description: None,
                    is_default: false,
                    effort_levels: None,
                    permission_modes: None,
                },
                // Runs in some permission modes only, like Claude Code's Haiku without auto
                // mode (`Model.permissionModes`).
                Model {
                    id: LIMITED_MODEL.into(),
                    display_name: "Fake lite".into(),
                    description: None,
                    is_default: false,
                    effort_levels: None,
                    permission_modes: Some(vec!["ask".into()]),
                },
            ],
            default_model: Some("fake-fast".into()),
            effort_levels: vec![
                EffortLevel {
                    id: "low".into(),
                    label: "Low".into(),
                },
                EffortLevel {
                    id: "high".into(),
                    label: "High".into(),
                },
            ],
            permission_modes: vec![
                PermissionMode {
                    id: "ask".into(),
                    label: "Ask".into(),
                    description: None,
                    is_default: true,
                },
                PermissionMode {
                    id: "auto".into(),
                    label: "Auto".into(),
                    description: None,
                    is_default: false,
                },
            ],
            default_permission_mode: Some("ask".into()),
        }
    }
}

#[async_trait]
impl HarnessAdapter for FakeAdapter {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn kind(&self) -> HarnessKind {
        HarnessKind::Fake
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    async fn probe(&self) -> HarnessInfo {
        let options = match self.options() {
            Ok(options) => options,
            Err(e) => return HarnessInfo::unavailable(e.to_string()),
        };
        match self.mode() {
            Ok(Mode::InProcess) => self.info(&options),
            Ok(Mode::Process { program }) => HarnessInfo {
                executable: Some(program),
                ..self.info(&options)
            },
            Err(e) => HarnessInfo::unavailable(e.to_string()),
        }
    }

    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError> {
        self.start_with(req, StartOptions::default()).await
    }

    async fn start_with(
        &self,
        req: StartRequest,
        start: StartOptions,
    ) -> Result<SessionHandle, AdapterError> {
        let sessions_dir = self
            .options()?
            .sessions_dir
            .map(|d| d.display().to_string());
        // Like the Claude and pi adapters, a fork gets its id from the adapter.
        let (session_id, resume, fork_from) = match &req.mode {
            StartMode::New => (uuid::Uuid::new_v4().to_string(), false, None),
            StartMode::Resume { native_session_id } => (native_session_id.clone(), true, None),
            StartMode::Fork { native_session_id } => {
                if sessions_dir.is_none() {
                    return Err(AdapterError::Unsupported("fork"));
                }
                (
                    uuid::Uuid::new_v4().to_string(),
                    false,
                    Some(native_session_id.clone()),
                )
            }
        };
        let fork_at = start.fork_at.as_ref().map(fork_cut).transpose()?;
        let hello = Op::Hello {
            session_id,
            resume,
            fork_from,
            fork_at,
            sessions_dir,
            modes: Modes {
                plan: start.modes.plan,
                fast: start.modes.fast,
            },
            project_trusted: start.project_trusted,
        };
        let options = AgentOptions {
            cwd: req.cwd.clone(),
            chunk_size: agent::DEFAULT_CHUNK_SIZE,
        };
        let (exit, reader, writer): (
            ExitSource,
            Box<dyn AsyncRead + Send + Unpin>,
            Box<dyn AsyncWrite + Send + Unpin>,
        ) = match self.mode()? {
            Mode::InProcess => {
                let (adapter_out, agent_in) = tokio::io::duplex(IN_PROCESS_PIPE_BYTES);
                let (agent_out, adapter_in) = tokio::io::duplex(IN_PROCESS_PIPE_BYTES);
                let task = tokio::spawn(agent::run_agent(agent_in, agent_out, options));
                (
                    ExitSource::task(task),
                    Box::new(adapter_in),
                    Box::new(adapter_out),
                )
            }
            Mode::Process { program } => {
                let mut spec = SpawnSpec::new(
                    format!("{}[{}]", self.config.id, req.thread_id),
                    program,
                    &req.cwd,
                )
                .args(self.config.args.iter().cloned())
                .arg("agent")
                .owner(req.thread_id.as_str());
                for (k, v) in &self.config.env {
                    spec = spec.env(k, v);
                }
                let mut child = self
                    .ctx
                    .supervisor
                    .spawn(spec)
                    .await
                    .map_err(|e| AdapterError::Spawn(e.to_string()))?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| AdapterError::Spawn("no stdin".into()))?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| AdapterError::Spawn("no stdout".into()))?;
                (
                    ExitSource::Process(child.handle),
                    Box::new(stdout),
                    Box::new(stdin),
                )
            }
        };
        FakeSession::start(
            reader,
            writer,
            exit,
            hello,
            req.settings,
            start.modes,
            self.ctx.policy.clone(),
        )
        .await
    }

    /// The checks of [`fork_cut`].
    fn check_fork_point(&self, point: &ForkPoint) -> Result<(), AdapterError> {
        fork_cut(point).map(|_| ())
    }

    /// Like Claude Code's adapter, the fake takes the permission mode `plan` (which it does not
    /// offer) as plan mode over the default permission mode, so that the engine's handling of
    /// settings of an earlier form is exercised.
    fn upgrade_settings(&self, settings: ThreadSettings) -> UpgradedSettings {
        if settings.permission_mode.as_deref() != Some(LEGACY_PLAN_MODE) {
            return UpgradedSettings {
                settings,
                plan: false,
            };
        }
        UpgradedSettings {
            settings: ThreadSettings {
                permission_mode: None,
                ..settings
            },
            plan: true,
        }
    }

    /// Everything the port offers; the ones that need stored sessions (forks at a turn, forks of
    /// held sessions) only with `sessionsDir`.
    fn features(&self) -> HarnessFeatures {
        let stored = self
            .options()
            .is_ok_and(|options| options.sessions_dir.is_some());
        HarnessFeatures {
            fork_at_turn: stored,
            fork_while_held: stored,
            rename: true,
            side_question: true,
            move_to_background: true,
            status: true,
            project_trust: true,
            plan_mode: Some(PlanModeFeature {
                implement_prompt: Some(IMPLEMENT_PROMPT.into()),
                new_thread_preamble: Some(NEW_THREAD_PREAMBLE.into()),
            }),
            fast_mode_models: vec![agent::FAST_MODEL.into()],
        }
    }

    /// The fake agent's commands; `fake-project` only for a trusted project.
    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        Ok(agent::fake_commands(ctx.project_trusted))
    }

    /// `fake-clear` starts a new session in the running agent.
    fn session_switching_commands(&self) -> &'static [&'static str] {
        &[agent::SWITCH_COMMAND]
    }

    /// `fake-clear` and its alias from the command list.
    fn session_switching_names(&self) -> Vec<String> {
        std::iter::once(agent::SWITCH_COMMAND)
            .chain(agent::SWITCH_COMMAND_ALIASES.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    /// What the harness says without a session: where its sessions are and how it runs.
    async fn status(&self, _cwd: &Path) -> Result<Vec<StatusSection>, AdapterError> {
        let options = self.options()?;
        let row = |label: &str, value: String| StatusRow {
            label: label.into(),
            value,
        };
        Ok(vec![StatusSection {
            title: "Fake harness".into(),
            rows: vec![
                row(
                    "Sessions",
                    options
                        .sessions_dir
                        .map_or_else(|| "not stored".into(), |d| d.display().to_string()),
                ),
                row(
                    "Runs as",
                    match options.mode {
                        ModeOption::Process => "process".into(),
                        ModeOption::InProcess => "in process".into(),
                    },
                ),
            ],
        }])
    }

    async fn list_native_sessions(
        &self,
        cwd: &Path,
    ) -> Result<Vec<NativeSessionSummary>, AdapterError> {
        Ok(self
            .scan_native_sessions(cwd)
            .await?
            .into_logged_sessions(&self.config.id))
    }

    async fn scan_native_sessions(&self, cwd: &Path) -> Result<NativeSessionScan, AdapterError> {
        let store = self.store("nativeSessions")?;
        let cwd = cwd.to_path_buf();
        let policy = self.ctx.policy.clone();
        tokio::task::spawn_blocking(move || store.scan(&cwd, &policy))
            .await
            .map_err(|e| AdapterError::Other(format!("listing sessions failed: {e}")))?
            .map_err(store_error)
    }

    /// The anchor of each stored turn is its index in the transcript.
    async fn read_native_history_anchored(
        &self,
        cwd: &Path,
        id: &str,
    ) -> Result<(NativeHistory, Vec<Option<serde_json::Value>>), AdapterError> {
        let history = self.read_native_history(cwd, id).await?;
        let anchors = (0..history.turns.len())
            .map(|turn| Some(serde_json::json!({ "turn": turn })))
            .collect();
        Ok((history, anchors))
    }

    async fn read_native_history(
        &self,
        cwd: &Path,
        id: &str,
    ) -> Result<NativeHistory, AdapterError> {
        let store = self.store("nativeSessions")?;
        let cwd = cwd.to_path_buf();
        let id = id.to_owned();
        let policy = self.ctx.policy.clone();
        tokio::task::spawn_blocking(move || store.history(&cwd, &id, &policy))
            .await
            .map_err(|e| AdapterError::Other(format!("reading the session failed: {e}")))?
            .map_err(store_error)
    }
}

/// The stored turn a fake anchor names (`{"turn": <index>}`). A provisional anchor
/// (`{"pending": <index>}`, `@late-anchor`) cannot be branched at.
fn anchor_turn(anchor: &serde_json::Value) -> Result<usize, AdapterError> {
    if let Some(turn) = anchor["pending"].as_u64() {
        return Err(AdapterError::Harness(format!(
            "turn {turn} has not settled yet: it can be branched at once the next turn has started"
        )));
    }
    anchor["turn"]
        .as_u64()
        .map(|n| n as usize)
        .ok_or_else(|| AdapterError::Harness(format!("{anchor} is not an anchor of this harness")))
}

/// Where the stored agent cuts a fork at `point`, from the anchors this adapter reported
/// (`{"turn": <index>}`): with the turn, right after it; before it, right after the turn before
/// (`previous`), like a CLI that cuts after the last message it keeps (Claude Code), so the
/// engine's choice of that turn is what a fork before a turn exercises. The turn's own anchor
/// must have settled either way.
fn fork_cut(point: &ForkPoint) -> Result<ForkAt, AdapterError> {
    let turn = anchor_turn(&point.anchor)?;
    if !point.before {
        return Ok(ForkAt {
            turn,
            before: false,
        });
    }
    let previous = point.previous.as_ref().ok_or_else(|| {
        AdapterError::Harness(
            "the turn before the fork point has no recorded anchor, so the fork cannot end right before it"
                .into(),
        )
    })?;
    Ok(ForkAt {
        turn: anchor_turn(previous)?,
        before: false,
    })
}

/// The provisional anchor of stored turn `turn` (`@late-anchor`).
fn provisional_anchor(turn: usize) -> serde_json::Value {
    serde_json::json!({ "pending": turn })
}

/// Where the session's exit information comes from.
#[derive(Clone)]
enum ExitSource {
    Process(ChildHandle),
    Task {
        done: watch::Receiver<Option<ExitInfo>>,
        abort: Arc<tokio::task::AbortHandle>,
    },
}

impl ExitSource {
    fn task(handle: tokio::task::JoinHandle<i32>) -> Self {
        let (tx, rx) = watch::channel(None);
        let abort = Arc::new(handle.abort_handle());
        tokio::spawn(async move {
            let (code, stopped) = match handle.await {
                Ok(code) => (Some(code), None),
                Err(_) => (None, Some(StopReason::Shutdown)),
            };
            tx.send_replace(Some(ExitInfo {
                code,
                stopped,
                stderr_tail: String::new(),
                exited_at_ms: now_ms(),
            }));
        });
        ExitSource::Task { done: rx, abort }
    }

    async fn wait(&self) -> ExitInfo {
        match self {
            ExitSource::Process(h) => h.wait().await,
            ExitSource::Task { done, .. } => {
                let mut rx = done.clone();
                loop {
                    if let Some(info) = rx.borrow_and_update().clone() {
                        return info;
                    }
                    if rx.changed().await.is_err() {
                        return ExitInfo {
                            code: None,
                            stopped: None,
                            stderr_tail: String::new(),
                            exited_at_ms: now_ms(),
                        };
                    }
                }
            }
        }
    }

    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
        match self {
            ExitSource::Process(h) => h.shutdown(grace, reason).await,
            ExitSource::Task { abort, .. } => {
                if let Ok(info) = tokio::time::timeout(grace, self.wait()).await {
                    return info;
                }
                abort.abort();
                let mut info = self.wait().await;
                info.stopped = Some(reason);
                info
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The agent's answer to a prompt (`Ev::PromptAck`): taken, or refused because a turn of its
/// own runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PromptAck {
    accepted: bool,
    own_run: bool,
}

/// Where the reader hands the answer to the prompt `send` waits for. Dropped (so `send` sees
/// the session closed) when the reader ends.
type AckSlot = Arc<parking_lot::Mutex<Option<oneshot::Sender<PromptAck>>>>;

/// Where the reader hands the answer (`Ev::SteerAck`: taken or not) to the steer that waits
/// for it. Dropped when the reader ends.
type SteerAckSlot = Arc<parking_lot::Mutex<Option<oneshot::Sender<bool>>>>;

/// Queries (`Op::Query`) waiting for their answer, by id. Emptied (so they see the session
/// closed) when the reader ends.
type QuerySlots = Arc<parking_lot::Mutex<HashMap<String, oneshot::Sender<Ev>>>>;

struct FakeSession {
    writer: SharedJsonLinesWriter,
    exit: ExitSource,
    policy: AdapterPolicy,
    model: Mutex<Option<String>>,
    modes: Mutex<ThreadModes>,
    ack: AckSlot,
    steer_ack: SteerAckSlot,
    queries: QuerySlots,
}

impl FakeSession {
    async fn start(
        reader: Box<dyn AsyncRead + Send + Unpin>,
        writer: Box<dyn AsyncWrite + Send + Unpin>,
        exit: ExitSource,
        hello: Op,
        settings: ThreadSettings,
        modes: ThreadModes,
        policy: AdapterPolicy,
    ) -> Result<SessionHandle, AdapterError> {
        let mut writer = JsonLinesWriter::new(writer);
        let mut reader = JsonLinesReader::new(reader, policy.max_line_bytes);
        writer
            .send(&hello)
            .await
            .map_err(|e| AdapterError::Spawn(e.to_string()))?;
        if settings.model.is_some() {
            writer
                .send(&Op::SetModel {
                    model: settings.model.clone(),
                })
                .await
                .map_err(|e| AdapterError::Spawn(e.to_string()))?;
        }
        // Handshake: wait for `ready`, keeping whatever arrives before it.
        let mut early = Vec::new();
        let ready = tokio::time::timeout(policy.handshake_timeout, async {
            loop {
                match reader.next().await {
                    Ok(Some(ReadLine::Json(v))) => match serde_json::from_value::<Ev>(v) {
                        Ok(Ev::Ready { session_id }) => return Ok(session_id),
                        Ok(Ev::Rejected { message }) => return Err(AdapterError::Harness(message)),
                        Ok(other) => early.push(other),
                        Err(e) => return Err(AdapterError::Protocol(e.to_string())),
                    },
                    Ok(Some(ReadLine::NotJson(line))) => {
                        tracing::debug!(line = %line, "the fake agent wrote a line that is not JSON during its handshake");
                    }
                    Ok(None) => {
                        return Err(AdapterError::Spawn(
                            "fake agent exited during handshake".into(),
                        ));
                    }
                    Err(e) => {
                        return Err(AdapterError::Spawn(format!(
                            "reading the fake agent's handshake failed: {e}"
                        )));
                    }
                }
            }
        })
        .await;
        let native_session_id = match ready {
            Ok(Ok(id)) => id,
            Ok(Err(e @ AdapterError::Harness(_))) => {
                // Rejected: the agent exits by itself, with the reason on its stderr too.
                let info = exit.shutdown(policy.stop_grace, StopReason::Shutdown).await;
                return Err(policy.with_stderr(e, &info.stderr_tail));
            }
            Ok(Err(e)) => {
                exit.shutdown(Duration::ZERO, StopReason::Shutdown).await;
                return Err(e);
            }
            Err(_) => {
                exit.shutdown(Duration::ZERO, StopReason::Shutdown).await;
                return Err(AdapterError::Spawn("fake agent handshake timed out".into()));
            }
        };

        let (tx, rx) = mpsc::unbounded_channel();
        for ev in early.into_iter().filter_map(map_event) {
            let _ = tx.send(ev);
        }
        let ack: AckSlot = Arc::new(parking_lot::Mutex::new(None));
        let steer_ack: SteerAckSlot = Arc::new(parking_lot::Mutex::new(None));
        let queries: QuerySlots = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let session = Arc::new(FakeSession {
            writer: SharedJsonLinesWriter::new(writer.into_inner()),
            exit: exit.clone(),
            policy,
            model: Mutex::new(settings.model),
            modes: Mutex::new(modes),
            ack: ack.clone(),
            steer_ack: steer_ack.clone(),
            queries: queries.clone(),
        });
        tokio::spawn(async move {
            loop {
                match reader.next().await {
                    Ok(Some(ReadLine::Json(v))) => match serde_json::from_value::<Ev>(v.clone()) {
                        // Every event before the answer (a turn the agent started by itself)
                        // has been handed on when `send` learns the answer.
                        Ok(Ev::PromptAck { accepted, own_run }) => match ack.lock().take() {
                            Some(waiter) => {
                                let _ = waiter.send(PromptAck { accepted, own_run });
                            }
                            None => {
                                tracing::warn!("the fake agent answered a prompt nobody waits for")
                            }
                        },
                        // A steer's answer comes after what the turn did before it: a returned
                        // steer (`steerReturned`) is handed on before the waiting call returns.
                        Ok(Ev::SteerAck { accepted }) => match steer_ack.lock().take() {
                            Some(waiter) => {
                                let _ = waiter.send(accepted);
                            }
                            None => {
                                tracing::warn!("the fake agent answered a steer nobody waits for")
                            }
                        },
                        Ok(ev @ Ev::QueryResult { .. }) => {
                            let Ev::QueryResult { id, .. } = &ev else {
                                unreachable!("matched above")
                            };
                            match queries.lock().remove(id) {
                                Some(waiter) => {
                                    let _ = waiter.send(ev);
                                }
                                None => {
                                    tracing::warn!(
                                        id,
                                        "the fake agent answered a query nobody waits for"
                                    )
                                }
                            }
                        }
                        Ok(ev) => {
                            if let Some(ev) = map_event(ev) {
                                let _ = tx.send(ev);
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(AdapterEvent::Native { payload: v });
                        }
                    },
                    Ok(Some(ReadLine::NotJson(line))) => {
                        let _ = tx.send(AdapterEvent::Native {
                            payload: serde_json::Value::String(line),
                        });
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "reading the fake agent's output failed; no further events are read");
                        let _ = tx.send(AdapterEvent::Notice {
                            level: aas_protocol::NoticeLevel::Error,
                            message: format!("reading the agent's output failed: {e}"),
                            code: None,
                        });
                        break;
                    }
                }
            }
            // A prompt, steer or query still waiting for its answer never gets one.
            ack.lock().take();
            steer_ack.lock().take();
            queries.lock().clear();
            let info = exit.wait().await;
            let _ = tx.send(AdapterEvent::Exited { info });
        });
        Ok(SessionHandle {
            native_session_id: Some(native_session_id),
            control: session,
            events: rx,
        })
    }

    async fn write(&self, op: Op) -> Result<(), AdapterError> {
        self.writer
            .send(&op)
            .await
            .map_err(|_| AdapterError::Closed)
    }

    /// Sends a steer and waits for the agent's answer (bounded by `handshake_timeout`): a steer
    /// that came after the turn ended is refused, like Claude's adapter refuses a message for
    /// a turn that completed (the engine then records nothing, and the client sends it again
    /// as a new turn).
    async fn steer_op(&self, op: Op) -> Result<(), AdapterError> {
        let (waiter, answer) = oneshot::channel();
        *self.steer_ack.lock() = Some(waiter);
        self.write(op).await?;
        let timeout = self.policy.handshake_timeout;
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(AdapterError::Other(
                "the turn ended before the message could be added to it; send it as a new message"
                    .into(),
            )),
            Ok(Err(_)) => Err(AdapterError::Closed),
            Err(_) => Err(AdapterError::Protocol(format!(
                "the fake agent did not answer the steer within {timeout:?}"
            ))),
        }
    }

    /// Asks `query` and waits for its answer (bounded by `handshake_timeout`).
    async fn query(&self, query: Query) -> Result<Ev, AdapterError> {
        let id = uuid::Uuid::new_v4().to_string();
        let (waiter, answer) = oneshot::channel();
        self.queries.lock().insert(id.clone(), waiter);
        if let Err(e) = self
            .write(Op::Query {
                id: id.clone(),
                query,
            })
            .await
        {
            self.queries.lock().remove(&id);
            return Err(e);
        }
        let timeout = self.policy.handshake_timeout;
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(ev)) => Ok(ev),
            Ok(Err(_)) => Err(AdapterError::Closed),
            Err(_) => {
                self.queries.lock().remove(&id);
                Err(AdapterError::Protocol(format!(
                    "the fake agent did not answer within {timeout:?}"
                )))
            }
        }
    }
}

fn image_paths(input: &TurnInput) -> Vec<String> {
    input
        .images()
        .map(|(path, _)| path.display().to_string())
        .collect()
}

/// The adapter event of an agent event (`None` for the answers to prompts, which go to the
/// waiting `send`).
fn map_event(ev: Ev) -> Option<AdapterEvent> {
    Some(match ev {
        Ev::PromptAck { .. } | Ev::SteerAck { .. } => return None,
        Ev::Ready { session_id } => AdapterEvent::SessionIdentified {
            native_session_id: session_id,
        },
        // Only valid as the answer to `hello` (handled by the handshake).
        Ev::Rejected { message } => AdapterEvent::Notice {
            level: NoticeLevel::Error,
            message,
            code: None,
        },
        Ev::SessionInfo {
            model,
            permission_mode,
            effort,
        } => AdapterEvent::SessionInfo {
            model,
            permission_mode,
            effort,
        },
        Ev::Modes { plan, fast_state } => AdapterEvent::ModesReported { plan, fast_state },
        Ev::Title { title } => AdapterEvent::SessionTitle { title },
        Ev::Anchor { turn } => AdapterEvent::TurnAnchor {
            anchor: serde_json::json!({ "turn": turn }),
        },
        Ev::ProvisionalAnchor { turn } => AdapterEvent::TurnAnchor {
            anchor: provisional_anchor(turn),
        },
        Ev::AnchorSettled { turn } => AdapterEvent::TurnAnchorReplaced {
            previous: provisional_anchor(turn),
            anchor: serde_json::json!({ "turn": turn }),
        },
        Ev::Backgroundable {
            key,
            backgroundable,
        } => AdapterEvent::ItemBackgroundable {
            key,
            backgroundable,
        },
        Ev::SteerReturned { message_id } => AdapterEvent::SteerReturned { message_id },
        Ev::EditorText { text } => AdapterEvent::ComposerText { text },
        // Answers to queries go to the waiting call.
        Ev::QueryResult { .. } => return None,
        Ev::Commands { commands } => AdapterEvent::CommandsChanged { commands },
        Ev::TurnStarted => AdapterEvent::TurnStarted,
        Ev::ItemStarted { key, body } => AdapterEvent::ItemStarted { key, body },
        Ev::Delta { key, field, text } => AdapterEvent::ItemDelta { key, field, text },
        Ev::ItemUpdated { key, body } => AdapterEvent::ItemUpdated { key, body },
        Ev::ItemCompleted { key, status, body } => {
            AdapterEvent::ItemCompleted { key, body, status }
        }
        Ev::Request {
            request_id,
            request,
            item_key,
            background_key,
        } => AdapterEvent::InteractionRequested {
            request_id,
            request,
            item_key,
            background_key,
        },
        Ev::Withdraw { request_id } => AdapterEvent::InteractionWithdrawn { request_id },
        Ev::Usage { usage } => AdapterEvent::TurnUsage { usage },
        Ev::TurnCompleted {
            status,
            usage,
            error,
            trigger,
        } => AdapterEvent::TurnCompleted {
            status,
            usage,
            error,
            trigger,
        },
        Ev::Notice { level, message } => AdapterEvent::Notice {
            level,
            message,
            code: None,
        },
        Ev::Background { task } => AdapterEvent::BackgroundTask {
            task: Box::new(task),
        },
        Ev::BackgroundOutput { key, text, replace } => AdapterEvent::BackgroundOutput {
            key,
            output: if replace {
                OutputUpdate::Replace(text)
            } else {
                OutputUpdate::Append(text)
            },
        },
    })
}

#[async_trait]
impl SessionControl for FakeSession {
    /// Waits for the agent's answer to the prompt: a prompt refused because the agent runs a
    /// turn of its own is [`AdapterError::TurnInProgress`] (that turn's `TurnStarted` has been
    /// emitted before).
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        let (waiter, answer) = oneshot::channel();
        *self.ack.lock() = Some(waiter);
        self.write(Op::Prompt {
            text: input.to_plain_text(),
            images: image_paths(&input),
        })
        .await?;
        let timeout = self.policy.handshake_timeout;
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(PromptAck { accepted: true, .. })) => Ok(()),
            Ok(Ok(PromptAck { own_run: true, .. })) => Err(AdapterError::TurnInProgress),
            Ok(Ok(_)) => Err(AdapterError::Harness("a turn is already running".into())),
            Ok(Err(_)) => Err(AdapterError::Closed),
            Err(_) => Err(AdapterError::Protocol(format!(
                "the fake agent did not answer the prompt within {timeout:?}"
            ))),
        }
    }

    async fn steer(&self, input: TurnInput) -> Result<(), AdapterError> {
        self.steer_op(Op::Steer {
            text: input.to_plain_text(),
            images: image_paths(&input),
            message_id: None,
        })
        .await
    }

    /// Taken into the running turn, which takes it in at its next pause or returns it before
    /// its completion (`SteerReturned`); refused when the turn is over.
    async fn steer_message(&self, message_id: &str, input: TurnInput) -> Result<(), AdapterError> {
        self.steer_op(Op::Steer {
            text: input.to_plain_text(),
            images: image_paths(&input),
            message_id: Some(message_id.to_owned()),
        })
        .await
    }

    async fn apply_modes(&self, modes: &ThreadModes) -> Result<SettingsApplied, AdapterError> {
        let mut current = self.modes.lock().await;
        if *current != *modes {
            *current = *modes;
            drop(current);
            self.write(Op::SetModes {
                modes: Modes {
                    plan: modes.plan,
                    fast: modes.fast,
                },
            })
            .await?;
        }
        Ok(SettingsApplied::Live)
    }

    async fn rename(&self, title: &str) -> Result<(), AdapterError> {
        self.write(Op::Rename {
            title: title.to_owned(),
        })
        .await
    }

    async fn status(&self) -> Result<Vec<StatusSection>, AdapterError> {
        match self.query(Query::Status).await? {
            Ev::QueryResult { sections, .. } => Ok(sections),
            other => Err(AdapterError::Protocol(format!(
                "a status answered with {other:?}"
            ))),
        }
    }

    async fn side_question(&self, question: &str) -> Result<SideAnswer, AdapterError> {
        match self
            .query(Query::SideQuestion {
                question: question.to_owned(),
            })
            .await?
        {
            Ev::QueryResult { answer, .. } => Ok(SideAnswer {
                answer,
                synthetic: false,
            }),
            other => Err(AdapterError::Protocol(format!(
                "a side question answered with {other:?}"
            ))),
        }
    }

    async fn move_to_background(&self, item_key: &str) -> Result<(), AdapterError> {
        self.write(Op::Background {
            key: item_key.to_owned(),
        })
        .await
    }

    async fn interrupt(&self) -> Result<(), AdapterError> {
        // Bounded like the real adapters: an agent that no longer reads its input must not
        // hold the engine's forced stop.
        let grace = self.policy.stop_grace;
        match tokio::time::timeout(grace, self.write(Op::Interrupt)).await {
            Ok(written) => written,
            Err(_) => Err(AdapterError::Protocol(format!(
                "the fake agent did not read the interrupt within {grace:?}"
            ))),
        }
    }

    async fn respond(
        &self,
        request_id: &str,
        resolution: &InteractionResolution,
    ) -> Result<(), AdapterError> {
        self.write(Op::Respond {
            request_id: request_id.to_owned(),
            resolution: resolution.clone(),
        })
        .await
    }

    async fn apply_settings(
        &self,
        settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError> {
        let mut model = self.model.lock().await;
        if *model != settings.model {
            *model = settings.model.clone();
            drop(model);
            self.write(Op::SetModel {
                model: settings.model.clone(),
            })
            .await?;
        }
        Ok(SettingsApplied::Live)
    }

    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        self.writer.close().await;
        self.exit.shutdown(self.policy.stop_grace, reason).await
    }

    async fn stop_background(&self, key: &str) -> Result<(), AdapterError> {
        self.write(Op::StopBackground {
            key: key.to_owned(),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> AdapterContext {
        AdapterContext {
            supervisor: aas_supervisor::Supervisor::new(
                &dir.join("state"),
                aas_supervisor::SupervisorPolicy {
                    prevent_sleep: false,
                    ..Default::default()
                },
            )
            .unwrap(),
            state_dir: dir.join("adapter"),
            policy: AdapterPolicy {
                stop_grace: Duration::from_millis(500),
                ..Default::default()
            },
        }
    }

    async fn start(adapter: &FakeAdapter, cwd: &Path) -> SessionHandle {
        adapter
            .start(StartRequest {
                thread_id: ThreadId::from("thr_test"),
                cwd: cwd.to_path_buf(),
                settings: ThreadSettings::default(),
                mode: StartMode::New,
            })
            .await
            .unwrap()
    }

    async fn next_turn(events: &mut mpsc::UnboundedReceiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut out = Vec::new();
        while let Some(ev) = events.recv().await {
            let done = matches!(
                ev,
                AdapterEvent::TurnCompleted { .. } | AdapterEvent::Exited { .. }
            );
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn echo_turn_streams_and_completes() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        assert!(session.native_session_id.is_some());
        session.control.send(TurnInput::text("hi")).await.unwrap();
        let events = next_turn(&mut session.events).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::ItemDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "echo: hi");
        assert!(matches!(
            events.last(),
            Some(AdapterEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
        let info = session.control.shutdown(StopReason::User).await;
        assert_eq!(info.code, Some(0));
        let rest: Vec<_> = std::iter::from_fn(|| session.events.try_recv().ok()).collect();
        assert!(
            matches!(rest.last(), Some(AdapterEvent::Exited { .. }))
                || session.events.recv().await.is_some()
        );
    }

    #[tokio::test]
    async fn approval_round_trip_and_interrupt() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        session
            .control
            .send(TurnInput::text("@approve cargo test"))
            .await
            .unwrap();
        let request_id = loop {
            match session.events.recv().await.unwrap() {
                AdapterEvent::InteractionRequested { request_id, .. } => break request_id,
                _ => continue,
            }
        };
        session
            .control
            .respond(
                &request_id,
                &InteractionResolution::Approval {
                    option_id: "allow".into(),
                    feedback: None,
                },
            )
            .await
            .unwrap();
        let events = next_turn(&mut session.events).await;
        assert!(events.iter().any(|e| matches!(
            e,
            AdapterEvent::ItemCompleted {
                status: ItemStatus::Completed,
                body: Some(ItemBody::CommandExecution {
                    exit_code: Some(0),
                    ..
                }),
                ..
            }
        )));

        session
            .control
            .send(TurnInput::text("@stream 1000 10"))
            .await
            .unwrap();
        // wait for the first delta, then interrupt
        loop {
            if let AdapterEvent::ItemDelta { .. } = session.events.recv().await.unwrap() {
                break;
            }
        }
        session.control.interrupt().await.unwrap();
        let events = next_turn(&mut session.events).await;
        assert!(matches!(
            events.last(),
            Some(AdapterEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn the_context_directive_reports_context_usage() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        session
            .control
            .send(TurnInput::text(
                "@context 1500 10000
@text hi",
            ))
            .await
            .unwrap();
        let events = next_turn(&mut session.events).await;
        let expected = Some(aas_protocol::ContextUsage {
            used_tokens: 1500,
            window_tokens: 10000,
        });
        let reported: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::TurnUsage { usage } => Some(usage.context),
                _ => None,
            })
            .collect();
        assert_eq!(
            reported.first(),
            Some(&expected),
            "reported right away: {events:?}"
        );
        match events.last() {
            Some(AdapterEvent::TurnCompleted {
                usage: Some(usage), ..
            }) => assert_eq!(usage.context, expected),
            other => panic!("expected TurnCompleted with usage, got {other:?}"),
        }
        // Without the directive nothing is reported.
        session
            .control
            .send(TurnInput::text("plain"))
            .await
            .unwrap();
        let events = next_turn(&mut session.events).await;
        assert!(
            matches!(events.last(), Some(AdapterEvent::TurnCompleted { usage: Some(u), .. }) if u.context.is_none())
        );
    }

    #[tokio::test]
    async fn attached_images_are_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("photo.png");
        std::fs::write(&image, [0u8; 1234]).unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        assert!(adapter.probe().await.capabilities.images);
        let mut session = start(&adapter, dir.path()).await;
        let input = TurnInput {
            parts: vec![
                TurnInputPart::Text("look".into()),
                TurnInputPart::Image {
                    path: image.clone(),
                    mime: "image/png".into(),
                },
                TurnInputPart::Image {
                    path: dir.path().join("missing.png"),
                    mime: "image/png".into(),
                },
            ],
        };
        session.control.send(input).await.unwrap();
        let events = next_turn(&mut session.events).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::ItemDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text,
            "echo: lookreceived 2 images (1234 bytes), 1 unreadable"
        );
    }

    #[tokio::test]
    async fn crash_emits_exited_with_code() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        session
            .control
            .send(TurnInput::text("@crash 7"))
            .await
            .unwrap();
        let events = next_turn(&mut session.events).await;
        match events.last() {
            Some(AdapterEvent::Exited { info }) => assert_eq!(info.code, Some(7)),
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    fn stored(dir: &Path) -> FakeAdapter {
        FakeAdapter::new(
            HarnessConfig {
                id: "fake".into(),
                kind: HarnessKind::Fake,
                display_name: None,
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
                options: serde_json::json!({
                    "mode": "inProcess",
                    "sessionsDir": dir.join("store").display().to_string(),
                }),
            },
            ctx(dir),
        )
    }

    async fn start_mode(
        adapter: &FakeAdapter,
        cwd: &Path,
        mode: StartMode,
    ) -> Result<SessionHandle, AdapterError> {
        adapter
            .start(StartRequest {
                thread_id: ThreadId::from("thr_test"),
                cwd: cwd.to_path_buf(),
                settings: ThreadSettings::default(),
                mode,
            })
            .await
    }

    async fn turn(session: &mut SessionHandle, text: &str) -> Vec<AdapterEvent> {
        session.control.send(TurnInput::text(text)).await.unwrap();
        next_turn(&mut session.events).await
    }

    fn prompts(history: &NativeHistory) -> Vec<String> {
        history
            .turns
            .iter()
            .map(|t| match &t.items[0].body {
                ItemBody::UserMessage { text, .. } => text.clone(),
                other => panic!("a turn starts with the user's message: {other:?}"),
            })
            .collect()
    }

    /// With a session store the fake behaves like a CLI with native sessions: what it ran can be
    /// listed and read back, resumed by id, and branched off into a new session.
    #[tokio::test]
    async fn stored_sessions_are_listed_read_resumed_and_forked() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let adapter = stored(dir.path());
        let caps = adapter.probe().await.capabilities;
        assert!(caps.fork && caps.native_sessions, "{caps:?}");

        let mut session = start_mode(&adapter, &cwd, StartMode::New).await.unwrap();
        let id = session.native_session_id.clone().unwrap();
        turn(&mut session, "Say hello\n@text hello there").await;
        turn(&mut session, "@exec cargo test\n@plan").await;
        session.control.shutdown(StopReason::User).await;

        let listed = adapter.list_native_sessions(&cwd).await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].native_session_id, id);
        assert_eq!(listed[0].title.as_deref(), Some("Say hello"));
        assert!(
            adapter
                .list_native_sessions(dir.path())
                .await
                .unwrap()
                .is_empty(),
            "sessions belong to the folder they ran in"
        );

        let history = adapter.read_native_history(&cwd, &id).await.unwrap();
        assert_eq!(history.title.as_deref(), Some("Say hello"));
        assert_eq!(history.turns.len(), 2);
        let kinds: Vec<Vec<&str>> = history
            .turns
            .iter()
            .map(|t| t.items.iter().map(|i| i.body.kind_str()).collect())
            .collect();
        assert_eq!(
            kinds,
            vec![
                // Plain lines of a prompt become a message of their own.
                vec!["userMessage", "agentMessage", "agentMessage"],
                vec!["userMessage", "commandExecution", "plan"],
            ]
        );
        assert!(matches!(
            &history.turns[0].items[2],
            HistoryItem { body: ItemBody::AgentMessage { text }, status: ItemStatus::Completed } if text == "hello there"
        ));
        assert!(matches!(
            &history.turns[1].items[1].body,
            ItemBody::CommandExecution { output, exit_code: Some(0), .. } if output == "ran cargo test\n"
        ));
        assert!(history.turns.iter().all(|t| t.started_at <= t.completed_at));

        // Resumed by id, the session goes on in the same transcript.
        let mut resumed = start_mode(
            &adapter,
            &cwd,
            StartMode::Resume {
                native_session_id: id.clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(resumed.native_session_id.as_deref(), Some(id.as_str()));
        turn(&mut resumed, "third").await;
        resumed.control.shutdown(StopReason::User).await;

        // A fork is a new session with the turns so far; later turns stay on their side.
        let mut fork = start_mode(
            &adapter,
            &cwd,
            StartMode::Fork {
                native_session_id: id.clone(),
            },
        )
        .await
        .unwrap();
        let fork_id = fork.native_session_id.clone().unwrap();
        assert_ne!(fork_id, id);
        turn(&mut fork, "only in the fork").await;
        fork.control.shutdown(StopReason::User).await;
        let source = adapter.read_native_history(&cwd, &id).await.unwrap();
        let branch = adapter.read_native_history(&cwd, &fork_id).await.unwrap();
        assert_eq!(
            prompts(&source),
            vec![
                "Say hello\n@text hello there",
                "@exec cargo test\n@plan",
                "third"
            ]
        );
        assert_eq!(
            prompts(&branch),
            vec![
                "Say hello\n@text hello there",
                "@exec cargo test\n@plan",
                "third",
                "only in the fork"
            ]
        );
        assert_eq!(adapter.list_native_sessions(&cwd).await.unwrap().len(), 2);

        // Sessions that do not exist cannot be resumed, forked or read.
        let missing = StartMode::Resume {
            native_session_id: "no-such-session".into(),
        };
        assert!(matches!(
            start_mode(&adapter, &cwd, missing).await,
            Err(AdapterError::Harness(_))
        ));
        let missing = StartMode::Fork {
            native_session_id: "no-such-session".into(),
        };
        assert!(matches!(
            start_mode(&adapter, &cwd, missing).await,
            Err(AdapterError::Harness(_))
        ));
        assert!(matches!(
            adapter.read_native_history(&cwd, "../escape").await,
            Err(AdapterError::Harness(_))
        ));
    }

    /// What a turn did before the agent crashed stays in the transcript, as a failed turn.
    #[tokio::test]
    async fn a_crashed_turn_keeps_what_it_did() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = stored(dir.path());
        let mut session = start_mode(&adapter, dir.path(), StartMode::New)
            .await
            .unwrap();
        let id = session.native_session_id.clone().unwrap();
        let events = turn(&mut session, "@text partial\n@crash 5").await;
        assert!(matches!(events.last(), Some(AdapterEvent::Exited { .. })));
        let history = adapter.read_native_history(dir.path(), &id).await.unwrap();
        assert_eq!(history.turns.len(), 1);
        assert!(matches!(
            &history.turns[0].items[1].body,
            ItemBody::AgentMessage { text } if text == "partial"
        ));
    }

    #[tokio::test]
    async fn without_a_store_there_is_no_fork_and_no_native_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let caps = adapter.probe().await.capabilities;
        assert!(!caps.fork && !caps.native_sessions, "{caps:?}");
        let fork = StartMode::Fork {
            native_session_id: "x".into(),
        };
        assert!(matches!(
            start_mode(&adapter, dir.path(), fork).await,
            Err(AdapterError::Unsupported("fork"))
        ));
        assert!(matches!(
            adapter.list_native_sessions(dir.path()).await,
            Err(AdapterError::Unsupported("nativeSessions"))
        ));
        assert!(matches!(
            adapter.read_native_history(dir.path(), "x").await,
            Err(AdapterError::Unsupported("nativeSessions"))
        ));
    }

    #[tokio::test]
    async fn invalid_options_make_the_harness_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        for options in [
            serde_json::json!({"mode": "inProcess", "sessionsDir": "relative/dir"}),
            serde_json::json!({"mode": "inProcess", "sessionDir": "C:\\typo"}),
            serde_json::json!({"mode": "sideways"}),
        ] {
            let adapter = FakeAdapter::new(
                HarnessConfig {
                    id: "fake".into(),
                    kind: HarnessKind::Fake,
                    display_name: None,
                    command: String::new(),
                    args: Vec::new(),
                    env: Default::default(),
                    options: options.clone(),
                },
                ctx(dir.path()),
            );
            let info = adapter.probe().await;
            assert!(!info.available, "{options}: {info:?}");
        }
    }

    /// `@bg` reports a task launched by an item that closes as backgrounded; the task runs on
    /// after the turn, reports its end with an exit code, and the agent starts a turn by itself
    /// about it (with its trigger). A prompt sent while that turn runs is refused as
    /// `TurnInProgress`, after the turn's start was reported.
    #[tokio::test]
    async fn background_tasks_outlive_their_turn_and_wake_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let caps = adapter.probe().await.capabilities;
        assert!(caps.background_tasks && caps.background_stop);
        let mut session = start(&adapter, dir.path()).await;
        let events = turn(
            &mut session,
            "@bg b1 kind=shell ms=50 progress=1 wake npm run build\n@sleep 300",
        )
        .await;
        let kinds: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::ItemStarted { key, .. } => Some(format!("start {key}")),
                AdapterEvent::BackgroundTask { task } => {
                    Some(format!("task {:?} live={}", task.state, task.live))
                }
                AdapterEvent::ItemCompleted { key, status, .. } => {
                    Some(format!("done {key} {status:?}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            &kinds[..3],
            &[
                "start k1".to_owned(),
                "task Running live=true".to_owned(),
                "done k1 Backgrounded".to_owned()
            ],
            "{kinds:?}"
        );
        let ended = events
            .iter()
            .find_map(|e| match e {
                AdapterEvent::BackgroundTask { task }
                    if task.state == BackgroundState::Completed =>
                {
                    Some(task.clone())
                }
                _ => None,
            })
            .expect("the task ended during the turn");
        assert!(!ended.live);
        assert_eq!(ended.result.unwrap().exit_code, Some(0));
        assert_eq!(ended.origin_item_key.as_deref(), Some("k1"));
        // The agent's own turn about it starts right away; the user's prompt waits.
        assert_eq!(
            session.control.send(TurnInput::text("next")).await,
            Err(AdapterError::TurnInProgress)
        );
        let own = next_turn(&mut session.events).await;
        assert_eq!(own.first(), Some(&AdapterEvent::TurnStarted));
        assert!(matches!(
            own.last(),
            Some(AdapterEvent::TurnCompleted {
                trigger: Some(TurnTrigger::BackgroundTask),
                ..
            })
        ));
        let next = turn(&mut session, "next").await;
        assert!(matches!(
            next.last(),
            Some(AdapterEvent::TurnCompleted { trigger: None, .. })
        ));
    }

    /// A task that runs until it is stopped ends when the adapter asks; its approval names it;
    /// a key nobody runs is reported, not guessed.
    #[tokio::test]
    async fn background_tasks_are_stopped_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        turn(&mut session, "@bg dev ms=0 approve detached dev server").await;
        let request = loop {
            match session.events.recv().await.unwrap() {
                AdapterEvent::InteractionRequested {
                    background_key,
                    request_id,
                    item_key,
                    ..
                } => {
                    assert_eq!(item_key, None);
                    break (request_id, background_key);
                }
                _ => continue,
            }
        };
        assert_eq!(request.1.as_deref(), Some("dev"));
        session.control.stop_background("dev").await.unwrap();
        let stopped = loop {
            if let AdapterEvent::BackgroundTask { task } = session.events.recv().await.unwrap()
                && task.state.is_ended()
            {
                break task;
            }
        };
        assert_eq!(stopped.state, BackgroundState::Stopped);
        assert_eq!(stopped.origin_item_key, None);
        session.control.stop_background("nobody").await.unwrap();
        assert!(matches!(
            session.events.recv().await.unwrap(),
            AdapterEvent::Notice { message, .. } if message.contains("nobody")
        ));
    }

    /// Events of one turn up to its completion (not after).
    async fn turn_events(session: &mut SessionHandle, text: &str) -> Vec<AdapterEvent> {
        turn(session, text).await
    }

    fn anchors(events: &[AdapterEvent]) -> Vec<serde_json::Value> {
        events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::TurnAnchor { anchor } => Some(anchor.clone()),
                _ => None,
            })
            .collect()
    }

    /// A stored session reports each turn's anchor (its index) before its completion, and a
    /// fork at a turn keeps the turns up to it (included, or not: `before`), the anchors of
    /// the copied turns staying valid.
    #[tokio::test]
    async fn turns_are_anchored_and_forks_branch_at_them() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let adapter = stored(dir.path());
        assert!(adapter.features().fork_at_turn && adapter.features().fork_while_held);
        let mut session = start_mode(&adapter, &cwd, StartMode::New).await.unwrap();
        let id = session.native_session_id.clone().unwrap();
        for (n, prompt) in ["one", "two", "three"].iter().enumerate() {
            let events = turn_events(&mut session, prompt).await;
            assert_eq!(anchors(&events), vec![serde_json::json!({ "turn": n })]);
            let anchor_at = events
                .iter()
                .position(|e| matches!(e, AdapterEvent::TurnAnchor { .. }))
                .unwrap();
            assert!(matches!(
                events[anchor_at + 1..].last(),
                Some(AdapterEvent::TurnCompleted { .. })
            ));
        }
        session.control.shutdown(StopReason::User).await;
        let (history, anchored) = adapter
            .read_native_history_anchored(&cwd, &id)
            .await
            .unwrap();
        assert_eq!(history.turns.len(), 3);
        assert_eq!(anchored[1], Some(serde_json::json!({ "turn": 1 })));

        let fork_at = |turn: usize, before: bool| StartOptions {
            fork_at: Some(ForkPoint {
                anchor: serde_json::json!({ "turn": turn }),
                before,
                previous: turn
                    .checked_sub(1)
                    .map(|p| serde_json::json!({ "turn": p })),
            }),
            ..StartOptions::default()
        };
        let fork = StartMode::Fork {
            native_session_id: id.clone(),
        };
        let request = |mode: StartMode| StartRequest {
            thread_id: ThreadId::from("thr_fork"),
            cwd: cwd.clone(),
            settings: ThreadSettings::default(),
            mode,
        };
        for (turn_at, before, kept) in [(1, false, 2), (1, true, 1), (2, false, 3)] {
            let mut branch = adapter
                .start_with(request(fork.clone()), fork_at(turn_at, before))
                .await
                .unwrap();
            let branch_id = branch.native_session_id.clone().unwrap();
            // The next turn of the branch comes right after what it kept.
            let events = turn_events(&mut branch, "next").await;
            assert_eq!(anchors(&events), vec![serde_json::json!({ "turn": kept })]);
            branch.control.shutdown(StopReason::User).await;
            let history = adapter.read_native_history(&cwd, &branch_id).await.unwrap();
            assert_eq!(
                history.turns.len(),
                kept + 1,
                "at {turn_at} before={before}"
            );
        }
        // An anchor that is not this harness's is refused, and so is a fork before a turn
        // without the anchor of the turn before it; the check says so without starting.
        let wrong = ForkPoint {
            anchor: serde_json::json!("t2"),
            before: false,
            previous: None,
        };
        let no_previous = ForkPoint {
            anchor: serde_json::json!({ "turn": 1 }),
            before: true,
            previous: None,
        };
        for point in [wrong, no_previous] {
            assert!(matches!(
                adapter.check_fork_point(&point),
                Err(AdapterError::Harness(_))
            ));
            let options = StartOptions {
                fork_at: Some(point),
                ..StartOptions::default()
            };
            assert!(matches!(
                adapter.start_with(request(fork.clone()), options).await,
                Err(AdapterError::Harness(_))
            ));
        }
        assert_eq!(
            adapter.check_fork_point(&fork_at(1, true).fork_at.unwrap()),
            Ok(())
        );
    }

    /// Under `@late-anchor` a turn reports a provisional anchor, which a fork refuses, and the
    /// agent's next turn settles it first thing (`TurnAnchorReplaced`); `@settle-anchor`
    /// settles it within the turn, and nothing more is reported at its end.
    #[tokio::test]
    async fn late_anchors_settle_at_the_next_turn() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let adapter = stored(dir.path());
        let mut session = start_mode(&adapter, &cwd, StartMode::New).await.unwrap();
        let id = session.native_session_id.clone().unwrap();
        let replaced = |events: &[AdapterEvent]| -> Vec<(serde_json::Value, serde_json::Value)> {
            events
                .iter()
                .filter_map(|e| match e {
                    AdapterEvent::TurnAnchorReplaced { previous, anchor } => {
                        Some((previous.clone(), anchor.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        let events = turn_events(&mut session, "@late-anchor\n@text one").await;
        assert_eq!(anchors(&events), vec![serde_json::json!({ "pending": 0 })]);
        assert!(replaced(&events).is_empty());

        let fork_at_first = StartOptions {
            fork_at: Some(ForkPoint {
                anchor: serde_json::json!({ "pending": 0 }),
                before: false,
                previous: None,
            }),
            ..StartOptions::default()
        };
        let request = StartRequest {
            thread_id: ThreadId::from("thr_fork"),
            cwd: cwd.clone(),
            settings: ThreadSettings::default(),
            mode: StartMode::Fork {
                native_session_id: id.clone(),
            },
        };
        match adapter.start_with(request, fork_at_first).await {
            Err(AdapterError::Harness(message)) => {
                assert!(message.contains("has not settled yet"), "{message}")
            }
            other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
        }

        let events = turn_events(&mut session, "@late-anchor\n@settle-anchor\n@text two").await;
        let first = events
            .iter()
            .position(|e| matches!(e, AdapterEvent::TurnAnchorReplaced { .. }))
            .unwrap();
        assert!(
            events[..first]
                .iter()
                .all(|e| matches!(e, AdapterEvent::TurnStarted)),
            "{events:?}"
        );
        assert_eq!(
            replaced(&events),
            vec![
                (
                    serde_json::json!({ "pending": 0 }),
                    serde_json::json!({ "turn": 0 })
                ),
                (
                    serde_json::json!({ "pending": 1 }),
                    serde_json::json!({ "turn": 1 })
                ),
            ]
        );
        assert_eq!(anchors(&events), vec![serde_json::json!({ "pending": 1 })]);
        session.control.shutdown(StopReason::User).await;
    }

    /// A session another process holds cannot be resumed (the CLI says so in colour, on its
    /// stderr too); it can still be forked.
    #[tokio::test]
    async fn a_held_session_is_refused_for_a_resume_but_forks() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let adapter = stored(dir.path());
        let mut session = start_mode(&adapter, &cwd, StartMode::New).await.unwrap();
        let id = session.native_session_id.clone().unwrap();
        turn_events(&mut session, "hello").await;
        session.control.shutdown(StopReason::User).await;
        SessionStore::new(dir.path().join("store"))
            .hold(&id)
            .unwrap();
        let resumed = start_mode(
            &adapter,
            &cwd,
            StartMode::Resume {
                native_session_id: id.clone(),
            },
        )
        .await;
        match resumed {
            Err(e @ AdapterError::Harness(_)) => {
                let text = e.to_string();
                assert!(text.contains("is held by another process"), "{text}");
                assert!(
                    !text.contains('\u{1b}'),
                    "escape sequences are removed: {text:?}"
                );
            }
            other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
        }
        let mut fork = start_mode(
            &adapter,
            &cwd,
            StartMode::Fork {
                native_session_id: id,
            },
        )
        .await
        .unwrap();
        turn_events(&mut fork, "in the fork").await;
        fork.control.shutdown(StopReason::User).await;
    }

    /// The agent's own reports: permission mode and effort it changed, plan mode it entered
    /// (its answers become proposed plans), fast mode, its name, a composer text, another
    /// session it switched to.
    #[tokio::test]
    async fn the_agent_reports_what_it_changed_by_itself() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        let first = session.native_session_id.clone().unwrap();
        let events = turn_events(
            &mut session,
            "@permission auto\n@effort high\n@plan-mode on\n@fast-state cooldown\n@rename Named\n@editor draft text\n@switch-session",
        )
        .await;
        let has = |f: &dyn Fn(&AdapterEvent) -> bool| events.iter().any(f);
        assert!(has(
            &|e| matches!(e, AdapterEvent::SessionInfo { permission_mode: Some(m), .. } if m == "auto")
        ));
        assert!(has(
            &|e| matches!(e, AdapterEvent::SessionInfo { effort: Some(m), .. } if m == "high")
        ));
        assert!(has(&|e| matches!(
            e,
            AdapterEvent::ModesReported {
                plan: Some(true),
                ..
            }
        )));
        assert!(has(
            &|e| matches!(e, AdapterEvent::ModesReported { fast_state: Some(s), .. } if s == "cooldown")
        ));
        assert!(has(
            &|e| matches!(e, AdapterEvent::SessionTitle { title } if title == "Named")
        ));
        assert!(has(
            &|e| matches!(e, AdapterEvent::ComposerText { text } if text == "draft text")
        ));
        assert!(has(
            &|e| matches!(e, AdapterEvent::SessionIdentified { native_session_id } if *native_session_id != first)
        ));
        // In plan mode a plain prompt is answered with a proposed plan.
        let events = turn_events(&mut session, "fix the bug").await;
        let plan: String = events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::ItemDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(plan.starts_with("1. Look into: fix the bug\n"), "{plan}");
        assert!(events.iter().any(|e| matches!(
            e,
            AdapterEvent::ItemStarted {
                body: ItemBody::ProposedPlan { .. },
                ..
            }
        )));
        // The harness's session switch, typed, does what the CLI would.
        let events = turn_events(&mut session, "/fake-reset").await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AdapterEvent::SessionIdentified { .. }))
        );
    }

    /// Modes switched by the adapter are reported; fast mode is on only with `fake-fast`. The
    /// status and side questions are answered right away, also while a turn runs. A rename is
    /// echoed.
    #[tokio::test]
    async fn modes_status_side_questions_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let features = adapter.features();
        assert_eq!(features.fast_mode_models, vec!["fake-fast".to_owned()]);
        assert!(
            features
                .plan_mode
                .as_ref()
                .unwrap()
                .implement_prompt
                .is_some()
        );
        assert_eq!(
            adapter.session_switching_names(),
            vec!["fake-clear".to_owned(), "fake-reset".to_owned()]
        );
        let mut session = start(&adapter, dir.path()).await;
        let modes = ThreadModes {
            plan: true,
            fast: true,
        };
        assert_eq!(
            session.control.apply_modes(&modes).await,
            Ok(SettingsApplied::Live)
        );
        let reported = loop {
            if let AdapterEvent::ModesReported { plan, fast_state } =
                session.events.recv().await.unwrap()
            {
                break (plan, fast_state);
            }
        };
        assert_eq!(reported, (Some(true), Some("on".into())));
        session
            .control
            .apply_settings(&ThreadSettings {
                model: Some("fake-slow".into()),
                ..ThreadSettings::default()
            })
            .await
            .unwrap();
        let fast = loop {
            if let AdapterEvent::ModesReported { fast_state, .. } =
                session.events.recv().await.unwrap()
            {
                break fast_state;
            }
        };
        assert_eq!(fast.as_deref(), Some("off"), "fake-slow has no fast mode");

        session
            .control
            .send(TurnInput::text("@sleep 300"))
            .await
            .unwrap();
        let answer = session.control.side_question("why?").await.unwrap();
        assert_eq!(answer.answer.as_deref(), Some("side answer: why?"));
        let status = session.control.status().await.unwrap();
        assert_eq!(status[0].title, "Fake agent");
        assert!(
            status[0]
                .rows
                .iter()
                .any(|r| r.label == "Plan mode" && r.value == "on")
        );
        next_turn(&mut session.events).await;
        session.control.rename("New name").await.unwrap();
        loop {
            if let AdapterEvent::SessionTitle { title } = session.events.recv().await.unwrap() {
                assert_eq!(title, "New name");
                break;
            }
        }
        let harness = adapter.status(dir.path()).await.unwrap();
        assert_eq!(harness[0].title, "Fake harness");
    }

    /// A running `@tool` command is reported backgroundable and moves to the background on
    /// request (its item closes as backgrounded after its task is reported); a steer under
    /// `@refuse-steers` is returned by its id; the project's trust decision reaches the agent.
    #[tokio::test]
    async fn running_work_moves_to_the_background_and_steers_can_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = adapter
            .start_with(
                StartRequest {
                    thread_id: ThreadId::from("thr_test"),
                    cwd: dir.path().to_path_buf(),
                    settings: ThreadSettings::default(),
                    mode: StartMode::New,
                },
                StartOptions {
                    project_trusted: Some(false),
                    ..StartOptions::default()
                },
            )
            .await
            .unwrap();
        session
            .control
            .send(TurnInput::text(
                "@refuse-steers\n@tool 5000 npm run dev\n@trust",
            ))
            .await
            .unwrap();
        let key = loop {
            if let AdapterEvent::ItemBackgroundable {
                key,
                backgroundable: true,
            } = session.events.recv().await.unwrap()
            {
                break key;
            }
        };
        session
            .control
            .steer_message("itm_steer", TurnInput::text("also this"))
            .await
            .unwrap();
        loop {
            if let AdapterEvent::SteerReturned { message_id } = session.events.recv().await.unwrap()
            {
                assert_eq!(message_id, "itm_steer");
                break;
            }
        }
        session.control.move_to_background(&key).await.unwrap();
        let events = next_turn(&mut session.events).await;
        let task_at = events
            .iter()
            .position(|e| matches!(e, AdapterEvent::BackgroundTask { task } if task.origin_item_key.as_deref() == Some(key.as_str())))
            .expect("the task is reported");
        let closed_at = events
            .iter()
            .position(|e| matches!(e, AdapterEvent::ItemCompleted { key: k, status: ItemStatus::Backgrounded, .. } if *k == key))
            .expect("the item closes as backgrounded");
        assert!(task_at < closed_at);
        assert!(events.iter().any(|e| matches!(
            e,
            AdapterEvent::ItemStarted { body: ItemBody::AgentMessage { text }, .. } if text == "project trusted: no"
        )));
    }

    /// A steer is taken while the turn runs: taken in at the turn's pause, or — left unread —
    /// returned before the turn's completion; one that comes after the turn is refused (the
    /// engine records nothing and the message is sent again as a new turn).
    #[tokio::test]
    async fn steers_are_taken_returned_or_refused() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        session
            .control
            .send(TurnInput::text("@await-steer\n@text done"))
            .await
            .unwrap();
        session
            .control
            .steer_message("m1", TurnInput::text("go faster"))
            .await
            .unwrap();
        let events = next_turn(&mut session.events).await;
        assert!(events.iter().any(
            |e| matches!(e, AdapterEvent::Notice { message, .. } if message == "steered: go faster")
        ));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AdapterEvent::SteerReturned { .. }))
        );

        session
            .control
            .send(TurnInput::text("@await-steer unread\n@text done"))
            .await
            .unwrap();
        session
            .control
            .steer_message("m2", TurnInput::text("too late"))
            .await
            .unwrap();
        let events = next_turn(&mut session.events).await;
        let returned = events
            .iter()
            .position(
                |e| matches!(e, AdapterEvent::SteerReturned { message_id } if message_id == "m2"),
            )
            .expect("returned");
        assert_eq!(
            returned + 1,
            events.len() - 1,
            "right before the completion"
        );
        assert!(!events.iter().any(
            |e| matches!(e, AdapterEvent::Notice { message, .. } if message.contains("too late"))
        ));

        // The turn is over: a steer is refused.
        assert!(matches!(
            session
                .control
                .steer_message("m3", TurnInput::text("after"))
                .await,
            Err(AdapterError::Other(m)) if m.contains("send it as a new message")
        ));
    }

    /// A shell's output streams while it runs (the launching command printed the early lines),
    /// as appended text or as snapshots, and its end brings the whole output.
    #[tokio::test]
    async fn background_output_streams_while_the_task_runs() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        session
            .control
            .send(TurnInput::text(
                "@bg b kind=shell ms=200 output=3 early=1 make\n@bg s kind=shell ms=200 output=2 snapshots detached serve",
            ))
            .await
            .unwrap();
        let mut events = Vec::new();
        let mut ended = 0;
        while ended < 2 {
            let ev = session.events.recv().await.unwrap();
            if matches!(&ev, AdapterEvent::BackgroundTask { task } if task.state.is_ended()) {
                ended += 1;
            }
            events.push(ev);
        }
        assert!(events.iter().any(|e| matches!(e,
            AdapterEvent::ItemDelta { field: DeltaField::Output, text, .. } if text == "b line 1\n")));
        let outputs = |key: &str| -> Vec<OutputUpdate> {
            events
                .iter()
                .filter_map(|e| match e {
                    AdapterEvent::BackgroundOutput { key: k, output } if k == key => {
                        Some(output.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            outputs("b"),
            [
                OutputUpdate::Append("b line 2\n".into()),
                OutputUpdate::Append("b line 3\n".into())
            ]
        );
        assert_eq!(
            outputs("s"),
            [
                OutputUpdate::Replace("s line 1\n".into()),
                OutputUpdate::Replace("s line 1\ns line 2\n".into())
            ]
        );
        let end = |key: &str| {
            events
                .iter()
                .find_map(|e| match e {
                    AdapterEvent::BackgroundTask { task }
                        if task.key == key && task.state.is_ended() =>
                    {
                        task.result.clone()
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(
            end("b").output.as_deref(),
            Some("b line 1\nb line 2\nb line 3\nran make\n")
        );
        assert_eq!(
            end("s").output.as_deref(),
            Some("s line 1\ns line 2\nran serve\n")
        );
    }

    /// `@wakeup` schedules an unstoppable wakeup launched by its item; when it comes due the
    /// agent runs its prompt in a turn of its own marked `scheduled`, `times` times.
    #[tokio::test]
    async fn wakeups_run_their_prompt_when_they_come_due() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        let events = turn(&mut session, "@wakeup 50 times=2 @text woke").await;
        let task = events
            .iter()
            .find_map(|e| match e {
                AdapterEvent::BackgroundTask { task } => Some(task.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(task.key, "wakeup:k1");
        assert!(!task.stoppable && task.next_run_at.is_some());
        assert!(events.iter().any(|e| matches!(e,
            AdapterEvent::ItemCompleted { key, status: ItemStatus::Backgrounded, .. } if key == "k1")));
        // (The first run may come due before the scheduling turn's completion is read.)
        let mut all = events.clone();
        for _ in 0..2 {
            let own = next_turn(&mut session.events).await;
            assert!(own.iter().any(|e| matches!(
                e,
                AdapterEvent::ItemStarted {
                    body: ItemBody::AgentMessage { .. },
                    ..
                }
            )));
            assert!(matches!(
                own.last(),
                Some(AdapterEvent::TurnCompleted {
                    trigger: Some(TurnTrigger::Scheduled),
                    ..
                })
            ));
            all.extend(own);
        }
        // Each run completed when it came due (before the turn it started).
        let ends: Vec<u32> = all
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::BackgroundTask { task }
                    if task.key == "wakeup:k1" && task.state == BackgroundState::Completed =>
                {
                    Some(task.runs)
                }
                _ => None,
            })
            .collect();
        assert_eq!(ends, [1, 2]);
    }

    /// `@dialog` asks after the turn's completion, naming neither an item nor a task; the answer
    /// reaches the agent, which reports it.
    #[tokio::test]
    async fn dialogs_are_asked_after_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = FakeAdapter::in_process("fake", ctx(dir.path()));
        let mut session = start(&adapter, dir.path()).await;
        let events = turn(&mut session, "@dialog Deploy now?\n@text asked").await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AdapterEvent::InteractionRequested { .. }))
        );
        let (request_id, item_key, background_key) = match session.events.recv().await.unwrap() {
            AdapterEvent::InteractionRequested {
                request_id,
                item_key,
                background_key,
                request: InteractionRequest::Question { title, .. },
            } => {
                assert_eq!(title, "Deploy now?");
                (request_id, item_key, background_key)
            }
            other => panic!("{other:?}"),
        };
        assert_eq!((item_key, background_key), (None, None));
        session
            .control
            .respond(
                &request_id,
                &InteractionResolution::Question {
                    answers: vec![aas_protocol::types::QuestionAnswer {
                        question_id: "choice".into(),
                        choice_ids: vec!["yes".into()],
                        text: None,
                    }],
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            session.events.recv().await.unwrap(),
            AdapterEvent::Notice { message, .. } if message == "dialog Deploy now?: yes"
        ));
    }

    /// A recorded session is what a user of the CLI on the PC leaves behind: requests are
    /// answered by the scripted user, and the session is ready to be listed and imported.
    #[tokio::test]
    async fn recorded_sessions_can_be_imported() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("pc");
        std::fs::create_dir_all(&cwd).unwrap();
        let adapter = stored(dir.path());
        let id = agent::record_session(
            &dir.path().join("store"),
            &cwd,
            &[
                "Fix the build\n@approve cargo build".to_owned(),
                "@question".to_owned(),
            ],
        )
        .await
        .unwrap();
        let listed = adapter.list_native_sessions(&cwd).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].native_session_id, id);
        assert_eq!(listed[0].title.as_deref(), Some("Fix the build"));
        let history = adapter.read_native_history(&cwd, &id).await.unwrap();
        assert!(matches!(
            &history.turns[0].items[2],
            HistoryItem {
                body: ItemBody::CommandExecution {
                    exit_code: Some(0),
                    ..
                },
                status: ItemStatus::Completed
            }
        ));
        assert!(matches!(
            &history.turns[1].items[1].body,
            ItemBody::AgentMessage { text } if text == "answer: red"
        ));
    }
}
