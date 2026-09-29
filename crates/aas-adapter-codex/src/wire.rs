//! The subset of the Codex app-server v2 protocol this adapter uses.
//!
//! Types mirror `codex app-server generate-ts` output of codex-cli 0.148.0. Every field the
//! adapter does not strictly need is optional so that additive protocol changes never break
//! parsing; unknown fields are ignored.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `thread/start`, `thread/resume` and `thread/fork` responses share this shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadOpenResponse {
    pub thread: WireThread,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<Value>,
    #[serde(default)]
    pub approvals_reviewer: Option<String>,
    #[serde(default)]
    pub sandbox: Option<Value>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// The thread's service tier (`null`: none requested; a persisted tier on resume).
    #[serde(default)]
    pub service_tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireThread {
    pub id: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Unix seconds.
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub turns: Vec<WireTurn>,
    /// The thread that spawned this one (sub-agent threads).
    #[serde(default)]
    pub parent_thread_id: Option<String>,
    /// Name Codex gave a sub-agent (e.g. "Boole").
    #[serde(default)]
    pub agent_nickname: Option<String>,
    /// `SessionSource`; sub-agents have `{subAgent: {thread_spawn: {agent_path, …}}}`.
    #[serde(default)]
    pub source: Option<Value>,
}

impl WireThread {
    /// `source.subAgent.thread_spawn.agent_path` of a sub-agent thread (v2 multi-agent).
    pub fn agent_path(&self) -> Option<&str> {
        self.source
            .as_ref()?
            .get("subAgent")?
            .get("thread_spawn")?
            .get("agent_path")?
            .as_str()
            .filter(|p| !p.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTurn {
    pub id: String,
    #[serde(default)]
    pub items: Vec<Value>,
    pub status: String,
    #[serde(default)]
    pub error: Option<WireTurnError>,
    /// Unix seconds.
    #[serde(default)]
    pub started_at: Option<i64>,
    /// Unix seconds.
    #[serde(default)]
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTurnError {
    pub message: String,
    #[serde(default)]
    pub codex_error_info: Option<Value>,
    #[serde(default)]
    pub additional_details: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnStartResponse {
    pub turn: WireTurn,
}

/// `review/start` (the review runs as turn `turn.id`; for an inline review Codex sends no
/// `turn/started` for it).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStartResponse {
    pub turn: WireTurn,
}

/// `thread/settings/updated` (sent with the experimental API whenever the thread's settings
/// change: the model, effort, service tier and collaboration mode Codex uses from now on).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSettingsUpdated {
    pub thread_settings: WireThreadSettings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireThreadSettings {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<Value>,
    #[serde(default)]
    pub approvals_reviewer: Option<String>,
    #[serde(default)]
    pub sandbox_policy: Option<Value>,
    #[serde(default)]
    pub collaboration_mode: Option<WireCollaborationMode>,
}

/// `CollaborationMode` (`mode` is `plan` or `default`).
#[derive(Debug, Clone, Deserialize)]
pub struct WireCollaborationMode {
    pub mode: String,
}

/// `ThreadGoal`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireGoal {
    pub objective: String,
    /// `active`, `paused`, `blocked`, `usageLimited`, `budgetLimited` or `complete`.
    pub status: String,
    #[serde(default)]
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub tokens_used: u64,
    #[serde(default)]
    pub time_used_seconds: u64,
}

/// `thread/goal/updated`: `turnId` is the turn the change happened in (`null` for a change a
/// client made, and for the snapshot sent on resume).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalUpdated {
    #[serde(default)]
    pub turn_id: Option<String>,
    pub goal: WireGoal,
}

/// `thread/goal/set`.
#[derive(Debug, Clone, Deserialize)]
pub struct GoalSetResponse {
    pub goal: WireGoal,
}

/// `thread/goal/get`.
#[derive(Debug, Clone, Deserialize)]
pub struct GoalGetResponse {
    #[serde(default)]
    pub goal: Option<WireGoal>,
}

/// `thread/goal/clear`.
#[derive(Debug, Clone, Deserialize)]
pub struct GoalClearResponse {
    pub cleared: bool,
}

/// `account/read`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountReadResponse {
    /// `{type: "apiKey"}`, `{type: "chatgpt", email, planType}`, `{type: "amazonBedrock", …}`.
    #[serde(default)]
    pub account: Option<Value>,
    #[serde(default)]
    pub requires_openai_auth: bool,
}

/// `account/rateLimits/read`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsReadResponse {
    pub rate_limits: RateLimitSnapshot,
}

