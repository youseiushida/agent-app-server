//! One Codex thread served by one `codex app-server` process.
//!
//! The app-server also runs the thread's sub-agents (threads of their own) and keeps commands
//! running after their turn (background terminals). Their messages are routed by `threadId`:
//! the session's own thread maps to turns and items, the sub-agent threads and the terminals
//! map to background tasks (`background.rs`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use aas_harness::protocol::{
    FileChange, InteractionResolution, ItemBody, ItemStatus, NoticeLevel, ThreadSettings,
};
use aas_harness::{
    AdapterError, AdapterEvent, AdapterPolicy, BackgroundTaskInfo, SessionControl, SessionHandle,
    SettingsApplied, StartMode, StartOptions, StatusSection, ThreadModes, TurnInput,
};
use aas_stdio::{Incoming, IncomingRequest, RpcCallError, RpcPeer, RpcWireError};
use aas_supervisor::{ExitInfo, StopReason};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, OnceCell, mpsc};

use crate::background::{Background, CommandEnd, Route, TurnEnd};
use crate::commands::{self, GoalCommand, Intercept, SkillInfo};
use crate::link::ProcessLink;
use crate::mapping::{
    self, DecisionTable, ElicitationTable, MappedItem, ReasoningStream, UsageTracker,
    UserInputTable,
};
use crate::rpc_err;
use crate::settings::{self, FastTiers};
use crate::status::{self, RateLimits, ThreadFacts};
use crate::texts;
use crate::wire::*;

/// Notifications that are understood and deliberately not forwarded (see the adapter doc).
const IGNORED_NOTIFICATIONS: &[&str] = &[
    "thread/queue/changed",
    "thread/archived",
    "thread/unarchived",
    "thread/reverted",
    "thread/compacted",
    "thread/environment/connected",
    "thread/environment/disconnected",
    "turn/diff/updated",
    "turn/moderationMetadata",
    "item/reasoning/summaryPartAdded",
    "item/fileChange/outputDelta",
    "item/commandExecution/terminalInteraction",
    "hook/started",
    "hook/completed",
    "account/updated",
    "account/login/completed",
    "remoteControl/status/changed",
    "app/list/updated",
    "fs/changed",
    "command/exec/outputDelta",
    "process/outputDelta",
    "process/exited",
    "model/verification",
    "model/safetyBuffering/updated",
    "windowsSandbox/setupCompleted",
    "externalAgentConfig/import/progress",
    "externalAgentConfig/import/completed",
    "mcpServer/oauthLogin/completed",
    "fuzzyFileSearch/sessionUpdated",
    "fuzzyFileSearch/sessionCompleted",
];

/// Title of a sub-agent Codex describes with neither an agent path nor a nickname.
const UNNAMED_SUB_AGENT: &str = "Sub-agent";

/// Arguments of [`establish`].
pub struct EstablishArgs {
    pub mode: StartMode,
    pub cwd: PathBuf,
    pub settings: ThreadSettings,
    pub policy: AdapterPolicy,
    /// Modes and the fork point (`HarnessAdapter::start_with`).
    pub options: StartOptions,
    /// The fast mode of each model, from the last probe's `model/list`.
    pub fast_tiers: FastTiers,
    /// Codex's default model (the last probe's), which a thread without a model runs.
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum TurnPhase {
    #[default]
    Idle,
    /// `turn/start` (or an intercepted command that runs as a Codex turn) was sent; the turn
    /// id is not known yet. `named`: the answer names the turn (`turn/start`, `review/start`),
    /// so its anchor comes from the answer; `thread/compact/start` answers `{}` and the
    /// compaction's turn is known from `turn/started` only.
    Starting {
        named: bool,
    },
    /// A command that is not a Codex turn (`/goal`) waits for its answer. The adapter reports
    /// it as a turn of its own and ends that turn with the answer; a turn Codex starts
    /// meanwhile (a goal's continuation) is reported after it.
    Command,
    Running(String),
}

/// An inline review (`review/start`) between its `enteredReviewMode` and `exitedReviewMode`
/// items. The reviewer's agent messages there are its structured findings (the JSON Codex's
/// review contract asks for; the one of codex-cli 0.148.0 is never completed), which Codex
/// renders itself into the agent message that follows `exitedReviewMode`: they are not shown.
#[derive(Debug, Default)]
struct ReviewTrack {
    active: bool,
    /// Items of the reviewer's messages, whose later deltas are not shown either.
    hidden: HashSet<String>,
}

/// The `turn/start` parameters that bring Codex to the thread's modes.
#[derive(Debug, Default)]
struct ModeParams {
    /// `collaborationMode`, and whether it is plan mode.
    collaboration: Option<(bool, Value)>,
    /// `serviceTier`: `Some(None)` clears the fast mode's tier.
    service_tier: Option<Option<String>>,
}

#[derive(Debug, Default)]
struct ItemTrack {
    reasoning: Option<ReasoningStream>,
    last_index: Option<i64>,
    has_text: bool,
    changes: Vec<FileChange>,
}

enum Pending {
    Decision {
        rpc_id: Value,
        table: DecisionTable,
    },
    Permissions {
        rpc_id: Value,
        requested: RequestedPermissions,
    },
    UserInput {
        rpc_id: Value,
        table: UserInputTable,
    },
    Elicitation {
        rpc_id: Value,
        table: ElicitationTable,
    },
}

impl Pending {
    fn rpc_id(&self) -> &Value {
        match self {
            Pending::Decision { rpc_id, .. }
            | Pending::Permissions { rpc_id, .. }
            | Pending::UserInput { rpc_id, .. }
            | Pending::Elicitation { rpc_id, .. } => rpc_id,
        }
    }
}

struct State {
    thread_id: String,
    settings: ThreadSettings,
    /// Permission preset known to be in effect in Codex.
    applied_mode: Option<String>,
    turn: TurnPhase,
    completed_turns: HashSet<String>,
    /// Turns Codex reported started on this thread while another turn of it ran: the reviewer's
    /// turn of an inline review (codex-cli 0.148.0 starts it under an id of its own and never
    /// completes it). They are not the session's turns.
    foreign_turns: HashSet<String>,
    /// The turn whose anchor was reported last. A turn is anchored once, before its
    /// `TurnCompleted`: from `turn/started`, from the answer that names it, or at the latest
    /// from its own `turn/completed` (when the answer is handled after the turn ended).
    anchored: Option<String>,
    usage: UsageTracker,
    /// Items of the session's thread (reasoning streams, file changes).
    items: HashMap<String, ItemTrack>,
    /// File changes of the sub-agents' fileChange items, by (thread, item): the subject of
    /// their approvals.
    child_changes: HashMap<(String, String), Vec<FileChange>>,
    /// Requests of Codex waiting for an answer, of any thread. A request leaves only when it is
    /// answered (`respond`, `expire_request`), when Codex withdraws it
    /// (`serverRequest/resolved`) or with the process: never with a turn.
    pending: HashMap<String, Pending>,
    seen_notices: HashSet<String>,
    plan_key: Option<String>,
    skills: Vec<SkillInfo>,
    background: Background,
    /// `shutdown` began: the process is going away with everything it runs, so the end of a
    /// turn no longer asks for the background terminals.
    closing: bool,
    /// The notice that Codex's terminal list is unavailable was shown (once per session).
    terminal_list_noticed: bool,
    /// The modes the thread asks for (the start options, then `apply_modes`). They reach Codex
    /// with the next `turn/start` when Codex's differ.
    desired: ThreadModes,
    /// Codex's collaboration mode as last known: `Some(false)` for a new thread; `None` after
    /// a resume or a fork, because Codex does not restore the mode then (its history may end
    /// with an earlier mode's instructions, recorded with codex-cli 0.148.0), so the next turn
    /// states it; then what `thread/settings/updated` reports.
    known_plan: Option<bool>,
    /// The thread's service tier as Codex last reported it (open response,
    /// `thread/settings/updated`).
    known_tier: Option<String>,
    /// The model and the effort Codex reports for the thread.
    codex_model: Option<String>,
    codex_effort: Option<String>,
    fast_tiers: FastTiers,
    /// The thread's goal as Codex last reported it (`thread/goal/*` answers and notifications).
    goal: Option<WireGoal>,
    /// Rate limits from Codex's rolling updates, merged.
    rate_limits: Option<RateLimitSnapshot>,
    /// The thread's last token usage (for the status).
    token_usage: Option<ThreadTokenUsage>,
    review: ReviewTrack,
    /// A `/goal` steered into the running turn waits for its answer, whose notice belongs to
    /// that turn: the pump reports the turn's end after it.
    steered_command: bool,
}

/// What a session starts with.
struct StateInit {
    thread_id: String,
    settings: ThreadSettings,
    applied_mode: Option<String>,
    skills: Vec<SkillInfo>,
    desired: ThreadModes,
    known_plan: Option<bool>,
    known_tier: Option<String>,
    codex_model: Option<String>,
    codex_effort: Option<String>,
    fast_tiers: FastTiers,
}

impl State {
    fn new(init: StateInit) -> Self {
        Self {
            background: Background::new(init.thread_id.clone()),
            thread_id: init.thread_id,
            settings: init.settings,
            applied_mode: init.applied_mode,
            turn: TurnPhase::Idle,
            completed_turns: HashSet::new(),
            foreign_turns: HashSet::new(),
            anchored: None,
            usage: UsageTracker::default(),
            items: HashMap::new(),
            child_changes: HashMap::new(),
            pending: HashMap::new(),
            seen_notices: HashSet::new(),
            plan_key: None,
            skills: init.skills,
            closing: false,
            terminal_list_noticed: false,
            desired: init.desired,
            known_plan: init.known_plan,
            known_tier: init.known_tier,
            codex_model: init.codex_model,
            codex_effort: init.codex_effort,
            fast_tiers: init.fast_tiers,
            goal: None,
            rate_limits: None,
            token_usage: None,
            review: ReviewTrack::default(),
            steered_command: false,
        }
    }

    /// A command waits for its answer (see [`wait_for_command`]).
    fn command_in_flight(&self) -> bool {
        self.turn == TurnPhase::Command || self.steered_command
    }

    /// The model the thread uses: the user's choice, else what Codex reports.
    fn model(&self) -> Option<String> {
        self.settings.model.clone().or(self.codex_model.clone())
    }

    /// The `turn/start` parameters for the desired modes, given what Codex runs with.
    fn mode_params(&self) -> Result<ModeParams, String> {
        let mut params = ModeParams::default();
        let model = self.model();
        if self.known_plan != Some(self.desired.plan) {
            let model = model.as_deref().ok_or_else(|| {
                "Codex did not report the thread's model, which its collaboration mode needs"
                    .to_owned()
            })?;
            let effort = self.settings.effort.clone().or(self.codex_effort.clone());
            params.collaboration = Some((
                self.desired.plan,
                settings::collaboration_mode(self.desired.plan, model, effort.as_deref()),
            ));
        }
        let fast = model.as_deref().and_then(|m| self.fast_tiers.get(m));
        params.service_tier = match (self.desired.fast, fast) {
            (true, Some(tier)) if self.known_tier.as_deref() != Some(tier.id.as_str()) => {
                Some(Some(tier.id.clone()))
            }
            (true, Some(_)) => None,
            (true, None) => return Err(no_fast_mode(model.as_deref())),
            // Only a tier fast mode sets is cleared (a tier from `config.toml` stays).
            (false, _) => match &self.known_tier {
                Some(tier) if self.fast_tiers.values().any(|t| &t.id == tier) => Some(None),
                _ => None,
            },
        };
        Ok(params)
    }
}

fn no_fast_mode(model: Option<&str>) -> String {
    match model {
        Some(model) => format!("Codex lists no fast mode for model {model}"),
        None => "Codex did not report the thread's model, so fast mode cannot be set".into(),
    }
}

pub(crate) struct Shared {
    peer: RpcPeer,
    link: Arc<dyn ProcessLink>,
    policy: AdapterPolicy,
    cwd: PathBuf,
    state: Mutex<State>,
    events: Mutex<Option<mpsc::UnboundedSender<AdapterEvent>>>,
    turn_changed: Notify,
    shutdown: OnceCell<ExitInfo>,
}

impl Shared {
    fn emit(&self, event: AdapterEvent) {
        if let Some(tx) = self.events.lock().as_ref() {
            let _ = tx.send(event);
        }
    }

