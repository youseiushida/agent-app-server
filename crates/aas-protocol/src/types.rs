//! Domain types carried by requests, responses and events.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::*;

/// Unix epoch milliseconds.
pub type Millis = i64;

// ---------------------------------------------------------------------------------------------
// Harnesses
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HarnessKind {
    Codex,
    Claude,
    Pi,
    Acp,
    Fake,
}

impl HarnessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::Codex => "codex",
            HarnessKind::Claude => "claude",
            HarnessKind::Pi => "pi",
            HarnessKind::Acp => "acp",
            HarnessKind::Fake => "fake",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Harness {
    pub id: String,
    pub kind: HarnessKind,
    pub display_name: String,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unavailable_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub executable: Option<String>,
    pub capabilities: HarnessCapabilities,
    pub models: Vec<Model>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub default_model: Option<String>,
    /// Empty when the harness has no notion of reasoning effort.
    pub effort_levels: Vec<EffortLevel>,
    pub permission_modes: Vec<PermissionMode>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub default_permission_mode: Option<String>,
    /// What the harness offers beyond `capabilities` (all off when absent).
    #[serde(default)]
    pub features: HarnessFeatures,
}

/// Features of a harness beyond [`HarnessCapabilities`]: the app offers the matching commands
/// and actions only where they are on. Everything is off by default, and a client that does not
/// know a field ignores it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HarnessFeatures {
    /// `thread/fork` at any turn whose native anchor was recorded (`Turn.forkable`): with the
    /// turn included, or right before it (`before`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub fork_at_turn: bool,
    /// A native session another process holds (so that resuming it fails) can still be forked:
    /// after a failed resume (`Turn.error.kind` `resumeFailed`) the client may offer a fork into
    /// a new thread besides a retry.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fork_while_held: bool,
    /// A user's title (`thread/update { title }`) is given to the native session too.
    #[serde(default, skip_serializing_if = "is_false")]
    pub rename: bool,
    /// `thread/sideQuestion` (a question answered beside the conversation, not in it).
    #[serde(default, skip_serializing_if = "is_false")]
    pub side_question: bool,
    /// `item/moveToBackground` on items that report `backgroundable`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub move_to_background: bool,
    /// `thread/harnessStatus` reports sections of the harness's own status.
    #[serde(default, skip_serializing_if = "is_false")]
    pub status: bool,
    /// The harness loads a project's own resources (extensions, prompts, skills) only when the
    /// user trusts the project: the client asks per project (`Project.harnessTrust`), never on
    /// its own.
    #[serde(default, skip_serializing_if = "is_false")]
    pub project_trust: bool,
    /// Plan mode driven by the app's `/plan` (`ThreadModes.plan`). Absent: the app does not offer
    /// `/plan` for this harness (a harness command of that name stays the harness's).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plan_mode: Option<PlanModeFeature>,
    /// Ids of the models (`Harness.models`) that support fast mode (`ThreadModes.fast`). Empty:
    /// no fast mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fast_mode_models: Vec<String>,
}

/// How the harness's plan mode continues from a proposed plan (`proposedPlan` items). The
/// texts are the harness's own, verbatim (Codex 0.148.0's for Codex).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeFeature {
    /// Sent (with plan mode off) to implement the proposed plan in the same thread. Absent: the
    /// harness continues by itself (e.g. through the approval of its plan).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub implement_prompt: Option<String>,
    /// Starts a new thread that implements the proposed plan: this text, a blank line
    /// (`"\n\n"`), then the plan's text. Absent: not offered.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub new_thread_preamble: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCapabilities {
    pub interrupt: bool,
    pub steer: bool,
    pub approvals: bool,
    pub questions: bool,
    pub resume: bool,
    pub fork: bool,
    pub images: bool,
    pub model_switch_live: bool,
    pub native_sessions: bool,
    /// The harness reports work that runs outside the turn lifecycle (background agents,
    /// shells, workflows, …) as background tasks (`backgroundTask/updated`).
    #[serde(default)]
    pub background_tasks: bool,
    /// Single background tasks can be stopped (`backgroundTask/stop`).
    #[serde(default)]
    pub background_stop: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    pub is_default: bool,
    /// Effort level ids this model accepts; absent means "all of the harness' levels".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort_levels: Option<Vec<String>>,
    /// Permission mode ids (`Harness.permissionModes`) this model can run in, when the harness
    /// says explicitly that it cannot run in all of them (e.g. Claude Code's auto mode, which
    /// the CLI offers only for models with `supportsAutoMode`); absent means "all of the
    /// harness' modes".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_modes: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EffortLevel {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionMode {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    pub is_default: bool,
}

