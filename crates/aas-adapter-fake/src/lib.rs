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

pub mod agent;
pub mod background;
pub mod store;
pub mod wire;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aas_harness::*;
use aas_protocol::types::{EffortLevel, HarnessCapabilities, Model, PermissionMode};
use aas_stdio::{JsonLinesReader, JsonLinesWriter, ReadLine, SharedJsonLinesWriter};
use aas_supervisor::{ChildHandle, SpawnSpec};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::agent::AgentOptions;
use crate::store::{SessionStore, StoreError};
use crate::wire::{Ev, Op};

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
        | StoreError::OtherFolder { .. } => AdapterError::Harness(e.to_string()),
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
                },
                Model {
                    id: "fake-slow".into(),
                    display_name: "Fake slow".into(),
                    description: None,
                    is_default: false,
                    effort_levels: None,
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
        let hello = Op::Hello {
            session_id,
            resume,
            fork_from,
            sessions_dir,
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
            self.ctx.policy.clone(),
        )
        .await
    }

    async fn commands(&self, _ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        Ok(vec![Command {
            name: "fake-help".into(),
            description: Some("Show the fake agent's scenario directives".into()),
            source: aas_protocol::CommandSource::Harness,
            argument_hint: None,
            action: aas_protocol::CommandAction::InsertText {
                text: "/fake-help ".into(),
            },
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

struct FakeSession {
    writer: SharedJsonLinesWriter,
    exit: ExitSource,
    policy: AdapterPolicy,
    model: Mutex<Option<String>>,
    ack: AckSlot,
}

impl FakeSession {
    async fn start(
        reader: Box<dyn AsyncRead + Send + Unpin>,
        writer: Box<dyn AsyncWrite + Send + Unpin>,
        exit: ExitSource,
        hello: Op,
        settings: ThreadSettings,
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
                // Rejected: the agent exits by itself.
                exit.shutdown(policy.stop_grace, StopReason::Shutdown).await;
                return Err(e);
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
        let session = Arc::new(FakeSession {
            writer: SharedJsonLinesWriter::new(writer.into_inner()),
            exit: exit.clone(),
            policy,
            model: Mutex::new(settings.model),
            ack: ack.clone(),
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
            // A prompt still waiting for its answer never gets one.
            ack.lock().take();
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
        Ev::PromptAck { .. } => return None,
        Ev::Ready { session_id } => AdapterEvent::SessionIdentified {
            native_session_id: session_id,
        },
        // Only valid as the answer to `hello` (handled by the handshake).
        Ev::Rejected { message } => AdapterEvent::Notice {
            level: NoticeLevel::Error,
            message,
            code: None,
        },
        Ev::SessionInfo { model } => AdapterEvent::SessionInfo {
            model,
            permission_mode: None,
            effort: None,
        },
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
        self.write(Op::Steer {
            text: input.to_plain_text(),
            images: image_paths(&input),
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