    fn emit_tasks(&self, tasks: impl IntoIterator<Item = BackgroundTaskInfo>) {
        for task in tasks {
            self.emit(AdapterEvent::BackgroundTask {
                task: Box::new(task),
            });
        }
    }

    fn notice(&self, level: NoticeLevel, message: String, code: &str) {
        self.emit(AdapterEvent::Notice {
            level,
            message,
            code: Some(code.to_owned()),
        });
    }

    /// Emits a notice only the first time this exact (code, message) pair appears in the
    /// session (Codex repeats identical plugin/config warnings on every thread load, sub-agent
    /// threads included).
    fn notice_once(&self, level: NoticeLevel, message: String, code: &str) {
        let first = self
            .state
            .lock()
            .seen_notices
            .insert(format!("{code}\u{0}{message}"));
        if first {
            self.notice(level, message, code);
        }
    }

    pub(crate) fn peer(&self) -> &RpcPeer {
        &self.peer
    }

    pub(crate) fn is_alive(&self) -> bool {
        !self.peer.is_closed()
    }

    fn thread_id(&self) -> String {
        self.state.lock().thread_id.clone()
    }
}

/// Completes the thread handshake over an initialized peer and starts the event pump.
pub(crate) async fn establish(
    peer: RpcPeer,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    link: Arc<dyn ProcessLink>,
    args: EstablishArgs,
) -> Result<(SessionHandle, Arc<Shared>), AdapterError> {
    let desired = args.options.modes;
    // Fast mode at the start: the tier of the model the thread asks for, else of Codex's
    // default model (the one a thread without a model runs).
    let start_tier = if desired.fast {
        let model = args
            .settings
            .model
            .clone()
            .or_else(|| args.default_model.clone());
        let tier = model.as_deref().and_then(|m| args.fast_tiers.get(m));
        Some(
            tier.ok_or_else(|| AdapterError::Other(no_fast_mode(model.as_deref())))?
                .id
                .clone(),
        )
    } else {
        None
    };
    let mut params = settings::open_overrides(&args.settings, start_tier.as_deref())
        .map_err(AdapterError::Other)?;
    params.insert("cwd".into(), json!(args.cwd.to_string_lossy()));
    if args.options.fork_at.is_some() && !matches!(args.mode, StartMode::Fork { .. }) {
        return Err(AdapterError::Other(
            "a fork point was given for a start that is not a fork".into(),
        ));
    }
    let method = match &args.mode {
        StartMode::New => "thread/start",
        StartMode::Resume { native_session_id } => {
            params.insert("threadId".into(), json!(native_session_id));
            "thread/resume"
        }
        StartMode::Fork { native_session_id } => {
            params.insert("threadId".into(), json!(native_session_id));
            if let Some(point) = &args.options.fork_at {
                // The turn's own id; `beforeTurnId` is experimental (sent with the experimental
                // API on, like every request of this adapter).
                let turn = anchor_turn_id(&point.anchor).map_err(AdapterError::Other)?;
                let key = if point.before {
                    "beforeTurnId"
                } else {
                    "lastTurnId"
                };
                params.insert(key.into(), json!(turn));
            }
            "thread/fork"
        }
    };
    let opened: ThreadOpenResponse = peer
        .request_timeout(method, Value::Object(params), args.policy.handshake_timeout)
        .await
        .map_err(|e| rpc_err(method, e))?;

    let skills = fetch_skills(&peer, &args.cwd, &args.policy).await;
    let applied_mode = args.settings.permission_mode.clone().or_else(|| {
        settings::preset_from_response(
            opened.approval_policy.as_ref(),
            opened.approvals_reviewer.as_deref(),
            opened.sandbox.as_ref(),
        )
        .map(str::to_owned)
    });

    let (tx, rx) = mpsc::unbounded_channel();
    let fast_state = opened
        .service_tier
        .as_deref()
        .map(|tier| settings::tier_word(tier, &args.fast_tiers));
    let shared = Arc::new(Shared {
        peer,
        link,
        policy: args.policy,
        cwd: args.cwd,
        state: Mutex::new(State::new(StateInit {
            thread_id: opened.thread.id.clone(),
            settings: args.settings,
            applied_mode: applied_mode.clone(),
            skills,
            desired,
            known_plan: match args.mode {
                StartMode::New => Some(false),
                StartMode::Resume { .. } | StartMode::Fork { .. } => None,
            },
            known_tier: opened.service_tier.clone(),
            codex_model: opened.model.clone(),
            codex_effort: opened.reasoning_effort.clone(),
            fast_tiers: args.fast_tiers,
        })),
        events: Mutex::new(Some(tx)),
        turn_changed: Notify::new(),
        shutdown: OnceCell::new(),
    });
    shared.emit(AdapterEvent::SessionInfo {
        model: opened.model.clone(),
        permission_mode: applied_mode,
        effort: opened.reasoning_effort.clone(),
    });
    if fast_state.is_some() {
        // The plan mode is not reported here: Codex does not say it (the next turn states it).
        shared.emit(AdapterEvent::ModesReported {
            plan: None,
            fast_state,
        });
    }
    // A name the thread got elsewhere (Codex desktop, another client) is only seen here: Codex
    // notifies renames within one process only.
    if let Some(name) = opened
        .thread
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        shared.emit(AdapterEvent::SessionTitle {
            title: name.to_owned(),
        });
    }
    tokio::spawn(pump(shared.clone(), incoming));
    let control = Arc::new(CodexSession {
        shared: shared.clone(),
    });
    Ok((
        SessionHandle {
            native_session_id: Some(opened.thread.id),
            control,
            events: rx,
        },
        shared,
    ))
}

/// The anchor of a turn this adapter reports (`TurnAnchor`): the turn's own id, which Codex
/// keeps in forks (recorded with codex-cli 0.148.0).
pub(crate) fn turn_anchor(turn_id: &str) -> Value {
    json!({ "turnId": turn_id })
}

/// The turn id of an anchor this adapter reported.
pub(crate) fn anchor_turn_id(anchor: &Value) -> Result<&str, String> {
    anchor
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| format!("{anchor} is not the anchor of a Codex turn"))
}

pub(crate) async fn fetch_skills(
    peer: &RpcPeer,
    cwd: &std::path::Path,
    policy: &AdapterPolicy,
) -> Vec<SkillInfo> {
    let resp = peer
        .request_timeout::<_, SkillsListResponse>(
            "skills/list",
            json!({"cwds": [cwd.to_string_lossy()]}),
            policy.handshake_timeout,
        )
        .await;
    match resp {
        Ok(list) => list
            .data
            .iter()
            .flat_map(|entry| entry.skills.iter())
            .filter(|s| s.enabled)
            .map(SkillInfo::from_metadata)
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "codex skills/list failed; no skill commands");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Event pump
// ---------------------------------------------------------------------------------------------

async fn pump(shared: Arc<Shared>, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
    while let Some(message) = incoming.recv().await {
        match message {
            Incoming::Notification { method, params } => {
                on_notification(&shared, &method, params).await
            }
            Incoming::Request(request) => on_request(&shared, request).await,
            Incoming::Malformed(line) => {
                tracing::warn!("codex app-server wrote a non-JSON-RPC line");
                shared.emit(AdapterEvent::Native {
                    payload: json!({ "malformedLine": line }),
                });
            }
        }
    }
    let info = shared.link.wait().await;
    {
        let mut st = shared.state.lock();
        st.pending.clear();
        st.turn = TurnPhase::Idle;
    }
    shared.turn_changed.notify_waiters();
    // `Exited` ends every background task that has not ended (the port contract).
    shared.emit(AdapterEvent::Exited { info });
    shared.events.lock().take();
}

fn parse<T: DeserializeOwned>(shared: &Shared, method: &str, params: &Value) -> Option<T> {
    match serde_json::from_value::<T>(params.clone()) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(method, error = %e, "unexpected Codex notification shape; forwarded as native");
            shared.emit(AdapterEvent::Native {
                payload: json!({ "method": method, "params": params, "parseError": e.to_string() }),
            });
            None
        }
    }
}

async fn on_notification(shared: &Arc<Shared>, method: &str, params: Value) {
    if IGNORED_NOTIFICATIONS.contains(&method) {
        return;
    }
    if on_session_notification(shared, method, &params) {
        return;
    }
    let route = match params.get("threadId").and_then(Value::as_str) {
        // A notification that names no thread is the session's.
        None => Route::Main,
        Some(thread) => thread_route(shared, thread, method, &params).await,
    };
    match route {
        Route::Main => on_main_notification(shared, method, params).await,
        Route::Child(id) => on_child_notification(shared, &id, method, params).await,
        Route::Foreign | Route::Unknown => {
            tracing::debug!(
                method,
                "notification of a thread outside this session; ignored"
            );
        }
    }
}

