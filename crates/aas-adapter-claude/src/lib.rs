//! Adapter for Claude Code (`claude`).
//!
//! Each session is one long-lived `claude -p --input-format stream-json
//! --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool
//! stdio` process, driven with the same bidirectional control protocol the official Agent
//! SDK uses (`control_request` / `control_response` / `control_cancel_request`).
//! See `docs/adapters/claude.md` for the full mapping.

mod background;
mod mapping;
mod native;
mod session;
mod time;

#[cfg(test)]
mod replay_tests;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aas_harness::protocol::{Command, HarnessCapabilities, HarnessKind, ThreadSettings};
use aas_harness::{
    AdapterContext, AdapterError, CommandContext, HarnessAdapter, HarnessConfig, HarnessInfo,
    NativeHistory, NativeSessionScan, NativeSessionSummary, SessionHandle, StartGuard, StartMode,
    StartRequest, StopReason,
};
use aas_supervisor::{SpawnSpec, ToolSpec, resolve_program};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Deserialize;

use crate::session::{ClaudeSession, CommandCache, ProcessLink, SessionParams};

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

/// Claude Code harness.
pub struct ClaudeAdapter {
    config: HarnessConfig,
    ctx: AdapterContext,
    display_name: String,
    options: Result<ClaudeOptions, String>,
    commands: CommandCache,
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
            commands: Arc::new(Mutex::new(HashMap::new())),
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

