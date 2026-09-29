//! Adapter for the pi coding agent (`pi --mode rpc`). See `docs/adapters/pi.md`.
//!
//! One pi process per session, driven over pi's RPC protocol (strict LF-delimited JSONL).
//! pi has no approval prompts by design; the adapter ships an extension (`aas-gate`, see
//! [`gate`]) that asks through pi's extension UI protocol according to the thread's
//! permission mode, and adds the commands pi's RPC mode lacks (`/reload`, the fork at an entry).
//!
//! Features beyond the capabilities (`docs/adapters/pi.md` §13): forks at any turn (anchors
//! from `get_entries`, see [`anchor`]), the native session name (`set_session_name`,
//! `session_info_changed`), the session's status (`get_state`, `get_session_stats`), and the
//! user's per-project trust decision (`--approve` / `--no-approve`).

mod anchor;
mod commands;
mod gate;
mod mapping;
mod native;
mod paths;
mod session;
mod status;
mod tools;
mod wire;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Weak;

use aas_harness::{
    AdapterContext, AdapterError, AdapterEvent, Command, CommandContext, ForkPoint, HarnessAdapter,
    HarnessCapabilities, HarnessConfig, HarnessFeatures, HarnessInfo, HarnessKind, NativeHistory,
    NativeSessionScan, NativeSessionSummary, PermissionMode, SessionControl, SessionHandle,
    StartGuard, StartMode, StartOptions, StartRequest, StopReason, stderr_excerpt,
};
use aas_supervisor::{SpawnSpec, ToolSpec, resolve_program};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

pub use anchor::{Anchor, ForkTarget, fork_target};
pub use gate::{MODE_ASK, MODE_ASK_COMMANDS, MODE_AUTO};
pub use session::{ChildLink, PiSession, ProcessLink, SessionConfig, handshake};

/// `[harness.options]` of a pi harness.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiOptions {
    /// Permission mode for threads that do not choose one (`ask`, `askCommands`, `auto`).
    /// Defaults to `ask`.
    #[serde(default)]
    pub default_permission_mode: Option<String>,
    /// Passed to pi as `--session-dir` (pi's flat custom session directory).
    #[serde(default)]
    pub session_dir: Option<PathBuf>,
    /// pi's agent directory (like `PI_CODING_AGENT_DIR`), used to find session files.
    #[serde(default)]
    pub agent_dir: Option<PathBuf>,
}

impl PiOptions {
    fn parse(value: &serde_json::Value) -> Result<Self, String> {
        let options: PiOptions = if value.is_null() {
            PiOptions::default()
        } else {
            serde_json::from_value(value.clone()).map_err(|e| e.to_string())?
        };
        if let Some(mode) = &options.default_permission_mode
            && !gate::is_valid_mode(mode)
        {
            return Err(format!(
                "default_permission_mode must be one of {}, got `{mode}`",
                gate::MODES.join(", ")
            ));
        }
        Ok(options)
    }
}

/// pi itself runs no work outside its runs (no sub-agents, no background shell); a run an
/// extension starts by itself is a turn of its own (see `session.rs`), not a background task.
/// Resources extensions keep (watchers, timers) send no signal (docs/design.md §1).
const CAPABILITIES: HarnessCapabilities = HarnessCapabilities {
    background_tasks: false,
    background_stop: false,
    interrupt: true,
    steer: true,
    approvals: true,
    questions: true,
    resume: true,
    fork: true,
    images: true,
    model_switch_live: true,
    native_sessions: true,
};

fn permission_modes(default: &str) -> Vec<PermissionMode> {
    vec![
        PermissionMode {
            id: MODE_ASK.into(),
            label: "Ask before edits and commands".into(),
            description: Some("Every shell command and file edit waits for approval".into()),
            is_default: default == MODE_ASK,
        },
        PermissionMode {
            id: MODE_ASK_COMMANDS.into(),
            label: "Ask before commands".into(),
            description: Some("Shell commands wait for approval; file edits run directly".into()),
            is_default: default == MODE_ASK_COMMANDS,
        },
        PermissionMode {
            id: MODE_AUTO.into(),
            label: "Auto".into(),
            description: Some("pi's own behaviour: every tool runs without asking".into()),
            is_default: default == MODE_AUTO,
        },
    ]
}

