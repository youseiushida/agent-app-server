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
    /// Head of the thread stream, refreshed at turn boundaries.
    pub head: u64,
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
}

impl ItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemStatus::InProgress => "inProgress",
            ItemStatus::Completed => "completed",
            ItemStatus::Failed => "failed",
            ItemStatus::Declined => "declined",
            ItemStatus::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "inProgress" => ItemStatus::InProgress,
            "completed" => ItemStatus::Completed,
            "failed" => ItemStatus::Failed,
            "declined" => ItemStatus::Declined,
            "interrupted" => ItemStatus::Interrupted,
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
        }
    }

    /// Appends streamed text to the field a delta targets. Returns `false` when the item kind
    /// has no such field.
    pub fn append(&mut self, field: DeltaField, text: &str) -> bool {
        match (self, field) {
            (ItemBody::AgentMessage { text: t }, DeltaField::Text)
            | (ItemBody::Reasoning { text: t }, DeltaField::Text) => {
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Interaction {
    pub id: InteractionId,
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_id: Option<TurnId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub item_id: Option<ItemId>,
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

/// Policy values the client must honour (sent in the `initialize` result).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientPolicy {
    pub heartbeat_interval_ms: u64,
    pub client_timeout_ms: u64,
    pub max_client_frame_bytes: u64,
    pub max_blob_bytes: u64,
}
