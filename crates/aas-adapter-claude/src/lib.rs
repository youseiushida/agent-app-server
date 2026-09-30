//! Adapter for Claude Code (`claude`).
//!
//! Each session is one long-lived `claude -p --input-format stream-json
//! --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool
//! stdio` process, driven with the same bidirectional control protocol the official Agent
//! SDK uses (`control_request` / `control_response` / `control_cancel_request`).
//! See `docs/adapters/claude.md` for the full mapping.

mod background;
mod commands;
mod mapping;
mod native;
mod session;
mod time;

#[cfg(test)]
mod replay_tests;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aas_harness::protocol::{
    Command, HarnessCapabilities, HarnessFeatures, HarnessKind, PlanModeFeature, ThreadSettings,
};
use aas_harness::{
    AdapterContext, AdapterError, CommandContext, ForkPoint, HarnessAdapter, HarnessConfig,
    HarnessInfo, NativeHistory, NativeSessionScan, NativeSessionSummary, SessionControl,
    SessionHandle, StartGuard, StartMode, StartOptions, StartRequest, StatusSection, StopReason,
    UpgradedSettings,
};
use aas_supervisor::{SpawnSpec, ToolSpec, resolve_program};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;

use crate::commands::{CommandCache, SESSION_SWITCHING_COMMANDS, menu_from_initialize};
use crate::session::{ClaudeSession, ProcessLink, SessionParams};

/// Adapter-specific options (`[[harness]] options = { … }`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeOptions {
    /// Offer the `bypassPermissions` mode (starts the CLI with
    /// `--allow-dangerously-skip-permissions`). Off by default.
    #[serde(default)]
    pub allow_bypass_permissions: bool,
    /// `initialize.agentProgressSummaries`: the CLI writes a one-line progress summary for
    /// each background agent (`task_progress.summary`). Unset leaves the CLI's own default.
    #[serde(default)]
    pub agent_progress_summaries: Option<bool>,
}

impl ClaudeOptions {
    pub fn parse(value: &serde_json::Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value.clone()).map_err(|e| format!("invalid claude options: {e}"))
    }
}

/// A process spawned and past its `initialize` handshake.
type Launched = (
    Arc<ClaudeSession>,
    tokio::sync::mpsc::UnboundedReceiver<aas_harness::AdapterEvent>,
    Value,
);

/// Claude Code harness.
pub struct ClaudeAdapter {
    config: HarnessConfig,
    ctx: AdapterContext,
    display_name: String,
    options: Result<ClaudeOptions, String>,
    commands: CommandCache,
    /// The models the last probe's `initialize` marked `supportsFastMode`.
    fast_mode_models: Mutex<Vec<String>>,
}

impl ClaudeAdapter {
    pub fn new(config: HarnessConfig, ctx: AdapterContext) -> Self {
        let display_name = config
            .display_name
            .clone()
            .unwrap_or_else(|| "Claude Code".to_owned());
        let options = ClaudeOptions::parse(&config.options);
        Self {
            config,
            ctx,
            display_name,
            options,
            commands: CommandCache::default(),
            fast_mode_models: Mutex::new(Vec::new()),
        }
    }

    fn options(&self) -> Result<&ClaudeOptions, AdapterError> {
        self.options
            .as_ref()
            .map_err(|e| AdapterError::Unavailable(e.clone()))
    }

    fn program(&self) -> Result<PathBuf, AdapterError> {
        resolve_program(&self.config.command).map_err(|e| AdapterError::Unavailable(e.to_string()))
    }

    fn projects_dir() -> Result<PathBuf, AdapterError> {
        native::claude_config_dir()
            .map(|d| d.join("projects"))
            .ok_or_else(|| {
                AdapterError::Other("cannot determine the Claude config directory".into())
            })
    }