    /// Spawns a process and completes the `initialize` handshake.
    async fn launch(
        &self,
        label: String,
        cwd: &Path,
        owner: Option<String>,
        args: Vec<OsString>,
        native_session_id: String,
        settings: ThreadSettings,
    ) -> Result<
        (
            Arc<ClaudeSession>,
            tokio::sync::mpsc::UnboundedReceiver<aas_harness::AdapterEvent>,
            serde_json::Value,
        ),
        AdapterError,
    > {
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
                let tail = exit.stderr_tail.trim().to_owned();
                let tail = if tail.is_empty() {
                    session.stderr_tail().trim().to_owned()
                } else {
                    tail
                };
                Err(if tail.is_empty() {
                    e
                } else {
                    AdapterError::Harness(format!("{e}; claude stderr: {tail}"))
                })
            }
        }
    }

    /// Spawns a throwaway process (no session persistence) to read models and commands.
    async fn handshake_probe(&self, cwd: &Path) -> Result<serde_json::Value, AdapterError> {
        let args = common_args(false);
        let mut args = args;
        args.push("--no-session-persistence".into());
        let (session, _events, raw) = self
            .launch(
                format!("{}[probe]", self.config.id),
                cwd,
                None,
                args,
                String::new(),
                ThreadSettings::default(),
            )
            .await?;
        StartGuard::for_session(session)
            .stop(StopReason::Shutdown)
            .await;
        Ok(raw)
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

/// `claude --version` prints e.g. `2.1.283 (Claude Code)`.
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
        let probe_dir = self.ctx.state_dir.join("probe");
        if let Err(e) = std::fs::create_dir_all(&probe_dir) {
            return HarnessInfo::unavailable(format!("cannot create {}: {e}", probe_dir.display()));
        }
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
                    out.stderr_lossy().trim()
                ));
            }
            Err(e) => return HarnessInfo::unavailable(format!("`claude --version` failed: {e}")),
        };
        let raw = match self.handshake_probe(&probe_dir).await {
            Ok(raw) => raw,
            Err(e) => {
                let mut info = HarnessInfo::unavailable(format!("claude handshake failed: {e}"));
                info.version = version;
                info.executable = Some(program);
                return info;
            }
        };
        self.commands
            .lock()
            .insert(probe_dir, mapping::commands_from_initialize(&raw));
        let models = mapping::models_from_initialize(&raw);
        let effort_levels = mapping::effort_levels(&models);
        let current_mode = raw.get("current_permission_mode").and_then(|v| v.as_str());
        let permission_modes =
            mapping::permission_modes(current_mode, options.allow_bypass_permissions);
        HarnessInfo {
            available: true,
            unavailable_reason: None,
            version,
            executable: Some(program),
            capabilities: HarnessCapabilities {
                background_tasks: true,
                background_stop: true,
                interrupt: true,
                steer: false,
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
        let options = self.options()?.clone();
        let mut args = common_args(options.allow_bypass_permissions);
        let native_session_id = match &req.mode {
            StartMode::New => {
                let id = uuid::Uuid::new_v4().to_string();
                args.push(format!("--session-id={id}").into());
                id
            }
            StartMode::Resume { native_session_id } => {
                args.push(
                    format!("--resume={}", checked_arg("session id", native_session_id)?).into(),
                );
                native_session_id.clone()
            }
            StartMode::Fork { native_session_id } => {
                let id = uuid::Uuid::new_v4().to_string();
                args.push(
                    format!("--resume={}", checked_arg("session id", native_session_id)?).into(),
                );
                args.push("--fork-session".into());
                args.push(format!("--session-id={id}").into());
                id
            }
        };
        args.extend(settings_args(&req.settings)?);
        let label = format!("{}[{}]", self.config.id, req.thread_id);
        let (session, events, _raw) = self
            .launch(
                label,
                &req.cwd,
                Some(req.thread_id.to_string()),
                args,
                native_session_id.clone(),
                req.settings.clone(),
            )
            .await?;
        Ok(SessionHandle {
            native_session_id: Some(native_session_id),
            control: session,
            events,
        })
    }

    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        if let Some(cached) = self.commands.lock().get(&ctx.cwd).cloned() {
            return Ok(cached);
        }
        let raw = self.handshake_probe(&ctx.cwd).await?;
        let commands = mapping::commands_from_initialize(&raw);
        self.commands
            .lock()
            .insert(ctx.cwd.clone(), commands.clone());
        Ok(commands)
    }

    fn session_switching_commands(&self) -> &'static [&'static str] {
        SESSION_SWITCHING_COMMANDS
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

/// Claude Code's commands that leave the session the thread is bound to (never offered, see
/// [`HarnessAdapter::session_switching_commands`]):
/// * `clear` — "Start a new session with empty context; previous session stays on disk
///   (resumable with /resume)" (Claude Code 2.1.283 lists it in `initialize.commands` and runs
///   it in stream-json mode; the CLI then reports a new `session_id`);
/// * `resume` — its session picker (interactive-only today; excluded should a version offer it
///   in this mode).
const SESSION_SWITCHING_COMMANDS: &[&str] = &["clear", "resume"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_switching_commands_are_named_as_claude_code_reports_them() {
        // The shape of `initialize.commands` of Claude Code 2.1.283 (descriptions as reported).
        let init = serde_json::json!({"commands": [
            {"name": "clear", "description": "Start a new session with empty context; previous session stays on disk (resumable with /resume)", "argumentHint": "[name]"},
            {"name": "compact", "description": "Free up context by summarizing the conversation so far", "argumentHint": "<optional custom summarization instructions>"},
            {"name": "rename", "description": "Rename the current conversation", "argumentHint": "[name]"},
            {"name": "/resume", "description": "Resume a conversation", "argumentHint": ""},
        ]});
        let kept: Vec<String> = mapping::commands_from_initialize(&init)
            .into_iter()
            .map(|c| c.name)
            .filter(|name| !SESSION_SWITCHING_COMMANDS.contains(&name.as_str()))
            .collect();
        assert_eq!(kept, ["compact", "rename"]);
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
        let args: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap()).collect();
        assert_eq!(
            args,
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

    #[test]
    fn version_parsing() {
        assert_eq!(
            parse_version("2.1.283 (Claude Code)\n").as_deref(),
            Some("2.1.283")
        );
        assert_eq!(parse_version("error").as_deref(), None);
    }
}
