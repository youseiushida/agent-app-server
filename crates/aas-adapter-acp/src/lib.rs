//! Generic adapter for agents that speak the Agent Client Protocol (ACP) v1 over stdio,
//! e.g. `devin acp`. See `docs/adapters/acp.md` for the supported subset and every mapping.
//!
//! One agent process per session. The client advertises no `fs` / `terminal` capabilities,
//! so agents use their own tools; agent calls to such client methods are rejected with a
//! JSON-RPC "method not found" error and surfaced as a notice.
//!
//! Standard ACP has no notion of work outside a prompt turn, so a plain ACP agent reports no
//! background tasks. Devin's background sub-agents and shells are mapped from Cognition's
//! extension when the agent confirms it (`cognition`), and so are its other extensions the
//! harness features use: forks at a turn (`revert`), the session's name, and its statistics
//! (`stats`).

mod cache;
mod cognition;
mod elicitation;
mod history;
mod mapping;
mod revert;
mod session;
mod stats;
mod tracker;
mod wire;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aas_harness::protocol::{Command, HarnessCapabilities, HarnessFeatures, HarnessKind};
use aas_harness::{
    AdapterContext, AdapterError, AdapterPolicy, CommandContext, ForkPoint, HarnessAdapter,
    HarnessConfig, HarnessInfo, NativeHistory, NativeSessionSet, NativeSessionSummary,
    SessionHandle, StartGuard, StartMode, StartOptions, StartRequest, StopReason, ThreadSettings,
};
use aas_stdio::{Incoming, RpcPeer, RpcWireError};
use aas_supervisor::{SpawnSpec, resolve_program};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::cache::OptionsCache;
use crate::cognition::Extensions;
use crate::mapping::SettingKind;
use crate::revert::{ForkTarget, StepIndex};
pub use crate::session::AdapterOptions;
use crate::session::{LaunchParams, ProcessLink, SetupMode};
use crate::wire::InitializeResponse;

/// What the agent's latest `initialize` answer says about the features the adapter offers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AgentProfile {
    /// Cognition's extensions the agent confirmed.
    ext: Extensions,
    /// ACP's own (unstable) `session/fork` of a whole session.
    session_fork: bool,
    /// `session/load` (which opens a branch made by `forkFromStep`).
    load_session: bool,
}

impl AgentProfile {
    fn of(init: &InitializeResponse) -> Self {
        let caps = &init.agent_capabilities;
        Self {
            ext: cognition::extensions(init),
            session_fork: caps.session_capabilities.fork(),
            load_session: caps.load_session,
        }
    }

    /// Forks at a step (Cognition's `forkFromStep`, whose branch is opened with
    /// `session/load`).
    fn fork_from_step(&self) -> bool {
        self.ext.revert && self.load_session
    }
}