/// The pi harness.
pub struct PiAdapter {
    config: HarnessConfig,
    ctx: AdapterContext,
    options: Result<PiOptions, String>,
    display_name: String,
    /// Running sessions, found by the native session each one runs now.
    live: Mutex<Vec<Weak<session::Shared>>>,
}

/// What a start opens.
enum Opening {
    /// The session of this id (pi opens it, or makes it with this id).
    Session(String),
    /// A branch of the opened session at a turn: pi picks the new id.
    Branch(anchor::ForkTarget),
}

/// pi's arguments for the user's trust decision: `--approve` / `--no-approve` override pi's
/// project trust for the run (pi 0.85.1 `--help`: "Trust project-local files for this run" /
/// "Ignore project-local files for this run"); without a decision pi's own saved decision and
/// `defaultProjectTrust` apply. pi's RPC mode never asks by itself.
fn trust_args(trusted: Option<bool>) -> Vec<OsString> {
    match trusted {
        Some(true) => vec!["--approve".into()],
        Some(false) => vec!["--no-approve".into()],
        None => Vec::new(),
    }
}

impl PiAdapter {
    /// Builds the adapter. Invalid options do not fail here: `probe` reports them and
    /// `start` refuses to run.
    pub fn new(config: HarnessConfig, ctx: AdapterContext) -> Self {
        let options = PiOptions::parse(&config.options);
        let display_name = config
            .display_name
            .clone()
            .unwrap_or_else(|| "pi".to_owned());
        Self {
            config,
            ctx,
            options,
            display_name,
            live: Mutex::new(Vec::new()),
        }
    }

    fn options(&self) -> Result<&PiOptions, AdapterError> {
        self.options
            .as_ref()
            .map_err(|e| AdapterError::Unavailable(format!("invalid pi options: {e}")))
    }

    fn env_value(&self, key: &str) -> Option<String> {
        self.config
            .env
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
    }

    fn path_inputs(&self) -> paths::PathInputs {
        let options = self.options.as_ref().ok();
        paths::PathInputs {
            agent_dir_option: options.and_then(|o| o.agent_dir.clone()),
            session_dir_option: options.and_then(|o| o.session_dir.clone()),
            env_agent_dir: self.env_value(paths::ENV_AGENT_DIR),
            env_session_dir: self.env_value(paths::ENV_SESSION_DIR),
        }
    }

    fn default_mode(&self) -> String {
        self.options
            .as_ref()
            .ok()
            .and_then(|o| o.default_permission_mode.clone())
            .unwrap_or_else(|| MODE_ASK.to_owned())
    }

    fn program(&self) -> Result<PathBuf, AdapterError> {
        resolve_program(&self.config.command).map_err(|e| AdapterError::Unavailable(e.to_string()))
    }

    fn session_config(&self, label: String, gate_file: Option<PathBuf>) -> SessionConfig {
        SessionConfig {
            label,
            gate_file,
            stop_grace: self.ctx.policy.stop_grace,
            request_timeout: self.ctx.policy.handshake_timeout,
            max_line_bytes: self.ctx.policy.max_line_bytes,
        }
    }

