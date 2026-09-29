//! Pure translation between Codex app-server messages and the normalized model.
//!
//! Every mapping here is a fixed table documented in `docs/adapters/codex.md`; nothing is
//! inferred from free text.

use std::collections::BTreeMap;
use std::path::Path;

use aas_harness::protocol::{
    ApprovalOption, ApprovalOptionKind, ContextUsage, FileChange, FileChangeKind,
    InteractionRequest, InteractionResolution, ItemBody, ItemStatus, NoticeLevel, PlanEntry,
    PlanEntryStatus, Question, QuestionChoice, Subject, ToolCategory, TurnError, TurnStatus, Usage,
};
use serde_json::{Map, Value, json};

use crate::wire::*;

// ---------------------------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------------------------

/// Which reasoning stream an item follows (the first one that produced text wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStream {
    Summary,
    Content,
}

/// Result of mapping a `ThreadItem`.
#[derive(Debug, Clone, PartialEq)]
pub enum MappedItem {
    /// The engine owns user messages; Codex echoes them and they are skipped.
    Skip,
    Item {
        key: String,
        body: ItemBody,
        status: ItemStatus,
    },
    /// An item type this adapter does not know.
    Unknown,
}

pub fn item_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

/// Maps a `ThreadItem` (as found in `item/started`, `item/completed` or `thread/read`).
///
/// `reasoning` selects the reasoning stream for the final text of a reasoning item;
/// `None` means "summary if present, else raw content".
pub fn map_item(item: &WireItem, cwd: &Path, reasoning: Option<ReasoningStream>) -> MappedItem {
    let (key, body, status) = match item {
        WireItem::UserMessage { .. } => return MappedItem::Skip,
        WireItem::Unknown => return MappedItem::Unknown,
        WireItem::HookPrompt { id, fragments } => {
            let text: Vec<&str> = fragments
                .iter()
                .map(|f| f.text.as_str())
                .filter(|t| !t.is_empty())
                .collect();
            let message = if text.is_empty() {
                "A hook added context to the prompt".to_owned()
            } else {
                format!("Hook added context: {}", text.join("\n"))
            };
            (
                id,
                notice(NoticeLevel::Info, message, "hookPrompt"),
                ItemStatus::Completed,
            )
        }
        WireItem::AgentMessage { id, text } => (
            id,
            ItemBody::AgentMessage { text: text.clone() },
            ItemStatus::Completed,
        ),
        // Plan mode's proposed plan (the Markdown inside `<proposed_plan>`; Codex removes the
        // block from the agent message that carried it).
        WireItem::Plan { id, text } => (
            id,
            ItemBody::ProposedPlan { text: text.clone() },
            ItemStatus::Completed,
        ),
        WireItem::Reasoning {
            id,
            summary,
            content,
        } => (
            id,
            ItemBody::Reasoning {
                text: reasoning_text(summary, content, reasoning),
            },
            ItemStatus::Completed,
        ),
        WireItem::CommandExecution {
            id,
            command,
            cwd: item_cwd,
            status,
            aggregated_output,
            exit_code,
            duration_ms,
        } => (
            id,
            ItemBody::CommandExecution {
                command: command.clone(),
                cwd: item_cwd.clone(),
                output: aggregated_output.clone().unwrap_or_default(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: *exit_code,
                duration_ms: *duration_ms,
            },
            item_status(status.as_deref()),
        ),
        WireItem::FileChange {
            id,
            changes,
            status,
        } => (
            id,
            ItemBody::FileChange {
                changes: file_changes(changes, cwd),
            },
            item_status(status.as_deref()),
        ),
        WireItem::McpToolCall {
            id,
            server,
            tool,
            status,
            arguments,
            result,
            error,
        } => {
            let output = match (result, error) {
                (_, Some(err)) => Some(err.message.clone()),
                (Some(res), None) => Some(content_text(&res.content)),
                (None, None) => None,
            };
            (
                id,
                ItemBody::ToolCall {
                    category: ToolCategory::Mcp,
                    name: tool.clone(),
                    title: format!("{server}: {tool}"),
                    server: Some(server.clone()),
                    input: non_null(arguments),
                    output,
                    output_truncated: false,
                    output_blob_id: None,
                },
                item_status(status.as_deref()),
            )
        }
        WireItem::DynamicToolCall {
            id,
            namespace,
            tool,
            arguments,
            status,
            content_items,
            success,
        } => {
            let title = match namespace {
                Some(ns) if !ns.is_empty() => format!("{ns}: {tool}"),
                _ => tool.clone(),
            };
            let mut st = item_status(status.as_deref());
            if success == &Some(false) && st == ItemStatus::Completed {
                st = ItemStatus::Failed;
            }
            (
                id,
                ItemBody::ToolCall {
                    category: ToolCategory::Other,
                    name: tool.clone(),
                    title,
                    server: None,
                    input: non_null(arguments),
                    output: content_items.as_ref().map(|c| content_text(c)),
                    output_truncated: false,
                    output_blob_id: None,
                },
                st,
            )
        }
        WireItem::CollabAgentToolCall {
            id,
            tool,
            status,
            prompt,
            model,
            receiver_thread_ids,
        } => {
            let mut input = Map::new();
            if let Some(p) = prompt {
                input.insert("prompt".into(), json!(p));
            }
            if let Some(m) = model {
                input.insert("model".into(), json!(m));
            }
            if !receiver_thread_ids.is_empty() {
                input.insert("receiverThreadIds".into(), json!(receiver_thread_ids));
            }
            (
                id,
                ItemBody::ToolCall {
                    category: ToolCategory::Subagent,
                    name: tool.clone(),
                    title: format!("Agent: {tool}"),
                    server: None,
                    input: (!input.is_empty()).then_some(Value::Object(input)),
                    output: None,
                    output_truncated: false,
                    output_blob_id: None,
                },
                item_status(status.as_deref()),
            )
        }
        WireItem::SubAgentActivity {
            id,
            kind,
            agent_thread_id,
            agent_path,
        } => {
            let who = agent_path
                .clone()
                .or_else(|| agent_thread_id.clone())
                .unwrap_or_default();
            (
                id,
                ItemBody::ToolCall {
                    category: ToolCategory::Subagent,
                    name: "subAgentActivity".into(),
                    title: format!("Sub-agent {kind}: {who}"),
                    server: None,
                    input: None,
                    output: None,
                    output_truncated: false,
                    output_blob_id: None,
                },
                ItemStatus::Completed,
            )
        }
        WireItem::WebSearch { id, query, action } => {
            let action_type = action
                .as_ref()
                .and_then(|a| a.get("type"))
                .and_then(Value::as_str);
            let (category, title) = match action_type {
                Some("openPage") | Some("findInPage") => {
                    let url = action
                        .as_ref()
                        .and_then(|a| a.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    (
                        ToolCategory::Fetch,
                        if url.is_empty() {
                            query.clone()
                        } else {
                            url.to_owned()
                        },
                    )
                }
                _ => (ToolCategory::Search, query.clone()),
            };
            (
                id,
                ItemBody::ToolCall {
                    category,
                    name: "webSearch".into(),
                    title: if title.is_empty() {
                        "Web search".into()
                    } else {
                        title
                    },
                    server: None,
                    input: action.clone(),
                    output: None,
                    output_truncated: false,
                    output_blob_id: None,
                },
                ItemStatus::Completed,
            )
        }
        WireItem::ImageView { id, path } => (
            id,
            ItemBody::ToolCall {
                category: ToolCategory::Read,
                name: "imageView".into(),
                title: relative_path(path, cwd),
                server: None,
                input: None,
                output: None,
                output_truncated: false,
                output_blob_id: None,
            },
            ItemStatus::Completed,
        ),
        WireItem::Sleep { id, duration_ms } => (
            id,
            ItemBody::ToolCall {
                category: ToolCategory::Other,
                name: "sleep".into(),
                title: format!("Sleep {duration_ms} ms"),
                server: None,
                input: None,
                output: None,
                output_truncated: false,
                output_blob_id: None,
            },
            ItemStatus::Completed,
        ),
        WireItem::ImageGeneration {
            id,
            status,
            revised_prompt,
            saved_path,
            failure,
        } => {
            let st = if failure.is_some() {
                ItemStatus::Failed
            } else {
                item_status(status.as_deref())
            };
            (
                id,
                ItemBody::ToolCall {
                    category: ToolCategory::Other,
                    name: "imageGeneration".into(),
                    title: revised_prompt
                        .clone()
                        .unwrap_or_else(|| "Image generation".into()),
                    server: None,
                    input: None,
                    output: saved_path.clone(),
                    output_truncated: false,
                    output_blob_id: None,
                },
                st,
            )
        }
        WireItem::EnteredReviewMode { id, review } => (
            id,
            notice(
                NoticeLevel::Info,
                format!("Review started: {review}"),
                "reviewStarted",
            ),
            ItemStatus::Completed,
        ),
        WireItem::ExitedReviewMode { id, review } => (
            id,
            ItemBody::AgentMessage {
                text: review.clone(),
            },
            ItemStatus::Completed,
        ),
        WireItem::ContextCompaction { id } => (
            id,
            notice(
                NoticeLevel::Info,
                "Context compacted".into(),
                "contextCompacted",
            ),
            ItemStatus::Completed,
        ),
    };
    MappedItem::Item {
        key: key.clone(),
        body,
        status,
    }
}

/// The live counterpart of `exitedReviewMode`: the end of the review that `enteredReviewMode`
/// started (its text arrives as an agent message of its own; see `session.rs`). History keeps
/// the item's text instead ([`map_item`]), because `thread/read` has no such agent message.
pub fn review_finished_notice() -> ItemBody {
    notice(
        NoticeLevel::Info,
        "Review finished".into(),
        "reviewFinished",
    )
}

fn notice(level: NoticeLevel, message: String, code: &str) -> ItemBody {
    ItemBody::Notice {
        level,
        message,
        code: Some(code.to_owned()),
    }
}

fn non_null(v: &Value) -> Option<Value> {
    (!v.is_null()).then(|| v.clone())
}

/// Text of MCP / dynamic tool content items: `{type:"text", text}` entries verbatim, anything
/// else as compact JSON, one per line.
pub fn content_text(items: &[Value]) -> String {
    items
        .iter()
        .map(|item| {
            match (
                item.get("type").and_then(Value::as_str),
                item.get("text").and_then(Value::as_str),
            ) {
                (Some("text") | Some("inputText"), Some(text)) => text.to_owned(),
                _ => item.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Status strings of commandExecution / fileChange / mcpToolCall / … items.
pub fn item_status(status: Option<&str>) -> ItemStatus {
    match status {
        Some("inProgress") => ItemStatus::InProgress,
        Some("failed") => ItemStatus::Failed,
        Some("declined") => ItemStatus::Declined,
        // "completed" and items without a status field.
        _ => ItemStatus::Completed,
    }
}

pub fn reasoning_text(
    summary: &[String],
    content: &[String],
    stream: Option<ReasoningStream>,
) -> String {
    let pick = |primary: &[String], fallback: &[String]| {
        let parts = if primary.iter().any(|p| !p.is_empty()) {
            primary
        } else {
            fallback
        };
        parts
            .iter()
            .filter(|p| !p.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    match stream {
        Some(ReasoningStream::Content) => pick(content, summary),
        Some(ReasoningStream::Summary) | None => pick(summary, content),
    }
}

pub fn file_changes(changes: &[WireFileChange], cwd: &Path) -> Vec<FileChange> {
    changes.iter().map(|c| file_change(c, cwd)).collect()
}

/// Codex sends the full new content for `add`, the removed content for `delete` and a
/// unified diff (hunks) for `update`. All three are rendered as unified-diff hunks.
pub fn file_change(change: &WireFileChange, cwd: &Path) -> FileChange {
    let path = relative_path(&change.path, cwd);
    match &change.kind {
        WirePatchKind::Add => {
            let lines = content_lines(&change.diff);
            FileChange {
                path,
                kind: FileChangeKind::Add,
                move_path: None,
                diff: Some(whole_file_hunk(&lines, '+')),
                added: Some(lines.len() as u64),
                removed: Some(0),
            }
        }
        WirePatchKind::Delete => {
            let lines = content_lines(&change.diff);
            FileChange {
                path,
                kind: FileChangeKind::Delete,
                move_path: None,
                diff: Some(whole_file_hunk(&lines, '-')),
                added: Some(0),
                removed: Some(lines.len() as u64),
            }
        }
        WirePatchKind::Update { move_path } => {
            let (added, removed) = count_diff_lines(&change.diff);
            FileChange {
                path,
                kind: if move_path.is_some() {
                    FileChangeKind::Move
                } else {
                    FileChangeKind::Update
                },
                move_path: move_path.as_deref().map(|p| relative_path(p, cwd)),
                diff: (!change.diff.is_empty()).then(|| change.diff.clone()),
                added: Some(added),
                removed: Some(removed),
            }
        }
    }
}

fn content_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    content
        .strip_suffix('\n')
        .unwrap_or(content)
        .split('\n')
        .collect()
}

fn whole_file_hunk(lines: &[&str], sign: char) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let n = lines.len();
    let mut out = if sign == '+' {
        format!("@@ -0,0 +1,{n} @@\n")
    } else {
        format!("@@ -1,{n} +0,0 @@\n")
    };
    for line in lines {
        out.push(sign);
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Counts added/removed lines of a unified diff (file headers `+++`/`---` excluded).
pub fn count_diff_lines(diff: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// `path` relative to `cwd` with `/` separators when it lies inside `cwd` (compared
/// case-insensitively on Windows); otherwise unchanged.
pub fn relative_path(path: &str, cwd: &Path) -> String {
    let base = cwd.to_string_lossy().replace('\\', "/");
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return path.to_owned();
    }
    let original = path.replace('\\', "/");
    let same = |a: char, b: char| {
        if cfg!(windows) {
            a.to_lowercase().eq(b.to_lowercase())
        } else {
            a == b
        }
    };
    let mut rest = original.char_indices();
    for b in base.chars() {
        match rest.next() {
            Some((_, a)) if same(a, b) => {}
            _ => return path.to_owned(),
        }
    }
    match rest.next() {
        Some((i, '/')) if i + 1 < original.len() => original[i + 1..].to_owned(),
        _ => path.to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Turns, usage, plans
// ---------------------------------------------------------------------------------------------

pub fn turn_status(status: &str) -> TurnStatus {
    match status {
        "completed" => TurnStatus::Completed,
        "interrupted" => TurnStatus::Interrupted,
        "inProgress" => TurnStatus::Running,
        // "failed" and anything unknown are failures.
        _ => TurnStatus::Failed,
    }
}

/// Codex error → protocol turn error. When the message itself is a JSON document of the form
/// `{"error":{"message":…}}` (provider errors are forwarded verbatim), the inner message is used.
pub fn turn_error(err: &WireTurnError) -> TurnError {
    let mut message = serde_json::from_str::<Value>(&err.message)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| err.message.clone());
    if let Some(details) = err.additional_details.as_deref().filter(|d| !d.is_empty()) {
        message.push('\n');
        message.push_str(details);
    }
    let kind = match err.codex_error_info.as_ref() {
        Some(Value::String(s)) if s != "other" => format!("codex:{s}"),
        Some(Value::Object(map)) => map
            .keys()
            .next()
            .map(|k| format!("codex:{k}"))
            .unwrap_or_else(|| "harnessError".into()),
        _ => "harnessError".into(),
    };
    TurnError { message, kind }
}

pub fn usage_from(b: &TokenBreakdown) -> Usage {
    Usage {
        input_tokens: b.input_tokens,
        output_tokens: b.output_tokens,
        cached_input_tokens: b.cached_input_tokens,
        reasoning_tokens: b.reasoning_output_tokens,
        cost_usd: None,
        context: None,
    }
}

/// Per-turn usage from `thread/tokenUsage/updated` notifications.
///
/// `total` is cumulative for the thread and grows by exactly `last` per model call. The turn's
/// usage is the sum of `total` increments over notifications that carry the current turn's id;
/// notifications for other turns (e.g. the replay of the previous total right after
/// `thread/resume`) only move the baseline. Without a baseline, `last` is the increment.
///
/// The context-window occupancy comes from the same notification, as Codex defines it: the
/// tokens of the last model call (`last.totalTokens`, what Codex's
/// `TokenUsage::tokens_in_context_window` returns) out of `modelContextWindow`. It is reported
/// only when Codex sends a window size.
#[derive(Debug, Clone, Default)]
pub struct UsageTracker {
    prev_total: Option<TokenBreakdown>,
    turn: TokenBreakdown,
    counted: bool,
    context: Option<ContextUsage>,
}

impl UsageTracker {
    pub fn start_turn(&mut self) {
        self.turn = TokenBreakdown::default();
        self.counted = false;
    }

    /// Returns the updated turn usage when the notification belongs to the current turn.
    pub fn observe(&mut self, usage: &ThreadTokenUsage, for_current_turn: bool) -> Option<Usage> {
        let increment = match &self.prev_total {
            Some(prev) => usage.total.since(prev),
            None => usage.last,
        };
        self.prev_total = Some(usage.total);
        if let Some(window) = usage.model_context_window.filter(|w| *w > 0) {
            self.context = Some(ContextUsage {
                used_tokens: usage.last.total_tokens,
                window_tokens: window,
            });
        }
        if !for_current_turn {
            return None;
        }
        self.turn.add(&increment);
        self.counted = true;
        Some(self.usage())
    }

    /// Usage of the current turn, if any notification was counted.
    pub fn turn_usage(&self) -> Option<Usage> {
        self.counted.then(|| self.usage())
    }

    fn usage(&self) -> Usage {
        Usage {
            context: self.context,
            ..usage_from(&self.turn)
        }
    }
}

pub fn plan_entries(steps: &[TurnPlanStep]) -> Vec<PlanEntry> {
    steps
        .iter()
        .map(|s| PlanEntry {
            text: s.step.clone(),
            status: match s.status.as_str() {
                "completed" => PlanEntryStatus::Completed,
                "inProgress" => PlanEntryStatus::InProgress,
                _ => PlanEntryStatus::Pending,
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Server requests → interactions
// ---------------------------------------------------------------------------------------------

/// Answer table of an approval: option id → the JSON `decision` sent back to Codex.
pub type DecisionTable = Vec<(String, Value)>;

fn decision_option(decision: &Value, index: usize) -> Option<(ApprovalOption, Value)> {
    let make = |id: &str, label: String, kind| ApprovalOption {
        id: id.to_owned(),
        label,
        kind,
    };
    match decision {
        Value::String(s) => {
            let option = match s.as_str() {
                "accept" => make("accept", "Allow once".into(), ApprovalOptionKind::AllowOnce),
                "acceptForSession" => make(
                    "acceptForSession",
                    "Allow for this session".into(),
                    ApprovalOptionKind::AllowForSession,
                ),
                "decline" => make("decline", "Deny".into(), ApprovalOptionKind::Deny),
                "cancel" => make(
                    "cancel",
                    "Deny and stop the turn".into(),
                    ApprovalOptionKind::Abort,
                ),
                _ => return None,
            };
            Some((option, decision.clone()))
        }
        Value::Object(map) => {
            if let Some(inner) = map.get("acceptWithExecpolicyAmendment") {
                let prefix = inner
                    .get("execpolicy_amendment")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let option = make(
                    "acceptWithExecpolicyAmendment",
                    format!("Always allow commands starting with `{prefix}`"),
                    ApprovalOptionKind::AllowAlways,
                );
                return Some((option, decision.clone()));
            }
            if let Some(inner) = map.get("applyNetworkPolicyAmendment") {
                let amendment = inner.get("network_policy_amendment");
                let host = amendment
                    .and_then(|a| a.get("host"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let allow = amendment
                    .and_then(|a| a.get("action"))
                    .and_then(Value::as_str)
                    == Some("allow");
                let option = if allow {
                    make(
                        &format!("network:{index}"),
                        format!("Always allow network access to {host}"),
                        ApprovalOptionKind::AllowAlways,
                    )
                } else {
                    make(
                        &format!("network:{index}"),
                        format!("Always block network access to {host}"),
                        ApprovalOptionKind::Deny,
                    )
                };
                return Some((option, decision.clone()));
            }
            None
        }
        _ => None,
    }
}

/// Builds the options of a command-execution / file-change approval.
///
/// When Codex sends `availableDecisions` those are offered, in order. `decline` is always
/// offered as well: codex-cli 0.148 omits it from `availableDecisions` under the `untrusted`
/// policy yet accepts it (verified live — the command is skipped and the turn continues).
/// Without `availableDecisions` the schema's decision set is offered.
pub fn approval_options(
    available: Option<&[Value]>,
    default: Vec<Value>,
) -> (Vec<ApprovalOption>, DecisionTable) {
    let decisions: Vec<Value> = match available {
        Some(list) if !list.is_empty() => list.to_vec(),
        _ => default,
    };
    let mut options = Vec::new();
    let mut table = Vec::new();
    for (i, d) in decisions.iter().enumerate() {
        match decision_option(d, i) {
            Some((option, value)) => {
                if table.iter().any(|(id, _)| id == &option.id) {
                    continue;
                }
                table.push((option.id.clone(), value));
                options.push(option);
            }
            None => tracing::warn!(decision = %d, "unknown Codex approval decision; not offered"),
        }
    }
    if !table.iter().any(|(id, _)| id == "decline") {
        let (option, value) = decision_option(&json!("decline"), 0).expect("decline maps");
        // Keep "deny and stop" last.
        let pos = options
            .iter()
            .position(|o| o.kind == ApprovalOptionKind::Abort)
            .unwrap_or(options.len());
        options.insert(pos, option);
        table.insert(pos, ("decline".into(), value));
    }
    (options, table)
}

pub fn command_approval(params: &CommandApprovalParams) -> (InteractionRequest, DecisionTable) {
    let mut default = vec![json!("accept"), json!("acceptForSession")];
    if let Some(prefix) = params
        .proposed_execpolicy_amendment
        .as_ref()
        .filter(|p| !p.is_empty())
    {
        default.push(json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": prefix}}));
    }
    for amendment in params.proposed_network_policy_amendments.iter().flatten() {
        default
            .push(json!({"applyNetworkPolicyAmendment": {"network_policy_amendment": amendment}}));
    }
    default.push(json!("decline"));
    default.push(json!("cancel"));
    let (options, table) = approval_options(params.available_decisions.as_deref(), default);

    let mut detail = Vec::new();
    if let Some(reason) = params.reason.as_deref().filter(|r| !r.is_empty()) {
        detail.push(reason.to_owned());
    }
    if let Some(ctx) = &params.network_approval_context {
        match &ctx.protocol {
            Some(p) => detail.push(format!("Network access to {} ({p})", ctx.host)),
            None => detail.push(format!("Network access to {}", ctx.host)),
        }
    }
    let request = InteractionRequest::Approval {
        title: "Run command?".into(),
        detail: (!detail.is_empty()).then(|| detail.join("\n")),
        subject: Subject::Command {
            command: params.command.clone().unwrap_or_default(),
            cwd: params.cwd.clone(),
        },
        options,
    };
    (request, table)
}

pub fn file_change_approval(
    params: &FileChangeApprovalParams,
    changes: Vec<FileChange>,
) -> (InteractionRequest, DecisionTable) {
    let default = vec![
        json!("accept"),
        json!("acceptForSession"),
        json!("decline"),
        json!("cancel"),
    ];
    let (mut options, table) = approval_options(params.available_decisions.as_deref(), default);
    for option in &mut options {
        if option.id == "acceptForSession" {
            option.label = "Allow edits for this session".into();
        }
    }
    let mut detail = Vec::new();
    if let Some(reason) = params.reason.as_deref().filter(|r| !r.is_empty()) {
        detail.push(reason.to_owned());
    }
    if let Some(root) = params.grant_root.as_deref().filter(|r| !r.is_empty()) {
        detail.push(format!(
            "Also grants write access under {root} for this session"
        ));
    }
    let request = InteractionRequest::Approval {
        title: "Apply file changes?".into(),
        detail: (!detail.is_empty()).then(|| detail.join("\n")),
        subject: Subject::FileChange { changes },
        options,
    };
    (request, table)
}

/// Answers an approval from its decision table.
pub fn decision_response(
    table: &DecisionTable,
    resolution: &InteractionResolution,
) -> Result<Value, String> {
    let decision = match resolution {
        InteractionResolution::Approval { option_id, .. } => table
            .iter()
            .find(|(id, _)| id == option_id)
            .map(|(_, d)| d.clone())
            .ok_or_else(|| format!("unknown option {option_id}"))?,
        InteractionResolution::Dismissed => json!("decline"),
        InteractionResolution::Question { .. } => {
            return Err("an approval needs an option, not answers".into());
        }
    };
    Ok(json!({ "decision": decision }))
}

pub fn permissions_approval(params: &PermissionsApprovalParams) -> InteractionRequest {
    let mut lines = Vec::new();
    if let Some(net) = &params.permissions.network
        && net.get("enabled").and_then(Value::as_bool) == Some(true)
    {
        lines.push("Network access".to_owned());
    }
    if let Some(fs) = &params.permissions.file_system {
        for (key, label) in [("read", "Read access"), ("write", "Write access")] {
            let paths: Vec<&str> = fs
                .get(key)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !paths.is_empty() {
                lines.push(format!("{label}: {}", paths.join(", ")));
            }
        }
    }
    if lines.is_empty() {
        lines.push("Additional sandbox permissions".to_owned());
    }
    InteractionRequest::Approval {
        title: "Grant additional permissions?".into(),
        detail: params.reason.clone().filter(|r| !r.is_empty()),
        subject: Subject::Permissions {
            description: lines.join("\n"),
        },
        options: vec![
            ApprovalOption {
                id: "turn".into(),
                label: "Allow for this turn".into(),
                kind: ApprovalOptionKind::AllowOnce,
            },
            ApprovalOption {
                id: "session".into(),
                label: "Allow for this session".into(),
                kind: ApprovalOptionKind::AllowForSession,
            },
            ApprovalOption {
                id: "deny".into(),
                label: "Deny".into(),
                kind: ApprovalOptionKind::Deny,
            },
        ],
    }
}

/// `PermissionsRequestApprovalResponse`: grants exactly what was requested, or nothing.
pub fn permissions_response(
    requested: &RequestedPermissions,
    resolution: &InteractionResolution,
) -> Result<Value, String> {
    let scope = match resolution {
        InteractionResolution::Approval { option_id, .. } => match option_id.as_str() {
            "turn" => Some("turn"),
            "session" => Some("session"),
            "deny" => None,
            other => return Err(format!("unknown option {other}")),
        },
        InteractionResolution::Dismissed => None,
        InteractionResolution::Question { .. } => {
            return Err("a permission request needs an option".into());
        }
    };
    Ok(match scope {
        Some(scope) => {
            let mut granted = Map::new();
            if let Some(n) = requested.network.as_ref().filter(|v| !v.is_null()) {
                granted.insert("network".into(), n.clone());
            }
            if let Some(f) = requested.file_system.as_ref().filter(|v| !v.is_null()) {
                granted.insert("fileSystem".into(), f.clone());
            }
            json!({ "permissions": granted, "scope": scope })
        }
        None => json!({ "permissions": {}, "scope": "turn" }),
    })
}

/// How to translate a question's answers back: question id → choice id → label.
#[derive(Debug, Clone, Default)]
pub struct UserInputTable {
    pub labels: BTreeMap<String, Vec<String>>,
}

pub fn user_input_question(params: &UserInputParams) -> (InteractionRequest, UserInputTable) {
    let mut table = UserInputTable::default();
    let questions = params
        .questions
        .iter()
        .map(|q| {
            let options = q.options.clone().unwrap_or_default();
            table.labels.insert(
                q.id.clone(),
                options.iter().map(|o| o.label.clone()).collect(),
            );
            Question {
                id: q.id.clone(),
                header: (!q.header.is_empty()).then(|| q.header.clone()),
                prompt: q.question.clone(),
                choices: options
                    .iter()
                    .enumerate()
                    .map(|(i, o)| QuestionChoice {
                        id: i.to_string(),
                        label: o.label.clone(),
                        description: (!o.description.is_empty()).then(|| o.description.clone()),
                    })
                    .collect(),
                multi_select: false,
                allow_free_text: q.is_other || q.options.as_ref().is_none_or(|o| o.is_empty()),
                placeholder: q
                    .is_secret
                    .then(|| "secret (sent to the agent as typed)".to_owned()),
            }
        })
        .collect::<Vec<_>>();
    let title = params
        .questions
        .first()
        .map(|q| q.header.clone())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "Codex needs your input".into());
    (InteractionRequest::Question { title, questions }, table)
}

pub fn user_input_response(
    table: &UserInputTable,
    resolution: &InteractionResolution,
) -> Result<Value, String> {
    let mut answers = Map::new();
    match resolution {
        InteractionResolution::Question { answers: given } => {
            for answer in given {
                let labels = table
                    .labels
                    .get(&answer.question_id)
                    .ok_or_else(|| format!("unknown question {}", answer.question_id))?;
                let mut values = Vec::new();
                for choice in &answer.choice_ids {
                    let label = choice
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| labels.get(i))
                        .ok_or_else(|| format!("unknown choice {choice}"))?;
                    values.push(Value::String(label.clone()));
                }
                if let Some(text) = answer.text.as_ref().filter(|t| !t.is_empty()) {
                    values.push(Value::String(text.clone()));
                }
                answers.insert(answer.question_id.clone(), json!({ "answers": values }));
            }
        }
        InteractionResolution::Dismissed => {}
        InteractionResolution::Approval { .. } => {
            return Err("a question needs answers, not an option".into());
        }
    }
    Ok(json!({ "answers": answers }))
}

/// One field of an elicitation form.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text,
    Number { integer: bool },
    Boolean,
    Single { values: Vec<String> },
    Multi { values: Vec<String> },
}

#[derive(Debug, Clone, Default)]
pub struct ElicitationTable {
    /// `form` fields (property name → kind), empty for URL mode.
    pub fields: Vec<(String, FieldKind)>,
    /// URL mode (single accept/decline question).
    pub url_mode: bool,
    /// Free-form JSON answer (`openai/form` whose schema is not a flat object).
    pub raw_json: bool,
}

fn enum_values(schema: &Value) -> Option<Vec<(String, String)>> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let names = schema.get("enumNames").and_then(Value::as_array);
        return Some(
            values
                .iter()
                .enumerate()
                .filter_map(|(i, v)| {
                    let value = v.as_str()?.to_owned();
                    let label = names
                        .and_then(|n| n.get(i))
                        .and_then(Value::as_str)
                        .unwrap_or(&value)
                        .to_owned();
                    Some((value, label))
                })
                .collect(),
        );
    }
    let options = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))?
        .as_array()?;
    Some(
        options
            .iter()
            .filter_map(|o| {
                let value = o.get("const")?.as_str()?.to_owned();
                let label = o
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(&value)
                    .to_owned();
                Some((value, label))
            })
            .collect(),
    )
}

fn field_kind(schema: &Value) -> Option<(FieldKind, Vec<(String, String)>)> {
    match schema.get("type").and_then(Value::as_str)? {
        "string" => Some(match enum_values(schema) {
            Some(values) => (
                FieldKind::Single {
                    values: values.iter().map(|(v, _)| v.clone()).collect(),
                },
                values,
            ),
            None => (FieldKind::Text, Vec::new()),
        }),
        "number" => Some((FieldKind::Number { integer: false }, Vec::new())),
        "integer" => Some((FieldKind::Number { integer: true }, Vec::new())),
        "boolean" => Some((
            FieldKind::Boolean,
            vec![("true".into(), "Yes".into()), ("false".into(), "No".into())],
        )),
        "array" => {
            let values = enum_values(schema.get("items")?)?;
            Some((
                FieldKind::Multi {
                    values: values.iter().map(|(v, _)| v.clone()).collect(),
                },
                values,
            ))
        }
        _ => None,
    }
}

pub fn elicitation_question(params: &ElicitationParams) -> (InteractionRequest, ElicitationTable) {
    let mut table = ElicitationTable::default();
    let title = if params.server_name.is_empty() {
        "An MCP server needs your input".to_owned()
    } else {
        format!("{} needs your input", params.server_name)
    };
    if params.mode == "url" {
        table.url_mode = true;
        let url = params.url.clone().unwrap_or_default();
        let question = Question {
            id: "url".into(),
            header: None,
            prompt: format!("{}\n{url}", params.message).trim().to_owned(),
            choices: vec![
                QuestionChoice {
                    id: "accept".into(),
                    label: "Done — continue".into(),
                    description: None,
                },
                QuestionChoice {
                    id: "decline".into(),
                    label: "Decline".into(),
                    description: None,
                },
            ],
            multi_select: false,
            allow_free_text: false,
            placeholder: None,
        };
        return (
            InteractionRequest::Question {
                title,
                questions: vec![question],
            },
            table,
        );
    }

    let properties = params
        .requested_schema
        .as_ref()
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object);
    let mut questions = Vec::new();
    let mut flat = properties.is_some();
    if let Some(props) = properties {
        for (name, schema) in props {
            let Some((kind, choices)) = field_kind(schema) else {
                flat = false;
                break;
            };
            let title_text = schema.get("title").and_then(Value::as_str);
            let description = schema.get("description").and_then(Value::as_str);
            questions.push(Question {
                id: name.clone(),
                header: title_text.map(str::to_owned),
                prompt: description.or(title_text).unwrap_or(name).to_owned(),
                choices: choices
                    .into_iter()
                    .map(|(value, label)| QuestionChoice {
                        id: value,
                        label,
                        description: None,
                    })
                    .collect(),
                multi_select: matches!(kind, FieldKind::Multi { .. }),
                allow_free_text: matches!(kind, FieldKind::Text | FieldKind::Number { .. }),
                placeholder: match kind {
                    FieldKind::Number { integer: true } => Some("integer".into()),
                    FieldKind::Number { integer: false } => Some("number".into()),
                    _ => None,
                },
            });
            table.fields.push((name.clone(), kind));
        }
    }
    if !flat {
        table.fields.clear();
        table.raw_json = true;
        questions = vec![Question {
            id: "json".into(),
            header: None,
            prompt: format!(
                "{}\nAnswer with a JSON object matching:\n{}",
                params.message,
                params.requested_schema.clone().unwrap_or(Value::Null)
            ),
            choices: Vec::new(),
            multi_select: false,
            allow_free_text: true,
            placeholder: Some("{ … }".into()),
        }];
    } else if !params.message.is_empty() {
        if let Some(first) = questions.first_mut() {
            first.prompt = format!("{}\n{}", params.message, first.prompt);
        } else {
            // A form without fields: a plain confirmation.
            questions.push(Question {
                id: "confirm".into(),
                header: None,
                prompt: params.message.clone(),
                choices: vec![QuestionChoice {
                    id: "accept".into(),
                    label: "Accept".into(),
                    description: None,
                }],
                multi_select: false,
                allow_free_text: false,
                placeholder: None,
            });
        }
    }
    (InteractionRequest::Question { title, questions }, table)
}

pub fn elicitation_response(
    table: &ElicitationTable,
    resolution: &InteractionResolution,
) -> Result<Value, String> {
    let answers = match resolution {
        InteractionResolution::Dismissed => {
            return Ok(json!({"action": "cancel", "content": null, "_meta": null}));
        }
        InteractionResolution::Approval { .. } => return Err("an elicitation needs answers".into()),
        InteractionResolution::Question { answers } => answers,
    };
    let find = |id: &str| answers.iter().find(|a| a.question_id == id);
    if table.url_mode {
        let choice = find("url")
            .and_then(|a| a.choice_ids.first())
            .map(String::as_str)
            .unwrap_or("decline");
        let action = if choice == "accept" {
            "accept"
        } else {
            "decline"
        };
        return Ok(json!({"action": action, "content": null, "_meta": null}));
    }
    if table.raw_json {
        let text = find("json")
            .and_then(|a| a.text.clone())
            .unwrap_or_default();
        let content: Value =
            serde_json::from_str(&text).map_err(|e| format!("answer is not valid JSON: {e}"))?;
        return Ok(json!({"action": "accept", "content": content, "_meta": null}));
    }
    let mut content = Map::new();
    for (name, kind) in &table.fields {
        let Some(answer) = find(name) else { continue };
        let value = match kind {
            FieldKind::Text => match answer.text.as_ref() {
                Some(t) => Value::String(t.clone()),
                None => continue,
            },
            FieldKind::Number { integer } => {
                let Some(t) = answer.text.as_ref().filter(|t| !t.trim().is_empty()) else {
                    continue;
                };
                let t = t.trim();
                if *integer {
                    json!(
                        t.parse::<i64>()
                            .map_err(|_| format!("{name}: `{t}` is not an integer"))?
                    )
                } else {
                    json!(
                        t.parse::<f64>()
                            .map_err(|_| format!("{name}: `{t}` is not a number"))?
                    )
                }
            }
            FieldKind::Boolean => match answer.choice_ids.first().map(String::as_str) {
                Some("true") => Value::Bool(true),
                Some("false") => Value::Bool(false),
                _ => continue,
            },
            FieldKind::Single { values } => match answer.choice_ids.first() {
                Some(v) if values.contains(v) => Value::String(v.clone()),
                Some(v) => return Err(format!("{name}: unknown choice {v}")),
                None => continue,
            },
            FieldKind::Multi { values } => {
                for v in &answer.choice_ids {
                    if !values.contains(v) {
                        return Err(format!("{name}: unknown choice {v}"));
                    }
                }
                json!(answer.choice_ids)
            }
        };
        content.insert(name.clone(), value);
    }
    Ok(json!({"action": "accept", "content": content, "_meta": null}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aas_harness::protocol::QuestionAnswer;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn cwd() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\ws\proj")
        } else {
            PathBuf::from("/ws/proj")
        }
    }

    fn inside(rel: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\ws\proj\{}", rel.replace('/', "\\"))
        } else {
            format!("/ws/proj/{rel}")
        }
    }

    #[test]
    fn user_messages_are_skipped_and_unknown_reported() {
        let item: WireItem =
            serde_json::from_value(json!({"type":"userMessage","id":"u","content":[]})).unwrap();
        assert_eq!(map_item(&item, &cwd(), None), MappedItem::Skip);
        let item: WireItem =
            serde_json::from_value(json!({"type":"somethingNew","id":"x"})).unwrap();
        assert_eq!(map_item(&item, &cwd(), None), MappedItem::Unknown);
    }

    #[test]
    fn command_execution_maps_fields_and_status() {
        let item: WireItem = serde_json::from_value(json!({
            "type":"commandExecution","id":"c1","command":"echo hi","cwd":"C:\\ws","status":"failed",
            "aggregatedOutput":"boom","exitCode":1,"durationMs":796
        }))
        .unwrap();
        let MappedItem::Item { key, body, status } = map_item(&item, &cwd(), None) else {
            panic!()
        };
        assert_eq!(key, "c1");
        assert_eq!(status, ItemStatus::Failed);
        assert_eq!(
            body,
            ItemBody::CommandExecution {
                command: "echo hi".into(),
                cwd: Some("C:\\ws".into()),
                output: "boom".into(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: Some(1),
                duration_ms: Some(796),
            }
        );
    }

    #[test]
    fn added_file_becomes_a_unified_hunk_with_relative_path() {
        let change = WireFileChange {
            path: inside("src/hello.txt"),
            kind: WirePatchKind::Add,
            diff: "hi\nthere\n".into(),
        };
        let fc = file_change(&change, &cwd());
        assert_eq!(fc.path, "src/hello.txt");
        assert_eq!(fc.kind, FileChangeKind::Add);
        assert_eq!(fc.diff.as_deref(), Some("@@ -0,0 +1,2 @@\n+hi\n+there\n"));
        assert_eq!((fc.added, fc.removed), (Some(2), Some(0)));
    }

    #[test]
    fn update_and_move_count_lines() {
        let diff = "@@ -1,2 +1,2 @@\n-old\n+new\n context\n";
        let change = WireFileChange {
            path: inside("a.rs"),
            kind: WirePatchKind::Update {
                move_path: Some(inside("b.rs")),
            },
            diff: diff.into(),
        };
        let fc = file_change(&change, &cwd());
        assert_eq!(fc.kind, FileChangeKind::Move);
        assert_eq!(fc.move_path.as_deref(), Some("b.rs"));
        assert_eq!((fc.added, fc.removed), (Some(1), Some(1)));
        assert_eq!(count_diff_lines("--- a/x\n+++ b/x\n+a\n+b\n-c\n"), (2, 1));
    }

    #[test]
    fn paths_outside_cwd_stay_absolute() {
        let outside = if cfg!(windows) {
            r"D:\other\x.txt"
        } else {
            "/other/x.txt"
        };
        assert_eq!(relative_path(outside, &cwd()), outside);
        if cfg!(windows) {
            assert_eq!(
                relative_path(r"c:\WS\proj\Src\Main.rs", &cwd()),
                "Src/Main.rs"
            );
        }
    }

    #[test]
    fn reasoning_prefers_the_streamed_kind() {
        let summary = vec!["S1".to_owned(), "S2".to_owned()];
        let content = vec!["C1".to_owned()];
        assert_eq!(reasoning_text(&summary, &content, None), "S1\n\nS2");
        assert_eq!(
            reasoning_text(&summary, &content, Some(ReasoningStream::Content)),
            "C1"
        );
        assert_eq!(
            reasoning_text(&[], &content, Some(ReasoningStream::Summary)),
            "C1"
        );
    }

    #[test]
    fn review_and_compaction_items() {
        let item: WireItem =
            serde_json::from_value(json!({"type":"exitedReviewMode","id":"r","review":"LGTM"}))
                .unwrap();
        let MappedItem::Item { body, .. } = map_item(&item, &cwd(), None) else {
            panic!()
        };
        assert_eq!(
            body,
            ItemBody::AgentMessage {
                text: "LGTM".into()
            }
        );
        let item: WireItem =
            serde_json::from_value(json!({"type":"plan","id":"t-plan","text":"# Plan

1. Do it.
"}))
            .unwrap();
        let MappedItem::Item { key, body, .. } = map_item(&item, &cwd(), None) else {
            panic!()
        };
        assert_eq!(key, "t-plan");
        assert_eq!(
            body,
            ItemBody::ProposedPlan {
                text: "# Plan

1. Do it.
"
                .into()
            }
        );
        let item: WireItem =
            serde_json::from_value(json!({"type":"contextCompaction","id":"k"})).unwrap();
        let MappedItem::Item { body, .. } = map_item(&item, &cwd(), None) else {
            panic!()
        };
        assert!(
            matches!(body, ItemBody::Notice { code: Some(ref c), .. } if c == "contextCompacted")
        );
    }

    #[test]
    fn mcp_tool_call_output_from_content() {
        let item: WireItem = serde_json::from_value(json!({
            "type":"mcpToolCall","id":"m","server":"docs","tool":"search","status":"completed",
            "arguments":{"q":"x"},"result":{"content":[{"type":"text","text":"3 results"}],"structuredContent":null,"_meta":null},"error":null
        }))
        .unwrap();
        let MappedItem::Item { body, status, .. } = map_item(&item, &cwd(), None) else {
            panic!()
        };
        assert_eq!(status, ItemStatus::Completed);
        match body {
            ItemBody::ToolCall {
                category,
                title,
                output,
                server,
                ..
            } => {
                assert_eq!(category, ToolCategory::Mcp);
                assert_eq!(title, "docs: search");
                assert_eq!(output.as_deref(), Some("3 results"));
                assert_eq!(server.as_deref(), Some("docs"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn provider_json_errors_are_unwrapped() {
        let err = WireTurnError {
            message: r#"{"error":{"message":"bad model","type":"invalid_request_error"}}"#.into(),
            codex_error_info: Some(json!("other")),
            additional_details: None,
        };
        assert_eq!(
            turn_error(&err),
            TurnError {
                message: "bad model".into(),
                kind: "harnessError".into()
            }
        );
        let err = WireTurnError {
            message: "limit".into(),
            codex_error_info: Some(json!("usageLimitExceeded")),
            additional_details: None,
        };
        assert_eq!(turn_error(&err).kind, "codex:usageLimitExceeded");
        let err = WireTurnError {
            message: "conn".into(),
            codex_error_info: Some(json!({"httpConnectionFailed": {"httpStatusCode": 502}})),
            additional_details: Some("retry later".into()),
        };
        assert_eq!(
            turn_error(&err),
            TurnError {
                message: "conn\nretry later".into(),
                kind: "codex:httpConnectionFailed".into()
            }
        );
    }

    fn usage(total: u64, last: u64) -> ThreadTokenUsage {
        ThreadTokenUsage {
            total: TokenBreakdown {
                total_tokens: total,
                input_tokens: total,
                ..Default::default()
            },
            last: TokenBreakdown {
                total_tokens: last,
                input_tokens: last,
                ..Default::default()
            },
            model_context_window: None,
        }
    }

    #[test]
    fn context_comes_from_the_last_call_and_the_reported_window() {
        let mut t = UsageTracker::default();
        t.start_turn();
        // No window reported: no context.
        assert_eq!(t.observe(&usage(12428, 12428), true).unwrap().context, None);
        let with_window = ThreadTokenUsage {
            model_context_window: Some(996147),
            ..usage(25000, 12572)
        };
        let u = t.observe(&with_window, true).unwrap();
        assert_eq!(
            u.context,
            Some(ContextUsage {
                used_tokens: 12572,
                window_tokens: 996147
            })
        );
        assert_eq!(t.turn_usage().unwrap().context, u.context);
        // A notification for another turn still updates the snapshot of the session.
        let other = ThreadTokenUsage {
            model_context_window: Some(996147),
            ..usage(30000, 5000)
        };
        assert_eq!(t.observe(&other, false), None);
        t.start_turn();
        assert_eq!(
            t.observe(&usage(31000, 1000), true).unwrap().context,
            Some(ContextUsage {
                used_tokens: 5000,
                window_tokens: 996147
            })
        );
        // A zero window is not a window.
        let zero = ThreadTokenUsage {
            model_context_window: Some(0),
            ..usage(32000, 1000)
        };
        assert_eq!(
            t.observe(&zero, true).unwrap().context,
            Some(ContextUsage {
                used_tokens: 5000,
                window_tokens: 996147
            })
        );
    }

    #[test]
    fn usage_tracker_counts_increments_of_the_current_turn() {
        let mut t = UsageTracker::default();
        // Replay of the previous total after resume: baseline only.
        assert_eq!(t.observe(&usage(91216, 13460), false), None);
        t.start_turn();
        assert_eq!(t.turn_usage(), None);
        assert_eq!(
            t.observe(&usage(104000, 12784), true).unwrap().input_tokens,
            12784
        );
        // Duplicate notification (seen on interrupt): no increment.
        assert_eq!(
            t.observe(&usage(104000, 12784), true).unwrap().input_tokens,
            12784
        );
        assert_eq!(
            t.observe(&usage(117000, 13000), true).unwrap().input_tokens,
            25784
        );
        t.start_turn();
        assert_eq!(t.turn_usage(), None);
        // First notification ever without baseline: `last` is the increment.
        let mut fresh = UsageTracker::default();
        fresh.start_turn();
        assert_eq!(
            fresh
                .observe(&usage(12428, 12428), true)
                .unwrap()
                .input_tokens,
            12428
        );
    }

    #[test]
    fn recorded_available_decisions_drive_options_and_decline_is_added() {
        let params: CommandApprovalParams = serde_json::from_value(json!({
            "itemId":"call_1","command":"powershell -Command whoami","cwd":"C:\\ws",
            "proposedExecpolicyAmendment":["whoami"],
            "availableDecisions":["accept",{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["whoami"]}},"cancel"]
        }))
        .unwrap();
        let (req, table) = command_approval(&params);
        let InteractionRequest::Approval {
            options, subject, ..
        } = req
        else {
            panic!()
        };
        let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "accept",
                "acceptWithExecpolicyAmendment",
                "decline",
                "cancel"
            ]
        );
        assert_eq!(options[1].kind, ApprovalOptionKind::AllowAlways);
        assert_eq!(options[3].kind, ApprovalOptionKind::Abort);
        assert!(
            matches!(subject, Subject::Command { ref command, .. } if command == "powershell -Command whoami")
        );
        let resp = decision_response(
            &table,
            &InteractionResolution::Approval {
                option_id: "acceptWithExecpolicyAmendment".into(),
                feedback: None,
            },
        )
        .unwrap();
        assert_eq!(
            resp,
            json!({"decision": {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["whoami"]}}})
        );
        assert_eq!(
            decision_response(&table, &InteractionResolution::Dismissed).unwrap(),
            json!({"decision": "decline"})
        );
        assert!(
            decision_response(
                &table,
                &InteractionResolution::Approval {
                    option_id: "nope".into(),
                    feedback: None
                }
            )
            .is_err()
        );
    }

    #[test]
    fn schema_default_decisions_without_available_list() {
        let params: CommandApprovalParams = serde_json::from_value(json!({
            "command":"curl x","proposedNetworkPolicyAmendments":[{"host":"example.com","action":"allow"}],
            "networkApprovalContext":{"host":"example.com","protocol":"https"},"reason":"needs network"
        }))
        .unwrap();
        let (req, table) = command_approval(&params);
        let InteractionRequest::Approval {
            options, detail, ..
        } = req
        else {
            panic!()
        };
        let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "accept",
                "acceptForSession",
                "network:2",
                "decline",
                "cancel"
            ]
        );
        assert_eq!(
            detail.as_deref(),
            Some("needs network\nNetwork access to example.com (https)")
        );
        let resp = decision_response(
            &table,
            &InteractionResolution::Approval {
                option_id: "network:2".into(),
                feedback: None,
            },
        )
        .unwrap();
        assert_eq!(
            resp,
            json!({"decision": {"applyNetworkPolicyAmendment": {"network_policy_amendment": {"host":"example.com","action":"allow"}}}})
        );
    }

    #[test]
    fn permissions_grant_what_was_requested() {
        let params: PermissionsApprovalParams = serde_json::from_value(json!({
            "itemId":"i","cwd":"C:\\ws","reason":"write outside",
            "permissions":{"network":{"enabled":true},"fileSystem":{"read":null,"write":["C:\\out"]}}
        }))
        .unwrap();
        let InteractionRequest::Approval { subject, .. } = permissions_approval(&params) else {
            panic!()
        };
        assert_eq!(
            subject,
            Subject::Permissions {
                description: "Network access\nWrite access: C:\\out".into()
            }
        );
        let resp = permissions_response(
            &params.permissions,
            &InteractionResolution::Approval {
                option_id: "session".into(),
                feedback: None,
            },
        )
        .unwrap();
        assert_eq!(
            resp,
            json!({"permissions": {"network": {"enabled": true}, "fileSystem": {"read": null, "write": ["C:\\out"]}}, "scope": "session"})
        );
        let resp =
            permissions_response(&params.permissions, &InteractionResolution::Dismissed).unwrap();
        assert_eq!(resp, json!({"permissions": {}, "scope": "turn"}));
    }

    #[test]
    fn user_input_round_trip() {
        let params: UserInputParams = serde_json::from_value(json!({
            "itemId":"i","isBlocking":true,"autoResolutionMs":null,
            "questions":[
                {"id":"db","header":"DB","question":"Which database?","isOther":true,"isSecret":false,
                 "options":[{"label":"SQLite","description":"embedded"},{"label":"Postgres","description":""}]},
                {"id":"name","header":"","question":"Project name?","isOther":false,"isSecret":false,"options":null}
            ]
        }))
        .unwrap();
        let (req, table) = user_input_question(&params);
        let InteractionRequest::Question { title, questions } = req else {
            panic!()
        };
        assert_eq!(title, "DB");
        assert_eq!(questions[0].choices[0].id, "0");
        assert!(questions[0].allow_free_text);
        assert!(questions[1].allow_free_text);
        let resp = user_input_response(
            &table,
            &InteractionResolution::Question {
                answers: vec![
                    QuestionAnswer {
                        question_id: "db".into(),
                        choice_ids: vec!["1".into()],
                        text: None,
                    },
                    QuestionAnswer {
                        question_id: "name".into(),
                        choice_ids: vec![],
                        text: Some("aas".into()),
                    },
                ],
            },
        )
        .unwrap();
        assert_eq!(
            resp,
            json!({"answers": {"db": {"answers": ["Postgres"]}, "name": {"answers": ["aas"]}}})
        );
        assert_eq!(
            user_input_response(&table, &InteractionResolution::Dismissed).unwrap(),
            json!({"answers": {}})
        );
    }

    #[test]
    fn elicitation_form_types() {
        let params: ElicitationParams = serde_json::from_value(json!({
            "threadId":"t","turnId":null,"serverName":"github","mode":"form","_meta":null,"message":"Configure",
            "requestedSchema":{"type":"object","properties":{
                "count":{"type":"integer","title":"Count"},
                "private":{"type":"boolean"},
                "visibility":{"type":"string","enum":["a","b"],"enumNames":["A","B"]},
                "labels":{"type":"array","items":{"anyOf":[{"const":"x","title":"X"},{"const":"y","title":"Y"}]}},
                "title":{"type":"string","description":"PR title"}
            }}
        }))
        .unwrap();
        let (req, table) = elicitation_question(&params);
        let InteractionRequest::Question { title, questions } = req else {
            panic!()
        };
        assert_eq!(title, "github needs your input");
        assert_eq!(questions.len(), 5);
        let resp = elicitation_response(
            &table,
            &InteractionResolution::Question {
                answers: vec![
                    QuestionAnswer {
                        question_id: "count".into(),
                        choice_ids: vec![],
                        text: Some("3".into()),
                    },
                    QuestionAnswer {
                        question_id: "private".into(),
                        choice_ids: vec!["true".into()],
                        text: None,
                    },
                    QuestionAnswer {
                        question_id: "visibility".into(),
                        choice_ids: vec!["b".into()],
                        text: None,
                    },
                    QuestionAnswer {
                        question_id: "labels".into(),
                        choice_ids: vec!["x".into(), "y".into()],
                        text: None,
                    },
                    QuestionAnswer {
                        question_id: "title".into(),
                        choice_ids: vec![],
                        text: Some("Fix".into()),
                    },
                ],
            },
        )
        .unwrap();
        assert_eq!(
            resp,
            json!({"action":"accept","_meta":null,"content":{"count":3,"private":true,"visibility":"b","labels":["x","y"],"title":"Fix"}})
        );
        let bad = elicitation_response(
            &table,
            &InteractionResolution::Question {
                answers: vec![QuestionAnswer {
                    question_id: "count".into(),
                    choice_ids: vec![],
                    text: Some("x".into()),
                }],
            },
        );
        assert!(bad.is_err());
        assert_eq!(
            elicitation_response(&table, &InteractionResolution::Dismissed).unwrap()["action"],
            "cancel"
        );
    }

    #[test]
    fn elicitation_url_and_raw_json_modes() {
        let params: ElicitationParams = serde_json::from_value(json!({
            "serverName":"auth","mode":"url","message":"Sign in","url":"https://x","elicitationId":"e","_meta":null
        }))
        .unwrap();
        let (_, table) = elicitation_question(&params);
        let resp = elicitation_response(
            &table,
            &InteractionResolution::Question {
                answers: vec![QuestionAnswer {
                    question_id: "url".into(),
                    choice_ids: vec!["accept".into()],
                    text: None,
                }],
            },
        )
        .unwrap();
        assert_eq!(resp["action"], "accept");

        let params: ElicitationParams = serde_json::from_value(json!({
            "serverName":"s","mode":"openai/form","message":"m","_meta":null,
            "requestedSchema":{"type":"object","properties":{"nested":{"type":"object"}}}
        }))
        .unwrap();
        let (_, table) = elicitation_question(&params);
        assert!(table.raw_json);
        let resp = elicitation_response(
            &table,
            &InteractionResolution::Question {
                answers: vec![QuestionAnswer {
                    question_id: "json".into(),
                    choice_ids: vec![],
                    text: Some("{\"nested\":{}}".into()),
                }],
            },
        )
        .unwrap();
        assert_eq!(resp["content"], json!({"nested": {}}));
    }

    #[test]
    fn plan_steps_map_statuses() {
        let steps = vec![
            TurnPlanStep {
                step: "a".into(),
                status: "completed".into(),
            },
            TurnPlanStep {
                step: "b".into(),
                status: "inProgress".into(),
            },
            TurnPlanStep {
                step: "c".into(),
                status: "pending".into(),
            },
        ];
        let entries = plan_entries(&steps);
        assert_eq!(entries[0].status, PlanEntryStatus::Completed);
        assert_eq!(entries[1].status, PlanEntryStatus::InProgress);
        assert_eq!(entries[2].status, PlanEntryStatus::Pending);
    }
}
