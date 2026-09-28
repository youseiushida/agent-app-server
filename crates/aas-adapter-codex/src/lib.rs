//! Adapter for OpenAI Codex (`codex app-server`, protocol v2). See `docs/adapters/codex.md`.
//!
//! One `codex app-server` process serves one thread. The protocol core (`session`) is
//! independent of process spawning so that recorded transcripts can be replayed over in-memory
//! pipes (see `tests/replay.rs`).

mod commands;
mod history;
mod link;
mod mapping;
mod server;
mod session;
mod settings;
mod wire;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use aas_harness::protocol::{Command, HarnessCapabilities, HarnessKind};
use aas_harness::{
    AdapterContext, AdapterError, AdapterPolicy, CommandContext, HarnessAdapter, HarnessConfig,
    HarnessInfo, NativeHistory, NativeSessionSummary, SessionHandle, StartRequest,
};
use aas_stdio::{RpcCallError, RpcPeer};
use aas_supervisor::{StopReason, ToolSpec, resolve_program};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

pub use link::ProcessLink;

/// Adapter options (`[[harness]] options = { … }` in config.toml). Parsed strictly, like the
/// options of every other adapter: an unknown key (a typo, another adapter's key style) or a
/// value of the wrong type makes the harness unavailable with the reason, instead of being
/// ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct CodexOptions {
    /// Upper bound of native sessions returned by `list_native_sessions` (policy value: the
    /// import list on a phone stays short; older threads remain importable by id). At least
    /// 1: zero would list nothing, which reads as "Codex has no sessions here".
    pub native_session_list_limit: usize,
}

impl Default for CodexOptions {
    fn default() -> Self {
        Self {
            native_session_list_limit: 200,
        }
    }
}

impl CodexOptions {
    /// The options of `[[harness]] options`; `null` (none given) is the defaults.
    pub fn parse(value: &Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let options: Self = serde_json::from_value(value.clone())
            .map_err(|e| format!("invalid codex options: {e}"))?;
        if options.native_session_list_limit == 0 {
            return Err("invalid codex options: nativeSessionListLimit must be at least 1".into());
        }
        Ok(options)
    }
}

/// Threads asked for per `thread/list` page when listing native sessions. A pure batching
/// size: pages are read until `nativeSessionListLimit` sessions are collected or the list ends.
const THREAD_LIST_PAGE_SIZE: usize = 100;