    /// Spawns `pi --mode rpc <args>` in `cwd`.
    async fn spawn_rpc(
        &self,
        label: String,
        cwd: &Path,
        args: Vec<OsString>,
        gate_file: Option<PathBuf>,
        owner: Option<String>,
    ) -> Result<(PiSession, mpsc::UnboundedReceiver<AdapterEvent>), AdapterError> {
        let program = self.program()?;
        let mut spec = SpawnSpec::new(label.clone(), program, cwd)
            .args(self.config.args.iter().map(OsString::from))
            .args([OsString::from("--mode"), OsString::from("rpc")])
            .args(args);
        for (k, v) in &self.config.env {
            spec = spec.env(k, v);
        }
        if let Some(file) = &gate_file {
            spec = spec.env(gate::MODE_FILE_ENV, file.as_os_str());
        }
        if let Some(owner) = owner {
            spec = spec.owner(owner);
        }
        let mut child = self
            .ctx
            .supervisor
            .spawn(spec)
            .await
            .map_err(|e| AdapterError::Spawn(e.to_string()))?;
        let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
            child.handle.kill(StopReason::Abandoned);
            return Err(AdapterError::Spawn(
                "pi's stdio pipes are unavailable".into(),
            ));
        };
        let link = std::sync::Arc::new(ChildLink(child.handle));
        Ok(PiSession::start(
            stdout,
            stdin,
            link,
            self.session_config(label, gate_file),
        ))
    }

    /// A short-lived process without a session file (probes, command listings), with the
    /// user's trust decision for the project (`trusted`, see [`trust_args`]). The guard stops
    /// it, also when the caller is dropped before it is done.
    async fn temporary(
        &self,
        cwd: &Path,
        trusted: Option<bool>,
    ) -> Result<(PiSession, StartGuard), AdapterError> {
        let mut args: Vec<OsString> = vec!["--no-session".into()];
        args.extend(trust_args(trusted));
        let (session, _events) = self
            .spawn_rpc(format!("{}[probe]", self.config.id), cwd, args, None, None)
            .await?;
        let guard = StartGuard::for_session(std::sync::Arc::new(session.clone()));
        Ok((session, guard))
    }

    fn live_session(&self, native_session_id: &str) -> Option<PiSession> {
        let live = self.live.lock();
        live.iter()
            .filter_map(PiSession::upgrade)
            .find(|s| !s.is_closed() && s.native_session_id().as_deref() == Some(native_session_id))
    }

    fn register_live(&self, session: &PiSession) {
        let mut live = self.live.lock();
        live.retain(|weak| weak.strong_count() > 0);
        live.push(session.downgrade());
    }

    /// The session file of `native_session_id` (searched by the header's id).
    async fn session_file(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<PathBuf, AdapterError> {
        let inputs = self.path_inputs();
        let cwd = cwd.to_path_buf();
        let id = native_session_id.to_owned();
        let found =
            tokio::task::spawn_blocking(move || native::find_session_file(&inputs, &cwd, &id))
                .await
                .map_err(|e| AdapterError::Other(e.to_string()))??;
        found.ok_or_else(|| {
            AdapterError::Harness(format!("pi session {native_session_id} was not found"))
        })
    }

    /// Stops a process whose start failed and adds the end of its stderr to the error.
    async fn failed_start(&self, guard: StartGuard, error: AdapterError) -> AdapterError {
        let exit = guard.stop(StopReason::Shutdown).await;
        self.ctx.policy.with_stderr(error, &exit.stderr_tail)
    }
}

#[async_trait]
impl HarnessAdapter for PiAdapter {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn kind(&self) -> HarnessKind {
        HarnessKind::Pi
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    async fn probe(&self) -> HarnessInfo {
        if let Err(e) = self.options() {
            return HarnessInfo::unavailable(e.to_string());
        }
        let program = match self.program() {
            Ok(p) => p,
            Err(e) => return HarnessInfo::unavailable(e.to_string()),
        };
        let probe_dir = self.ctx.state_dir.join("probe");
        if let Err(e) = std::fs::create_dir_all(&probe_dir) {
            return HarnessInfo::unavailable(format!("cannot create {}: {e}", probe_dir.display()));
        }
        let version = match self
            .ctx
            .supervisor
            .run_tool(
                ToolSpec::new(&program, &probe_dir)
                    .args(["--version"])
                    .timeout(self.ctx.policy.handshake_timeout),
            )
            .await
        {
            Ok(out) if out.success() => out
                .stdout_lossy()
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            Ok(out) => {
                return HarnessInfo::unavailable(format!(
                    "`pi --version` failed: {}",
                    stderr_excerpt(&out.stderr_lossy(), self.ctx.policy.stderr_excerpt_lines)
                ));
            }
            Err(e) => return HarnessInfo::unavailable(format!("`pi --version` failed: {e}")),
        };
        let (session, guard) = match self.temporary(&probe_dir, None).await {
            Ok(s) => s,
            Err(e) => return HarnessInfo::unavailable(e.to_string()),
        };
        let queried = async {
            Ok::<_, AdapterError>((session.get_models().await?, session.get_state().await?))
        }
        .await;
        let exit = guard.stop(StopReason::Shutdown).await;
        let (models, state) = match queried {
            Ok(v) => v,
            Err(e) => {
                let e = self.ctx.policy.with_stderr(e, &exit.stderr_tail);
                return HarnessInfo::unavailable(format!("pi did not answer: {}", e.detail()));
            }
        };
        if models.is_empty() {
            return HarnessInfo::unavailable(
                "pi has no usable model; configure a provider by running `pi` once",
            );
        }
        if cfg!(windows) && !bash_available(&self.path_inputs().agent_dir()) {
            tracing::warn!(harness = %self.config.id, "Git Bash was not found; pi's bash tool will fail (the powershell tool still works)");
        }
        let default_model = state.model.as_ref().map(wire::PiModel::qualified_id);
        let default_mode = self.default_mode();
        HarnessInfo {
            available: true,
            unavailable_reason: None,
            version: Some(version),
            executable: Some(program),
            capabilities: CAPABILITIES,
            models: models
                .iter()
                .map(|m| m.to_protocol(Some(m.qualified_id()) == default_model))
                .collect(),
            default_model,
            effort_levels: wire::effort_levels(),
            permission_modes: permission_modes(&default_mode),
            default_permission_mode: Some(default_mode),
        }
    }

    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError> {
        self.start_with(req, StartOptions::default()).await
    }