/// Notifications about the session as a whole, whichever thread reports them (warnings about
/// the configuration repeat for every thread, sub-agents included). Returns whether `method`
/// was one of them.
fn on_session_notification(shared: &Arc<Shared>, method: &str, params: &Value) -> bool {
    match method {
        "warning" => {
            if let Some(n) = parse::<WarningNotification>(shared, method, params) {
                shared.notice_once(NoticeLevel::Warning, n.message, "warning");
            }
        }
        "configWarning" | "deprecationNotice" => {
            let Some(n) = parse::<SummaryNotification>(shared, method, params) else {
                return true;
            };
            let message = match n.details.filter(|d| !d.is_empty()) {
                Some(d) => format!("{}\n{d}", n.summary),
                None => n.summary,
            };
            let (level, code) = if method == "configWarning" {
                (NoticeLevel::Warning, "configWarning")
            } else {
                (NoticeLevel::Info, "deprecation")
            };
            shared.notice_once(level, message, code);
        }
        "guardianWarning" => {
            if let Some(n) = parse::<WarningNotification>(shared, method, params) {
                shared.notice(NoticeLevel::Warning, n.message, "guardianWarning");
            }
        }
        "windows/worldWritableWarning" => {
            let Some(n) = parse::<WorldWritableWarning>(shared, method, params) else {
                return true;
            };
            let mut message = "Codex found world-writable folders in the workspace".to_owned();
            if !n.sample_paths.is_empty() {
                message.push_str(&format!(": {}", n.sample_paths.join(", ")));
            }
            if n.extra_count > 0 {
                message.push_str(&format!(" (+{} more)", n.extra_count));
            }
            if n.failed_scan {
                message.push_str(" (scan incomplete)");
            }
            shared.notice_once(NoticeLevel::Warning, message, "worldWritable");
        }
        "mcpServer/startupStatus/updated" => {
            let Some(n) = parse::<McpServerStatusUpdated>(shared, method, params) else {
                return true;
            };
            if n.status == "failed" {
                let message = match n.error {
                    Some(e) => format!("MCP server {} failed to start: {e}", n.name),
                    None => format!("MCP server {} failed to start", n.name),
                };
                shared.notice_once(NoticeLevel::Warning, message, "mcpServerFailed");
            }
        }
        "serverRequest/resolved" => {
            // Request ids are unique on the connection, whatever thread asked.
            let Some(n) = parse::<ServerRequestResolved>(shared, method, params) else {
                return true;
            };
            let id = request_key(&n.request_id);
            if shared.state.lock().pending.remove(&id).is_some() {
                shared.emit(AdapterEvent::InteractionWithdrawn { request_id: id });
            }
        }
        "account/rateLimits/updated" => {
            // Sparse: values an update leaves out keep what an earlier one said.
            if let Some(n) = parse::<RateLimitsUpdated>(shared, method, params) {
                shared
                    .state
                    .lock()
                    .rate_limits
                    .get_or_insert_with(RateLimitSnapshot::default)
                    .merge(n.rate_limits);
            }
        }
        "skills/changed" => {
            let shared = shared.clone();
            tokio::spawn(async move {
                let skills = fetch_skills(&shared.peer, &shared.cwd, &shared.policy).await;
                shared.state.lock().skills = skills.clone();
                shared.emit(AdapterEvent::CommandsChanged {
                    commands: commands::commands(&skills),
                });
            });
        }
        "thread/started" => {
            // Codex 0.148 does not announce spawned sub-agents this way (they are identified
            // by their spawning item); a thread it does announce with a parent in this
            // session's tree is one of its sub-agents.
            let Some(n) = parse::<ThreadStartedNotification>(shared, method, params) else {
                return true;
            };
            let task = {
                let mut st = shared.state.lock();
                match n.thread.parent_thread_id.as_deref() {
                    Some(parent)
                        if st.background.in_tree(parent)
                            && st.background.route(&n.thread.id) == Route::Unknown =>
                    {
                        st.background.child_spawned(
                            &n.thread.id,
                            parent,
                            child_title(&n.thread),
                            None,
                        )
                    }
                    _ => None,
                }
            };
            shared.emit_tasks(task);
        }
        _ => return false,
    }
    true
}

/// The route of a message of `thread`. A thread not seen before is looked up (`thread/read`)
/// when it reports activity; an idle or closing thread nobody knows needs nothing.
async fn thread_route(shared: &Arc<Shared>, thread: &str, method: &str, params: &Value) -> Route {
    let route = shared.state.lock().background.route(thread);
    if route != Route::Unknown {
        return route;
    }
    let quiet = match method {
        "thread/status/changed" => !matches!(
            parse::<ThreadStatusChanged>(shared, method, params).map(|n| n.status),
            Some(WireThreadStatus::Active)
        ),
        "thread/closed" | "thread/deleted" | "thread/name/updated" => true,
        _ => false,
    };
    if quiet {
        return Route::Unknown;
    }
    resolve_thread(shared, thread).await
}

/// Title of a sub-agent thread from Codex's description of it: its agent path (v2), else its
/// nickname.
fn child_title(thread: &WireThread) -> String {
    thread
        .agent_path()
        .or(thread.agent_nickname.as_deref().filter(|n| !n.is_empty()))
        .unwrap_or(UNNAMED_SUB_AGENT)
        .to_owned()
}

/// Looks up a thread this session has not seen: a thread whose `parentThreadId` is in the
/// session's tree is a sub-agent (its spawning item may still be on its way), any other thread
/// is not part of the session.
async fn resolve_thread(shared: &Arc<Shared>, thread: &str) -> Route {
    let read = shared
        .peer
        .request_timeout::<_, ThreadReadResponse>(
            "thread/read",
            json!({ "threadId": thread, "includeTurns": false }),
            shared.policy.handshake_timeout,
        )
        .await;
    let (route, task) = {
        let mut st = shared.state.lock();
        // A spawning item may have named it meanwhile.
        match st.background.route(thread) {
            Route::Unknown => {}
            known => return known,
        }
        match read {
            Ok(resp) => match resp.thread.parent_thread_id.as_deref() {
                Some(parent) if st.background.in_tree(parent) => {
                    let task = st.background.child_spawned(
                        thread,
                        parent,
                        child_title(&resp.thread),
                        None,
                    );
                    (Route::Child(thread.to_owned()), task)
                }
                _ => {
                    st.background.mark_foreign(thread);
                    (Route::Foreign, None)
                }
            },
            Err(RpcCallError::Closed) => (Route::Unknown, None),
            Err(e) => {
                // The app-server serves only this session's thread tree, and this thread is
                // working in it: shown and answered as a sub-agent's work rather than dropped.
                tracing::warn!(thread, error = %e, "thread/read of an unknown working thread failed");
                let main = st.thread_id.clone();
                let task = st.background.child_spawned(
                    thread,
                    &main,
                    format!("{UNNAMED_SUB_AGENT} {thread}"),
                    None,
                );
                drop(st);
                shared.notice(
                    NoticeLevel::Warning,
                    format!("Codex did not describe its thread {thread} ({e}); its work is shown as a sub-agent's"),
                    "unknownThread",
                );
                (Route::Child(thread.to_owned()), task)
            }
        }
    };
    shared.emit_tasks(task);
    route
}

async fn on_main_notification(shared: &Arc<Shared>, method: &str, params: Value) {
    match method {
        // The thread's own status carries nothing the turns do not (Codex has no status for
        // background work), and the session ends with its process, not with the thread.
        "thread/status/changed" | "thread/closed" | "thread/deleted" => {}
        "thread/name/updated" => {
            let name = params
                .get("threadName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty());
            if let Some(name) = name {
                shared.emit(AdapterEvent::SessionTitle {
                    title: name.to_owned(),
                });
            }
        }
        "turn/started" => {
            let Some(n) = parse::<TurnNotification>(shared, method, &params) else {
                return;
            };
            // A turn Codex starts while a command of ours waits for its answer (a goal's
            // continuation right after `/goal`) follows that command's turn.
            wait_for_command(shared).await;
            {
                let mut st = shared.state.lock();
                let id = n.turn.id.clone();
                match st.turn.clone() {
                    _ if st.completed_turns.contains(&id) => {
                        tracing::debug!(turn = %id, "turn/started of a turn that already completed");
                    }
                    _ if st.foreign_turns.contains(&id) => {}
                    TurnPhase::Running(current) if current != id => {
                        // codex-cli 0.148.0's inline review starts its reviewer's turn on this
                        // thread under another id, and never completes it; the review turn
                        // (`review/start`'s) stays the running one.
                        tracing::debug!(running = %current, other = %id, "turn/started of another turn while a turn runs; not the session's");
                        st.foreign_turns.insert(id);
                    }
                    phase => {
                        if matches!(phase, TurnPhase::Idle | TurnPhase::Command) {
                            // A turn the agent started by itself: Codex's goal continuation
                            // starts turns without `turn/start` whenever the thread becomes
                            // idle with an active goal.
                            st.usage.start_turn();
                        }
                        st.turn = TurnPhase::Running(id.clone());
                        // Emitted under the state lock: `send` refuses with
                        // `TurnInProgress` only once this turn's `TurnStarted` is out. The
                        // anchor follows (the turn a fork at it keeps), unless the answer of
                        // the request that started it names the turn (`started`): an inline
                        // review's reviewer turn may come before that answer.
                        shared.emit(AdapterEvent::TurnStarted);
                        let named = matches!(phase, TurnPhase::Starting { named: true });
                        if !named && st.anchored.as_deref() != Some(id.as_str()) {
                            shared.emit(AdapterEvent::TurnAnchor {
                                anchor: turn_anchor(&id),
                            });
                            st.anchored = Some(id);
                        }
                    }
                }
            }
            shared.turn_changed.notify_waiters();
        }
        "turn/completed" => {
            let Some(n) = parse::<TurnNotification>(shared, method, &params) else {
                return;
            };
            // The answer of a `/goal` steered into this turn is a notice of it.
            wait_for_command(shared).await;
            let (thread, plan_key, usage) = {
                let mut st = shared.state.lock();
                if st.foreign_turns.contains(&n.turn.id) {
                    // The inline review's reviewer turn never completes on 0.148.0; should a
                    // later version complete it, it does not end the review turn.
                    tracing::debug!(turn = %n.turn.id, "turn/completed of a turn that is not the session's; ignored");
                    return;
                }
                st.completed_turns.insert(n.turn.id.clone());
                st.turn = TurnPhase::Idle;
                st.items.clear();
                st.review = ReviewTrack::default();
                if st.anchored.as_deref() != Some(n.turn.id.as_str()) {
                    // The answer that names the turn is handled after its end: the turn's own
                    // id anchors it, before its `TurnCompleted`.
                    shared.emit(AdapterEvent::TurnAnchor {
                        anchor: turn_anchor(&n.turn.id),
                    });
                    st.anchored = Some(n.turn.id.clone());
                }
                (
                    st.thread_id.clone(),
                    st.plan_key.take(),
                    st.usage.turn_usage(),
                )
            };
            shared.turn_changed.notify_waiters();
            // Commands still running become background terminals: each task first, then its
            // item closed as backgrounded, both before the turn ends.
            let end = end_turn_terminals(shared, &thread).await;
            shared.emit_tasks(end.tasks);
            for key in end.backgrounded {
                shared.emit(AdapterEvent::ItemCompleted {
                    key,
                    body: None,
                    status: ItemStatus::Backgrounded,
                });
            }
            if let Some(key) = plan_key {
                shared.emit(AdapterEvent::ItemCompleted {
                    key,
                    body: None,
                    status: ItemStatus::Completed,
                });
            }
            let status = mapping::turn_status(&n.turn.status);
            let error = match status {
                aas_harness::TurnStatus::Failed => Some(match &n.turn.error {
                    Some(e) => mapping::turn_error(e),
                    None => aas_harness::TurnError {
                        message: "Codex reported the turn as failed".into(),
                        kind: "harnessError".into(),
                    },
                }),
                _ => None,
            };
            // Codex never starts a turn of this thread because background work ended: the
            // turns it starts by itself (goal continuation) carry no trigger.
            shared.emit(AdapterEvent::TurnCompleted {
                trigger: None,
                status,
                usage,
                error,
            });
        }
        "item/started" | "item/completed" => on_main_item(shared, method, params).await,
        "item/agentMessage/delta" | "item/plan/delta" => {
            let Some(d) = parse::<DeltaNotification>(shared, method, &params) else {
                return;
            };
            if shared.state.lock().review.hidden.contains(&d.item_id) {
                return;
            }
            shared.emit(AdapterEvent::ItemDelta {
                key: d.item_id,
                field: aas_harness::DeltaField::Text,
                text: d.delta,
            });
        }
        "item/reasoning/summaryTextDelta" => {
            if let Some(d) = parse::<DeltaNotification>(shared, method, &params) {
                on_reasoning_delta(shared, d, ReasoningStream::Summary);
            }
        }
        "item/reasoning/textDelta" => {
            if let Some(d) = parse::<DeltaNotification>(shared, method, &params) {
                on_reasoning_delta(shared, d, ReasoningStream::Content);
            }
        }
        "item/commandExecution/outputDelta" => {
            let Some(d) = parse::<DeltaNotification>(shared, method, &params) else {
                return;
            };
            let background = {
                let st = shared.state.lock();
                st.background.is_terminal_item(&st.thread_id, &d.item_id)
            };
            // A background terminal's item is closed; its output arrives whole with its end.
            if !background {
                shared.emit(AdapterEvent::ItemDelta {
                    key: d.item_id,
                    field: aas_harness::DeltaField::Output,
                    text: d.delta,
                });
            }
        }
        "item/fileChange/patchUpdated" => {
            let Some(n) = parse::<PatchUpdatedNotification>(shared, method, &params) else {
                return;
            };
            let changes = mapping::file_changes(&n.changes, &shared.cwd);
            shared
                .state
                .lock()
                .items
                .entry(n.item_id.clone())
                .or_default()
                .changes = changes.clone();
            shared.emit(AdapterEvent::ItemUpdated {
                key: n.item_id,
                body: ItemBody::FileChange { changes },
            });
        }
        "item/mcpToolCall/progress" => {
            let Some(n) = parse::<McpProgressNotification>(shared, method, &params) else {
                return;
            };
            shared.emit(AdapterEvent::ItemDelta {
                key: n.item_id,
                field: aas_harness::DeltaField::Output,
                text: format!("{}\n", n.message),
            });
        }
        "turn/plan/updated" => {
            let Some(n) = parse::<TurnPlanUpdated>(shared, method, &params) else {
                return;
            };
            let body = ItemBody::Plan {
                entries: mapping::plan_entries(&n.plan),
            };
            let key = format!("plan:{}", n.turn_id);
            let first = {
                let mut st = shared.state.lock();
                let first = st.plan_key.as_deref() != Some(key.as_str());
                st.plan_key = Some(key.clone());
                first
            };
            if first {
                shared.emit(AdapterEvent::ItemStarted { key, body });
            } else {
                shared.emit(AdapterEvent::ItemUpdated { key, body });
            }
        }
        "thread/tokenUsage/updated" => {
            let Some(n) = parse::<TokenUsageNotification>(shared, method, &params) else {
                return;
            };
            let usage = {
                let mut st = shared.state.lock();
                let current =
                    matches!(&st.turn, TurnPhase::Running(id) if Some(id) == n.turn_id.as_ref());
                st.token_usage = Some(n.token_usage.clone());
                st.usage.observe(&n.token_usage, current)
            };
            if let Some(usage) = usage {
                shared.emit(AdapterEvent::TurnUsage { usage });
            }
        }
        "thread/settings/updated" => {
            if let Some(n) = parse::<ThreadSettingsUpdated>(shared, method, &params) {
                on_settings_updated(shared, n.thread_settings);
            }
        }
        "thread/goal/updated" => {
            let Some(n) = parse::<GoalUpdated>(shared, method, &params) else {
                return;
            };
            let changed = {
                let mut st = shared.state.lock();
                let before = st.goal.replace(n.goal.clone());
                // A change within a turn (the model's `update_goal`, Codex stopping it at a
                // limit) is shown; the accounting update at every turn's end keeps the status,
                // and changes without a turn are answers to requests (`/goal`) or the
                // snapshot a resume sends.
                n.turn_id.is_some()
                    && before.as_ref().map(|g| g.status.as_str()) != Some(n.goal.status.as_str())
            };
            if changed {
                let (level, message) = goal_notice(&n.goal);
                shared.notice(level, message, "goalUpdated");
            }
        }
        "thread/goal/cleared" => {
            shared.state.lock().goal = None;
        }
        "error" => {
            let Some(n) = parse::<ErrorNotification>(shared, method, &params) else {
                return;
            };
            // Final errors arrive again in `turn/completed`; only retries are worth a notice.
            if n.will_retry {
                let err = mapping::turn_error(&n.error);
                shared.notice(
                    NoticeLevel::Warning,
                    format!("Retrying after error: {}", err.message),
                    "retrying",
                );
            }
        }
        "model/rerouted" => {
            let Some(n) = parse::<ModelRerouted>(shared, method, &params) else {
                return;
            };
            let reason = n.reason.map(|r| {
                r.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| r.to_string())
            });
            let message = match reason {
                Some(r) => format!(
                    "Model rerouted from {} to {} ({r})",
                    n.from_model, n.to_model
                ),
                None => format!("Model rerouted from {} to {}", n.from_model, n.to_model),
            };
            shared.notice(NoticeLevel::Info, message, "modelRerouted");
            let (mode, effort) = {
                let st = shared.state.lock();
                (st.applied_mode.clone(), st.settings.effort.clone())
            };
            shared.emit(AdapterEvent::SessionInfo {
                model: Some(n.to_model),
                permission_mode: mode,
                effort,
            });
        }
        _ => shared.emit(AdapterEvent::Native {
            payload: json!({ "method": method, "params": params }),
        }),
    }
}