// ---------------------------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub path: String,
    pub created_at: Millis,
    pub updated_at: Millis,
    pub archived: bool,
    pub defaults: ProjectDefaults,
    pub git: GitInfo,
    /// The user's decision, per harness id, whether the harness may load this project's own
    /// resources (harnesses with the feature `projectTrust`). A harness without an entry has
    /// not been decided for: the agent starts without a decision (its own saved one applies).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub harness_trust: std::collections::BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDefaults {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub harness_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitInfo {
    pub is_repo: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub root: Option<String>,
}

/// How a new project folder is initialised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ProjectInit {
    Empty,
    GitInit,
    GitClone { url: String },
}

// ---------------------------------------------------------------------------------------------
// Threads, turns
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSettings {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
}

/// Modes of a thread that are switched on and off (`thread/update { modes }`), next to the
/// picked [`ThreadSettings`]. A mode is only set on a harness that offers it
/// (`HarnessFeatures.planMode`, `HarnessFeatures.fastModeModels`); the harness's own reports
/// of plan mode are reflected here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadModes {
    /// The agent plans instead of acting (plan mode).
    #[serde(default)]
    pub plan: bool,
    /// Fast mode is requested (the harness reports what it does with it in
    /// `Thread.fastModeState`).
    #[serde(default)]
    pub fast: bool,
}

/// A change of [`ThreadModes`]: only the fields present change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadModesUpdate {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plan: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fast: Option<bool>,
}

/// Where a thread's agent works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Workspace {
    Local,
    Worktree {
        path: String,
        branch: String,
        base_ref: String,
    },
}

/// Requested workspace for `thread/create`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WorkspaceSpec {
    #[default]
    Local,
    Worktree {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        base_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        branch: Option<String>,
    },
    /// Where thread `thread_id` of the same project works: its worktree (shared, like a fork
    /// of that thread), or the project's folder. For a new conversation about the same working
    /// tree (e.g. implementing a worktree thread's proposed plan in a new thread).
    Thread { thread_id: ThreadId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ThreadStatus {
    Idle,
    Queued,
    Starting,
    Ready,
    Running,
    Stopping,
}

impl ThreadStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadStatus::Idle => "idle",
            ThreadStatus::Queued => "queued",
            ThreadStatus::Starting => "starting",
            ThreadStatus::Ready => "ready",
            ThreadStatus::Running => "running",
            ThreadStatus::Stopping => "stopping",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "idle" => ThreadStatus::Idle,
            "queued" => ThreadStatus::Queued,
            "starting" => ThreadStatus::Starting,
            "ready" => ThreadStatus::Ready,
            "running" => ThreadStatus::Running,
            "stopping" => ThreadStatus::Stopping,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub harness_id: String,
    pub title: String,
    pub cwd: String,
    pub workspace: Workspace,
    pub settings: ThreadSettings,
    pub status: ThreadStatus,
    pub pending_interactions: u32,
    pub queued_inputs: u32,
    /// Queued inputs do not start automatically (after an interrupted or failed turn) until
    /// `queue/resume` or a new `turn/start`.
    pub queue_paused: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_turn: Option<TurnSummary>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_error: Option<ThreadError>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub native_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub forked_from: Option<ForkOrigin>,
    pub usage: Usage,
    pub diff_available: bool,
    pub created_at: Millis,
    pub updated_at: Millis,
    pub last_activity_at: Millis,
    pub archived: bool,
    /// Pinned by the user (`thread/update { pinned }`); clients list pinned threads first.
    /// Pinning changes neither the order of `thread/list` nor `lastActivityAt`.
    #[serde(default)]
    pub pinned: bool,
    /// The thread's background work: how many tasks run, and the last one that ended.
    #[serde(default)]
    pub background: ThreadBackground,
    /// Plan mode and fast mode (`thread/update { modes }`).
    #[serde(default)]
    pub modes: ThreadModes,
    /// What the harness last reported about fast mode, verbatim (Claude Code's
    /// `fast_mode_state`: `on`, `off`, `cooldown`). Display only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fast_mode_state: Option<String>,
    /// Head of the thread stream, refreshed at turn boundaries.
    pub head: u64,
}