    /// Starts pi for the thread: a new session (`--session-id`), a resumed one (`--session
    /// <file>`), a copy of a whole session (`--fork <file> --session-id`), or a branch of a
    /// session at a turn (`--session <file>`, then the gate's `ctx.fork` at the turn's anchor:
    /// pi picks the new id and runs the branch). The user's trust decision is passed as
    /// `--approve` / `--no-approve` (`options.project_trusted`).
    async fn start_with(
        &self,
        req: StartRequest,
        start: StartOptions,
    ) -> Result<SessionHandle, AdapterError> {
        let options = self.options()?.clone();
        let mode = req
            .settings
            .permission_mode
            .clone()
            .unwrap_or_else(|| self.default_mode());
        if !gate::is_valid_mode(&mode) {
            return Err(AdapterError::Other(format!(
                "unknown permission mode `{mode}`"
            )));
        }
        let (opening, mut args): (Opening, Vec<OsString>) = match (&req.mode, &start.fork_at) {
            (StartMode::New, None) => {
                let id = uuid::Uuid::new_v4().to_string();
                (
                    Opening::Session(id.clone()),
                    vec!["--session-id".into(), id.into()],
                )
            }
            (StartMode::Resume { native_session_id }, None) => {
                let file = self.session_file(&req.cwd, native_session_id).await?;
                (
                    Opening::Session(native_session_id.clone()),
                    vec!["--session".into(), file.into_os_string()],
                )
            }
            (StartMode::Fork { native_session_id }, Some(point)) => {
                let target = anchor::fork_target(point)?;
                let file = self.session_file(&req.cwd, native_session_id).await?;
                (
                    Opening::Branch(target),
                    vec!["--session".into(), file.into_os_string()],
                )
            }
            (StartMode::Fork { native_session_id }, None) => {
                let file = self.session_file(&req.cwd, native_session_id).await?;
                let id = uuid::Uuid::new_v4().to_string();
                (
                    Opening::Session(id.clone()),
                    vec![
                        "--fork".into(),
                        file.into_os_string(),
                        "--session-id".into(),
                        id.into(),
                    ],
                )
            }
            (StartMode::New | StartMode::Resume { .. }, Some(_)) => {
                return Err(AdapterError::Other(
                    "a fork at a turn needs a fork of the session".into(),
                ));
            }
        };
        if let Some(dir) = &options.session_dir {
            args.push("--session-dir".into());
            args.push(dir.clone().into_os_string());
        }
        args.extend(trust_args(start.project_trusted));
        let extension = gate::install_extension(&self.ctx.state_dir)
            .map_err(|e| AdapterError::Spawn(format!("installing the approval extension: {e}")))?;
        args.push("-e".into());
        args.push(extension.into_os_string());
        // One mode file per thread: the thread's processes follow each other, and the session id
        // of a branch is only known once pi made it.
        let gate_file = self
            .ctx
            .state_dir
            .join("gate")
            .join(format!("{}.json", req.thread_id));
        gate::write_mode(&gate_file, &mode)
            .map_err(|e| AdapterError::Spawn(format!("writing gate mode: {e}")))?;

        let label = format!("{}[{}]", self.config.id, req.thread_id);
        let (session, events) = self
            .spawn_rpc(
                label,
                &req.cwd,
                args,
                Some(gate_file),
                Some(req.thread_id.to_string()),
            )
            .await?;
        // The reader task keeps the process handle alive: if this future is dropped during the
        // handshake, the guard stops the process.
        let guard = StartGuard::for_session(std::sync::Arc::new(session.clone()));
        let native_id = match opening {
            Opening::Session(id) => id,
            Opening::Branch(target) => match session.fork_to(&target).await {
                Ok(id) => id,
                Err(e) => return Err(self.failed_start(guard, e).await),
            },
        };
        if let Err(e) = session::handshake(&session, &native_id, &req.settings, &mode).await {
            return Err(self.failed_start(guard, e).await);
        }
        guard.disarm();
        self.register_live(&session);
        Ok(SessionHandle {
            native_session_id: Some(native_id),
            control: std::sync::Arc::new(session) as std::sync::Arc<dyn SessionControl>,
            events,
        })
    }