/// `thread/settings/updated`: what Codex runs the thread with from now on. The collaboration
/// mode is the explicit signal of plan mode (`ModesReported`), the service tier is the fast
/// mode's state; model, effort and permission preset are reported as `SessionInfo`.
fn on_settings_updated(shared: &Shared, ts: WireThreadSettings) {
    let (plan, fast_state, permission_mode) = {
        let mut st = shared.state.lock();
        if let Some(model) = &ts.model {
            st.codex_model = Some(model.clone());
        }
        st.codex_effort = ts.effort.clone();
        st.known_tier = ts.service_tier.clone();
        let plan = ts.collaboration_mode.as_ref().map(|c| c.mode == "plan");
        if plan.is_some() {
            st.known_plan = plan;
        }
        let preset = settings::preset_from_response(
            ts.approval_policy.as_ref(),
            ts.approvals_reviewer.as_deref(),
            ts.sandbox_policy.as_ref(),
        );
        if let Some(preset) = preset {
            st.applied_mode = Some(preset.to_owned());
        }
        let fast_state = ts
            .service_tier
            .as_deref()
            .map(|tier| settings::tier_word(tier, &st.fast_tiers));
        (plan, fast_state, preset.map(str::to_owned))
    };
    shared.emit(AdapterEvent::SessionInfo {
        model: ts.model,
        permission_mode,
        effort: ts.effort,
    });
    if plan.is_some() || fast_state.is_some() {
        shared.emit(AdapterEvent::ModesReported { plan, fast_state });
    }
}

/// The notice of a goal's new status, in the words of `status::goal_status_words`.
fn goal_notice(goal: &WireGoal) -> (NoticeLevel, String) {
    let level = match goal.status.as_str() {
        "blocked" | "usageLimited" | "budgetLimited" => NoticeLevel::Warning,
        _ => NoticeLevel::Info,
    };
    (
        level,
        format!(
            "Goal {}: {}",
            status::goal_status_words(&goal.status),
            goal.objective
        ),
    )
}

/// Waits (up to `handshake_timeout`, the bound of the command itself) while a command that is
/// not a Codex turn waits for its answer: a `/goal` sent as a turn of its own (a continuation
/// Codex starts meanwhile follows that turn) or steered into the running turn (whose end
/// follows the answer's notice).
async fn wait_for_command(shared: &Shared) {
    let deadline = tokio::time::Instant::now() + shared.policy.handshake_timeout;
    loop {
        let notified = shared.turn_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !shared.state.lock().command_in_flight() {
            return;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            tracing::warn!("a command's answer did not arrive in time; Codex's turn is reported");
            return;
        }
    }
}

async fn on_main_item(shared: &Arc<Shared>, method: &str, params: Value) {
    let Some(n) = parse::<ItemNotification>(shared, method, &params) else {
        return;
    };
    let item: WireItem = match serde_json::from_value(n.item.clone()) {
        Ok(item) => item,
        Err(e) => {
            tracing::warn!(method, error = %e, "unexpected Codex item shape");
            WireItem::Unknown
        }
    };
    let started = method == "item/started";
    let thread = shared.thread_id();
    // An inline review: its markers, and the reviewer's own messages in between.
    {
        let mut st = shared.state.lock();
        match &item {
            WireItem::EnteredReviewMode { .. } => st.review.active = true,
            WireItem::ExitedReviewMode { .. } => st.review.active = false,
            WireItem::AgentMessage { id, .. } if st.review.hidden.contains(id) => {
                if !started {
                    st.review.hidden.remove(id);
                }
                return;
            }
            WireItem::AgentMessage { id, .. } if st.review.active => {
                if started {
                    st.review.hidden.insert(id.clone());
                }
                return;
            }
            _ => {}
        }
    }
    if let WireItem::ExitedReviewMode { id, .. } = &item {
        // The review's text follows as an agent message of its own; the marker closes the
        // review started by `enteredReviewMode`.
        let body = mapping::review_finished_notice();
        shared.emit(if started {
            AdapterEvent::ItemStarted {
                key: id.clone(),
                body,
            }
        } else {
            AdapterEvent::ItemCompleted {
                key: id.clone(),
                body: Some(body),
                status: ItemStatus::Completed,
            }
        });
        return;
    }
    // Commands are tracked from their start: one still open when the turn ends may run on.
    if let (
        true,
        WireItem::CommandExecution {
            id,
            command,
            status,
            ..
        },
    ) = (started, &item)
        && mapping::item_status(status.as_deref()) == ItemStatus::InProgress
    {
        shared
            .state
            .lock()
            .background
            .command_started(&thread, id, command);
    }
    if let (
        false,
        WireItem::CommandExecution {
            id,
            status,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        },
    ) = (started, &item)
    {
        let end = shared.state.lock().background.command_completed(
            &thread,
            id,
            status.as_deref(),
            *exit_code,
            aggregated_output.clone(),
            *duration_ms,
        );
        match end {
            CommandEnd::Item => {}
            CommandEnd::Terminal(task) => {
                shared.emit_tasks([*task]);
                relist_terminals(shared, &thread).await;
                return;
            }
            CommandEnd::Closed => {
                tracing::debug!(item = %id, "completion of a command its turn already closed");
                return;
            }
        }
    }
    let key = mapping::item_id(&n.item).unwrap_or_default().to_owned();
    let reasoning = shared
        .state
        .lock()
        .items
        .get(&key)
        .and_then(|t| t.reasoning);
    match mapping::map_item(&item, &shared.cwd, reasoning) {
        MappedItem::Skip => {}
        MappedItem::Unknown => {
            if started {
                shared.emit(AdapterEvent::Native {
                    payload: json!({ "method": method, "params": params }),
                });
            }
        }
        MappedItem::Item {
            key,
            body,
            mut status,
        } => {
            if let ItemBody::FileChange { changes } = &body {
                shared
                    .state
                    .lock()
                    .items
                    .entry(key.clone())
                    .or_default()
                    .changes = changes.clone();
            }
            if started {
                shared.emit(AdapterEvent::ItemStarted {
                    key: key.clone(),
                    body,
                });
                // A v2 spawn names its sub-agent as soon as it starts; its task names this item.
                let spawned = spawned_children(shared, &thread, &item, Some(&key));
                shared.emit_tasks(spawned);
            } else {
                shared.state.lock().items.remove(&key);
                let spawned = spawned_children(shared, &thread, &item, Some(&key));
                if spawned_something(&item, &shared.state.lock().background) {
                    status = ItemStatus::Backgrounded;
                }
                shared.emit_tasks(spawned);
                shared.emit(AdapterEvent::ItemCompleted {
                    key,
                    body: Some(body),
                    status,
                });
            }
        }
    }
}