const CAPABILITIES: HarnessCapabilities = HarnessCapabilities {
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

pub(crate) fn rpc_err(method: &str, error: RpcCallError) -> AdapterError {
    match error {
        RpcCallError::Rpc(e) => AdapterError::Harness(format!("{method}: {}", e.message)),
        RpcCallError::Closed => AdapterError::Closed,
        RpcCallError::Io(e) => AdapterError::Other(format!("{method}: {e}")),
        RpcCallError::Decode(e) => AdapterError::Protocol(format!("{method}: {e}")),
        RpcCallError::Timeout(d) => {
            AdapterError::Harness(format!("{method}: no response within {d:?}"))
        }
    }
}

/// The Codex harness.
pub struct CodexAdapter {
    config: HarnessConfig,
    ctx: AdapterContext,
    /// Parsed `[harness.options]`; an error makes the harness unavailable (`probe`) and fails
    /// every call that needs the CLI.
    options: Result<CodexOptions, String>,
    display_name: String,
    live: Mutex<Vec<Weak<session::Shared>>>,
}

impl CodexAdapter {
    pub fn new(config: HarnessConfig, ctx: AdapterContext) -> Self {
        let options = CodexOptions::parse(&config.options);
        let display_name = config
            .display_name
            .clone()
            .unwrap_or_else(|| "Codex".to_owned());
        Self {
            config,
            ctx,
            options,
            display_name,
            live: Mutex::new(Vec::new()),
        }
    }

    fn options(&self) -> Result<&CodexOptions, AdapterError> {
        self.options
            .as_ref()
            .map_err(|e| AdapterError::Unavailable(e.clone()))
    }

    /// The executable, once the options are known to be valid: every call that runs the CLI
    /// goes through here, so a configuration error is reported instead of being ignored.
    fn program(&self) -> Result<PathBuf, AdapterError> {
        self.options()?;
        resolve_program(&self.config.command).map_err(|e| AdapterError::Unavailable(e.to_string()))
    }

    /// A peer of a live session (any thread: listing requests take explicit arguments).
    fn live_peer(&self) -> Option<RpcPeer> {
        let mut live = self.live.lock();
        live.retain(|w| w.upgrade().is_some_and(|s| s.is_alive()));
        live.iter()
            .find_map(|w| w.upgrade())
            .map(|s| s.peer().clone())
    }

    async fn with_peer<T, F, Fut>(&self, f: F) -> Result<T, AdapterError>
    where
        F: FnOnce(RpcPeer) -> Fut,
        Fut: std::future::Future<Output = Result<T, AdapterError>>,
    {
        match self.live_peer() {
            Some(peer) => f(peer).await,
            None => {
                let program = self.program()?;
                server::with_short_lived(&self.config, &self.ctx, &program, f).await
            }
        }
    }

    fn policy(&self) -> &AdapterPolicy {
        &self.ctx.policy
    }
}

async fn list_models(
    peer: &RpcPeer,
    policy: &AdapterPolicy,
) -> Result<Vec<wire::WireModel>, AdapterError> {
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut params = json!({});
        if let Some(c) = &cursor {
            params["cursor"] = json!(c);
        }
        let page: wire::ModelListResponse = peer
            .request_timeout("model/list", params, policy.handshake_timeout)
            .await
            .map_err(|e| rpc_err("model/list", e))?;
        models.extend(page.data);
        match page.next_cursor {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(models)
}

#[async_trait]
impl HarnessAdapter for CodexAdapter {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    async fn probe(&self) -> HarnessInfo {
        let program = match self.program() {
            Ok(p) => p,
            Err(e) => return HarnessInfo::unavailable(e.to_string()),
        };
        if let Err(e) = std::fs::create_dir_all(&self.ctx.state_dir) {
            return HarnessInfo::unavailable(format!(
                "cannot create {}: {e}",
                self.ctx.state_dir.display()
            ));
        }
        let version = match self
            .ctx
            .supervisor
            .run_tool(
                ToolSpec::new(&program, &self.ctx.state_dir)
                    .args(["--version"])
                    .timeout(self.policy().handshake_timeout),
            )
            .await
        {
            Ok(out) if out.success() => out
                .stdout_lossy()
                .lines()
                .next()
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty()),
            Ok(out) => {
                return HarnessInfo::unavailable(format!(
                    "`codex --version` failed ({}): {}",
                    out.code
                        .map_or("no exit code".into(), |c| format!("exit code {c}")),
                    out.stderr_lossy().trim()
                ));
            }
            Err(e) => return HarnessInfo::unavailable(format!("`codex --version` failed: {e}")),
        };
        let policy = self.policy().clone();
        let models =
            match server::with_short_lived(&self.config, &self.ctx, &program, |peer| async move {
                list_models(&peer, &policy).await
            })
            .await
            {
                Ok(m) => m,
                Err(e) => {
                    return HarnessInfo::unavailable(format!(
                        "codex app-server did not start: {e}"
                    ));
                }
            };
        let catalog = settings::model_catalog(&models);
        HarnessInfo {
            available: true,
            unavailable_reason: None,
            version,
            executable: Some(program),
            capabilities: CAPABILITIES,
            models: catalog.models,
            default_model: catalog.default_model,
            effort_levels: catalog.effort_levels,
            permission_modes: settings::permission_modes(),
            default_permission_mode: Some(settings::DEFAULT_PRESET.to_owned()),
        }
    }

    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError> {
        let program = self.program()?;
        let spawned = server::spawn_app_server(
            &self.config,
            &self.ctx,
            &program,
            &req.cwd,
            format!("{}[{}]", self.config.id, req.thread_id),
            Some(req.thread_id.to_string()),
        )
        .await?;
        let handle = spawned.handle.clone();
        // The peer's reader and the event pump keep the process alive: if this future is
        // dropped during the handshake, the guard runs the staged stop (design §4.3).
        let guard = server::start_guard(&spawned.peer, &handle, self.policy().stop_grace);
        let result = async {
            server::initialize(&spawned.peer, self.policy().handshake_timeout).await?;
            session::establish(
                spawned.peer.clone(),
                spawned.incoming,
                Arc::new(spawned.handle),
                session::EstablishArgs {
                    mode: req.mode.clone(),
                    cwd: req.cwd.clone(),
                    settings: req.settings.clone(),
                    policy: self.policy().clone(),
                },
            )
            .await
        }
        .await;
        match result {
            Ok((session, shared)) => {
                guard.disarm();
                self.live.lock().push(Arc::downgrade(&shared));
                Ok(session)
            }
            Err(e) => {
                // Stopped first so the stderr tail is complete.
                guard.stop(StopReason::Shutdown).await;
                Err(server::with_stderr(e, &handle))
            }
        }
    }

    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        let policy = self.policy().clone();
        let cwd = ctx.cwd.clone();
        let skills = self
            .with_peer(|peer| async move { Ok(session::fetch_skills(&peer, &cwd, &policy).await) })
            .await?;
        Ok(commands::commands(&skills))
    }

    async fn list_native_sessions(
        &self,
        cwd: &Path,
    ) -> Result<Vec<NativeSessionSummary>, AdapterError> {
        let limit = self.options()?.native_session_list_limit;
        let policy = self.policy().clone();
        let cwd = cwd.to_string_lossy().into_owned();
        self.with_peer(|peer| async move {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            while out.len() < limit {
                let mut params = json!({
                    "cwd": cwd,
                    "limit": (limit - out.len()).min(THREAD_LIST_PAGE_SIZE),
                    "sortKey": "updated_at",
                    "sortDirection": "desc",
                    "archived": false,
                });
                if let Some(c) = &cursor {
                    params["cursor"] = json!(c);
                }
                let page: wire::ThreadListResponse = peer
                    .request_timeout("thread/list", params, policy.handshake_timeout)
                    .await
                    .map_err(|e| rpc_err("thread/list", e))?;
                out.extend(page.data.iter().map(history::summary));
                match page.next_cursor {
                    Some(next) if Some(&next) != cursor.as_ref() && !page.data.is_empty() => {
                        cursor = Some(next)
                    }
                    _ => break,
                }
            }
            out.truncate(limit);
            Ok(out)
        })
        .await
    }

    async fn read_native_history(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<NativeHistory, AdapterError> {
        let policy = self.policy().clone();
        let id = native_session_id.to_owned();
        let thread = self
            .with_peer(|peer| async move {
                let resp: wire::ThreadReadResponse = peer
                    .request_timeout(
                        "thread/read",
                        json!({ "threadId": id, "includeTurns": true }),
                        policy.handshake_timeout,
                    )
                    .await
                    .map_err(|e| rpc_err("thread/read", e))?;
                Ok(resp.thread)
            })
            .await?;
        Ok(history::history(&thread, cwd))
    }
}

/// Entry points for replay tests (not part of the stable API).
#[doc(hidden)]
pub mod testing {
    use super::*;
    use aas_stdio::RpcPeerConfig;
    use tokio::io::{AsyncRead, AsyncWrite};

    pub use crate::mapping::UsageTracker;

    /// Runs the `initialize` + thread handshake over arbitrary streams and returns the session.
    pub async fn establish<R, W>(
        reader: R,
        writer: W,
        link: Arc<dyn ProcessLink>,
        req: StartRequest,
        policy: AdapterPolicy,
    ) -> Result<SessionHandle, AdapterError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (peer, incoming) = RpcPeer::start(
            reader,
            writer,
            RpcPeerConfig {
                emit_jsonrpc_field: false,
                max_line_bytes: policy.max_line_bytes,
                label: "codex[test]".into(),
            },
        );
        server::initialize(&peer, policy.handshake_timeout).await?;
        let (handle, _shared) = session::establish(
            peer,
            incoming,
            link,
            session::EstablishArgs {
                mode: req.mode,
                cwd: req.cwd,
                settings: req.settings,
                policy,
            },
        )
        .await?;
        Ok(handle)
    }

    /// Commands advertised for the given skills (`name`, `description`, `path`).
    pub fn commands_for(skills: &[(String, String, String)]) -> Vec<Command> {
        let skills: Vec<commands::SkillInfo> = skills
            .iter()
            .map(|(name, description, path)| commands::SkillInfo {
                name: name.clone(),
                description: description.clone(),
                path: path.clone(),
            })
            .collect();
        commands::commands(&skills)
    }

    /// The Codex sandbox modes (`sandbox_mode` values) the adapter's permission modes request,
    /// each once, in the order of the permission modes.
    pub fn sandbox_modes() -> Vec<&'static str> {
        let mut modes: Vec<&'static str> = Vec::new();
        for preset in settings::PRESETS {
            if !modes.contains(&preset.sandbox_mode) {
                modes.push(preset.sandbox_mode);
            }
        }
        modes
    }

    /// Maps a raw `thread/read` result to history (for tests of native import).
    pub fn history_from_thread_read(result: Value, cwd: &Path) -> Result<NativeHistory, String> {
        let resp: wire::ThreadReadResponse =
            serde_json::from_value(result).map_err(|e| e.to_string())?;
        Ok(history::history(&resp.thread, cwd))
    }

    /// Maps a raw `thread/list` result to summaries.
    pub fn summaries_from_thread_list(result: Value) -> Result<Vec<NativeSessionSummary>, String> {
        let resp: wire::ThreadListResponse =
            serde_json::from_value(result).map_err(|e| e.to_string())?;
        Ok(resp.data.iter().map(history::summary).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn options_parse_strictly() {
        assert_eq!(
            CodexOptions::parse(&Value::Null).unwrap(),
            CodexOptions::default()
        );
        assert_eq!(
            CodexOptions::parse(&json!({"nativeSessionListLimit": 500}))
                .unwrap()
                .native_session_list_limit,
            500
        );
        // Another adapter's key style names the key it expected instead of being ignored.
        let err = CodexOptions::parse(&json!({"native_session_list_limit": 500})).unwrap_err();
        assert!(err.contains("nativeSessionListLimit"), "{err}");
        // A value of the wrong type is an error, not the default.
        let err = CodexOptions::parse(&json!({"nativeSessionListLimit": "500"})).unwrap_err();
        assert!(err.contains("invalid codex options"), "{err}");
        // Zero would list nothing, which reads as "no sessions".
        let err = CodexOptions::parse(&json!({"nativeSessionListLimit": 0})).unwrap_err();
        assert!(err.contains("at least 1"), "{err}");
    }

    #[tokio::test]
    async fn invalid_options_make_the_harness_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = AdapterContext {
            supervisor: aas_supervisor::Supervisor::new(
                &dir.path().join("state"),
                aas_supervisor::SupervisorPolicy {
                    prevent_sleep: false,
                    ..Default::default()
                },
            )
            .unwrap(),
            state_dir: dir.path().join("adapter"),
            policy: AdapterPolicy {
                stop_grace: Duration::from_millis(500),
                ..Default::default()
            },
        };
        let adapter = CodexAdapter::new(
            HarnessConfig {
                id: "codex".into(),
                kind: HarnessKind::Codex,
                display_name: None,
                command: "codex".into(),
                args: Vec::new(),
                env: Default::default(),
                options: json!({"nativeSessionListLimit": 0}),
            },
            ctx,
        );
        let info = adapter.probe().await;
        assert!(!info.available, "{info:?}");
        let reason = info.unavailable_reason.unwrap();
        assert!(reason.contains("nativeSessionListLimit"), "{reason}");
        match adapter.list_native_sessions(dir.path()).await {
            Err(AdapterError::Unavailable(m)) => assert!(reason.contains(&m), "{m} / {reason}"),
            other => panic!("{other:?}"),
        }
    }
}