    /// Spawns a process and completes the `initialize` handshake. A failed handshake stops the
    /// process; its error carries the CLI's refusal (a failed `result` before the handshake,
    /// e.g. an unknown `--resume-session-at` anchor) or else the last lines of its stderr.
    async fn launch(
        &self,
        label: String,
        cwd: &Path,
        owner: Option<String>,
        args: Vec<OsString>,
        native_session_id: String,
        settings: ThreadSettings,
    ) -> Result<Launched, AdapterError> {
        let program = self.program()?;
        let mut spec = SpawnSpec::new(label.clone(), program, cwd)
            .args(self.config.args.iter().map(OsString::from))
            .args(args);
        for (k, v) in &self.config.env {
            spec = spec.env(k, v);
        }
        // An inherited CLAUDECODE marker makes the CLI believe it runs nested in another
        // Claude Code session (the official SDK strips it as well).
        spec = spec.env_remove("CLAUDECODE");
        if let Some(owner) = owner {
            spec = spec.owner(owner);
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
            .ok_or_else(|| AdapterError::Spawn("stdin not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AdapterError::Spawn("stdout not piped".into()))?;
        let params = SessionParams {
            label,
            native_session_id,
            cwd: cwd.to_path_buf(),
            settings,
            stop_grace: self.ctx.policy.stop_grace,
            request_timeout: self.ctx.policy.handshake_timeout,
            max_line_bytes: self.ctx.policy.max_line_bytes,
            command_cache: self.commands.clone(),
            agent_progress_summaries: self.options()?.agent_progress_summaries,
            max_output_file_bytes: self.ctx.policy.max_output_file_bytes,
        };
        let (session, events) =
            ClaudeSession::start(stdout, stdin, ProcessLink::Child(child.handle), params);
        // The reader task keeps the process handle alive: if this future is dropped during the
        // handshake, the guard stops the process.
        let guard = StartGuard::for_session(session.clone());
        match session.initialize().await {
            Ok(info) => {
                guard.disarm();
                Ok((session, events, info.raw))
            }
            Err(e) => {
                let exit = guard.stop(StopReason::Shutdown).await;
                if session.startup_error().is_some() {
                    // The CLI's own words; its stderr repeats them.
                    return Err(e);
                }
                let tail = if exit.stderr_tail.trim().is_empty() {
                    session.stderr_tail()
                } else {
                    exit.stderr_tail
                };
                Err(self.ctx.policy.with_stderr(e, &tail))
            }
        }
    }

    /// Spawns a throwaway process (no session persistence) in `cwd`: it answers requests
    /// about the CLI (models, commands, usage) without a conversation.
    async fn probe_process(&self, cwd: &Path) -> Result<Launched, AdapterError> {
        let mut args = common_args(false);
        args.push("--no-session-persistence".into());
        self.launch(
            format!("{}[probe]", self.config.id),
            cwd,
            None,
            args,
            String::new(),
            ThreadSettings::default(),
        )
        .await
    }

    /// The `initialize` response of a throwaway process in `cwd`.
    async fn handshake_probe(&self, cwd: &Path) -> Result<Value, AdapterError> {
        let (session, _events, raw) = self.probe_process(cwd).await?;
        StartGuard::for_session(session)
            .stop(StopReason::Shutdown)
            .await;
        Ok(raw)
    }

    /// The directory the throwaway processes of `probe` and `status` run in.
    fn probe_dir(&self) -> Result<PathBuf, AdapterError> {
        let dir = self.ctx.state_dir.join("probe");
        std::fs::create_dir_all(&dir)
            .map_err(|e| AdapterError::Other(format!("cannot create {}: {e}", dir.display())))?;
        Ok(dir)
    }

    /// Brings a session that just started to `modes` (`StartOptions::modes`): plan mode begins
    /// from the permission mode the process started with, which Claude Code returns to after
    /// the plan's approval. A mode the CLI does not take fails the start.
    async fn start_in_modes(
        &self,
        session: &Arc<ClaudeSession>,
        options: &StartOptions,
    ) -> Result<(), AdapterError> {
        if options.modes == aas_harness::ThreadModes::default() {
            return Ok(());
        }
        session.apply_modes(&options.modes).await.map(|_| ())
    }
}

/// Flags shared by every invocation.
fn common_args(allow_bypass: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if allow_bypass {
        args.push("--allow-dangerously-skip-permissions".into());
    }
    args
}

/// Values placed on the command line. The npm shim is a `.cmd` file run through cmd.exe,
/// so only a conservative character set is accepted (no cmd metacharacters, no leading `-`).
fn checked_arg(what: &str, value: &str) -> Result<String, AdapterError> {
    let ok = !value.is_empty()
        && !value.starts_with('-')
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '[' | ']' | '/')
        });
    if ok {
        Ok(value.to_owned())
    } else {
        Err(AdapterError::Other(format!("invalid {what}: {value:?}")))
    }
}