/// Summary of a thread's background tasks (`Thread.background`). It changes when a task
/// starts, ends or changes its `ambient` flag; progress alone does not change it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackground {
    /// Tasks with status `running` that are not `ambient`.
    pub running: u32,
    /// The last end of a task that is not `ambient` (by `endedAt`, then task id). It only moves
    /// on to later ends: when that task starts a new run, this stays its previous end until
    /// the new run ends.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_ended: Option<BackgroundTaskEnded>,
}

/// The task a thread's background work ended with last (`ThreadBackground.lastEnded`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskEnded {
    pub task_id: BackgroundTaskId,
    pub title: String,
    pub kind: BackgroundTaskKind,
    pub status: BackgroundTaskStatus,
    pub ended_at: Millis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnSummary {
    pub id: TurnId,
    pub index: u32,
    pub status: TurnStatus,
    pub started_at: Millis,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub completed_at: Option<Millis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadError {
    pub message: String,
    pub kind: String,
    pub at: Millis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ForkOrigin {
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_id: Option<TurnId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cost_usd: Option<f64>,
    /// Occupancy of the model's context window as the harness last reported it. Present only
    /// when the harness reports both numbers explicitly; it is never estimated.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context: Option<ContextUsage>,
}

/// Context-window occupancy, both numbers as reported by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    /// Tokens the conversation currently occupies in the context window.
    pub used_tokens: u64,
    /// Size of the model's context window.
    pub window_tokens: u64,
}

impl Usage {
    /// Adds `other` to `self` (cost is summed when either side has it). `context` is a
    /// snapshot, not a count: the latest reported value wins (`other`'s, when it has one).
    pub fn accumulate(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        self.context = other.context.or(self.context);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
}

impl TurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnStatus::Running => "running",
            TurnStatus::Completed => "completed",
            TurnStatus::Interrupted => "interrupted",
            TurnStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => TurnStatus::Running,
            "completed" => TurnStatus::Completed,
            "interrupted" => TurnStatus::Interrupted,
            "failed" => TurnStatus::Failed,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, TurnStatus::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: TurnId,
    pub thread_id: ThreadId,
    pub index: u32,
    pub status: TurnStatus,
    pub started_at: Millis,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub completed_at: Option<Millis>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<TurnError>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub diff: Option<DiffSummary>,
    /// Why the harness started this run by itself, when it said so explicitly (only on turns
    /// the agent started; see `TurnTrigger`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trigger: Option<TurnTrigger>,
    /// The harness's own anchor of this turn was recorded while it ran, so the thread can be
    /// forked at it (`thread/fork { atTurnId }`, feature `forkAtTurn`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub forkable: bool,
}

/// What made the harness start a run by itself, as the harness reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TurnTrigger {
    /// A background task ended (or reported something) and the harness took it up.
    BackgroundTask,
    /// A wakeup the harness had scheduled for itself came due.
    Scheduled,
}

impl TurnTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnTrigger::BackgroundTask => "backgroundTask",
            TurnTrigger::Scheduled => "scheduled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "backgroundTask" => TurnTrigger::BackgroundTask,
            "scheduled" => TurnTrigger::Scheduled,
            _ => return None,
        })
    }
}