/// Sub-agents `item` (of `thread`) spawned, registered in the background work; returns the
/// tasks that changed. `origin` is the item's key when it is an item of the session (a
/// sub-agent's spawn items are not).
fn spawned_children(
    shared: &Shared,
    thread: &str,
    item: &WireItem,
    origin: Option<&str>,
) -> Vec<BackgroundTaskInfo> {
    let mut st = shared.state.lock();
    let mut changed = Vec::new();
    match item {
        WireItem::SubAgentActivity {
            kind,
            agent_thread_id: Some(child),
            agent_path,
            ..
        } if kind == "started" => {
            let title = agent_path
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| format!("{UNNAMED_SUB_AGENT} {child}"));
            changed.extend(st.background.child_spawned(
                child,
                thread,
                title,
                origin.map(str::to_owned),
            ));
        }
        WireItem::CollabAgentToolCall {
            tool,
            status,
            prompt,
            receiver_thread_ids,
            ..
        } if tool == "spawnAgent"
            && mapping::item_status(status.as_deref()) == ItemStatus::Completed =>
        {
            let title = prompt
                .as_deref()
                .and_then(|p| shared.policy.prompt_title(p))
                .unwrap_or_else(|| UNNAMED_SUB_AGENT.to_owned());
            for child in receiver_thread_ids {
                changed.extend(st.background.child_spawned(
                    child,
                    thread,
                    title.clone(),
                    origin.map(str::to_owned),
                ));
            }
        }
        _ => {}
    }
    changed
}

/// Whether completed `item` spawned a sub-agent this session tracks: its work goes on as that
/// sub-agent's task.
fn spawned_something(item: &WireItem, background: &Background) -> bool {
    match item {
        WireItem::SubAgentActivity {
            kind,
            agent_thread_id: Some(child),
            ..
        } => kind == "started" && matches!(background.route(child), Route::Child(_)),
        WireItem::CollabAgentToolCall {
            tool,
            status,
            receiver_thread_ids,
            ..
        } => {
            tool == "spawnAgent"
                && mapping::item_status(status.as_deref()) == ItemStatus::Completed
                && receiver_thread_ids
                    .iter()
                    .any(|c| matches!(background.route(c), Route::Child(_)))
        }
        _ => false,
    }
}

/// The name of the tool a sub-agent's item runs (its progress), for tool items.
fn tool_name(item: &WireItem) -> Option<String> {
    Some(match item {
        WireItem::CommandExecution { .. } => "commandExecution".into(),
        WireItem::FileChange { .. } => "fileChange".into(),
        WireItem::McpToolCall { server, tool, .. } => format!("{server}: {tool}"),
        WireItem::DynamicToolCall {
            namespace, tool, ..
        } => match namespace {
            Some(ns) if !ns.is_empty() => format!("{ns}: {tool}"),
            _ => tool.clone(),
        },
        WireItem::CollabAgentToolCall { tool, .. } => tool.clone(),
        WireItem::WebSearch { .. } => "webSearch".into(),
        WireItem::ImageView { .. } => "imageView".into(),
        WireItem::ImageGeneration { .. } => "imageGeneration".into(),
        WireItem::Sleep { .. } => "sleep".into(),
        _ => return None,
    })
}

/// The final answer of a finished turn (its last agentMessage item), else its error.
fn turn_summary(turn: &WireTurn) -> Option<String> {
    turn.items
        .iter()
        .rev()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))
        .and_then(|item| item.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .or_else(|| turn.error.as_ref().map(|e| mapping::turn_error(e).message))
}

async fn on_child_notification(shared: &Arc<Shared>, child: &str, method: &str, params: Value) {
    match method {
        "turn/started" => {
            let Some(n) = parse::<TurnNotification>(shared, method, &params) else {
                return;
            };
            let task = shared
                .state
                .lock()
                .background
                .child_turn_started(child, &n.turn.id);
            shared.emit_tasks(task);
        }
        "turn/completed" => {
            let Some(n) = parse::<TurnNotification>(shared, method, &params) else {
                return;
            };
            // Commands of the sub-agent still running (an interrupt does not stop them) become
            // background terminals of its own.
            let end = end_turn_terminals(shared, child).await;
            shared.emit_tasks(end.tasks);
            let task = {
                let mut st = shared.state.lock();
                st.child_changes.retain(|(t, _), _| t != child);
                st.background
                    .child_turn_completed(child, &n.turn.status, turn_summary(&n.turn))
            };
            shared.emit_tasks(task);
        }
        "thread/status/changed" => {
            let Some(n) = parse::<ThreadStatusChanged>(shared, method, &params) else {
                return;
            };
            let active = n.status == WireThreadStatus::Active;
            let task = shared.state.lock().background.child_status(child, active);
            shared.emit_tasks(task);
        }
        "thread/tokenUsage/updated" => {
            let Some(n) = parse::<TokenUsageNotification>(shared, method, &params) else {
                return;
            };
            let task = shared
                .state
                .lock()
                .background
                .child_usage(child, n.token_usage.total.total_tokens);
            shared.emit_tasks(task);
        }
        "item/started" | "item/completed" => {
            let Some(n) = parse::<ItemNotification>(shared, method, &params) else {
                return;
            };
            let Ok(item) = serde_json::from_value::<WireItem>(n.item.clone()) else {
                tracing::debug!(method, "unexpected Codex item shape of a sub-agent");
                return;
            };
            if method == "item/started" {
                on_child_item_started(shared, child, &item);
            } else {
                on_child_item_completed(shared, child, &item).await;
            }
        }
        "item/fileChange/patchUpdated" => {
            let Some(n) = parse::<PatchUpdatedNotification>(shared, method, &params) else {
                return;
            };
            let changes = mapping::file_changes(&n.changes, &shared.cwd);
            shared
                .state
                .lock()
                .child_changes
                .insert((child.to_owned(), n.item_id), changes);
        }
        "thread/closed" | "thread/deleted" => {
            let tasks = shared.state.lock().background.child_gone(child);
            shared.emit_tasks(tasks);
        }
        // What a sub-agent streams (messages, reasoning, output, plans) is its own; the session
        // shows its runs as a task (progress, usage, final answer).
        _ => {}
    }
}

fn on_child_item_started(shared: &Arc<Shared>, child: &str, item: &WireItem) {
    let mut tasks = Vec::new();
    {
        let mut st = shared.state.lock();
        match item {
            WireItem::CommandExecution {
                id,
                command,
                status,
                ..
            } if mapping::item_status(status.as_deref()) == ItemStatus::InProgress => {
                st.background.command_started(child, id, command);
            }
            WireItem::FileChange { id, changes, .. } => {
                let changes = mapping::file_changes(changes, &shared.cwd);
                st.child_changes
                    .insert((child.to_owned(), id.clone()), changes);
            }
            _ => {}
        }
        if let Some(tool) = tool_name(item) {
            tasks.extend(st.background.child_tool_started(child, tool));
        }
    }
    // A v2 sub-agent of the sub-agent.
    tasks.extend(spawned_children(shared, child, item, None));
    shared.emit_tasks(tasks);
}

async fn on_child_item_completed(shared: &Arc<Shared>, child: &str, item: &WireItem) {
    match item {
        WireItem::CommandExecution {
            id,
            status,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } => {
            let end = shared.state.lock().background.command_completed(
                child,
                id,
                status.as_deref(),
                *exit_code,
                aggregated_output.clone(),
                *duration_ms,
            );
            if let CommandEnd::Terminal(task) = end {
                shared.emit_tasks([*task]);
                relist_terminals(shared, child).await;
            }
        }
        WireItem::FileChange { id, .. } => {
            shared
                .state
                .lock()
                .child_changes
                .remove(&(child.to_owned(), id.clone()));
        }
        _ => {
            // A v1 sub-agent of the sub-agent.
            let tasks = spawned_children(shared, child, item, None);
            shared.emit_tasks(tasks);
        }
    }
}

