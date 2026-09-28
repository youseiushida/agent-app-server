//! Generic adapter for agents that speak the Agent Client Protocol (ACP) v1 over stdio,
//! e.g. `devin acp`. See `docs/adapters/acp.md` for the supported subset and every mapping.
//!
//! One agent process per session. The client advertises no `fs` / `terminal` capabilities,
//! so agents use their own tools; agent calls to such client methods are rejected with a
//! JSON-RPC "method not found" error and surfaced as a notice.

mod cache;
mod elicitation;
mod history;
mod mapping;
mod session;
mod tracker;
mod wire;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aas_harness::protocol::{Command, HarnessCapabilities, HarnessKind};
use aas_harness::{
    AdapterContext, AdapterError, CommandContext, HarnessAdapter, HarnessConfig, HarnessInfo,
    NativeHistory, NativeSessionSummary, SessionHandle, StartGuard, StartMode, StartRequest,
    StopReason, ThreadSettings,
};
use aas_stdio::{Incoming, RpcPeer, RpcWireError};
use aas_supervisor::{SpawnSpec, resolve_program};
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::cache::OptionsCache;
use crate::mapping::SettingKind;
pub use crate::session::AdapterOptions;
use crate::session::{LaunchParams, ProcessLink, SetupMode};
use crate::wire::InitializeResponse;

/// A configured ACP agent.
pub struct AcpAdapter {
    config: HarnessConfig,
    ctx: AdapterContext,
    /// Parsed `[harness.options]`; an error is reported by `probe` / `start`.
    options: Result<AdapterOptions, String>,
    cache: OptionsCache,
    display_name: String,
}

impl AcpAdapter {
    /// Never fails: invalid options make the harness unavailable with the parse error as the
    /// reason, so one misconfigured agent does not prevent the daemon from starting.
    pub fn new(config: HarnessConfig, ctx: AdapterContext) -> Self {
        let options = if config.options.is_null() {
            Ok(AdapterOptions::default())
        } else {
            serde_json::from_value::<AdapterOptions>(config.options.clone())
                .map_err(|e| e.to_string())
        };
        let cache = OptionsCache::load(ctx.state_dir.join("session-options.json"));
        let display_name = config
            .display_name
            .clone()
            .unwrap_or_else(|| config.id.clone());
        Self {
            config,
            ctx,
            options,
            cache,
            display_name,
        }
    }

    fn options(&self) -> Result<&AdapterOptions, AdapterError> {
        self.options.as_ref().map_err(|e| {
            AdapterError::Unavailable(format!(
                "invalid options for harness `{}`: {e}",
                self.config.id
            ))
        })
    }

    fn program(&self) -> Result<PathBuf, AdapterError> {
        resolve_program(&self.config.command).map_err(|e| AdapterError::Unavailable(e.to_string()))
    }

    async fn spawn(
        &self,
        label: String,
        cwd: &Path,
        owner: Option<String>,
    ) -> Result<aas_supervisor::ManagedChild, AdapterError> {
        let program = self.program()?;
        let mut spec = SpawnSpec::new(label, program, cwd).args(self.config.args.iter());
        for (k, v) in &self.config.env {
            spec = spec.env(k, v);
        }
        if let Some(owner) = owner {
            spec = spec.owner(owner);
        }
        self.ctx
            .supervisor
            .spawn(spec)
            .await
            .map_err(|e| AdapterError::Spawn(e.to_string()))
    }