    /// The anchors [`anchor::fork_target`] needs: the turn's own, and before a turn without
    /// the entry where it began, the one of the turn before.
    fn check_fork_point(&self, point: &ForkPoint) -> Result<(), AdapterError> {
        anchor::fork_target(point).map(|_| ())
    }

    fn features(&self) -> HarnessFeatures {
        HarnessFeatures {
            fork_at_turn: true,
            // pi has no writer lock: a session another process holds resumes like any other,
            // so a failed resume is never a reason to branch instead (see docs/adapters/pi.md).
            fork_while_held: false,
            rename: true,
            side_question: false,
            move_to_background: false,
            status: true,
            project_trust: true,
            // pi has no plan mode of its own; a plan-mode extension's `/plan` stays pi's command.
            plan_mode: None,
            fast_mode_models: Vec::new(),
        }
    }

    fn session_switching_commands(&self) -> &'static [&'static str] {
        SESSION_SWITCHING_COMMANDS
    }

    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        self.options()?;
        if let Some(session) = ctx
            .native_session_id
            .as_deref()
            .and_then(|id| self.live_session(id))
        {
            return Ok(commands::commands(session.get_commands().await?));
        }
        let (session, guard) = self.temporary(&ctx.cwd, ctx.project_trusted).await?;
        let result = session.get_commands().await;
        guard.stop(StopReason::Shutdown).await;
        Ok(commands::commands(result?))
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
        let inputs = self.path_inputs();
        let cwd = cwd.to_path_buf();
        let policy = self.ctx.policy.clone();
        tokio::task::spawn_blocking(move || native::list_sessions(&inputs, &cwd, &policy))
            .await
            .map_err(|e| AdapterError::Other(e.to_string()))?
    }

    async fn read_native_history(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<NativeHistory, AdapterError> {
        let inputs = self.path_inputs();
        let cwd = cwd.to_path_buf();
        let id = native_session_id.to_owned();
        let policy = self.ctx.policy.clone();
        tokio::task::spawn_blocking(move || {
            let file = native::find_session_file(&inputs, &cwd, &id)?
                .ok_or_else(|| AdapterError::Harness(format!("pi session {id} was not found")))?;
            native::read_history(&file, &policy).map_err(|e| {
                AdapterError::Harness(format!(
                    "pi session {id} could not be read from {}: {e}",
                    file.display()
                ))
            })
        })
        .await
        .map_err(|e| AdapterError::Other(e.to_string()))?
    }

    /// The history with each turn's anchor from the session file's entry ids (the turn's user
    /// message and its last entry on the active branch; see [`native::history_with_anchors`]).
    async fn read_native_history_anchored(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<(NativeHistory, Vec<Option<Value>>), AdapterError> {
        let inputs = self.path_inputs();
        let cwd = cwd.to_path_buf();
        let id = native_session_id.to_owned();
        let policy = self.ctx.policy.clone();
        tokio::task::spawn_blocking(move || {
            let file = native::find_session_file(&inputs, &cwd, &id)?
                .ok_or_else(|| AdapterError::Harness(format!("pi session {id} was not found")))?;
            native::read_history_anchored(&file, &policy).map_err(|e| {
                AdapterError::Harness(format!(
                    "pi session {id} could not be read from {}: {e}",
                    file.display()
                ))
            })
        })
        .await
        .map_err(|e| AdapterError::Other(e.to_string()))?
    }
}

/// pi's bash lookup on Windows (`docs/windows.md`): `shellPath` in settings, Git Bash at its
/// default location, or `bash.exe` on PATH.
fn bash_available(agent_dir: &Path) -> bool {
    let from_settings = std::fs::read_to_string(agent_dir.join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v.get("shellPath")
                .and_then(|p| p.as_str())
                .map(PathBuf::from)
        });
    if from_settings.is_some_and(|p| p.is_file()) {
        return true;
    }
    if Path::new(r"C:\Program Files\Git\bin\bash.exe").is_file() {
        return true;
    }
    which_bash()
}