/// A configured ACP agent.
pub struct AcpAdapter {
    config: HarnessConfig,
    ctx: AdapterContext,
    /// Parsed `[harness.options]`; an error is reported by `probe` / `start`.
    options: Result<AdapterOptions, String>,
    cache: OptionsCache,
    display_name: String,
    /// From the latest `initialize` answer (a probe, a start or a listing): what `features` and
    /// `commands` offer.
    profile: Arc<Mutex<AgentProfile>>,
    /// The steps this adapter's sessions listed (`revert`).
    index: StepIndex,
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
            profile: Arc::default(),
            index: StepIndex::default(),
        }
    }

    fn profile(&self) -> AgentProfile {
        *self.profile.lock().expect("profile lock")
    }

    fn learn(&self, init: &InitializeResponse) {
        *self.profile.lock().expect("profile lock") = AgentProfile::of(init);
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
        let launched = session::launch(LaunchParams {
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
            index: self.index.clone(),
        })
        .await?;
        self.learn(&launched.init);
        Ok(launched)
    }

    /// How a fork is set up (`StartMode::Fork`): ACP's `session/fork` for a whole session when
    /// the agent offers it; otherwise Cognition's `forkFromStep` at the node the anchor (or,
    /// for a whole session, its last step) names. When that node was never listed after its
    /// turn, a short-lived process loads the source and lists its steps (which fails while
    /// another process holds the source). A whole session without a step is a new session.
    async fn fork_mode(
        &self,
        cwd: &Path,
        source: String,
        point: Option<&ForkPoint>,
    ) -> Result<SetupMode, AdapterError> {
        let profile = self.profile();
        if point.is_none() && profile.session_fork {
            return Ok(SetupMode::Fork(source));
        }
        if !profile.fork_from_step() {
            return match point {
                Some(_) => Err(AdapterError::Unsupported("forkAtTurn")),
                // The handshake refuses it (`fork`) unless the agent now offers `session/fork`.
                None => Ok(SetupMode::Fork(source)),
            };
        }
        let target = match revert::known_target(point, self.index.get(&source).as_ref())? {
            Some(target) => target,
            None => {
                let steps = self.source_steps(cwd, &source).await?;
                revert::listed_target(point, &steps)?
            }
        };
        Ok(match target {
            ForkTarget::Node(node) => SetupMode::ForkFromStep { source, node },
            ForkTarget::Empty => SetupMode::New,
        })
    }

    /// The steps of `source`, read by a short-lived process that loads it (`revert`).
    async fn source_steps(
        &self,
        cwd: &Path,
        source: &str,
    ) -> Result<Vec<revert::Step>, AdapterError> {
        let options = self.options()?.clone();
        let short = self.connect(cwd, "steps").await?;
        let result = if !AgentProfile::of(short.init()).fork_from_step() {
            Err(AdapterError::Unsupported(
                "fork at a turn (the agent confirms neither cognition.ai/revert nor session/load)",
            ))
        } else {
            session::read_source_steps(
                &short.peer,
                cwd,
                source,
                &options,
                self.ctx.policy.handshake_timeout,
            )
            .await
        };
        short.close().await;
        result
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
        self.learn(&init);
        Ok(ShortLived {
            init: Some(init),
            ..short
        })
    }

    fn info_from(&self, program: PathBuf, init: &InitializeResponse) -> HarnessInfo {
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
            capabilities: capabilities(init, cached.options.has(SettingKind::Model)),
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

/// What a harness can do, from its `initialize` answer. Background tasks (and stopping them)
/// only when the agent confirmed Cognition's extension: standard ACP has no signal for work
/// outside a turn. Forks with ACP's `session/fork`, or with Cognition's `forkFromStep`.
/// `model_switch_live`: a model selector was seen in an earlier session.
fn capabilities(init: &InitializeResponse, model_switch_live: bool) -> HarnessCapabilities {
    let caps = &init.agent_capabilities;
    let profile = AgentProfile::of(init);
    HarnessCapabilities {
        background_tasks: profile.ext.background,
        background_stop: profile.ext.background,
        interrupt: true,
        steer: false,
        approvals: true,
        questions: true,
        resume: caps.load_session || caps.session_capabilities.resume(),
        fork: profile.session_fork || profile.fork_from_step(),
        images: caps.prompt_capabilities.image,
        model_switch_live,
        native_sessions: caps.session_capabilities.list() && caps.load_session,
    }
}

/// The harness features Cognition's extensions give (design.md §9.6; docs/adapters/acp.md
/// §17): forks at a turn with `forkFromStep` (also of a session another process holds, once
/// the turn's node was listed), the session's name, and Devin's own statistics as its status.
/// Plan mode stays Devin's own `/plan` (a command of the agent, whose mode changes are
/// reported as the permission mode); moving a sub-agent to the background is out of scope
/// (design.md §1).
fn features(profile: AgentProfile) -> HarnessFeatures {
    let fork = profile.fork_from_step();
    HarnessFeatures {
        fork_at_turn: fork,
        fork_while_held: fork,
        rename: profile.ext.rename,
        status: profile.ext.cognition,
        ..HarnessFeatures::default()
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
        if let Incoming::Request(req) = msg
            && let Err(e) = peer
                .respond_error(req.id, RpcWireError::method_not_found(&req.method))
                .await
        {
            // The short-lived agent's stdin is closed or broken: it is ending anyway.
            tracing::warn!(method = %req.method, error = %e, "could not refuse a request of a short-lived agent");
        }
    }
}

/// Pages through `session/list` for `cwd`. Sessions whose `cwd` differs are dropped (the
/// request already filters; this guards against agents that ignore the filter). A repeated
/// cursor ends the listing instead of looping forever. A session listed twice (it moved
/// between pages while they were read) is one session ([`NativeSessionSet`]: the first
/// position, the latest `updatedAt`). Titles are the agent's, cut to
/// `policy.harness_title_chars`.
async fn list_sessions(
    peer: &RpcPeer,
    cwd: &Path,
    policy: &AdapterPolicy,
) -> Result<Vec<NativeSessionSummary>, AdapterError> {
    let mut out = NativeSessionSet::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    loop {
        let req = wire::ListSessionsRequest {
            cwd: session::path_string(cwd),
            cursor: cursor.clone(),
        };
        let page: wire::ListSessionsResponse = peer
            .request_timeout("session/list", req, policy.handshake_timeout)
            .await
            .map_err(|e| AdapterError::Harness(format!("session/list: {e}")))?;
        for s in page.sessions {
            if !same_path(&s.cwd, cwd) {
                continue;
            }
            out.insert(NativeSessionSummary {
                updated_at: s.updated_at.as_deref().and_then(rfc3339_millis),
                native_session_id: s.session_id,
                title: s.title.as_deref().and_then(|t| policy.harness_title(t)),
                cwd: Some(s.cwd),
            });
        }
        match page.next_cursor.filter(|c| !c.is_empty()) {
            Some(next) if seen_cursors.insert(next.clone()) => cursor = Some(next),
            Some(next) => {
                tracing::warn!(cursor = %next, "session/list repeated a cursor; stopping");
                return Ok(out.into_sessions());
            }
            None => return Ok(out.into_sessions()),
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
        self.start_with(req, StartOptions::default()).await
    }

    /// `fork_at` is honoured with Cognition's `forkFromStep` (`fork_mode`). The features offer
    /// no mode (`StartOptions::modes`) and no project trust, so the engine sets neither.
    async fn start_with(
        &self,
        req: StartRequest,
        options: StartOptions,
    ) -> Result<SessionHandle, AdapterError> {
        let mode = match req.mode {
            StartMode::New => SetupMode::New,
            StartMode::Resume { native_session_id } => SetupMode::Resume(native_session_id),
            StartMode::Fork { native_session_id } => {
                self.fork_mode(&req.cwd, native_session_id, options.fork_at.as_ref())
                    .await?
            }
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

    /// None by name. ACP has no command that changes the session a connection works on (the
    /// client names the session in every request, and nothing announces a new one), so an
    /// agent's command cannot switch the session under the thread. The commands are the
    /// agent's own (`available_commands_update`); the engine still drops a `resume`, which
    /// would hide the app's own `/resume`.
    fn session_switching_commands(&self) -> &'static [&'static str] {
        &[]
    }

    /// Read after every probe: see [`features`].
    fn features(&self) -> HarnessFeatures {
        features(self.profile())
    }

    /// The anchor is one of Devin's steps ([`revert::check_point`]); the node a fork branches at
    /// is settled at the start, from Devin's own list of the steps when needed.
    fn check_fork_point(&self, point: &ForkPoint) -> Result<(), AdapterError> {
        revert::check_point(point)
    }

    /// Commands come from `available_commands_update` of the agent's most recent session
    /// (ACP publishes them only inside a session), without the ones this adapter does not
    /// offer (`cognition::HIDDEN_COMMANDS`; filtered again here for a list recorded by an
    /// earlier version).
    async fn commands(&self, _ctx: CommandContext) -> Result<Vec<Command>, AdapterError> {
        Ok(
            cognition::visible_commands(&self.cache.get().commands, self.profile().ext)
                .iter()
                .map(mapping::command)
                .collect(),
        )
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
        let result = list_sessions(&short.peer, cwd, &self.ctx.policy).await;
        short.close().await;
        result
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

    /// With Cognition's revert extension, each turn's anchor is its step: the replayed user
    /// chunk's `cognition.ai/clientMessageId`, with the node ids `listSteps` gives for the
    /// loaded session.
    async fn read_native_history_anchored(
        &self,
        cwd: &Path,
        native_session_id: &str,
    ) -> Result<(NativeHistory, Vec<Option<Value>>), AdapterError> {
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
        let mut history = launched.history.unwrap_or_default();
        // The agent's title (`session_info_update`), cut like every harness title.
        history.title = history
            .title
            .and_then(|t| self.ctx.policy.harness_title(&t));
        Ok((history, launched.history_anchors))
    }
}

/// Entry points for replay tests: run the protocol core over arbitrary streams.
#[doc(hidden)]
pub mod testing {
    use super::*;
    use aas_harness::ExitInfo;
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
        /// Cognition's `forkFromStep` of `source` at `node`, then `session/load` of the branch.
        ForkFromStep {
            source: String,
            node: i64,
        },
        History(String),
    }

    pub struct Launched {
        pub handle: SessionHandle,
        pub history: Option<NativeHistory>,
        pub history_anchors: Vec<Option<Value>>,
    }

    /// `(step id, revertTargetNodeId, forkTargetNodeId)` of a listed step (`-1` for a node id
    /// the listing did not give).
    pub type ListedStep = (String, i64, i64);

    /// The steps a session's processes listed (`revert`), shared by the launches given it.
    #[derive(Clone, Default)]
    pub struct Steps(StepIndex);

    impl Steps {
        /// Each step of completed turns of `session_id`, in order, and whether the list is the
        /// whole session.
        pub fn of(&self, session_id: &str) -> Option<(Vec<ListedStep>, bool)> {
            let session = self.0.get(session_id)?;
            let steps = session
                .order
                .iter()
                .filter_map(|id| session.steps.get(id))
                .map(|s| {
                    (
                        s.step_id.clone(),
                        s.revert_target_node_id.unwrap_or(-1),
                        s.fork_target_node_id.unwrap_or(-1),
                    )
                })
                .collect();
            Some((steps, session.complete))
        }

        /// What a fork at `point` (`None`: the whole session) of `source` branches at, from
        /// these steps and the anchor alone: `Some(node)`, `Some(-1)` for a new session, `None`
        /// when the source must be read first.
        pub fn fork_target(
            &self,
            source: &str,
            point: Option<&ForkPoint>,
        ) -> Result<Option<i64>, AdapterError> {
            Ok(
                revert::known_target(point, self.0.get(source).as_ref())?.map(|t| match t {
                    ForkTarget::Node(node) => node,
                    ForkTarget::Empty => -1,
                }),
            )
        }
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
        launch_with_steps(
            (reader, writer, link),
            cwd,
            mode,
            settings,
            options,
            policy,
            Steps::default(),
        )
        .await
    }

    /// [`launch_with_io`] sharing `steps` with other launches (as one adapter's sessions do).
    pub async fn launch_with_steps<R, W>(
        (reader, writer, link): (R, W, FakeLink),
        cwd: PathBuf,
        mode: Mode,
        settings: ThreadSettings,
        options: AdapterOptions,
        policy: AdapterPolicy,
        steps: Steps,
    ) -> Result<Launched, AdapterError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let mode = match mode {
            Mode::New => SetupMode::New,
            Mode::Resume(id) => SetupMode::Resume(id),
            Mode::Fork(id) => SetupMode::Fork(id),
            Mode::ForkFromStep { source, node } => SetupMode::ForkFromStep { source, node },
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
            index: steps.0,
        })
        .await?;
        Ok(Launched {
            handle: launched.handle,
            history: launched.history,
            history_anchors: launched.history_anchors,
        })
    }

    /// The source's steps as a fork reads them when their nodes were never listed: `initialize`,
    /// `session/load` of `source` and `listSteps`, over the given streams. Returns
    /// `(step id, revertTargetNodeId, forkTargetNodeId)` of each step.
    pub async fn source_steps_with_io<R, W>(
        reader: R,
        writer: W,
        cwd: &Path,
        source: &str,
    ) -> Result<Vec<ListedStep>, AdapterError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let policy = AdapterPolicy::default();
        let (peer, incoming) = RpcPeer::start(
            reader,
            writer,
            session::peer_config("acp[test-steps]", &policy),
        );
        tokio::spawn(reject_requests(peer.clone(), incoming));
        let read = async {
            peer.request_timeout::<_, InitializeResponse>(
                "initialize",
                session::client_init(),
                policy.handshake_timeout,
            )
            .await
            .map_err(|e| session::request_error("initialize", e))?;
            session::read_source_steps(
                &peer,
                cwd,
                source,
                &AdapterOptions::default(),
                policy.handshake_timeout,
            )
            .await
        };
        let steps = read.await;
        peer.close_writer().await;
        Ok(steps?
            .into_iter()
            .map(|s| {
                (
                    s.step_id,
                    s.revert_target_node_id.unwrap_or(-1),
                    s.fork_target_node_id.unwrap_or(-1),
                )
            })
            .collect())
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
        super::list_sessions(&peer, cwd, &policy).await
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
    fn background_capabilities_need_the_confirmed_extension() {
        let init = |caps: serde_json::Value| -> InitializeResponse {
            serde_json::from_value(
                serde_json::json!({"protocolVersion": 1, "agentCapabilities": caps}),
            )
            .unwrap()
        };
        let plain = capabilities(&init(serde_json::json!({"loadSession": true})), false);
        assert!(!plain.background_tasks && !plain.background_stop);
        assert!(plain.resume && plain.interrupt && plain.approvals && plain.questions);
        assert!(!plain.steer && !plain.model_switch_live && !plain.fork);
        let devin = capabilities(
            &init(serde_json::json!({"_meta": {"cognition.ai/subagentControl": true}})),
            true,
        );
        assert!(devin.background_tasks && devin.background_stop && devin.model_switch_live);
    }

    #[test]
    fn cognitions_extensions_give_the_features() {
        let profile = |caps: serde_json::Value| -> AgentProfile {
            AgentProfile::of(
                &serde_json::from_value::<InitializeResponse>(
                    serde_json::json!({"protocolVersion": 1, "agentCapabilities": caps}),
                )
                .unwrap(),
            )
        };
        // Devin 3000.11.3 with the revert extension declared (revert.jsonl).
        let devin = profile(serde_json::json!({"loadSession": true, "_meta": {
            "cognition.ai/revert": true, "cognition.ai/revertHistoryRewound": true,
            "cognition.ai/sessionRename": true, "cognition.ai/subagentControl": true}}));
        assert_eq!(
            features(devin),
            HarnessFeatures {
                fork_at_turn: true,
                fork_while_held: true,
                rename: true,
                status: true,
                ..HarnessFeatures::default()
            }
        );
        // Without `session/load` the branch cannot be opened: no fork at a turn.
        let no_load = profile(serde_json::json!({"_meta": {"cognition.ai/revert": true}}));
        assert!(!features(no_load).fork_at_turn && !no_load.fork_from_step());
        // A plain ACP agent: nothing beyond the capabilities.
        assert_eq!(
            features(profile(serde_json::json!({"loadSession": true}))),
            HarnessFeatures::default()
        );
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