    async fn launch(
        &self,
        label: String,
        cwd: &Path,
        owner: Option<String>,
        mode: SetupMode,
        settings: ThreadSettings,
    ) -> Result<session::Launched, AdapterError> {
        let options = self.options()?.clone();
        let mut child = self.spawn(label.clone(), cwd, owner).await?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            child.handle.kill(StopReason::Shutdown);
            return Err(AdapterError::Spawn("child pipes unavailable".into()));
        };
        let link: Arc<dyn ProcessLink> = Arc::new(child.handle.clone());
        session::launch(LaunchParams {
            reader: stdout,
            writer: stdin,
            link,
            label,
            agent_name: self.display_name.clone(),
            cwd: cwd.to_path_buf(),
            mode,
            settings,
            options,
            policy: self.ctx.policy.clone(),
            cache: self.cache.clone(),
        })
        .await
    }

    /// A short-lived connection (initialize only) used by probe / listing.
    async fn connect(&self, cwd: &Path, purpose: &str) -> Result<ShortLived, AdapterError> {
        let options = self.options()?.clone();
        let label = format!("{}[{purpose}]", self.config.id);
        let mut child = self.spawn(label.clone(), cwd, None).await?;
        let handle = child.handle.clone();
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            handle.kill(StopReason::Shutdown);
            return Err(AdapterError::Spawn("child pipes unavailable".into()));
        };
        let (peer, incoming) = RpcPeer::start(
            stdout,
            stdin,
            session::peer_config(&label, &self.ctx.policy),
        );
        tokio::spawn(reject_requests(peer.clone(), incoming));
        // Staged stop (design §4.3), also when the caller drops the connection half-way.
        let guard = {
            let (peer, grace) = (peer.clone(), self.ctx.policy.stop_grace);
            StartGuard::new(move |reason| async move {
                peer.close_writer().await;
                handle.shutdown(grace, reason).await
            })
        };
        let short = ShortLived {
            peer,
            guard,
            init: None,
        };
        let init = match short
            .peer
            .request_timeout::<_, InitializeResponse>(
                "initialize",
                session::client_init(),
                self.ctx.policy.handshake_timeout,
            )
            .await
        {
            Ok(init) => init,
            Err(e) => {
                let err = session::setup_error("initialize", e, &self.display_name, &options, None);
                let info = short.close().await;
                let tail = info.stderr_tail.trim();
                return Err(match err {
                    AdapterError::Spawn(m) if !tail.is_empty() => {
                        AdapterError::Spawn(format!("{m}: {tail}"))
                    }
                    other => other,
                });
            }
        };
        if let Some(method) = &options.auth_method
            && let Err(e) = short
                .peer
                .request_timeout::<_, serde_json::Value>(
                    "authenticate",
                    serde_json::json!({ "methodId": method }),
                    self.ctx.policy.handshake_timeout,
                )
                .await
        {
            let err =
                session::setup_error("authenticate", e, &self.display_name, &options, Some(&init));
            short.close().await;
            return Err(err);
        }
        Ok(ShortLived {
            init: Some(init),
            ..short
        })
    }

    fn info_from(&self, program: PathBuf, init: &InitializeResponse) -> HarnessInfo {
        let caps = &init.agent_capabilities;
        let cached = self.cache.get();
        let version = init.agent_info.as_ref().map(|a| {
            let name = a
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| a.name.clone());
            if a.version.is_empty() {
                name
            } else {
                format!("{name} {}", a.version)
            }
        });
        HarnessInfo {
            available: true,
            unavailable_reason: None,
            version,
            executable: Some(program),
            capabilities: HarnessCapabilities {
                interrupt: true,
                steer: false,
                approvals: true,
                questions: true,
                resume: caps.load_session || caps.session_capabilities.resume(),
                fork: caps.session_capabilities.fork(),
                images: caps.prompt_capabilities.image,
                model_switch_live: cached.options.has(SettingKind::Model),
                native_sessions: caps.session_capabilities.list() && caps.load_session,
            },
            models: cached.options.models(cached.defaults.model.as_deref()),
            default_model: cached.defaults.model.clone(),
            effort_levels: cached.options.effort_levels(),
            permission_modes: cached
                .options
                .permission_modes(cached.defaults.mode.as_deref()),
            default_permission_mode: cached.defaults.mode.clone(),
        }
    }
}

/// A process started for one probe or listing. Dropped without [`close`](Self::close), its
/// guard still runs the staged stop in the background.
struct ShortLived {
    peer: RpcPeer,
    guard: StartGuard,
    init: Option<InitializeResponse>,
}

impl ShortLived {
    async fn close(self) -> aas_harness::ExitInfo {
        self.guard.stop(StopReason::Shutdown).await
    }

    fn init(&self) -> &InitializeResponse {
        self.init.as_ref().expect("initialized")
    }
}

/// Drains a short-lived connection, rejecting agent requests.
async fn reject_requests(peer: RpcPeer, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request(req) = msg {
            let _ = peer
                .respond_error(req.id, RpcWireError::method_not_found(&req.method))
                .await;
        }
    }
}

/// Pages through `session/list` for `cwd`. Sessions whose `cwd` differs are dropped (the
/// request already filters; this guards against agents that ignore the filter). A repeated
/// cursor ends the listing instead of looping forever.
async fn list_sessions(
    peer: &RpcPeer,
    cwd: &Path,
    timeout: std::time::Duration,
) -> Result<Vec<NativeSessionSummary>, AdapterError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    loop {
        let req = wire::ListSessionsRequest {
            cwd: session::path_string(cwd),
            cursor: cursor.clone(),
        };
        let page: wire::ListSessionsResponse = peer
            .request_timeout("session/list", req, timeout)
            .await
            .map_err(|e| AdapterError::Harness(format!("session/list: {e}")))?;
        for s in page.sessions {
            if !same_path(&s.cwd, cwd) {
                continue;
            }
            out.push(NativeSessionSummary {
                updated_at: s.updated_at.as_deref().and_then(rfc3339_millis),
                native_session_id: s.session_id,
                title: s.title.filter(|t| !t.is_empty()),
                cwd: Some(s.cwd),
            });
        }
        match page.next_cursor.filter(|c| !c.is_empty()) {
            Some(next) if seen_cursors.insert(next.clone()) => cursor = Some(next),
            Some(next) => {
                tracing::warn!(cursor = %next, "session/list repeated a cursor; stopping");
                return Ok(out);
            }
            None => return Ok(out),
        }
    }
}