fn settings_args(settings: &ThreadSettings) -> Result<Vec<OsString>, AdapterError> {
    let mut args = Vec::new();
    if let Some(model) = &settings.model {
        args.push("--model".into());
        args.push(checked_arg("model", model)?.into());
    }
    if let Some(effort) = &settings.effort {
        args.push("--effort".into());
        args.push(checked_arg("effort", effort)?.into());
    }
    if let Some(mode) = &settings.permission_mode {
        args.push("--permission-mode".into());
        args.push(checked_arg("permission mode", mode)?.into());
    }
    Ok(args)
}

/// The session arguments of a start and the native session id it runs:
/// * new: `--session-id=<new>`;
/// * resume: `--resume=<id>`;
/// * fork: `--resume=<source> [--resume-session-at=<uuid>] --fork-session --session-id=<new>`.
///   A fork at a turn (`fork_at`) keeps the transcript up to the anchor of that turn, or of the
///   turn before it (`before`, the user edits the turn's prompt). `--resume-drops-turn` is not
///   used: Claude Code refuses it whenever a later turn exists (recording g3a), and the cut at
///   the previous turn's anchor alone keeps exactly the turns before (g3b).
fn session_args(
    mode: &StartMode,
    fork_at: Option<&ForkPoint>,
) -> Result<(Vec<OsString>, String), AdapterError> {
    let mut args: Vec<OsString> = Vec::new();
    if fork_at.is_some() && !matches!(mode, StartMode::Fork { .. }) {
        return Err(AdapterError::Other(
            "a fork point was given for a start that is not a fork".into(),
        ));
    }
    let id = match mode {
        StartMode::New => {
            let id = uuid::Uuid::new_v4().to_string();
            args.push(format!("--session-id={id}").into());
            id
        }
        StartMode::Resume { native_session_id } => {
            args.push(format!("--resume={}", checked_arg("session id", native_session_id)?).into());
            native_session_id.clone()
        }
        StartMode::Fork { native_session_id } => {
            let id = uuid::Uuid::new_v4().to_string();
            args.push(format!("--resume={}", checked_arg("session id", native_session_id)?).into());
            if let Some(point) = fork_at {
                let at = fork_anchor(point)?;
                args.push(
                    format!("--resume-session-at={}", checked_arg("fork anchor", at)?).into(),
                );
            }
            args.push("--fork-session".into());
            args.push(format!("--session-id={id}").into());
            id
        }
    };
    Ok((args, id))
}

/// The transcript uuid a fork at `point` keeps up to: the anchor of the turn, or with `before`
/// the anchor of the turn before it.
fn fork_anchor(point: &ForkPoint) -> Result<&str, AdapterError> {
    let anchor = if point.before {
        point.previous.as_ref().ok_or_else(|| {
            AdapterError::Other(
                "the turn before the fork point has no recorded anchor, so the fork cannot end right before it".into(),
            )
        })?
    } else {
        &point.anchor
    };
    mapping::anchor_uuid(anchor)
        .ok_or_else(|| AdapterError::Other(format!("not a Claude Code turn anchor: {anchor}")))
}

