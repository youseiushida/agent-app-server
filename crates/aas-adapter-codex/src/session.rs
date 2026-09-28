//! One Codex thread served by one `codex app-server` process.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use aas_harness::protocol::{
    FileChange, InteractionResolution, ItemBody, ItemStatus, NoticeLevel, ThreadSettings,
};
use aas_harness::{
    AdapterError, AdapterEvent, AdapterPolicy, SessionControl, SessionHandle, SettingsApplied,
    StartMode, TurnInput,
};
use aas_stdio::{Incoming, IncomingRequest, RpcCallError, RpcPeer, RpcWireError};
use aas_supervisor::{ExitInfo, StopReason};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, OnceCell, mpsc};

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
    "thread/started",
    "thread/status/changed",
    "thread/goal/updated",
    "thread/goal/cleared",
    "thread/settings/updated",
    "thread/queue/changed",
    "thread/archived",
    "thread/unarchived",
    "thread/deleted",
    "thread/closed",
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

#[derive(Default)]
struct State {
    thread_id: String,
    settings: ThreadSettings,
    /// Permission preset known to be in effect in Codex.
    applied_mode: Option<String>,
    turn: TurnPhase,
    completed_turns: HashSet<String>,
    usage: UsageTracker,
    items: HashMap<String, ItemTrack>,
    pending: HashMap<String, Pending>,
    seen_notices: HashSet<String>,
    plan_key: Option<String>,
    skills: Vec<SkillInfo>,
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

    fn notice(&self, level: NoticeLevel, message: String, code: &str) {
        self.emit(AdapterEvent::Notice {
            level,
            message,
            code: Some(code.to_owned()),
        });
    }

    /// Emits a notice only the first time this exact (code, message) pair appears in the
    /// session (Codex repeats identical plugin/config warnings on every thread load).
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
        state: Mutex::new(State {
            thread_id: opened.thread.id.clone(),
            settings: args.settings,
            applied_mode: applied_mode.clone(),
            skills,
            ..State::default()
        }),
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
            Incoming::Notification { method, params } => on_notification(&shared, &method, params),
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

/// Whether a notification concerns this session's thread (sub-agent threads report their own
/// progress through the parent's collab items and are not mirrored).
fn is_ours(shared: &Shared, params: &Value) -> bool {
    match params.get("threadId") {
        Some(Value::String(id)) => *id == shared.state.lock().thread_id,
        _ => true,
    }
}

fn on_notification(shared: &Arc<Shared>, method: &str, params: Value) {
    if IGNORED_NOTIFICATIONS.contains(&method) {
        return;
    }
    if !is_ours(shared, &params) {
        return;
    }
    match method {
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
                    // A turn we did not start ourselves (none expected, but stay consistent).
                    st.usage.start_turn();
                }
                if !st.completed_turns.contains(&n.turn.id) {
                    st.turn = TurnPhase::Running(n.turn.id.clone());
                }
            }
            shared.turn_changed.notify_waiters();
            shared.emit(AdapterEvent::TurnStarted);
        }
        "turn/completed" => {
            let Some(n) = parse::<TurnNotification>(shared, method, &params) else {
                return;
            };
            let (plan_key, usage) = {
                let mut st = shared.state.lock();
                st.completed_turns.insert(n.turn.id.clone());
                st.turn = TurnPhase::Idle;
                st.items.clear();
                st.pending.clear();
                (st.plan_key.take(), st.usage.turn_usage())
            };
            shared.turn_changed.notify_waiters();
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
            shared.emit(AdapterEvent::TurnCompleted {
                status,
                usage,
                error,
            });
        }
        "item/started" | "item/completed" => {
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
                    if method == "item/started" {
                        shared.emit(AdapterEvent::Native {
                            payload: json!({ "method": method, "params": params }),
                        });
                    }
                }
                MappedItem::Item { key, body, status } => {
                    if let ItemBody::FileChange { changes } = &body {
                        shared
                            .state
                            .lock()
                            .items
                            .entry(key.clone())
                            .or_default()
                            .changes = changes.clone();
                    }
                    if method == "item/started" {
                        shared.emit(AdapterEvent::ItemStarted { key, body });
                    } else {
                        shared.state.lock().items.remove(&key);
                        shared.emit(AdapterEvent::ItemCompleted {
                            key,
                            body: Some(body),
                            status,
                        });
                    }
                }
            }
        }
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
            shared.emit(AdapterEvent::ItemDelta {
                key: d.item_id,
                field: aas_harness::DeltaField::Output,
                text: d.delta,
            });
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
        "warning" => {
            if let Some(n) = parse::<WarningNotification>(shared, method, &params) {
                shared.notice_once(NoticeLevel::Warning, n.message, "warning");
            }
        }
        "configWarning" | "deprecationNotice" => {
            let Some(n) = parse::<SummaryNotification>(shared, method, &params) else {
                return;
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
            if let Some(n) = parse::<WarningNotification>(shared, method, &params) {
                shared.notice(NoticeLevel::Warning, n.message, "guardianWarning");
            }
        }
        "windows/worldWritableWarning" => {
            let Some(n) = parse::<WorldWritableWarning>(shared, method, &params) else {
                return;
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
        "mcpServer/startupStatus/updated" => {
            let Some(n) = parse::<McpServerStatusUpdated>(shared, method, &params) else {
                return;
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
            let Some(n) = parse::<ServerRequestResolved>(shared, method, &params) else {
                return;
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
        _ => shared.emit(AdapterEvent::Native {
            payload: json!({ "method": method, "params": params }),
        }),
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

async fn on_request(shared: &Arc<Shared>, req: IncomingRequest) {
    let request_id = request_key(&req.id);
    macro_rules! params {
        ($t:ty) => {
            match serde_json::from_value::<$t>(req.params.clone()) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(method = %req.method, error = %e, "unexpected Codex request shape");
                    let _ = shared.peer.respond_error(req.id.clone(), RpcWireError::new(-32602, format!("agent-app-server could not parse {}: {e}", req.method))).await;
                    shared.notice(NoticeLevel::Error, format!("Could not handle Codex request {}: {e}", req.method), "unsupportedRequest");
                    return;
                }
            }
        };
    }
    let (request, item_key, pending) = match req.method.as_str() {
        "item/commandExecution/requestApproval" => {
            let p = params!(CommandApprovalParams);
            let (request, table) = mapping::command_approval(&p);
            (
                request,
                p.item_id,
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
                .and_then(|id| shared.state.lock().items.get(id).map(|t| t.changes.clone()))
                .unwrap_or_default();
            let (request, table) = mapping::file_change_approval(&p, changes);
            (
                request,
                p.item_id,
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
                p.item_id,
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
                p.item_id,
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
            let _ = shared
                .peer
                .respond_error(req.id.clone(), RpcWireError::method_not_found(other))
                .await;
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
        if st.turn != TurnPhase::Idle {
            return Err(AdapterError::Other("a turn is already running".into()));
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

    async fn shutdown(&self, reason: StopReason) -> ExitInfo {
        let shared = self.shared.clone();
        self.shared
            .shutdown
            .get_or_init(|| async move {
                let running = match &shared.state.lock().turn {
                    TurnPhase::Running(id) => Some(id.clone()),
                    _ => None,
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