impl Turn {
    pub fn summary(&self) -> TurnSummary {
        TurnSummary {
            id: self.id.clone(),
            index: self.index,
            status: self.status,
            started_at: self.started_at,
            completed_at: self.completed_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    pub message: String,
    /// `agentExited`, `adapterError`, `forced`, `daemonShutdown`, `systemShutdown` (Windows ended the
    /// session), `daemonRestarted`, `harnessError`, `spawnFailed`, ... (protocol.md 3.1)
    pub kind: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    pub files: u32,
    pub insertions: u64,
    pub deletions: u64,
}

// ---------------------------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    Interrupted,
    /// The item launched work that goes on as a background task (`Item.backgroundTaskId`);
    /// the task has its own lifecycle.
    Backgrounded,
}

impl ItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemStatus::InProgress => "inProgress",
            ItemStatus::Completed => "completed",
            ItemStatus::Failed => "failed",
            ItemStatus::Declined => "declined",
            ItemStatus::Interrupted => "interrupted",
            ItemStatus::Backgrounded => "backgrounded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "inProgress" => ItemStatus::InProgress,
            "completed" => ItemStatus::Completed,
            "failed" => ItemStatus::Failed,
            "declined" => ItemStatus::Declined,
            "interrupted" => ItemStatus::Interrupted,
            "backgrounded" => ItemStatus::Backgrounded,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: ItemId,
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub status: ItemStatus,
    pub started_at: Millis,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub completed_at: Option<Millis>,
    /// The background task this item launched (the task names the item as its origin).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub background_task_id: Option<BackgroundTaskId>,
    /// The running item can be moved to the background now (`item/moveToBackground`): the
    /// harness said so explicitly. Only while the item is `inProgress`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub backgroundable: bool,
    #[serde(flatten)]
    pub body: ItemBody,
}

/// Kind-specific content of an item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ItemBody {
    UserMessage {
        text: String,
        attachments: Vec<Attachment>,
        mentions: Vec<Mention>,
        delivery: UserMessageDelivery,
    },
    /// Markdown text.
    AgentMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    CommandExecution {
        command: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        cwd: Option<String>,
        output: String,
        output_truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        output_blob_id: Option<BlobId>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        duration_ms: Option<u64>,
    },
    FileChange {
        changes: Vec<FileChange>,
    },
    ToolCall {
        category: ToolCategory,
        name: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        server: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        input: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        output: Option<String>,
        output_truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        output_blob_id: Option<BlobId>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    Notice {
        level: NoticeLevel,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        code: Option<String>,
    },
    /// A plan the agent proposes in plan mode (Markdown), to implement next (see
    /// `HarnessFeatures.planMode`). Streams with `text` deltas.
    ProposedPlan {
        text: String,
    },
}