/// The anchors [`session_args`] needs for a fork at `point`: the turn's own, or before it the
/// one of the turn before (Claude Code keeps the transcript up to a message; there is no cut
/// before one), each a plain id.
fn check_fork_point(point: &ForkPoint) -> Result<(), AdapterError> {
    fork_anchor(point)
        .and_then(|at| checked_arg("fork anchor", at))
        .map(|_| ())
}

/// `claude --version` prints e.g. `2.1.284 (Claude Code)`.
fn parse_version(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .next()
        .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_owned)
}

#[async_trait]
impl HarnessAdapter for ClaudeAdapter {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn kind(&self) -> HarnessKind {
        HarnessKind::Claude
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    async fn probe(&self) -> HarnessInfo {
        let options = match self.options() {
            Ok(o) => o.clone(),
            Err(e) => return HarnessInfo::unavailable(e.to_string()),
        };
        let program = match self.program() {
            Ok(p) => p,
            Err(e) => return HarnessInfo::unavailable(e.to_string()),
        };
        let probe_dir = match self.probe_dir() {
            Ok(dir) => dir,
            Err(e) => return HarnessInfo::unavailable(e.detail()),
        };
        let version = match self
            .ctx
            .supervisor
            .run_tool(ToolSpec::new(&program, &probe_dir).args(["--version"]))
            .await
        {
            Ok(out) if out.success() => parse_version(&out.stdout_lossy()),
            Ok(out) => {
                return HarnessInfo::unavailable(format!(
                    "`claude --version` failed: {}",
                    aas_harness::stderr_excerpt(
                        &out.stderr_lossy(),
                        self.ctx.policy.stderr_excerpt_lines
                    )
                ));
            }
            Err(e) => return HarnessInfo::unavailable(format!("`claude --version` failed: {e}")),
        };
        let raw = match self.handshake_probe(&probe_dir).await {
            Ok(raw) => raw,
            Err(e) => {
                let mut info =
                    HarnessInfo::unavailable(format!("claude handshake failed: {}", e.detail()));
                info.version = version;
                info.executable = Some(program);
                return info;
            }
        };
        menu_from_initialize(&self.commands, &probe_dir, &raw);
        *self.fast_mode_models.lock() = mapping::fast_mode_models(&raw);
        let current_mode = raw.get("current_permission_mode").and_then(|v| v.as_str());
        let permission_modes =
            mapping::permission_modes(current_mode, options.allow_bypass_permissions);
        let models = mapping::models_from_initialize(&raw, &permission_modes);
        let effort_levels = mapping::effort_levels(&models);
        HarnessInfo {
            available: true,
            unavailable_reason: None,
            version,
            executable: Some(program),
            capabilities: HarnessCapabilities {
                background_tasks: true,
                background_stop: true,
                interrupt: true,
                // Claude Code takes a message into the running turn at its next tool boundary
                // and says so (`command_lifecycle started`); one it did not take is returned.
                steer: true,
                approvals: true,
                questions: true,
                resume: true,
                fork: true,
                images: true,
                model_switch_live: true,
                native_sessions: true,
            },
            default_model: models.iter().find(|m| m.is_default).map(|m| m.id.clone()),
            models,
            effort_levels,
            default_permission_mode: permission_modes
                .iter()
                .find(|m| m.is_default)
                .map(|m| m.id.clone()),
            permission_modes,
        }
    }

    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError> {
        self.start_with(req, StartOptions::default()).await
    }