/// Parses an RFC 3339 timestamp into Unix milliseconds.
fn rfc3339_millis(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok()
}

/// Path equality as the OS sees it (case-insensitive on Windows, separator-agnostic,
/// trailing separators ignored).
fn same_path(a: &str, b: &Path) -> bool {
    fn norm(s: &str) -> String {
        let s = s.replace('\\', "/");
        let s = s.trim_end_matches('/').to_owned();
        if cfg!(windows) { s.to_lowercase() } else { s }
    }
    norm(a) == norm(&b.to_string_lossy())
}

#[async_trait]
impl HarnessAdapter for AcpAdapter {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn kind(&self) -> HarnessKind {
        HarnessKind::Acp
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    async fn probe(&self) -> HarnessInfo {
        let program = match self.options().and_then(|_| self.program()) {
            Ok(p) => p,
            Err(AdapterError::Unavailable(reason)) => return HarnessInfo::unavailable(reason),
            Err(e) => return HarnessInfo::unavailable(e.to_string()),
        };
        if let Err(e) = std::fs::create_dir_all(&self.ctx.state_dir) {
            return HarnessInfo::unavailable(format!(
                "cannot create {}: {e}",
                self.ctx.state_dir.display()
            ));
        }
        match self.connect(&self.ctx.state_dir, "probe").await {
            Ok(short) => {
                let info = self.info_from(program, short.init());
                short.close().await;
                info
            }
            Err(e) => HarnessInfo::unavailable(e.to_string()),
        }
    }

    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError> {
        let mode = match req.mode {
            StartMode::New => SetupMode::New,
            StartMode::Resume { native_session_id } => SetupMode::Resume(native_session_id),
            StartMode::Fork { native_session_id } => SetupMode::Fork(native_session_id),
        };
        let label = format!("{}[{}]", self.config.id, req.thread_id);
        let launched = self
            .launch(
                label,
                &req.cwd,
                Some(req.thread_id.to_string()),
                mode,
                req.settings,
            )
            .await?;
        Ok(launched.handle)
    }

    /// Commands come from `available_commands_update` of the agent's most recent session
    /// (ACP publishes them only inside a session).
    async fn commands(&self, _ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        Ok(self
            .cache
            .get()
            .commands
            .iter()
            .map(mapping::command)
            .collect())
    }

    async fn list_native_sessions(
        &self,
        cwd: &Path,
    ) -> Result<Vec<NativeSessionSummary>, AdapterError> {
        let short = self.connect(cwd, "list").await?;
        if !short.init().agent_capabilities.session_capabilities.list() {
            short.close().await;
            return Err(AdapterError::Unsupported("session/list"));
        }
        let result = list_sessions(&short.peer, cwd, self.ctx.policy.handshake_timeout).await;
        short.close().await;
        result
    }

    async fn read_native_history(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<NativeHistory, AdapterError> {
        let label = format!("{}[history]", self.config.id);
        let launched = self
            .launch(
                label,
                cwd,
                None,
                SetupMode::History(native_session_id.to_owned()),
                ThreadSettings::default(),
            )
            .await?;
        StartGuard::for_session(launched.handle.control.clone())
            .stop(StopReason::Shutdown)
            .await;
        Ok(launched.history.unwrap_or_default())
    }
}

/// Entry points for replay tests: run the protocol core over arbitrary streams.
#[doc(hidden)]
pub mod testing {
    use super::*;
    use aas_harness::{AdapterPolicy, ExitInfo};
    use async_trait::async_trait;
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::sync::watch;

    pub use crate::session::AdapterOptions;

    /// A process stand-in: exits when [`FakeLink::exit`] is called or on shutdown.
    #[derive(Clone)]
    pub struct FakeLink {
        tx: watch::Sender<Option<ExitInfo>>,
    }

    impl Default for FakeLink {
        fn default() -> Self {
            Self {
                tx: watch::channel(None).0,
            }
        }
    }