/// `account/rateLimits/updated` (a sparse rolling update: absent values do not clear earlier
/// ones).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsUpdated {
    pub rate_limits: RateLimitSnapshot,
}

/// `RateLimitSnapshot` (the fields shown in the status).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    #[serde(default)]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub primary: Option<RateLimitWindow>,
    #[serde(default)]
    pub secondary: Option<RateLimitWindow>,
    #[serde(default)]
    pub credits: Option<CreditsSnapshot>,
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub rate_limit_reached_type: Option<String>,
}

impl RateLimitSnapshot {
    /// Merges a rolling update into this snapshot: values the update carries replace the known
    /// ones, absent values keep them (Codex's rule for `account/rateLimits/updated`).
    pub fn merge(&mut self, update: RateLimitSnapshot) {
        fn keep<T>(slot: &mut Option<T>, new: Option<T>) {
            if new.is_some() {
                *slot = new;
            }
        }
        keep(&mut self.limit_name, update.limit_name);
        keep(&mut self.primary, update.primary);
        keep(&mut self.secondary, update.secondary);
        keep(&mut self.credits, update.credits);
        keep(&mut self.plan_type, update.plan_type);
        keep(
            &mut self.rate_limit_reached_type,
            update.rate_limit_reached_type,
        );
    }