    async fn start_with(
        &self,
        req: StartRequest,
        options: StartOptions,
    ) -> Result<SessionHandle, AdapterError> {
        let allow_bypass = self.options()?.allow_bypass_permissions;
        // A thread stored with the permission mode `plan` of earlier versions runs in plan mode
        // over the CLI's default permission mode; the CLI's reports of both reach the thread
        // (docs/adapters/claude.md §19.6).
        let UpgradedSettings { settings, plan } = mapping::upgrade_settings(req.settings.clone());
        let mut options = options;
        options.modes.plan |= plan;
        let mut args = common_args(allow_bypass);
        let (session_args, native_session_id) = session_args(&req.mode, options.fork_at.as_ref())?;
        args.extend(session_args);
        args.extend(settings_args(&settings)?);
        let label = format!("{}[{}]", self.config.id, req.thread_id);
        let (session, events, _raw) = self
            .launch(
                label,
                &req.cwd,
                Some(req.thread_id.to_string()),
                args,
                native_session_id.clone(),
                settings,
            )
            .await?;
        let guard = StartGuard::for_session(session.clone());
        if let Err(e) = self.start_in_modes(&session, &options).await {
            guard.stop(StopReason::Shutdown).await;
            return Err(e);
        }
        guard.disarm();
        Ok(SessionHandle {
            native_session_id: Some(native_session_id),
            control: session,
            events,
        })
    }

    /// What Claude Code offers beyond the capabilities (docs/adapters/claude.md §19):
    /// * fork at any turn (`--resume-session-at`), also of a session another process holds
    ///   (recording g7: a held session resumes and forks);
    /// * rename (`rename_session`), side questions (`side_question`), moving foreground work to
    ///   the background (`background_tasks`), status (`get_status` / `get_usage`);
    /// * plan mode (the permission mode `plan`; the CLI continues by itself after the plan's
    ///   approval, so there is no prompt to implement it);
    /// * fast mode for the models the CLI marks `supportsFastMode`.
    /// See [`check_fork_point`].
    fn check_fork_point(&self, point: &ForkPoint) -> Result<(), AdapterError> {
        check_fork_point(point)
    }

    /// The permission mode `plan` of earlier versions is plan mode ([`mapping::upgrade_settings`]).
    fn upgrade_settings(&self, settings: ThreadSettings) -> UpgradedSettings {
        mapping::upgrade_settings(settings)
    }

    fn features(&self) -> HarnessFeatures {
        HarnessFeatures {
            fork_at_turn: true,
            fork_while_held: true,
            rename: true,
            side_question: true,
            move_to_background: true,
            status: true,
            project_trust: false,
            plan_mode: Some(PlanModeFeature {
                implement_prompt: None,
                new_thread_preamble: None,
            }),
            fast_mode_models: self.fast_mode_models.lock().clone(),
        }
    }

    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        if let Some(cached) = self.commands.lock().menu(&ctx.cwd) {
            return Ok(cached);
        }
        let raw = self.handshake_probe(&ctx.cwd).await?;
        Ok(menu_from_initialize(&self.commands, &ctx.cwd, &raw))
    }