    impl FakeLink {
        /// Resolves once the stand-in process has ended.
        pub async fn wait_exit(&self) -> ExitInfo {
            ProcessLink::wait(self).await
        }

        pub fn exit(&self, code: i32, stderr: &str) {
            self.tx.send_if_modified(|v| {
                if v.is_none() {
                    *v = Some(ExitInfo {
                        code: Some(code),
                        stopped: None,
                        stderr_tail: stderr.into(),
                        exited_at_ms: 0,
                    });
                    true
                } else {
                    false
                }
            });
        }
    }

    #[async_trait]
    impl ProcessLink for FakeLink {
        async fn wait(&self) -> ExitInfo {
            let mut rx = self.tx.subscribe();
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

        async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
            if let Ok(info) = tokio::time::timeout(grace, self.wait()).await {
                return info;
            }
            self.tx.send_if_modified(|v| {
                if v.is_none() {
                    *v = Some(ExitInfo {
                        code: Some(1),
                        stopped: Some(reason),
                        stderr_tail: String::new(),
                        exited_at_ms: 0,
                    });
                    true
                } else {
                    false
                }
            });
            self.wait().await
        }
    }

    /// Mode of [`launch_with_io`].
    pub enum Mode {
        New,
        Resume(String),
        Fork(String),
        History(String),
    }

    pub struct Launched {
        pub handle: SessionHandle,
        pub history: Option<NativeHistory>,
    }

    /// Runs the full handshake over the given streams.
    // The arguments are the independent inputs of one launch (streams, link, cwd, mode and the
    // three settings layers); bundling them into a struct used only here would add no clarity.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch_with_io<R, W>(
        reader: R,
        writer: W,
        link: FakeLink,
        cwd: PathBuf,
        mode: Mode,
        settings: ThreadSettings,
        options: AdapterOptions,
        policy: AdapterPolicy,
    ) -> Result<Launched, AdapterError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let mode = match mode {
            Mode::New => SetupMode::New,
            Mode::Resume(id) => SetupMode::Resume(id),
            Mode::Fork(id) => SetupMode::Fork(id),
            Mode::History(id) => SetupMode::History(id),
        };
        let launched = session::launch(LaunchParams {
            reader,
            writer,
            link: Arc::new(link),
            label: "acp[test]".into(),
            agent_name: "Test Agent".into(),
            cwd,
            mode,
            settings,
            options,
            policy,
            cache: OptionsCache::memory(),
        })
        .await?;
        Ok(Launched {
            handle: launched.handle,
            history: launched.history,
        })
    }

    /// Runs the `session/list` pagination over the given streams. No `initialize` is sent;
    /// the scripted agent answers the listing requests directly.
    pub async fn list_with_io<R, W>(
        reader: R,
        writer: W,
        cwd: &Path,
    ) -> Result<Vec<NativeSessionSummary>, AdapterError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let policy = AdapterPolicy::default();
        let (peer, incoming) = RpcPeer::start(
            reader,
            writer,
            session::peer_config("acp[test-list]", &policy),
        );
        tokio::spawn(reject_requests(peer.clone(), incoming));
        super::list_sessions(&peer, cwd, policy.handshake_timeout).await
    }

    /// Converts a turn input to ACP prompt blocks (exposed for tests).
    pub async fn prompt_blocks(
        input: &aas_harness::TurnInput,
        image: bool,
    ) -> Result<Vec<serde_json::Value>, AdapterError> {
        let caps = wire::PromptCapabilities {
            image,
            audio: false,
            embedded_context: false,
        };
        session::prompt_blocks(input, &caps).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parses() {
        assert_eq!(
            rfc3339_millis("2026-09-27T03:52:09+00:00"),
            Some(1_790_481_129_000)
        );
        assert_eq!(rfc3339_millis("not a date"), None);
    }

    #[test]
    fn path_comparison() {
        assert!(same_path(
            r"C:\Users\me\proj\",
            Path::new(r"C:\Users\me\proj")
        ));
        if cfg!(windows) {
            assert!(same_path(
                r"c:\users\ME\proj",
                Path::new(r"C:\Users\me\proj")
            ));
        }
        assert!(!same_path(
            r"C:\Users\me\proj2",
            Path::new(r"C:\Users\me\proj")
        ));
    }

    #[test]
    fn options_parse_and_reject_unknown_keys() {
        let ok: AdapterOptions =
            serde_json::from_value(serde_json::json!({"auth_hint": "run `devin auth login`"}))
                .unwrap();
        assert_eq!(ok.auth_hint.as_deref(), Some("run `devin auth login`"));
        assert!(serde_json::from_value::<AdapterOptions>(serde_json::json!({"bogus": 1})).is_err());
    }
}
