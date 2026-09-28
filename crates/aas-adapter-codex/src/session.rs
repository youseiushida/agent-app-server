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
    SettingsApplied, StartMode, TurnInput,
};
use aas_stdio::{Incoming, IncomingRequest, RpcCallError, RpcPeer, RpcWireError};
use aas_supervisor::{ExitInfo, StopReason};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, OnceCell, mpsc};

use crate::background::{Background, CommandEnd, Route, TurnEnd};
use crate::commands::{self, Intercept, SkillInfo};
use crate::link::ProcessLink;
use crate::mapping::{
    self, DecisionTable, ElicitationTable, MappedItem, ReasoningStream, UsageTracker,
    UserInputTable,
};
use crate::rpc_err;
use crate::settings;
use crate::wire::*;

/// Notifications that are understood and deliberately not forwarded (see the adapter doc).
const IGNORED_NOTIFICATIONS: &[&str] = &[
    "thread/goal/updated",
    "thread/goal/cleared",
    "thread/settings/updated",
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
    "account/rateLimits/updated",
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum TurnPhase {
    #[default]
    Idle,
    /// `turn/start` (or an intercepted command) was sent; the turn id is not known yet.
    Starting,
    Running(String),
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
}

impl State {
    fn new(
        thread_id: String,
        settings: ThreadSettings,
        applied_mode: Option<String>,
        skills: Vec<SkillInfo>,
    ) -> Self {
        Self {
            background: Background::new(thread_id.clone()),
            thread_id,
            settings,
            applied_mode,
            turn: TurnPhase::Idle,
            completed_turns: HashSet::new(),
            usage: UsageTracker::default(),
            items: HashMap::new(),
            child_changes: HashMap::new(),
            pending: HashMap::new(),
            seen_notices: HashSet::new(),
            plan_key: None,
            skills,
            closing: false,
            terminal_list_noticed: false,
        }
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
    let mut params = settings::open_overrides(&args.settings).map_err(AdapterError::Other)?;
    params.insert("cwd".into(), json!(args.cwd.to_string_lossy()));
    let method = match &args.mode {
        StartMode::New => "thread/start",
        StartMode::Resume { native_session_id } => {
            params.insert("threadId".into(), json!(native_session_id));
            "thread/resume"
        }
        StartMode::Fork { native_session_id } => {
            params.insert("threadId".into(), json!(native_session_id));
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
    let shared = Arc::new(Shared {
        peer,
        link,
        policy: args.policy,
        cwd: args.cwd,
        state: Mutex::new(State::new(
            opened.thread.id.clone(),
            args.settings,
            applied_mode.clone(),
            skills,
        )),
        events: Mutex::new(Some(tx)),
        turn_changed: Notify::new(),
        shutdown: OnceCell::new(),
    });
    shared.emit(AdapterEvent::SessionInfo {
        model: opened.model.clone(),
        permission_mode: applied_mode,
        effort: opened.reasoning_effort.clone(),
    });
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
            {
                let mut st = shared.state.lock();
                if st.turn == TurnPhase::Idle {
                    // A turn the agent started by itself: Codex's goal continuation starts turns
                    // without `turn/start` whenever the thread becomes idle with a goal set.
                    st.usage.start_turn();
                }
                if !st.completed_turns.contains(&n.turn.id) {
                    st.turn = TurnPhase::Running(n.turn.id.clone());
                }
                // Emitted under the state lock: `send` refuses with `TurnInProgress` only once
                // this turn's `TurnStarted` is out.
                shared.emit(AdapterEvent::TurnStarted);
            }
            shared.turn_changed.notify_waiters();
        }
        "turn/completed" => {
            let Some(n) = parse::<TurnNotification>(shared, method, &params) else {
                return;
            };
            let (thread, plan_key, usage) = {
                let mut st = shared.state.lock();
                st.completed_turns.insert(n.turn.id.clone());
                st.turn = TurnPhase::Idle;
                st.items.clear();
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
                st.usage.observe(&n.token_usage, current)
            };
            if let Some(usage) = usage {
                shared.emit(AdapterEvent::TurnUsage { usage });
            }
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
                TurnPhase::Idle => return None,
                TurnPhase::Starting => {}
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }

    fn begin_turn(
        &self,
    ) -> Result<(String, ThreadSettings, Option<String>, Vec<SkillInfo>), AdapterError> {
        let mut st = self.shared.state.lock();
        match st.turn {
            TurnPhase::Idle => {}
            // The engine sends only while no turn of its own runs: a running turn is one the
            // agent started by itself (goal continuation), whose `TurnStarted` is out (it is
            // emitted under this lock).
            TurnPhase::Running(_) => return Err(AdapterError::TurnInProgress),
            TurnPhase::Starting => {
                return Err(AdapterError::Other("a turn is being started".into()));
            }
        }
        st.turn = TurnPhase::Starting;
        st.usage.start_turn();
        Ok((
            st.thread_id.clone(),
            st.settings.clone(),
            st.applied_mode.clone(),
            st.skills.clone(),
        ))
    }

    fn abort_start(&self) {
        let mut st = self.shared.state.lock();
        if st.turn == TurnPhase::Starting {
            st.turn = TurnPhase::Idle;
        }
        drop(st);
        self.shared.turn_changed.notify_waiters();
    }

    fn started(&self, turn_id: String, applied_mode: Option<String>) {
        let mut st = self.shared.state.lock();
        if applied_mode.is_some() {
            st.applied_mode = applied_mode;
        }
        if st.turn == TurnPhase::Starting && !st.completed_turns.contains(&turn_id) {
            st.turn = TurnPhase::Running(turn_id);
        }
        drop(st);
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

#[async_trait]
impl SessionControl for CodexSession {
    // Every request below is bounded: the engine waits for these calls, and an app-server
    // that stops answering must not keep the thread waiting (`handshake_timeout` for requests,
    // `stop_grace` for the interrupt, like `shutdown`).
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError> {
        let (thread_id, settings, applied_mode, skills) = self.begin_turn()?;
        let peer = &self.shared.peer;
        let timeout = self.shared.policy.handshake_timeout;
        match commands::parse_intercept(&input) {
            Some(Intercept::Compact) => {
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
            Some(Intercept::Review { instructions }) => {
                let params = json!({
                    "threadId": thread_id,
                    "target": commands::review_target(instructions.as_deref()),
                    "delivery": "inline",
                });
                match peer
                    .request_timeout::<_, Value>("review/start", params, timeout)
                    .await
                {
                    Ok(resp) => {
                        if let Some(id) = resp
                            .get("turn")
                            .and_then(|t| t.get("id"))
                            .and_then(Value::as_str)
                        {
                            self.started(id.to_owned(), None);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        self.abort_start();
                        Err(rpc_err("review/start", e))
                    }
                }
            }
            None => {
                let overrides = match settings::turn_overrides(&settings, applied_mode.as_deref()) {
                    Ok(o) => o,
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
                        self.started(resp.turn.id, settings.permission_mode.clone());
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

    async fn steer(&self, input: TurnInput) -> Result<(), AdapterError> {
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
    async fn interrupt(&self) -> Result<(), AdapterError> {
        let grace = self.shared.policy.stop_grace;
        let interrupt = async {
            let Some(turn_id) = self.current_turn_id().await else {
                return Ok(());
            };
            let params = json!({ "threadId": self.shared.thread_id(), "turnId": turn_id });
            self.shared
                .peer
                .request_value("turn/interrupt", params)
                .await
                .map(|_| ())
                .map_err(|e| rpc_err("turn/interrupt", e))
        };
        tokio::time::timeout(grace, interrupt)
            .await
            .unwrap_or_else(|_| Err(rpc_err("turn/interrupt", RpcCallError::Timeout(grace))))
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