/// `thread/backgroundTerminals/list` of `thread`, every page, within one
/// `policy.handshake_timeout`.
async fn list_terminals(
    shared: &Shared,
    thread: &str,
) -> Result<Vec<WireBackgroundTerminal>, RpcCallError> {
    let timeout = shared.policy.handshake_timeout;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut listed = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut params = json!({ "threadId": thread });
        if let Some(c) = &cursor {
            params["cursor"] = json!(c);
        }
        let page = tokio::time::timeout_at(
            deadline,
            shared.peer.request::<_, BackgroundTerminalsListResponse>(
                "thread/backgroundTerminals/list",
                params,
            ),
        )
        .await
        .map_err(|_| RpcCallError::Timeout(timeout))??;
        let empty = page.data.is_empty();
        listed.extend(page.data);
        match page.next_cursor {
            Some(next) if !empty && Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(listed)
}

/// The end of a turn of `thread` for its commands: when a command is still open (or a
/// terminal of the thread is live), Codex's list of live terminals says which of them go on.
/// Without the list (a Codex without the experimental API, an error) the open commands are
/// left to close with the turn.
async fn end_turn_terminals(shared: &Arc<Shared>, thread: &str) -> TurnEnd {
    let (closing, needed) = {
        let st = shared.state.lock();
        (st.closing, st.background.needs_list(thread))
    };
    if !needed {
        return TurnEnd::default();
    }
    if closing {
        shared.state.lock().background.turn_ended_unlisted(thread);
        return TurnEnd::default();
    }
    match list_terminals(shared, thread).await {
        Ok(listed) => shared.state.lock().background.turn_ended(thread, &listed),
        Err(e) => {
            terminal_list_failed(shared, &e);
            shared.state.lock().background.turn_ended_unlisted(thread);
            TurnEnd::default()
        }
    }
}

/// Lists the terminals of `thread` again after one of them ended (an event, never a timer), so
/// that the live set stays Codex's.
async fn relist_terminals(shared: &Arc<Shared>, thread: &str) {
    if shared.state.lock().closing {
        return;
    }
    match list_terminals(shared, thread).await {
        Ok(listed) => {
            let tasks = {
                let mut st = shared.state.lock();
                let turn_running = if *thread == st.thread_id {
                    st.turn != TurnPhase::Idle
                } else {
                    st.background
                        .child_turn(thread)
                        .is_some_and(|(_, turn)| turn.is_some())
                };
                st.background.relisted(thread, &listed, turn_running)
            };
            shared.emit_tasks(tasks);
        }
        Err(e) => terminal_list_failed(shared, &e),
    }
}

/// Codex did not list its terminals. Once per session the user is told what that means; a
/// closed connection needs no notice (the process is ending, and everything with it).
fn terminal_list_failed(shared: &Shared, error: &RpcCallError) {
    if matches!(error, RpcCallError::Closed) {
        tracing::debug!("background terminals not listed: the app-server connection is closed");
        return;
    }
    tracing::warn!(error = %error, "codex thread/backgroundTerminals/list failed");
    let first = !std::mem::replace(&mut shared.state.lock().terminal_list_noticed, true);
    if first {
        shared.notice(
            NoticeLevel::Warning,
            format!(
                "Codex did not list its background terminals ({error}). Commands still running when a turn ends are shown as ended with the turn; they keep running unseen, cannot be stopped one by one and end with the thread"
            ),
            "backgroundTerminalsUnavailable",
        );
    }
}

fn on_reasoning_delta(shared: &Shared, d: DeltaNotification, kind: ReasoningStream) {
    let text = {
        let mut st = shared.state.lock();
        let track = st.items.entry(d.item_id.clone()).or_default();
        match track.reasoning {
            None => track.reasoning = Some(kind),
            Some(k) if k != kind => return,
            Some(_) => {}
        }
        let index = match kind {
            ReasoningStream::Summary => d.summary_index,
            ReasoningStream::Content => d.content_index,
        };
        let mut text = String::new();
        if let (Some(prev), Some(cur)) = (track.last_index, index)
            && prev != cur
            && track.has_text
        {
            text.push_str("\n\n");
        }
        if index.is_some() {
            track.last_index = index;
        }
        if !d.delta.is_empty() {
            track.has_text = true;
        }
        text.push_str(&d.delta);
        text
    };
    shared.emit(AdapterEvent::ItemDelta {
        key: d.item_id,
        field: aas_harness::DeltaField::Text,
        text,
    });
}

/// Protocol request id as the interaction's `request_id` string.
pub(crate) fn request_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Logs an answer to a Codex request that could not be written: app-server's stdin is closed
/// or broken, so the process is ending (its exit is reported on its own). Such a failure is
/// never dropped silently.
fn log_unanswered(method: &str, written: Result<(), aas_stdio::RpcCallError>) {
    if let Err(e) = written {
        tracing::warn!(method, error = %e, "could not answer a Codex request");
    }
}

async fn on_request(shared: &Arc<Shared>, req: IncomingRequest) {
    let request_id = request_key(&req.id);
    macro_rules! params {
        ($t:ty) => {
            match serde_json::from_value::<$t>(req.params.clone()) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(method = %req.method, error = %e, "unexpected Codex request shape");
                    let written = shared.peer.respond_error(req.id.clone(), RpcWireError::new(-32602, format!("agent-app-server could not parse {}: {e}", req.method))).await;
                    log_unanswered(&req.method, written);
                    shared.notice(NoticeLevel::Error, format!("Could not handle Codex request {}: {e}", req.method), "unsupportedRequest");
                    return;
                }
            }
        };
    }
    // Who asks: the session's thread (its turn, or the thread), or a sub-agent (its task).
    let route = match req.params.get("threadId").and_then(Value::as_str) {
        None => Route::Main,
        Some(thread) => {
            let known = shared.state.lock().background.route(thread);
            match known {
                Route::Unknown => resolve_thread(shared, thread).await,
                other => other,
            }
        }
    };
    let (child, background_key) = match &route {
        Route::Main => (None, None),
        Route::Child(id) => (Some(id.clone()), Some(id.clone())),
        Route::Foreign | Route::Unknown => {
            let thread = req
                .params
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let written = shared
                .peer
                .respond_error(
                    req.id.clone(),
                    RpcWireError::new(
                        -32600,
                        format!("agent-app-server does not serve thread {thread}"),
                    ),
                )
                .await;
            log_unanswered(&req.method, written);
            tracing::warn!(method = %req.method, thread, "request of a thread outside this session refused");
            return;
        }
    };
    // Items of a sub-agent are not items of the session.
    let item_key = |id: Option<String>| if child.is_some() { None } else { id };
    let (request, item_key, pending) = match req.method.as_str() {
        "item/commandExecution/requestApproval" => {
            let p = params!(CommandApprovalParams);
            let (request, table) = mapping::command_approval(&p);
            (
                request,
                item_key(p.item_id),
                Pending::Decision {
                    rpc_id: req.id.clone(),
                    table,
                },
            )
        }
        "item/fileChange/requestApproval" => {
            let p = params!(FileChangeApprovalParams);
            let changes = p
                .item_id
                .as_ref()
                .and_then(|id| {
                    let st = shared.state.lock();
                    match &child {
                        None => st.items.get(id).map(|t| t.changes.clone()),
                        Some(thread) => {
                            st.child_changes.get(&(thread.clone(), id.clone())).cloned()
                        }
                    }
                })
                .unwrap_or_default();
            let (request, table) = mapping::file_change_approval(&p, changes);
            (
                request,
                item_key(p.item_id),
                Pending::Decision {
                    rpc_id: req.id.clone(),
                    table,
                },
            )
        }
        "item/permissions/requestApproval" => {
            let p = params!(PermissionsApprovalParams);
            let request = mapping::permissions_approval(&p);
            (
                request,
                item_key(p.item_id),
                Pending::Permissions {
                    rpc_id: req.id.clone(),
                    requested: p.permissions,
                },
            )
        }
        "item/tool/requestUserInput" => {
            let p = params!(UserInputParams);
            let (request, table) = mapping::user_input_question(&p);
            (
                request,
                item_key(p.item_id),
                Pending::UserInput {
                    rpc_id: req.id.clone(),
                    table,
                },
            )
        }
        "mcpServer/elicitation/request" => {
            let p = params!(ElicitationParams);
            let (request, table) = mapping::elicitation_question(&p);
            (
                request,
                None,
                Pending::Elicitation {
                    rpc_id: req.id.clone(),
                    table,
                },
            )
        }
        other => {
            let written = shared
                .peer
                .respond_error(req.id.clone(), RpcWireError::method_not_found(other))
                .await;
            log_unanswered(other, written);
            let (level, message) = match other {
                "account/chatgptAuthTokens/refresh" => (
                    NoticeLevel::Error,
                    "Codex asked for refreshed ChatGPT credentials; sign in again with `codex login` on the PC".to_owned(),
                ),
                _ => (NoticeLevel::Warning, format!("Codex sent a request agent-app-server does not support: {other}")),
            };
            shared.notice(level, message, "unsupportedRequest");
            return;
        }
    };
    shared
        .state
        .lock()
        .pending
        .insert(request_id.clone(), pending);
    shared.emit(AdapterEvent::InteractionRequested {
        background_key,
        request_id,
        request,
        item_key,
    });
}

// ---------------------------------------------------------------------------------------------
// Control
// ---------------------------------------------------------------------------------------------

pub(crate) struct CodexSession {
    shared: Arc<Shared>,
}

/// How a background task is stopped.
enum StopTarget {
    /// `thread/backgroundTerminals/terminate`.
    Terminal { thread: String, process_id: String },
    /// `turn/interrupt` of the sub-agent's running turn.
    SubAgent { thread: String, turn: String },
}