fn which_bash() -> bool {
    resolve_program("bash").is_ok()
}

/// pi's session commands, by the names of its built-ins (pi 0.85.1 `BUILTIN_SLASH_COMMANDS`;
/// never offered, and refused when typed, see [`HarnessAdapter::session_switching_commands`]):
/// `new` (a new session), `resume` and `import` (another session, `import` from a JSONL file),
/// `fork` and `clone` (a new session file from this one), `tree` (another point of the session
/// tree, same file). The built-ins are TUI-only and never listed by `get_commands` (typed over
/// RPC they would reach the model as text); an extension command registered under one of these
/// names does the same over RPC (`ctx.newSession`, `ctx.switchSession`, `ctx.fork`,
/// `ctx.navigateTree`). Also the approval gate's own fork command ([`gate::FORK_COMMAND`]),
/// which the adapter alone sends. Extension commands that switch sessions under other names
/// cannot be told apart by name; the session's end of turn reports the switch (`session.rs`).
const SESSION_SWITCHING_COMMANDS: &[&str] = &[
    "new",
    "resume",
    "import",
    "fork",
    "clone",
    "tree",
    gate::FORK_COMMAND,
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extension_commands_named_like_the_session_built_ins_are_the_ones_excluded() {
        let listed: Vec<wire::PiCommand> = serde_json::from_value(json!([
            {"name": "switch-to", "description": "An extension command", "source": "extension"},
            {"name": "resume", "description": "Pick a session", "source": "extension"},
            {"name": "tree", "description": "Navigate the tree", "source": "extension"},
            {"name": "import", "description": "Import a session", "source": "extension"},
            {"name": "skill:review", "description": "A skill", "source": "skill"},
        ]))
        .unwrap();
        let kept: Vec<String> = commands::commands(listed)
            .into_iter()
            .map(|c| c.name)
            .filter(|name| !SESSION_SWITCHING_COMMANDS.contains(&name.as_str()))
            .collect();
        // The adapter's own `/compact` stays; a name it cannot judge (`switch-to`) is offered.
        assert_eq!(kept, ["switch-to", "skill:review", "compact"]);
    }

    #[test]
    fn options_parse_and_validate() {
        assert_eq!(
            PiOptions::parse(&serde_json::Value::Null).unwrap(),
            PiOptions::default()
        );
        let o = PiOptions::parse(&json!({"default_permission_mode": "auto", "session_dir": "x"}))
            .unwrap();
        assert_eq!(o.default_permission_mode.as_deref(), Some("auto"));
        assert!(PiOptions::parse(&json!({"default_permission_mode": "yolo"})).is_err());
        assert!(PiOptions::parse(&json!({"unknown": 1})).is_err());
    }

    #[test]
    fn trust_decisions_become_pi_flags() {
        assert_eq!(trust_args(Some(true)), [OsString::from("--approve")]);
        assert_eq!(trust_args(Some(false)), [OsString::from("--no-approve")]);
        assert!(trust_args(None).is_empty());
    }

    #[test]
    fn features_are_the_ones_pi_offers() {
        let state = tempfile::tempdir().unwrap();
        let adapter = PiAdapter::new(
            HarnessConfig {
                id: "pi".into(),
                kind: HarnessKind::Pi,
                display_name: None,
                command: "pi".into(),
                args: vec![],
                env: Default::default(),
                options: serde_json::Value::Null,
            },
            AdapterContext {
                supervisor: aas_supervisor::Supervisor::new(
                    &state.path().join("supervisor"),
                    aas_supervisor::SupervisorPolicy {
                        prevent_sleep: false,
                        ..Default::default()
                    },
                )
                .unwrap(),
                state_dir: state.path().join("pi"),
                policy: aas_harness::AdapterPolicy::default(),
            },
        );
        let features = adapter.features();
        assert!(features.fork_at_turn && features.rename && features.status);
        assert!(features.project_trust);
        assert!(!features.fork_while_held && !features.side_question);
        assert!(features.plan_mode.is_none() && features.fast_mode_models.is_empty());
    }

    #[test]
    fn permission_modes_mark_the_default() {
        let modes = permission_modes(MODE_ASK_COMMANDS);
        assert_eq!(modes.iter().filter(|m| m.is_default).count(), 1);
        assert!(
            modes
                .iter()
                .any(|m| m.id == MODE_ASK_COMMANDS && m.is_default)
        );
    }
}