impl ItemBody {
    pub fn kind_str(&self) -> &'static str {
        match self {
            ItemBody::UserMessage { .. } => "userMessage",
            ItemBody::AgentMessage { .. } => "agentMessage",
            ItemBody::Reasoning { .. } => "reasoning",
            ItemBody::CommandExecution { .. } => "commandExecution",
            ItemBody::FileChange { .. } => "fileChange",
            ItemBody::ToolCall { .. } => "toolCall",
            ItemBody::Plan { .. } => "plan",
            ItemBody::Notice { .. } => "notice",
            ItemBody::ProposedPlan { .. } => "proposedPlan",
        }
    }

    /// Appends streamed text to the field a delta targets. Returns `false` when the item kind
    /// has no such field.
    pub fn append(&mut self, field: DeltaField, text: &str) -> bool {
        match (self, field) {
            (ItemBody::AgentMessage { text: t }, DeltaField::Text)
            | (ItemBody::Reasoning { text: t }, DeltaField::Text)
            | (ItemBody::ProposedPlan { text: t }, DeltaField::Text) => {
                t.push_str(text);
                true
            }
            (ItemBody::CommandExecution { output, .. }, DeltaField::Output) => {
                output.push_str(text);
                true
            }
            (ItemBody::ToolCall { output, .. }, DeltaField::Output) => {
                output.get_or_insert_with(String::new).push_str(text);
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum UserMessageDelivery {
    Normal,
    Steer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Attachment {
    Image { blob_id: BlobId, mime: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Mention {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub move_path: Option<String>,
    /// Unified diff of this file when the harness provides one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub added: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub removed: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeKind {
    Add,
    Delete,
    Update,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ToolCategory {
    Read,
    Search,
    Fetch,
    Mcp,
    Subagent,
    Edit,
    Execute,
    Think,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub text: String,
    pub status: PlanEntryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// Which field an `item/delta` appends to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DeltaField {
    Text,
    Output,
}

// ---------------------------------------------------------------------------------------------
// Interactions (approvals and questions)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum InteractionStatus {
    Pending,
    Resolved,
    Expired,
}

impl InteractionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            InteractionStatus::Pending => "pending",
            InteractionStatus::Resolved => "resolved",
            InteractionStatus::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => InteractionStatus::Pending,
            "resolved" => InteractionStatus::Resolved,
            "expired" => InteractionStatus::Expired,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ExpireReason {
    ProcessExited,
    TurnEnded,
    HarnessCancelled,
    DaemonRestarted,
    /// The background task that asked ended before an answer.
    TaskEnded,
}

/// An approval or a question. It belongs to exactly one of: the turn that asked (`turnId`),
/// the background task that asked (`backgroundTaskId`), or the thread (neither: the agent
/// asked while no turn ran and named no task). It expires with what it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Interaction {
    pub id: InteractionId,
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_id: Option<TurnId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub item_id: Option<ItemId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub background_task_id: Option<BackgroundTaskId>,
    pub status: InteractionStatus,
    pub created_at: Millis,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resolved_at: Option<Millis>,
    /// Device id that answered, or `"system"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resolved_by: Option<String>,
    pub request: InteractionRequest,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resolution: Option<InteractionResolution>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expire_reason: Option<ExpireReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InteractionRequest {
    Approval {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        detail: Option<String>,
        subject: Subject,
        options: Vec<ApprovalOption>,
    },
    Question {
        title: String,
        questions: Vec<Question>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Subject {
    Command {
        command: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        cwd: Option<String>,
    },
    FileChange {
        changes: Vec<FileChange>,
    },
    Tool {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        input: Option<Value>,
    },
    Plan {
        text: String,
    },
    Permissions {
        description: String,
    },
    Other {
        description: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalOption {
    pub id: String,
    pub label: String,
    pub kind: ApprovalOptionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalOptionKind {
    AllowOnce,
    AllowForSession,
    AllowAlways,
    Deny,
    DenyWithFeedback,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub header: Option<String>,
    pub prompt: String,
    pub choices: Vec<QuestionChoice>,
    pub multi_select: bool,
    pub allow_free_text: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub placeholder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuestionChoice {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InteractionResolution {
    Approval {
        option_id: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        feedback: Option<String>,
    },
    Question {
        answers: Vec<QuestionAnswer>,
    },
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuestionAnswer {
    pub question_id: String,
    pub choice_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------------------------

/// What kind of work a background task is, as the harness reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundTaskKind {
    /// An agent working on its own (a background sub-agent).
    Agent,
    /// A command left running (a background shell or terminal).
    Shell,
    /// A multi-agent workflow.
    Workflow,
    /// A watcher that reports what it observes.
    Monitor,
    /// Work that runs elsewhere (in the cloud) and that the harness tracks.
    Remote,
    /// A wakeup the harness scheduled for itself: when it comes due the harness starts a run
    /// (`nextRunAt`).
    Scheduled,
    Other,
}

impl BackgroundTaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackgroundTaskKind::Agent => "agent",
            BackgroundTaskKind::Shell => "shell",
            BackgroundTaskKind::Workflow => "workflow",
            BackgroundTaskKind::Monitor => "monitor",
            BackgroundTaskKind::Remote => "remote",
            BackgroundTaskKind::Scheduled => "scheduled",
            BackgroundTaskKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "agent" => BackgroundTaskKind::Agent,
            "shell" => BackgroundTaskKind::Shell,
            "workflow" => BackgroundTaskKind::Workflow,
            "monitor" => BackgroundTaskKind::Monitor,
            "remote" => BackgroundTaskKind::Remote,
            "scheduled" => BackgroundTaskKind::Scheduled,
            "other" => BackgroundTaskKind::Other,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundTaskStatus {
    Running,
    /// The harness reported that it finished.
    Completed,
    /// The harness reported that it failed.
    Failed,
    /// It was stopped: the harness reported it (e.g. after `backgroundTask/stop`), or the
    /// daemon stopped the agent's process (`endReason`).
    Stopped,
    /// The agent's process ended unexpectedly, or the daemon restarted, while it ran: how it
    /// ended is unknown (`endReason`).
    Lost,
}

impl BackgroundTaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BackgroundTaskStatus::Running => "running",
            BackgroundTaskStatus::Completed => "completed",
            BackgroundTaskStatus::Failed => "failed",
            BackgroundTaskStatus::Stopped => "stopped",
            BackgroundTaskStatus::Lost => "lost",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => BackgroundTaskStatus::Running,
            "completed" => BackgroundTaskStatus::Completed,
            "failed" => BackgroundTaskStatus::Failed,
            "stopped" => BackgroundTaskStatus::Stopped,
            "lost" => BackgroundTaskStatus::Lost,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, BackgroundTaskStatus::Running)
    }
}

/// Why a background task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundEndReason {
    /// The harness reported the end.
    Harness,
    /// The agent's process was stopped by `thread/stop` or an archive.
    ThreadStopped,
    /// The agent's process was stopped because the thread was idle (no turn, no queued input,
    /// no background work that keeps it alive): only `ambient` tasks, or a task reported while
    /// the process was already stopping.
    IdleStop,
    /// The daemon was stopped.
    DaemonShutdown,
    /// Windows ended the session (sign-out, shutdown, restart) and the daemon stopped with it.
    SystemShutdown,
    /// The agent's process was terminated because it did not honour an interrupt.
    ForcedStop,
    /// The agent's process was replaced by a new one to apply the thread's settings.
    ProcessReplaced,
    /// The agent's process ended by itself.
    ProcessExited,
    /// The daemon restarted while the task ran.
    DaemonRestarted,
}

/// A piece of work the harness runs outside the turn lifecycle: a background agent, a shell
/// left running, a workflow, a scheduled wakeup, … Reported only from explicit signals of the
/// harness; it can outlive turns and can start again (`runs`). Every `backgroundTask/updated`
/// carries the whole object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTask {
    pub id: BackgroundTaskId,
    pub thread_id: ThreadId,
    /// The harness's own id of the task (e.g. Claude's `task_id`, a Codex process id).
    pub native_id: String,
    pub kind: BackgroundTaskKind,
    /// The harness's description of the task, verbatim.
    pub title: String,
    pub status: BackgroundTaskStatus,
    /// The harness says the task is not activity: it is shown but not counted as running and
    /// does not keep the agent's process alive.
    pub ambient: bool,
    /// How many times the task has started under the same `nativeId` (1 for the first run).
    pub runs: u32,
    /// The turn that ran when the task was first reported (or the thread's last turn).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_id: Option<TurnId>,
    /// The item that launched the task (its status is `backgrounded`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub origin_item_id: Option<ItemId>,
    /// The background task that launched this one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_task_id: Option<BackgroundTaskId>,
    /// When the current run started.
    pub started_at: Millis,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ended_at: Option<Millis>,
    /// Present once the task ended.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_reason: Option<BackgroundEndReason>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub progress: Option<BackgroundProgress>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<BackgroundResult>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage: Option<BackgroundUsage>,
    /// The harness can stop this task on its own (`backgroundTask/stop`).
    pub stoppable: bool,
    /// Set while a `backgroundTask/stop` waits for the harness to report the end.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stop_requested_at: Option<Millis>,
    /// The harness did not report the end within `policy.background_stop_confirm_timeout` of
    /// the last stop request (the task goes on; nothing was escalated).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stop_unconfirmed_at: Option<Millis>,
    /// When the harness says the task runs next (scheduled wakeups).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub next_run_at: Option<Millis>,
    /// What the current run printed so far, as the harness streams it explicitly (a background
    /// shell's output): at most `policy.max_inline_output_bytes`, extended by
    /// `backgroundTask/outputDelta`. Absent while nothing was streamed, and once the run ended
    /// with an output of its own in `result` (the whole output, which supersedes it).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output: Option<String>,
    /// `output` reached `policy.max_inline_output_bytes`: the rest of the run's output is not
    /// streamed (the whole output comes with the end, in `result`, when the harness reports it).
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_truncated: bool,
}

/// Progress a harness reports for a running task. Every value is the harness's own; a harness
/// that does not report one leaves it out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundProgress {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_uses: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<u64>,
    /// What the task is doing, as the harness summarizes it (display only).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    /// The agents of a workflow, in the harness's order.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub workflow: Vec<WorkflowAgent>,
}

/// One agent of a workflow (`BackgroundProgress.workflow`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgent {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<String>,
    pub state: WorkflowAgentState,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowAgentState {
    Start,
    Progress,
    Done,
    Error,
}

/// What a finished task produced, only from explicit fields of the harness (never parsed out
/// of text written for people).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundResult {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit_code: Option<i32>,
    /// The task's output (at most `policy.max_inline_output_bytes`; the whole output is in
    /// `outputBlobId` when it is longer).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output: Option<String>,
    #[serde(default)]
    pub output_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output_blob_id: Option<BlobId>,
    /// Bytes at the start of the task's output that were not read: the harness kept the output
    /// in a file larger than `policy.max_output_file_bytes`, and the end of it was read
    /// (`output` and `outputBlobId` hold that end).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output_omitted_bytes: Option<u64>,
}

/// What a task used, as the harness reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundUsage {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_uses: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cost_usd: Option<f64>,
}

// ---------------------------------------------------------------------------------------------
// Input, queue, commands
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InputPart {
    Text { text: String },
    Image { blob_id: BlobId },
    Mention { path: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Delivery {
    #[default]
    Auto,
    Steer,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Disposition {
    Started,
    Steered,
    Queued,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueuedInput {
    pub id: QueuedInputId,
    pub thread_id: ThreadId,
    pub created_at: Millis,
    pub preview: String,
    pub input: Vec<InputPart>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    pub source: CommandSource,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub argument_hint: Option<String>,
    pub action: CommandAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CommandSource {
    App,
    Harness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CommandAction {
    /// Insert text into the composer (harness-native commands such as `/compact`).
    InsertText { text: String },
    /// Call a protocol method; the client fills in `clientRequestId` and `threadId`.
    Method {
        method: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        params: Option<Value>,
    },
    /// Open a picker in the client.
    Picker { picker: PickerKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PickerKind {
    Model,
    Effort,
    PermissionMode,
}

// ---------------------------------------------------------------------------------------------
// Operations, native sessions, devices, filesystem
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub id: OperationId,
    pub kind: OperationKind,
    pub status: OperationStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_id: Option<ProjectId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message: Option<String>,
    /// The latest progress line the tool printed, verbatim (e.g. git's
    /// `Receiving objects:  42% (…)`). For display only: its content is never interpreted.
    /// Present only while the operation runs.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub progress: Option<String>,
    pub started_at: Millis,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub finished_at: Option<Millis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OperationKind {
    GitClone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OperationStatus {
    Running,
    Succeeded,
    Failed,
    /// Ended by `operation/cancel`: the tool's process tree was terminated and nothing it
    /// produced was kept.
    Cancelled,
}

impl OperationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OperationStatus::Running => "running",
            OperationStatus::Succeeded => "succeeded",
            OperationStatus::Failed => "failed",
            OperationStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => OperationStatus::Running,
            "succeeded" => OperationStatus::Succeeded,
            "failed" => OperationStatus::Failed,
            "cancelled" => OperationStatus::Cancelled,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, OperationStatus::Running)
    }
}

/// A native session of a harness (`native/list`). `native_session_id` is unique within one
/// `native/list` result, for every harness: a session the CLI lists several times (Codex lists a
/// resumed thread once per rollout) is returned once, at the position of its first entry, with
/// the content of its latest one (`updated_at`). Clients may key their lists by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeSession {
    pub native_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub updated_at: Option<Millis>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub imported_thread_id: Option<ThreadId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub platform: Option<String>,
    pub created_at: Millis,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_seen_at: Option<Millis>,
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsRoot {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub is_git_repo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub modified_at: Option<Millis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffFile {
    pub path: String,
    pub kind: FileChangeKind,
    pub added: u64,
    pub removed: u64,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DiffScope {
    Turn { turn_id: TurnId },
    Thread,
}

/// One section of a harness's own status (`thread/harnessStatus`): what the harness reports,
/// in its order and words. Display only: nobody interprets the values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusSection {
    pub title: String,
    pub rows: Vec<StatusRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusRow {
    pub label: String,
    pub value: String,
}

/// Policy values the client must honour (sent in the `initialize` result).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientPolicy {
    pub heartbeat_interval_ms: u64,
    pub client_timeout_ms: u64,
    pub max_client_frame_bytes: u64,
    pub max_blob_bytes: u64,
}