    fn session_switching_commands(&self) -> &'static [&'static str] {
        SESSION_SWITCHING_COMMANDS
    }

    fn session_switching_names(&self) -> Vec<String> {
        self.commands.lock().switching_names()
    }

    /// The status without a session: the plan's usage (`get_usage` of a throwaway process).
    /// `get_status` describes the session of the process that answers, which a throwaway
    /// process does not have, so it is left out.
    async fn status(&self, _cwd: &Path) -> Result<Vec<StatusSection>, AdapterError> {
        let dir = self.probe_dir()?;
        let (session, _events, _raw) = self.probe_process(&dir).await?;
        let usage = session.usage().await;
        StartGuard::for_session(session)
            .stop(StopReason::Shutdown)
            .await;
        Ok(mapping::usage_sections(&usage?, false))
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
        let dir = Self::projects_dir()?;
        let cwd = cwd.to_path_buf();
        let policy = self.ctx.policy.clone();
        tokio::task::spawn_blocking(move || native::list_sessions(&dir, &cwd, &policy))
            .await
            .map_err(|e| AdapterError::Other(format!("listing sessions failed: {e}")))?
    }

    async fn read_native_history(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<NativeHistory, AdapterError> {
        Ok(self
            .read_native_history_anchored(cwd, native_session_id)
            .await?
            .0)
    }

    /// The history with each turn's anchor: the uuid of its last main-thread entry
    /// (docs/adapters/claude.md §10).
    async fn read_native_history_anchored(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<(NativeHistory, Vec<Option<Value>>), AdapterError> {
        let dir = Self::projects_dir()?;
        let cwd = cwd.to_path_buf();
        let id = native_session_id.to_owned();
        let policy = self.ctx.policy.clone();
        tokio::task::spawn_blocking(move || {
            let path = native::find_transcript(&dir, &cwd, &id)?.ok_or_else(|| {
                AdapterError::Other(format!(
                    "no Claude Code session {id} recorded for {}",
                    cwd.display()
                ))
            })?;
            native::read_history(&path, &policy)
        })
        .await
        .map_err(|e| AdapterError::Other(format!("reading session failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn options_parse_strictly() {
        assert_eq!(
            ClaudeOptions::parse(&serde_json::Value::Null).unwrap(),
            ClaudeOptions::default()
        );
        assert!(
            ClaudeOptions::parse(&serde_json::json!({"allowBypassPermissions": true}))
                .unwrap()
                .allow_bypass_permissions
        );
        assert!(ClaudeOptions::parse(&serde_json::json!({"bogus": 1})).is_err());
        // Unset: the CLI's own default (the field is not sent).
        assert_eq!(ClaudeOptions::default().agent_progress_summaries, None);
        assert_eq!(
            ClaudeOptions::parse(&serde_json::json!({"agentProgressSummaries": true}))
                .unwrap()
                .agent_progress_summaries,
            Some(true)
        );
        assert_eq!(
            session::initialize_request(None),
            serde_json::json!({"subtype": "initialize",
                "hooks": {"Stop": [{"hookCallbackIds": ["aas_stop"]}]}, "perTaskStopAffordance": true})
        );
        assert_eq!(
            session::initialize_request(Some(false))["agentProgressSummaries"],
            false
        );
    }

    #[test]
    fn argument_values_are_checked() {
        assert!(checked_arg("model", "claude-opus-4-1[1m]").is_ok());
        assert!(checked_arg("model", "opus&calc").is_err());
        assert!(checked_arg("model", "--dangerously-skip-permissions").is_err());
        assert!(checked_arg("model", "").is_err());
        let args = settings_args(&ThreadSettings {
            model: Some("haiku".into()),
            effort: Some("low".into()),
            permission_mode: Some("acceptEdits".into()),
        })
        .unwrap();
        assert_eq!(
            strings(args),
            vec![
                "--model",
                "haiku",
                "--effort",
                "low",
                "--permission-mode",
                "acceptEdits"
            ]
        );
    }

    /// The command lines of recordings g2 (the turn included) and g3b (right before it).
    #[test]
    fn a_fork_at_a_turn_resumes_at_the_anchor() {
        let source = "ab417fbf-6aad-4b16-8592-6c5ec1d777e5";
        let mode = StartMode::Fork {
            native_session_id: source.into(),
        };
        let t1 = mapping::turn_anchor("cffd151b-1a78-4509-882b-5c8460ad0617");
        let t2 = mapping::turn_anchor("018cc139-cfac-4f9a-bb3c-d5959641a34c");
        let at = ForkPoint {
            anchor: t2.clone(),
            before: false,
            previous: Some(t1.clone()),
        };
        let (args, id) = session_args(&mode, Some(&at)).unwrap();
        assert_eq!(
            strings(args),
            vec![
                format!("--resume={source}"),
                "--resume-session-at=018cc139-cfac-4f9a-bb3c-d5959641a34c".to_owned(),
                "--fork-session".to_owned(),
                format!("--session-id={id}"),
            ]
        );
        assert_ne!(id, source);
        let before = ForkPoint {
            before: true,
            ..at.clone()
        };
        let (args, _) = session_args(&mode, Some(&before)).unwrap();
        assert!(
            strings(args)
                .contains(&"--resume-session-at=cffd151b-1a78-4509-882b-5c8460ad0617".to_owned())
        );
        assert_eq!(check_fork_point(&at), Ok(()));
        assert_eq!(check_fork_point(&before), Ok(()));
        // Without the turn before, the fork cannot end right before the turn: the check says
        // so before anything starts.
        let no_previous = ForkPoint {
            previous: None,
            ..before
        };
        assert!(session_args(&mode, Some(&no_previous)).is_err());
        assert!(check_fork_point(&no_previous).is_err());
        // An anchor of another shape, or one that is not a plain id, is refused.
        let bogus = ForkPoint {
            anchor: json!({"turn": 3}),
            before: false,
            previous: None,
        };
        assert!(session_args(&mode, Some(&bogus)).is_err());
        let unsafe_anchor = ForkPoint {
            anchor: mapping::turn_anchor("x&calc"),
            before: false,
            previous: None,
        };
        assert!(session_args(&mode, Some(&unsafe_anchor)).is_err());
        assert!(check_fork_point(&bogus).is_err());
        assert!(check_fork_point(&unsafe_anchor).is_err());
        // The whole session, and the other modes.
        let (args, id) = session_args(&mode, None).unwrap();
        assert_eq!(
            strings(args),
            vec![
                format!("--resume={source}"),
                "--fork-session".to_owned(),
                format!("--session-id={id}"),
            ]
        );
        assert!(
            session_args(
                &StartMode::Resume {
                    native_session_id: source.into()
                },
                Some(&at)
            )
            .is_err()
        );
        let (args, id) = session_args(&StartMode::New, None).unwrap();
        assert_eq!(strings(args), vec![format!("--session-id={id}")]);
    }

    #[test]
    fn version_parsing() {
        assert_eq!(
            parse_version("2.1.284 (Claude Code)\n").as_deref(),
            Some("2.1.284")
        );
        assert_eq!(parse_version("error").as_deref(), None);
    }

    fn adapter(dir: &Path) -> ClaudeAdapter {
        let supervisor = aas_supervisor::Supervisor::new(
            &dir.join("supervisor"),
            aas_supervisor::SupervisorPolicy {
                prevent_sleep: false,
                ..Default::default()
            },
        )
        .unwrap();
        ClaudeAdapter::new(
            HarnessConfig {
                id: "claude".into(),
                kind: HarnessKind::Claude,
                display_name: None,
                command: "claude".into(),
                args: Vec::new(),
                env: Default::default(),
                options: Value::Null,
            },
            AdapterContext {
                supervisor,
                state_dir: dir.join("claude"),
                policy: aas_harness::AdapterPolicy::default(),
            },
        )
    }

    #[test]
    fn features_follow_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = adapter(dir.path());
        let features = adapter.features();
        assert!(features.fork_at_turn && features.fork_while_held && features.rename);
        assert!(features.side_question && features.move_to_background && features.status);
        assert!(!features.project_trust);
        // Claude Code continues by itself after the plan's approval.
        assert_eq!(features.plan_mode, Some(PlanModeFeature::default()));
        assert!(features.fast_mode_models.is_empty());
        *adapter.fast_mode_models.lock() = vec!["opus".into()];
        assert_eq!(adapter.features().fast_mode_models, ["opus"]);
    }

    #[test]
    fn switching_names_are_the_commands_and_their_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = adapter(dir.path());
        let names = adapter.session_switching_names();
        for name in ["clear", "reset", "new", "resume", "continue"] {
            assert!(names.iter().any(|n| n == name), "{name}: {names:?}");
        }
    }
}