impl CodexSession {
    /// Id of the running turn, waiting (bounded by the handshake timeout) while a start is in
    /// flight. `None` when no turn runs.
    async fn current_turn_id(&self) -> Option<String> {
        let deadline = tokio::time::Instant::now() + self.shared.policy.handshake_timeout;
        loop {
            let notified = self.shared.turn_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.shared.state.lock().turn.clone() {
                TurnPhase::Running(id) => return Some(id),
                // A command that is not a Codex turn has nothing to interrupt or steer.
                TurnPhase::Idle | TurnPhase::Command => return None,
                TurnPhase::Starting { .. } => {}
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }

    /// Begins what the engine sends: a Codex turn (`Starting`) or a command that is not one
    /// (`Command`).
    fn begin_turn(&self, phase: TurnPhase) -> Result<TurnContext, AdapterError> {
        let mut st = self.shared.state.lock();
        match st.turn {
            TurnPhase::Idle => {}
            // The engine sends only while no turn of its own runs: a running turn is one the
            // agent started by itself (goal continuation), whose `TurnStarted` is out (it is
            // emitted under this lock).
            TurnPhase::Running(_) => return Err(AdapterError::TurnInProgress),
            TurnPhase::Starting { .. } | TurnPhase::Command => {
                return Err(AdapterError::Other("a turn is being started".into()));
            }
        }
        st.turn = phase;
        st.usage.start_turn();
        Ok(TurnContext {
            thread_id: st.thread_id.clone(),
            settings: st.settings.clone(),
            applied_mode: st.applied_mode.clone(),
            skills: st.skills.clone(),
        })
    }

    fn abort_start(&self) {
        let mut st = self.shared.state.lock();
        if matches!(st.turn, TurnPhase::Starting { .. } | TurnPhase::Command) {
            st.turn = TurnPhase::Idle;
        }
        drop(st);
        self.shared.turn_changed.notify_waiters();
    }

    /// Codex took the turn `turn_id` (`turn/start` or `review/start` answered). Its anchor is
    /// reported now: an inline review gets no `turn/started` of its own.
    fn started(&self, turn_id: String, applied: Applied) {
        let mut st = self.shared.state.lock();
        if applied.mode.is_some() {
            st.applied_mode = applied.mode;
        }
        if let Some(plan) = applied.plan {
            st.known_plan = Some(plan);
        }
        if let Some(tier) = applied.tier {
            st.known_tier = tier;
        }
        if !st.completed_turns.contains(&turn_id) {
            match st.turn.clone() {
                TurnPhase::Starting { .. } => {
                    st.turn = TurnPhase::Running(turn_id.clone());
                    if applied.announce {
                        // Under the state lock, like the pump's `TurnStarted`.
                        self.shared.emit(AdapterEvent::TurnStarted);
                    }
                }
                // The answer names the turn: one the pump took for it before the answer
                // arrived (an inline review's reviewer turn) is not the session's.
                TurnPhase::Running(other) if other != turn_id => {
                    st.foreign_turns.insert(other);
                    st.turn = TurnPhase::Running(turn_id.clone());
                }
                _ => {}
            }
            if st.anchored.as_deref() != Some(turn_id.as_str()) {
                self.shared.emit(AdapterEvent::TurnAnchor {
                    anchor: turn_anchor(&turn_id),
                });
                st.anchored = Some(turn_id);
            }
        }
        drop(st);
        self.shared.turn_changed.notify_waiters();
    }

    /// Ends a command that is not a Codex turn with its answer: reported as a turn of its own
    /// (started, the answer as a notice, completed), under the state lock so that a turn Codex
    /// starts meanwhile is reported after it.
    fn finish_command(&self, level: NoticeLevel, message: String, code: &str) {
        let mut st = self.shared.state.lock();
        if st.turn == TurnPhase::Command {
            st.turn = TurnPhase::Idle;
        }
        self.shared.emit(AdapterEvent::TurnStarted);
        self.shared.notice(level, message, code);
        self.shared.emit(AdapterEvent::TurnCompleted {
            status: aas_harness::TurnStatus::Completed,
            usage: None,
            error: None,
            trigger: None,
        });
        drop(st);
        self.shared.turn_changed.notify_waiters();
    }

    /// Runs `/goal` (the `thread/goal/*` requests) and returns its answer as a notice.
    async fn goal_command(
        &self,
        thread_id: &str,
        command: &GoalCommand,
    ) -> Result<(NoticeLevel, String), AdapterError> {
        let peer = &self.shared.peer;
        let timeout = self.shared.policy.handshake_timeout;
        let set = |params: Value| async move {
            const METHOD: &str = "thread/goal/set";
            peer.request_timeout::<_, GoalSetResponse>(METHOD, params, timeout)
                .await
                .map_err(|e| rpc_err(METHOD, e))
                .map(|r| r.goal)
        };
        let goal = match command {
            GoalCommand::Show => {
                const METHOD: &str = "thread/goal/get";
                let resp: GoalGetResponse = peer
                    .request_timeout(METHOD, json!({ "threadId": thread_id }), timeout)
                    .await
                    .map_err(|e| rpc_err(METHOD, e))?;
                self.shared.state.lock().goal = resp.goal.clone();
                return Ok(match resp.goal {
                    Some(goal) => (NoticeLevel::Info, goal_summary(&goal)),
                    // The TUI's words.
                    None => (NoticeLevel::Info, "No goal is currently set.".into()),
                });
            }
            GoalCommand::Clear => {
                const METHOD: &str = "thread/goal/clear";
                let resp: GoalClearResponse = peer
                    .request_timeout(METHOD, json!({ "threadId": thread_id }), timeout)
                    .await
                    .map_err(|e| rpc_err(METHOD, e))?;
                self.shared.state.lock().goal = None;
                return Ok(if resp.cleared {
                    (NoticeLevel::Info, "Goal cleared".into())
                } else {
                    // The TUI's words.
                    (
                        NoticeLevel::Info,
                        "This thread does not currently have a goal.".into(),
                    )
                });
            }
            GoalCommand::Set { objective } => {
                set(json!({ "threadId": thread_id, "objective": objective, "status": "active" }))
                    .await?
            }
            GoalCommand::Edit { objective } => {
                set(json!({ "threadId": thread_id, "objective": objective })).await?
            }
            GoalCommand::Pause => set(json!({ "threadId": thread_id, "status": "paused" })).await?,
            GoalCommand::Resume => {
                set(json!({ "threadId": thread_id, "status": "active" })).await?
            }
        };
        self.shared.state.lock().goal = Some(goal.clone());
        Ok(goal_notice(&goal))
    }

    /// Pauses the thread's goal when Codex last reported it active (see `interrupt`), by
    /// `deadline`. Codex's answer is the goal's new state, told as a notice of the running turn
    /// (`goalUpdated`; like a steered `/goal`, the pump reports the turn's end after it); a
    /// pause that fails is told too, since the goal then goes on.
    async fn pause_goal_for_interrupt(&self, thread_id: &str, deadline: tokio::time::Instant) {
        let flagged = {
            let mut st = self.shared.state.lock();
            if !st.goal.as_ref().is_some_and(|g| g.status == "active") {
                return;
            }
            // A `/goal` steered meanwhile holds the turn's end already, and releases it.
            let flagged = !st.command_in_flight();
            if flagged {
                st.steered_command = true;
            }
            flagged
        };
        const METHOD: &str = "thread/goal/set";
        let params = json!({ "threadId": thread_id, "status": "paused" });
        let answer = tokio::time::timeout_at(
            deadline,
            self.shared
                .peer
                .request::<_, GoalSetResponse>(METHOD, params),
        )
        .await;
        {
            // Under the state lock: the pump reports the turn's end after the notice.
            let mut st = self.shared.state.lock();
            let error = match answer {
                Ok(Ok(resp)) => {
                    let (level, message) = goal_notice(&resp.goal);
                    st.goal = Some(resp.goal);
                    self.shared.notice(level, message, "goalUpdated");
                    None
                }
                Ok(Err(e)) => Some(rpc_err(METHOD, e)),
                Err(_) => Some(rpc_err(
                    METHOD,
                    RpcCallError::Timeout(self.shared.policy.stop_grace),
                )),
            };
            if let Some(error) = error {
                tracing::warn!(thread = %thread_id, error = %error, "could not pause the goal of the interrupted turn");
                self.shared.notice(
                    NoticeLevel::Warning,
                    format!(
                        "The goal could not be paused with the turn, so Codex continues it after the next turn (send /goal pause to pause it): {}",
                        error.detail()
                    ),
                    "goalNotPaused",
                );
            }
            if flagged {
                st.steered_command = false;
            }
        }
        self.shared.turn_changed.notify_waiters();
    }

    fn stop_target(&self, key: &str) -> Result<StopTarget, AdapterError> {
        let st = self.shared.state.lock();
        let task = st
            .background
            .task(key)
            .ok_or_else(|| AdapterError::Other(format!("unknown background task {key}")))?;
        if task.state.is_ended() {
            return Err(AdapterError::Other(format!(
                "background task {key} has already ended"
            )));
        }
        if let Some((thread, process_id)) = st.background.terminal(key) {
            return Ok(StopTarget::Terminal { thread, process_id });
        }
        match st.background.child_turn(key) {
            Some((thread, Some(turn))) => Ok(StopTarget::SubAgent { thread, turn }),
            Some((_, None)) => Err(AdapterError::Other(format!(
                "sub-agent {key} has no running turn to stop"
            ))),
            None => Err(AdapterError::Other(format!(
                "unknown background task {key}"
            ))),
        }
    }
}

/// What the engine sends that runs as a Codex turn.
enum CodexTurn {
    /// `thread/compact/start`.
    Compact,
    /// `review/start` (inline).
    Review { instructions: Option<String> },
    /// `turn/start` with this input.
    Message(TurnInput),
}

/// What `begin_turn` hands the request that starts a turn.
struct TurnContext {
    thread_id: String,
    settings: ThreadSettings,
    applied_mode: Option<String>,
    skills: Vec<SkillInfo>,
}

/// What Codex runs with once it took a turn.
#[derive(Debug, Default)]
struct Applied {
    /// The permission preset sent with the turn.
    mode: Option<String>,
    /// The collaboration mode sent with the turn.
    plan: Option<bool>,
    /// The service tier sent with the turn.
    tier: Option<Option<String>>,
    /// Codex sends no `turn/started` for this turn (an inline review): the answer is its
    /// start.
    announce: bool,
}

/// `/goal`'s answer about a goal.
fn goal_summary(goal: &WireGoal) -> String {
    let mut text = format!(
        "Goal {}: {} ({} tokens, {} used",
        status::goal_status_words(&goal.status),
        goal.objective,
        status::thousands(goal.tokens_used),
        status::duration_words(goal.time_used_seconds)
    );
    if let Some(budget) = goal.token_budget {
        text.push_str(&format!(", budget {}", status::thousands(budget)));
    }
    text.push(')');
    text
}

#[async_trait]
impl SessionControl for CodexSession {
    // Every request below is bounded: the engine waits for these calls, and an app-server
    // that stops answering must not keep the thread waiting (`handshake_timeout` for requests,
    // `stop_grace` for the interrupt, like `shutdown`).
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        let turn = match commands::parse_intercept(&input) {
            Some(Err(usage)) => return Err(AdapterError::Other(usage)),
            Some(Ok(Intercept::Goal(command))) => {
                let ctx = self.begin_turn(TurnPhase::Command)?;
                return match self.goal_command(&ctx.thread_id, &command).await {
                    Ok((level, message)) => {
                        self.finish_command(level, message, "goal");
                        Ok(())
                    }
                    Err(e) => {
                        self.abort_start();
                        Err(e)
                    }
                };
            }
            Some(Ok(Intercept::Compact)) => CodexTurn::Compact,
            Some(Ok(Intercept::Review { instructions })) => CodexTurn::Review { instructions },
            // `/init` is a turn with Codex's own prompt as its input.
            Some(Ok(Intercept::Init)) => CodexTurn::Message(TurnInput::text(texts::INIT_PROMPT)),
            None => CodexTurn::Message(input),
        };
        let TurnContext {
            thread_id,
            settings,
            applied_mode,
            skills,
        } = self.begin_turn(TurnPhase::Starting {
            named: !matches!(turn, CodexTurn::Compact),
        })?;
        let peer = &self.shared.peer;
        let timeout = self.shared.policy.handshake_timeout;
        match turn {
            CodexTurn::Compact => {
                // The compaction runs as a turn; its id arrives with `turn/started`.
                if let Err(e) = peer
                    .request_timeout::<_, Value>(
                        "thread/compact/start",
                        json!({ "threadId": thread_id }),
                        timeout,
                    )
                    .await
                {
                    self.abort_start();
                    return Err(rpc_err("thread/compact/start", e));
                }
                Ok(())
            }
            CodexTurn::Review { instructions } => {
                let params = json!({
                    "threadId": thread_id,
                    "target": commands::review_target(instructions.as_deref()),
                    "delivery": "inline",
                });
                match peer
                    .request_timeout::<_, ReviewStartResponse>("review/start", params, timeout)
                    .await
                {
                    Ok(resp) => {
                        self.started(
                            resp.turn.id,
                            Applied {
                                announce: true,
                                ..Applied::default()
                            },
                        );
                        Ok(())
                    }
                    Err(e) => {
                        self.abort_start();
                        Err(rpc_err("review/start", e))
                    }
                }
            }
            CodexTurn::Message(input) => {
                let (overrides, modes) = {
                    let st = self.shared.state.lock();
                    let overrides = settings::turn_overrides(&settings, applied_mode.as_deref());
                    (overrides, st.mode_params())
                };
                let (mut overrides, modes) = match overrides.and_then(|o| Ok((o, modes?))) {
                    Ok(both) => both,
                    Err(e) => {
                        self.abort_start();
                        return Err(AdapterError::Other(e));
                    }
                };
                let mut params = Map::new();
                params.insert("threadId".into(), json!(thread_id));
                params.insert(
                    "input".into(),
                    Value::Array(commands::user_inputs(&input, &skills)),
                );
                if let Some((_, mode)) = &modes.collaboration {
                    // Codex ignores a top-level effort sent with a mode: it is in the mode.
                    overrides.remove("effort");
                    params.insert("collaborationMode".into(), mode.clone());
                }
                if let Some(tier) = &modes.service_tier {
                    params.insert("serviceTier".into(), json!(tier));
                }
                params.extend(overrides);
                match peer
                    .request_timeout::<_, TurnStartResponse>(
                        "turn/start",
                        Value::Object(params),
                        timeout,
                    )
                    .await
                {
                    Ok(resp) => {
                        self.started(
                            resp.turn.id,
                            Applied {
                                mode: settings.permission_mode.clone(),
                                plan: modes.collaboration.map(|(plan, _)| plan),
                                tier: modes.service_tier,
                                announce: false,
                            },
                        );
                        Ok(())
                    }
                    Err(e) => {
                        self.abort_start();
                        Err(rpc_err("turn/start", e))
                    }
                }
            }
        }
    }

    /// `turn/steer`. `/goal` also works while a turn runs (a paused or cleared goal lets the
    /// running continuation finish and starts no other): its answer is a notice of the running
    /// turn. The other commands run as turns of their own and are refused while one runs (as
    /// Codex's TUI disables them during a task); typed text is never sent to the model in their
    /// place.
    async fn steer(&self, input: TurnInput) -> Result<(), AdapterError> {
        match commands::parse_intercept(&input) {
            Some(Err(usage)) => return Err(AdapterError::Other(usage)),
            Some(Ok(Intercept::Goal(command))) => {
                let thread_id = {
                    let mut st = self.shared.state.lock();
                    if st.command_in_flight() {
                        return Err(AdapterError::Other("a command is being run".into()));
                    }
                    st.steered_command = true;
                    st.thread_id.clone()
                };
                let answer = self.goal_command(&thread_id, &command).await;
                {
                    // Under the state lock: the pump reports the turn's end after the notice.
                    let mut st = self.shared.state.lock();
                    if let Ok((level, message)) = &answer {
                        self.shared.notice(*level, message.clone(), "goal");
                    }
                    st.steered_command = false;
                }
                self.shared.turn_changed.notify_waiters();
                return answer.map(|_| ());
            }
            Some(Ok(other)) => {
                return Err(AdapterError::Other(format!(
                    "{} runs as a turn of its own: send it when no turn runs",
                    other.command()
                )));
            }
            None => {}
        }
        let timeout = self.shared.policy.handshake_timeout;
        let steer = async {
            let Some(turn_id) = self.current_turn_id().await else {
                return Err(AdapterError::Other("no running turn to steer".into()));
            };
            let (thread_id, skills) = {
                let st = self.shared.state.lock();
                (st.thread_id.clone(), st.skills.clone())
            };
            let params = json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": commands::user_inputs(&input, &skills),
            });
            self.shared
                .peer
                .request_value("turn/steer", params)
                .await
                .map(|_| ())
                .map_err(|e| rpc_err("turn/steer", e))
        };
        tokio::time::timeout(timeout, steer)
            .await
            .unwrap_or_else(|_| Err(rpc_err("turn/steer", RpcCallError::Timeout(timeout))))
    }

    /// Asks the app-server to abort the running turn. Codex answers `turn/interrupt` only
    /// once the turn has been aborted; an app-server that does not within `stop_grace` gets an
    /// error back, so the engine's forced stop (`interrupt_grace`) is never held up here.
    /// Background terminals and sub-agents go on (Codex does not stop them with the turn).
    ///
    /// While the thread's goal is active, stopping the turn also pauses the goal, as Codex's
    /// own TUI does (`pause_active_goal_for_interrupt`, whenever a turn runs while the goal is
    /// active): otherwise Codex would continue the goal by itself after the next turn (docs/
    /// adapters/codex.md §14.6). The pause is written before the interrupt and both run within
    /// the same `stop_grace`; the outcome of the interrupt is what this returns.
    async fn interrupt(&self) -> Result<(), AdapterError> {
        let grace = self.shared.policy.stop_grace;
        let deadline = tokio::time::Instant::now() + grace;
        let late = || Err(rpc_err("turn/interrupt", RpcCallError::Timeout(grace)));
        let turn_id = match tokio::time::timeout_at(deadline, self.current_turn_id()).await {
            Ok(Some(turn_id)) => turn_id,
            Ok(None) => return Ok(()),
            Err(_) => return late(),
        };
        let thread_id = self.shared.thread_id();
        let pause = self.pause_goal_for_interrupt(&thread_id, deadline);
        let params = json!({ "threadId": thread_id, "turnId": turn_id });
        let interrupt = async {
            match tokio::time::timeout_at(
                deadline,
                self.shared.peer.request_value("turn/interrupt", params),
            )
            .await
            {
                Ok(answer) => answer.map(|_| ()).map_err(|e| rpc_err("turn/interrupt", e)),
                Err(_) => late(),
            }
        };
        let ((), interrupted) = tokio::join!(pause, interrupt);
        interrupted
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
            .pending
            .remove(request_id)
            .ok_or_else(|| AdapterError::UnknownRequest(request_id.to_owned()))?;
        let result = match &pending {
            Pending::Decision { table, .. } => mapping::decision_response(table, resolution),
            Pending::Permissions { requested, .. } => {
                mapping::permissions_response(requested, resolution)
            }
            Pending::UserInput { table, .. } => mapping::user_input_response(table, resolution),
            Pending::Elicitation { table, .. } => mapping::elicitation_response(table, resolution),
        };
        match result {
            Ok(value) => {
                // A write, bounded for an app-server that stopped reading its stdin.
                let rpc_id = pending.rpc_id().clone();
                let timeout = self.shared.policy.handshake_timeout;
                match tokio::time::timeout(timeout, self.shared.peer.respond(rpc_id, value)).await {
                    Ok(written) => written.map_err(|e| rpc_err("respond", e)),
                    Err(_) => Err(rpc_err("respond", RpcCallError::Timeout(timeout))),
                }
            }
            Err(message) => {
                // Invalid answer: keep the request open so the user can answer again.
                self.shared
                    .state
                    .lock()
                    .pending
                    .insert(request_id.to_owned(), pending);
                Err(AdapterError::Other(message))
            }
        }
    }

    async fn apply_settings(
        &self,
        settings: &ThreadSettings,
    ) -> Result<SettingsApplied, AdapterError> {
        if let Some(mode) = &settings.permission_mode
            && settings::preset(mode).is_none()
        {
            return Err(AdapterError::Other(format!(
                "unknown permission mode {mode}"
            )));
        }
        // Codex takes model / effort / permissions as `turn/start` overrides, so the next turn
        // in this same process uses them.
        self.shared.state.lock().settings = settings.clone();
        Ok(SettingsApplied::Live)
    }

    /// Plan mode (`collaborationMode`) and fast mode (`serviceTier`) are `turn/start`
    /// parameters: the next turn of this process takes them.
    async fn apply_modes(&self, modes: &ThreadModes) -> Result<SettingsApplied, AdapterError> {
        let mut st = self.shared.state.lock();
        if modes.fast {
            let model = st.model();
            if model
                .as_deref()
                .and_then(|m| st.fast_tiers.get(m))
                .is_none()
            {
                return Err(AdapterError::Other(no_fast_mode(model.as_deref())));
            }
        }
        st.desired = *modes;
        Ok(SettingsApplied::Live)
    }

    /// `thread/name/set`. Codex echoes the name with `thread/name/updated` (a `SessionTitle`
    /// that changes nothing).
    async fn rename(&self, title: &str) -> Result<(), AdapterError> {
        const METHOD: &str = "thread/name/set";
        let params = json!({ "threadId": self.shared.thread_id(), "name": title });
        self.shared
            .peer
            .request_timeout::<_, Value>(METHOD, params, self.shared.policy.handshake_timeout)
            .await
            .map(|_| ())
            .map_err(|e| rpc_err(METHOD, e))
    }

    /// The thread (model, effort, modes, token usage), its goal, the account and its rate
    /// limits. `account/read` and `account/rateLimits/read` are asked now (in that order);
    /// the rest is what Codex last reported. When the rate limits cannot be read (a provider
    /// other than OpenAI's needs no sign-in, and then Codex refuses the read), the rolling
    /// updates Codex sent after model calls are shown.
    async fn status(&self) -> Result<Vec<StatusSection>, AdapterError> {
        let peer = &self.shared.peer;
        let timeout = self.shared.policy.handshake_timeout;
        let account = read_account(peer, timeout).await;
        let limits = read_rate_limits(peer, timeout).await;
        let st = self.shared.state.lock();
        let facts = ThreadFacts {
            thread_id: st.thread_id.clone(),
            model: st.model(),
            effort: st.settings.effort.clone().or(st.codex_effort.clone()),
            permissions: st
                .applied_mode
                .as_deref()
                .and_then(settings::preset)
                .map(|p| p.label.to_owned()),
            plan: st.known_plan,
            service_tier: st
                .known_tier
                .as_deref()
                .map(|t| settings::tier_word(t, &st.fast_tiers)),
            usage: st.token_usage.clone(),
        };
        let mut sections = vec![status::thread_section(&facts)];
        if let Some(goal) = &st.goal {
            sections.push(status::goal_section(goal));
        }
        sections.push(status::account_section(
            account.as_ref().map_err(String::as_str),
        ));
        let limits = match &limits {
            Ok(snapshot) => RateLimits::Read(snapshot),
            Err(error) => RateLimits::Rolling {
                snapshot: st.rate_limits.as_ref(),
                read_error: error,
            },
        };
        sections.push(status::rate_limit_section(limits, unix_now()));
        Ok(sections)
    }

    /// Stops background task `key`: a terminal with `thread/backgroundTerminals/terminate`
    /// (Codex ends it as a failed command, reported as stopped), a sub-agent with
    /// `turn/interrupt` of its running turn (its commands go on as terminals of their own).
    /// `Ok` only says Codex took the request; the end comes as the task's new state.
    async fn stop_background(&self, key: &str) -> Result<(), AdapterError> {
        let timeout = self.shared.policy.handshake_timeout;
        match self.stop_target(key)? {
            StopTarget::Terminal { thread, process_id } => {
                const METHOD: &str = "thread/backgroundTerminals/terminate";
                self.shared
                    .state
                    .lock()
                    .background
                    .set_stop_requested(key, true);
                let answer = self
                    .shared
                    .peer
                    .request_timeout::<_, TerminateResponse>(
                        METHOD,
                        json!({ "threadId": thread, "processId": process_id }),
                        timeout,
                    )
                    .await;
                let refused = |e: AdapterError| {
                    self.shared
                        .state
                        .lock()
                        .background
                        .set_stop_requested(key, false);
                    Err(e)
                };
                match answer {
                    Ok(TerminateResponse { terminated: true }) => Ok(()),
                    Ok(TerminateResponse { terminated: false }) => {
                        refused(AdapterError::Harness(format!(
                            "Codex did not terminate background terminal {process_id} (it is not running or could not be stopped)"
                        )))
                    }
                    Err(e) => refused(rpc_err(METHOD, e)),
                }
            }
            StopTarget::SubAgent { thread, turn } => self
                .shared
                .peer
                .request_timeout::<_, Value>(
                    "turn/interrupt",
                    json!({ "threadId": thread, "turnId": turn }),
                    timeout,
                )
                .await
                .map(|_| ())
                .map_err(|e| rpc_err("turn/interrupt", e)),
        }
    }

    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        let shared = self.shared.clone();
        self.shared
            .shutdown
            .get_or_init(|| async move {
                let running = {
                    let mut st = shared.state.lock();
                    st.closing = true;
                    match &st.turn {
                        TurnPhase::Running(id) => Some(id.clone()),
                        _ => None,
                    }
                };
                if let Some(turn_id) = running {
                    let params = json!({ "threadId": shared.thread_id(), "turnId": turn_id });
                    let _ = tokio::time::timeout(
                        shared.policy.stop_grace,
                        shared.peer.request_value("turn/interrupt", params),
                    )
                    .await;
                }
                shared.peer.close_writer().await;
                shared.link.shutdown(shared.policy.stop_grace, reason).await
            })
            .await
            .clone()
    }
}