    /// Whether the snapshot reports anything (a provider without Codex's rate-limit headers
    /// reports every field `null`).
    pub fn is_empty(&self) -> bool {
        self.primary.is_none()
            && self.secondary.is_none()
            && self.credits.is_none()
            && self.rate_limit_reached_type.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    pub used_percent: f64,
    #[serde(default)]
    pub window_duration_mins: Option<u64>,
    /// Unix seconds.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    #[serde(default)]
    pub balance: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnNotification {
    pub turn: WireTurn,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub item: Value,
}

/// Delta notifications (`item/agentMessage/delta`, `item/reasoning/*Delta`, `item/plan/delta`,
/// `item/commandExecution/outputDelta`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeltaNotification {
    pub item_id: String,
    pub delta: String,
    #[serde(default)]
    pub summary_index: Option<i64>,
    #[serde(default)]
    pub content_index: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchUpdatedNotification {
    pub item_id: String,
    pub changes: Vec<WireFileChange>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpProgressNotification {
    pub item_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageNotification {
    #[serde(default)]
    pub turn_id: Option<String>,
    pub token_usage: ThreadTokenUsage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadTokenUsage {
    pub total: TokenBreakdown,
    pub last: TokenBreakdown,
    /// Context window of the model in use; `null` when Codex does not know it.
    #[serde(default, rename = "modelContextWindow")]
    pub model_context_window: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenBreakdown {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

impl TokenBreakdown {
    /// Component-wise `self - earlier`, saturating at zero.
    pub fn since(&self, earlier: &TokenBreakdown) -> TokenBreakdown {
        TokenBreakdown {
            total_tokens: self.total_tokens.saturating_sub(earlier.total_tokens),
            input_tokens: self.input_tokens.saturating_sub(earlier.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(earlier.cached_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_sub(earlier.cache_write_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(earlier.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(earlier.reasoning_output_tokens),
        }
    }

    pub fn add(&mut self, other: &TokenBreakdown) {
        self.total_tokens += other.total_tokens;
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.cache_write_input_tokens += other.cache_write_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnPlanUpdated {
    pub turn_id: String,
    #[serde(default)]
    pub plan: Vec<TurnPlanStep>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnPlanStep {
    pub step: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    pub error: WireTurnError,
    #[serde(default)]
    pub will_retry: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WarningNotification {
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SummaryNotification {
    pub summary: String,
    #[serde(default)]
    pub details: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRerouted {
    pub from_model: String,
    pub to_model: String,
    #[serde(default)]
    pub reason: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusUpdated {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolved {
    pub request_id: Value,
}

/// `thread/status/changed`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStatusChanged {
    pub status: WireThreadStatus,
}

/// `ThreadStatus`. Codex has no status for background work: `active` means a turn runs or
/// waits for an answer (`activeFlags`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WireThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    Active,
    #[serde(other)]
    Unknown,
}

/// `thread/started`.
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadStartedNotification {
    pub thread: WireThread,
}

/// `thread/backgroundTerminals/list` (experimental API): the terminals of one thread whose
/// process has not exited, one page.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTerminalsListResponse {
    #[serde(default)]
    pub data: Vec<WireBackgroundTerminal>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireBackgroundTerminal {
    /// The commandExecution item that started the terminal.
    pub item_id: String,
    /// Codex's own process number (not an OS pid), the key of `…/terminate`.
    pub process_id: String,
    /// The command as the agent wrote it (without the shell wrapper the item shows).
    #[serde(default)]
    pub command: String,
}

/// `thread/backgroundTerminals/terminate` (experimental API).
#[derive(Debug, Clone, Deserialize)]
pub struct TerminateResponse {
    pub terminated: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldWritableWarning {
    #[serde(default)]
    pub sample_paths: Vec<String>,
    #[serde(default)]
    pub extra_count: u64,
    #[serde(default)]
    pub failed_scan: bool,
}

// ----- items -----

/// `ThreadItem`. Unknown item types deserialize to [`WireItem::Unknown`].
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WireItem {
    UserMessage {
        #[serde(default)]
        content: Vec<Value>,
    },
    HookPrompt {
        id: String,
        #[serde(default)]
        fragments: Vec<HookPromptFragment>,
    },
    AgentMessage {
        id: String,
        #[serde(default)]
        text: String,
    },
    Plan {
        id: String,
        #[serde(default)]
        text: String,
    },
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<String>,
        #[serde(default)]
        content: Vec<String>,
    },
    CommandExecution {
        id: String,
        command: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        aggregated_output: Option<String>,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
    FileChange {
        id: String,
        #[serde(default)]
        changes: Vec<WireFileChange>,
        #[serde(default)]
        status: Option<String>,
    },
    McpToolCall {
        id: String,
        server: String,
        tool: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        result: Option<McpToolCallResult>,
        #[serde(default)]
        error: Option<MessageOnly>,
    },
    DynamicToolCall {
        id: String,
        #[serde(default)]
        namespace: Option<String>,
        tool: String,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        content_items: Option<Vec<Value>>,
        #[serde(default)]
        success: Option<bool>,
    },
    CollabAgentToolCall {
        id: String,
        tool: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        receiver_thread_ids: Vec<String>,
    },
    SubAgentActivity {
        id: String,
        kind: String,
        #[serde(default)]
        agent_thread_id: Option<String>,
        #[serde(default)]
        agent_path: Option<String>,
    },
    WebSearch {
        id: String,
        #[serde(default)]
        query: String,
        #[serde(default)]
        action: Option<Value>,
    },
    ImageView {
        id: String,
        path: String,
    },
    Sleep {
        id: String,
        #[serde(default)]
        duration_ms: u64,
    },
    ImageGeneration {
        id: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        revised_prompt: Option<String>,
        #[serde(default)]
        saved_path: Option<String>,
        #[serde(default)]
        failure: Option<Value>,
    },
    EnteredReviewMode {
        id: String,
        #[serde(default)]
        review: String,
    },
    ExitedReviewMode {
        id: String,
        #[serde(default)]
        review: String,
    },
    ContextCompaction {
        id: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookPromptFragment {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireFileChange {
    pub path: String,
    pub kind: WirePatchKind,
    #[serde(default)]
    pub diff: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WirePatchKind {
    Add,
    Delete,
    Update {
        #[serde(default)]
        move_path: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpToolCallResult {
    #[serde(default)]
    pub content: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageOnly {
    pub message: String,
}

// ----- server requests -----

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandApprovalParams {
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub proposed_execpolicy_amendment: Option<Vec<String>>,
    #[serde(default)]
    pub proposed_network_policy_amendments: Option<Vec<NetworkPolicyAmendment>>,
    #[serde(default)]
    pub network_approval_context: Option<NetworkApprovalContext>,
    /// Sent by codex-cli 0.148 although absent from the generated schema: the decisions the
    /// server offers for this request, in display order.
    #[serde(default)]
    pub available_decisions: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPolicyAmendment {
    pub host: String,
    pub action: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkApprovalContext {
    pub host: String,
    #[serde(default)]
    pub protocol: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeApprovalParams {
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub grant_root: Option<String>,
    #[serde(default)]
    pub available_decisions: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsApprovalParams {
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    pub permissions: RequestedPermissions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestedPermissions {
    #[serde(default)]
    pub network: Option<Value>,
    #[serde(default)]
    pub file_system: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputParams {
    #[serde(default)]
    pub item_id: Option<String>,
    pub questions: Vec<UserInputQuestion>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub options: Option<Vec<UserInputOption>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInputOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationParams {
    #[serde(default)]
    pub server_name: String,
    pub mode: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub requested_schema: Option<Value>,
    #[serde(default)]
    pub url: Option<String>,
}

// ----- listings -----

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponse {
    pub data: Vec<WireModel>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireModel {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
    #[serde(default)]
    pub is_default: bool,
    /// The service tiers the model offers besides the default one (codex-cli 0.148.0: the
    /// GPT models list `{id: "priority", name: "Fast"}`).
    #[serde(default)]
    pub service_tiers: Vec<WireServiceTier>,
}

/// `ModelServiceTier`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireServiceTier {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortOption {
    pub reasoning_effort: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillsListResponse {
    pub data: Vec<SkillsListEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillsListEntry {
    #[serde(default)]
    pub skills: Vec<SkillMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillMetadata {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub short_description: Option<String>,
    pub path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    pub data: Vec<WireThread>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadReadResponse {
    pub thread: WireThread,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_item_types_do_not_fail() {
        let item: WireItem =
            serde_json::from_value(json!({"type": "brandNewThing", "id": "x"})).unwrap();
        assert!(matches!(item, WireItem::Unknown));
    }

    #[test]
    fn parses_recorded_command_item() {
        let item: WireItem = serde_json::from_value(json!({
            "type":"commandExecution","id":"call_1","pluginId":null,"scriptPath":null,
            "command":"powershell.exe -Command whoami","cwd":"C:\\ws","processId":null,
            "source":"agent","status":"declined","commandActions":[],"aggregatedOutput":null,
            "exitCode":null,"durationMs":null
        }))
        .unwrap();
        match item {
            WireItem::CommandExecution {
                status,
                aggregated_output,
                ..
            } => {
                assert_eq!(status.as_deref(), Some("declined"));
                assert_eq!(aggregated_output, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn breakdown_difference_saturates() {
        let a = TokenBreakdown {
            total_tokens: 10,
            input_tokens: 8,
            ..Default::default()
        };
        let b = TokenBreakdown {
            total_tokens: 4,
            input_tokens: 9,
            ..Default::default()
        };
        let d = a.since(&b);
        assert_eq!(d.total_tokens, 6);
        assert_eq!(d.input_tokens, 0);
    }
}