/// `account/read`; the error is Codex's (or the transport's) text.
pub(crate) async fn read_account(
    peer: &RpcPeer,
    timeout: std::time::Duration,
) -> Result<AccountReadResponse, String> {
    const METHOD: &str = "account/read";
    peer.request_timeout::<_, AccountReadResponse>(
        METHOD,
        json!({ "refreshToken": false }),
        timeout,
    )
    .await
    .map_err(|e| match e {
        RpcCallError::Rpc(e) => e.message,
        other => rpc_err(METHOD, other).detail(),
    })
}

/// `account/rateLimits/read` (it takes no parameters); the error is Codex's text (codex-cli
/// 0.148.0: "codex account authentication required to read rate limits" without an OpenAI
/// sign-in).
pub(crate) async fn read_rate_limits(
    peer: &RpcPeer,
    timeout: std::time::Duration,
) -> Result<RateLimitSnapshot, String> {
    const METHOD: &str = "account/rateLimits/read";
    peer.request_timeout::<_, RateLimitsReadResponse>(METHOD, Value::Null, timeout)
        .await
        .map(|r| r.rate_limits)
        .map_err(|e| match e {
            RpcCallError::Rpc(e) => e.message,
            other => rpc_err(METHOD, other).detail(),
        })
}

/// The current Unix time in seconds (for "resets in" of the rate limits).
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
