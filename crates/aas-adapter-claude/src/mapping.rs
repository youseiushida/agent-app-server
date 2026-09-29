//! Pure translation between Claude Code's stream-json / control-protocol shapes and the
//! normalized model of `aas-harness`. Every table here is fixed and documented in
//! `docs/adapters/claude.md`; nothing is inferred from human-readable text.

use aas_harness::protocol::ThreadSettings;
use aas_harness::protocol::{
    ApprovalOption, ApprovalOptionKind, ContextUsage, EffortLevel, ExpireReason, FileChange,
    FileChangeKind, InteractionRequest, InteractionResolution, ItemBody, ItemStatus, Model,
    PermissionMode, PlanEntry, PlanEntryStatus, Question, QuestionChoice, Subject, ToolCategory,
    TurnError, TurnStatus, TurnTrigger, Usage,
};
use aas_harness::{
    BackgroundTaskKind, StatusRow, StatusSection, UpgradedSettings, sanitize_terminal_text,
};
use serde_json::{Map, Value, json};

use crate::background::{Cron, Launch, task_kind};

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

/// How a Claude Code tool is represented in the normalized model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// `Bash`, `PowerShell` → `commandExecution`.
    Command,
    /// `Edit`, `MultiEdit`, `Write`, `NotebookEdit` → `fileChange`.
    FileChange,
    /// `TodoWrite`, `TaskCreate`, `TaskUpdate`, `TaskList` → folded into the turn's `plan` item.
    Plan,
    /// `AskUserQuestion` → `toolCall` (the question itself becomes a `question` interaction).
    Question,
    /// `ExitPlanMode` → `proposedPlan` with its `input.plan` (approval subject `plan`).
    ExitPlan,
    /// Everything else → `toolCall` with the given category.
    Generic(ToolCategory),
}

/// Fixed tool table (see docs/adapters/claude.md §4).
pub fn classify_tool(name: &str) -> ToolClass {
    match name {
        "Bash" | "PowerShell" => ToolClass::Command,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolClass::FileChange,
        "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskList" => ToolClass::Plan,
        "AskUserQuestion" => ToolClass::Question,
        "ExitPlanMode" => ToolClass::ExitPlan,
        "Read"
        | "NotebookRead"
        | "TaskGet"
        | "ReadMcpResourceTool"
        | "ReadMcpResourceDirTool"
        | "ListMcpResourcesTool" => ToolClass::Generic(ToolCategory::Read),
        "Glob" | "Grep" | "LS" | "WebSearch" | "ToolSearch" | "LSP" => {
            ToolClass::Generic(ToolCategory::Search)
        }
        "WebFetch" => ToolClass::Generic(ToolCategory::Fetch),
        "Task" | "Agent" | "Workflow" => ToolClass::Generic(ToolCategory::Subagent),
        "BashOutput" | "KillShell" | "KillBash" | "TaskStop" | "Monitor" => {
            ToolClass::Generic(ToolCategory::Execute)
        }
        "EnterPlanMode" => ToolClass::Generic(ToolCategory::Think),
        n if n.starts_with("mcp__") => ToolClass::Generic(ToolCategory::Mcp),
        _ => ToolClass::Generic(ToolCategory::Other),
    }
}

/// `mcp__<server>__<tool>` → `(server, tool)`.
pub fn split_mcp_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some((server, tool))
}

fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

/// Title of a generic tool call.
pub fn tool_title(name: &str, input: &Value) -> String {
    let with = |verb: &str, key: &str| match str_field(input, key) {
        Some(v) if !v.is_empty() => format!("{verb} {v}"),
        _ => verb.to_owned(),
    };
    match name {
        "Read" => with("Read", "file_path"),
        "NotebookRead" => with("Read", "notebook_path"),
        "Grep" => with("Grep", "pattern"),
        "Glob" => with("Glob", "pattern"),
        "LS" => with("List", "path"),
        "WebFetch" => with("Fetch", "url"),
        "WebSearch" => with("Search", "query"),
        "Task" | "Agent" => {
            let agent = str_field(input, "subagent_type").unwrap_or("Agent");
            match str_field(input, "description") {
                Some(d) if !d.is_empty() => format!("{agent}: {d}"),
                _ => agent.to_owned(),
            }
        }
        "AskUserQuestion" => {
            let first = input
                .get("questions")
                .and_then(Value::as_array)
                .and_then(|q| q.first())
                .and_then(|q| str_field(q, "question"));
            match first {
                Some(q) => format!("Question: {q}"),
                None => "Question".to_owned(),
            }
        }
        "ExitPlanMode" => "Plan ready for approval".to_owned(),
        n => match split_mcp_name(n) {
            Some((server, tool)) => format!("{server}: {tool}"),
            None => n.to_owned(),
        },
    }
}

/// Unified-diff style rendering of a string replacement requested by `Edit`
/// (no line numbers: they are only known once the tool ran).
pub fn replacement_diff(old: &str, new: &str) -> (String, u64, u64) {
    let mut out = String::from("@@\n");
    let mut removed = 0;
    let mut added = 0;
    for line in old.lines() {
        out.push('-');
        out.push_str(line);
        out.push('\n');
        removed += 1;
    }
    for line in new.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
        added += 1;
    }
    (out, added, removed)
}

/// Diff of a newly created file.
pub fn new_file_diff(content: &str) -> (String, u64) {
    let count = content.lines().count() as u64;
    let mut out = format!("@@ -0,0 +1,{count} @@\n");
    for line in content.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    (out, count)
}

/// Renders Claude Code's `structuredPatch` (list of hunks with `oldStart`, `oldLines`,
/// `newStart`, `newLines`, `lines`) as unified-diff hunks.
pub fn structured_patch_diff(patch: &Value) -> Option<(String, u64, u64)> {
    let hunks = patch.as_array()?;
    if hunks.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut added = 0;
    let mut removed = 0;
    for hunk in hunks {
        let n = |k: &str| hunk.get(k).and_then(Value::as_u64).unwrap_or(0);
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            n("oldStart"),
            n("oldLines"),
            n("newStart"),
            n("newLines")
        ));
        for line in hunk
            .get("lines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(line) = line.as_str() else { continue };
            if line.starts_with('+') {
                added += 1;
            } else if line.starts_with('-') {
                removed += 1;
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    Some((out, added, removed))
}

/// File changes requested by a file tool's input (before it runs).
pub fn requested_file_changes(name: &str, input: &Value) -> Vec<FileChange> {
    match name {
        "Write" => {
            let path = str_field(input, "file_path").unwrap_or_default().to_owned();
            let content = str_field(input, "content").unwrap_or_default();
            let (diff, added) = new_file_diff(content);
            // Whether the file already exists is only known from the tool result; until then
            // the change is reported as an update of the path.
            vec![FileChange {
                path,
                kind: FileChangeKind::Update,
                move_path: None,
                diff: Some(diff),
                added: Some(added),
                removed: None,
            }]
        }
        "Edit" => {
            let path = str_field(input, "file_path").unwrap_or_default().to_owned();
            let (diff, added, removed) = replacement_diff(
                str_field(input, "old_string").unwrap_or_default(),
                str_field(input, "new_string").unwrap_or_default(),
            );
            vec![FileChange {
                path,
                kind: FileChangeKind::Update,
                move_path: None,
                diff: Some(diff),
                added: Some(added),
                removed: Some(removed),
            }]
        }
        "MultiEdit" => {
            let path = str_field(input, "file_path").unwrap_or_default().to_owned();
            let mut diff = String::new();
            let mut added = 0;
            let mut removed = 0;
            for edit in input
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let (d, a, r) = replacement_diff(
                    str_field(edit, "old_string").unwrap_or_default(),
                    str_field(edit, "new_string").unwrap_or_default(),
                );
                diff.push_str(&d);
                added += a;
                removed += r;
            }
            vec![FileChange {
                path,
                kind: FileChangeKind::Update,
                move_path: None,
                diff: Some(diff),
                added: Some(added),
                removed: Some(removed),
            }]
        }
        "NotebookEdit" => vec![FileChange {
            path: str_field(input, "notebook_path")
                .unwrap_or_default()
                .to_owned(),
            kind: FileChangeKind::Update,
            move_path: None,
            diff: None,
            added: None,
            removed: None,
        }],
        _ => Vec::new(),
    }
}

/// Body of the item created when the tool call is announced (`None` for plan tools, which
/// are folded into the plan item).
pub fn tool_started_body(name: &str, input: &Value) -> Option<ItemBody> {
    Some(match classify_tool(name) {
        ToolClass::Plan => return None,
        ToolClass::Command => ItemBody::CommandExecution {
            command: str_field(input, "command").unwrap_or_default().to_owned(),
            cwd: None,
            output: String::new(),
            output_truncated: false,
            output_blob_id: None,
            exit_code: None,
            duration_ms: None,
        },
        ToolClass::FileChange => ItemBody::FileChange {
            changes: requested_file_changes(name, input),
        },
        ToolClass::Question => generic_body(ToolCategory::Other, name, input),
        // The plan the agent presents for approval (plan mode): the tool's own `plan` text.
        ToolClass::ExitPlan => ItemBody::ProposedPlan {
            text: str_field(input, "plan").unwrap_or_default().to_owned(),
        },
        ToolClass::Generic(category) => generic_body(category, name, input),
    })
}

fn generic_body(category: ToolCategory, name: &str, input: &Value) -> ItemBody {
    ItemBody::ToolCall {
        category,
        name: name.to_owned(),
        title: tool_title(name, input),
        server: split_mcp_name(name).map(|(s, _)| s.to_owned()),
        input: Some(input.clone()),
        output: None,
        output_truncated: false,
        output_blob_id: None,
    }
}

/// The `tool_result` of a tool call, as delivered in a `user` message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolResult {
    /// Text of the `tool_result.content` (string, or the text blocks joined by newlines).
    pub text: String,
    pub is_error: bool,
    /// The structured `tool_use_result` of the enclosing user message.
    pub structured: Option<Value>,
    /// We answered this tool's permission request with a denial.
    pub denied_by_user: bool,
}

/// Text of a `tool_result.content` value.
pub fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Final body and status of a tool item once its result arrived.
pub fn tool_completed(
    name: &str,
    input: &Value,
    started: &ItemBody,
    result: &ToolResult,
) -> (ItemBody, ItemStatus) {
    let structured = result.structured.as_ref().filter(|v| v.is_object());
    let interrupted = structured
        .and_then(|s| s.get("interrupted"))
        .and_then(Value::as_bool)
        == Some(true);
    let status = if result.denied_by_user {
        ItemStatus::Declined
    } else if interrupted {
        ItemStatus::Interrupted
    } else if result.is_error {
        ItemStatus::Failed
    } else {
        ItemStatus::Completed
    };
    let body = match started.clone() {
        ItemBody::CommandExecution { command, cwd, .. } => {
            let output = match structured {
                Some(s) if s.get("stdout").is_some() || s.get("stderr").is_some() => {
                    let stdout = str_field(s, "stdout").unwrap_or_default();
                    let stderr = str_field(s, "stderr").unwrap_or_default();
                    match (stdout.is_empty(), stderr.is_empty()) {
                        (_, true) => stdout.to_owned(),
                        (true, false) => stderr.to_owned(),
                        (false, false) => format!("{stdout}\n{stderr}"),
                    }
                }
                _ => result.text.clone(),
            };
            ItemBody::CommandExecution {
                command,
                cwd,
                output,
                output_truncated: false,
                output_blob_id: None,
                exit_code: None,
                duration_ms: None,
            }
        }
        ItemBody::FileChange { changes } => ItemBody::FileChange {
            changes: completed_file_changes(name, input, changes, structured),
        },
        ItemBody::ToolCall {
            category,
            name: tool_name,
            title,
            server,
            input,
            ..
        } => ItemBody::ToolCall {
            category,
            // A workflow is known by the name its script gives it, which only the launch
            // result carries (`workflowName`).
            title: match structured.and_then(|s| str_field(s, "workflowName")) {
                Some(workflow) if name == "Workflow" && !workflow.is_empty() => {
                    format!("Workflow: {workflow}")
                }
                _ => title,
            },
            name: tool_name,
            server,
            input,
            output: Some(result.text.clone()),
            output_truncated: false,
            output_blob_id: None,
        },
        other => other,
    };
    (body, status)
}

/// What a finished tool call did to the session's background work, from the fields of its
/// structured result (`tool_use_result`) only.
#[derive(Debug, Clone, PartialEq)]
pub enum BackgroundEffect {
    /// The tool left work running as a background task: `status: "async_launched"` with
    /// `agentId` (Agent) or `taskId` (Workflow, with `taskType`), `status: "remote_launched"`
    /// with `taskId`, or `backgroundTaskId` (Bash, PowerShell run in the background).
    Task(Launch),
    /// `CronCreate` scheduled a wakeup (`{id, humanSchedule, recurring, durable}`).
    Cron(Cron),
    /// `CronDelete` cancelled the wakeup with this id (`{id}`).
    CronDeleted(String),
    /// `CronList` listed every pending wakeup (`{jobs: [...]}`).
    CronList(Vec<Cron>),
}

/// See [`BackgroundEffect`]. A failed or denied call has none.
pub fn background_effect(
    name: &str,
    input: &Value,
    result: &ToolResult,
) -> Option<BackgroundEffect> {
    if result.is_error || result.denied_by_user {
        return None;
    }
    let s = result.structured.as_ref().filter(|v| v.is_object())?;
    match name {
        "CronCreate" => {
            let id = str_field(s, "id")?;
            let human = str_field(s, "humanSchedule");
            return Some(BackgroundEffect::Cron(Cron {
                id: id.to_owned(),
                prompt: str_field(input, "prompt").unwrap_or_default().to_owned(),
                schedule: human
                    .or_else(|| str_field(input, "cron"))
                    .map(str::to_owned),
                human_schedule: human.is_some(),
                recurring: s
                    .get("recurring")
                    .or_else(|| input.get("recurring"))
                    .and_then(Value::as_bool)
                    == Some(true),
            }));
        }
        "CronDelete" => {
            return str_field(s, "id").map(|id| BackgroundEffect::CronDeleted(id.to_owned()));
        }
        "CronList" => {
            let jobs = s.get("jobs")?.as_array()?;
            return Some(BackgroundEffect::CronList(
                jobs.iter().filter_map(Cron::from_entry).collect(),
            ));
        }
        _ => {}
    }
    let title = |fallback: &str| {
        str_field(s, "description")
            .or_else(|| str_field(s, "summary"))
            .or_else(|| str_field(input, "description"))
            .unwrap_or(fallback)
            .to_owned()
    };
    let launch = match str_field(s, "status") {
        Some("async_launched") => {
            let id = str_field(s, "agentId").or_else(|| str_field(s, "taskId"))?;
            let kind = match str_field(s, "taskType") {
                Some(task_type) => task_kind(task_type),
                None if name == "Workflow" => BackgroundTaskKind::Workflow,
                None => BackgroundTaskKind::Agent,
            };
            Launch {
                task_id: id.to_owned(),
                kind,
                title: title(name),
            }
        }
        Some("remote_launched") => Launch {
            task_id: str_field(s, "taskId")?.to_owned(),
            kind: BackgroundTaskKind::Remote,
            title: title(name),
        },
        _ => {
            let id = str_field(s, "backgroundTaskId")?;
            Launch {
                task_id: id.to_owned(),
                kind: match classify_tool(name) {
                    ToolClass::Command => BackgroundTaskKind::Shell,
                    _ if name == "Monitor" => BackgroundTaskKind::Monitor,
                    _ => BackgroundTaskKind::Other,
                },
                title: title(str_field(input, "command").unwrap_or(name)),
            }
        }
    };
    Some(BackgroundEffect::Task(launch))
}

fn completed_file_changes(
    name: &str,
    input: &Value,
    requested: Vec<FileChange>,
    structured: Option<&Value>,
) -> Vec<FileChange> {
    let Some(s) = structured else {
        return requested;
    };
    let path = str_field(s, "filePath").map(str::to_owned);
    let created = str_field(s, "type") == Some("create");
    let patch = s.get("structuredPatch").and_then(structured_patch_diff);
    let mut changes = requested;
    if let Some(change) = changes.first_mut() {
        if let Some(path) = path {
            change.path = path;
        }
        if created {
            change.kind = FileChangeKind::Add;
            let content = str_field(s, "content")
                .or_else(|| str_field(input, "content"))
                .unwrap_or_default();
            let (diff, added) = new_file_diff(content);
            change.diff = Some(diff);
            change.added = Some(added);
            change.removed = Some(0);
        } else if let Some((diff, added, removed)) = patch {
            change.diff = Some(diff);
            change.added = Some(added);
            change.removed = Some(removed);
        } else if name == "Write" {
            // Overwrite without a structured patch: keep the requested content diff.
        }
    }
    changes
}

// ---------------------------------------------------------------------------------------------
// Plan (TodoWrite / Task* tools)
// ---------------------------------------------------------------------------------------------

/// The session's task list, maintained from the plan tools' inputs and results.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskList {
    tasks: Vec<Task>,
}

#[derive(Debug, Clone, PartialEq)]
struct Task {
    id: Option<String>,
    subject: String,
    status: PlanEntryStatus,
}

fn plan_status(s: &str) -> Option<PlanEntryStatus> {
    Some(match s {
        "pending" => PlanEntryStatus::Pending,
        "in_progress" => PlanEntryStatus::InProgress,
        "completed" => PlanEntryStatus::Completed,
        _ => return None,
    })
}

impl TaskList {
    /// Applies a successful plan tool call. Returns whether the list changed.
    pub fn apply(&mut self, name: &str, input: &Value, result: &ToolResult) -> bool {
        if result.is_error || result.denied_by_user {
            return false;
        }
        let structured = result.structured.as_ref();
        let before = self.clone();
        match name {
            "TodoWrite" => {
                let Some(todos) = input.get("todos").and_then(Value::as_array) else {
                    return false;
                };
                self.tasks = todos
                    .iter()
                    .map(|t| Task {
                        id: None,
                        subject: str_field(t, "content").unwrap_or_default().to_owned(),
                        status: str_field(t, "status")
                            .and_then(plan_status)
                            .unwrap_or(PlanEntryStatus::Pending),
                    })
                    .collect();
            }
            "TaskCreate" => {
                let task = structured.and_then(|s| s.get("task"));
                let id = task.and_then(|t| str_field(t, "id")).map(str::to_owned);
                let subject = task
                    .and_then(|t| str_field(t, "subject"))
                    .or_else(|| str_field(input, "subject"))
                    .unwrap_or_default()
                    .to_owned();
                self.tasks.push(Task {
                    id,
                    subject,
                    status: PlanEntryStatus::Pending,
                });
            }
            "TaskUpdate" => {
                if structured
                    .and_then(|s| s.get("success"))
                    .and_then(Value::as_bool)
                    == Some(false)
                {
                    return false;
                }
                let Some(id) = str_field(input, "taskId") else {
                    return false;
                };
                if str_field(input, "status") == Some("deleted") {
                    self.tasks.retain(|t| t.id.as_deref() != Some(id));
                } else if let Some(task) =
                    self.tasks.iter_mut().find(|t| t.id.as_deref() == Some(id))
                {
                    if let Some(status) = str_field(input, "status").and_then(plan_status) {
                        task.status = status;
                    }
                    if let Some(subject) = str_field(input, "subject") {
                        task.subject = subject.to_owned();
                    }
                }
            }
            "TaskList" => {
                let Some(tasks) = structured
                    .and_then(|s| s.get("tasks"))
                    .and_then(Value::as_array)
                else {
                    return false;
                };
                self.tasks = tasks
                    .iter()
                    .map(|t| Task {
                        id: str_field(t, "id").map(str::to_owned),
                        subject: str_field(t, "subject").unwrap_or_default().to_owned(),
                        status: str_field(t, "status")
                            .and_then(plan_status)
                            .unwrap_or(PlanEntryStatus::Pending),
                    })
                    .collect();
            }
            _ => return false,
        }
        *self != before
    }

    pub fn entries(&self) -> Vec<PlanEntry> {
        self.tasks
            .iter()
            .map(|t| PlanEntry {
                text: t.subject.clone(),
                status: t.status,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Permission requests (can_use_tool)
// ---------------------------------------------------------------------------------------------

pub const OPT_ALLOW: &str = "allow";
pub const OPT_ALLOW_SESSION: &str = "allow_session";
pub const OPT_ALLOW_ALWAYS: &str = "allow_always";
pub const OPT_DENY: &str = "deny";
pub const OPT_DENY_FEEDBACK: &str = "deny_feedback";
pub const OPT_ABORT: &str = "abort";

/// A pending `can_use_tool` request, kept to build the response.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionAsk {
    pub tool_name: String,
    pub tool_use_id: Option<String>,
    pub input: Value,
    pub suggestions: Vec<Value>,
    pub suppress_always_allow: bool,
}

impl PermissionAsk {
    pub fn from_request(request: &Value) -> Self {
        Self {
            tool_name: str_field(request, "tool_name")
                .unwrap_or_default()
                .to_owned(),
            tool_use_id: str_field(request, "tool_use_id").map(str::to_owned),
            input: request.get("input").cloned().unwrap_or_else(|| json!({})),
            suggestions: request
                .get("permission_suggestions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            suppress_always_allow: request
                .get("suppress_always_allow_rule")
                .and_then(Value::as_bool)
                == Some(true),
        }
    }

    pub fn is_question(&self) -> bool {
        self.tool_name == "AskUserQuestion"
    }

    fn has_persistent_suggestion(&self) -> bool {
        self.suggestions
            .iter()
            .any(|s| str_field(s, "destination").is_some_and(|d| d != "session"))
    }
}

/// The interaction shown for a `can_use_tool` request. The CLI's texts may carry terminal
/// escape sequences (`decision_reason`), which are removed ([`sanitize_terminal_text`]).
pub fn permission_interaction(request: &Value, ask: &PermissionAsk) -> InteractionRequest {
    if ask.is_question()
        && let Some(q) = question_interaction(&ask.input)
    {
        return q;
    }
    let name = ask.tool_name.as_str();
    let display = str_field(request, "display_name").unwrap_or(name);
    let subject = match classify_tool(name) {
        ToolClass::Command => Subject::Command {
            command: str_field(&ask.input, "command")
                .unwrap_or_default()
                .to_owned(),
            cwd: None,
        },
        ToolClass::FileChange => Subject::FileChange {
            changes: requested_file_changes(name, &ask.input),
        },
        ToolClass::ExitPlan => Subject::Plan {
            text: str_field(&ask.input, "plan").unwrap_or_default().to_owned(),
        },
        _ => Subject::Tool {
            name: name.to_owned(),
            input: Some(ask.input.clone()),
        },
    };
    let title = match str_field(request, "title") {
        Some(t) if !t.is_empty() => sanitize_terminal_text(t),
        _ => match &subject {
            Subject::Command { .. } => "Run command?".to_owned(),
            Subject::FileChange { changes } => match (name, changes.first()) {
                ("Write", Some(c)) => format!("Write {}?", c.path),
                (_, Some(c)) => format!("Edit {}?", c.path),
                _ => format!("Use {display}?"),
            },
            Subject::Plan { .. } => "Approve the plan?".to_owned(),
            _ => format!("Use {display}?"),
        },
    };
    let mut detail_parts = Vec::new();
    if let Some(d) = str_field(request, "description").filter(|d| !d.is_empty()) {
        detail_parts.push(sanitize_terminal_text(d));
    }
    if let Some(r) = str_field(request, "decision_reason").filter(|d| !d.is_empty()) {
        detail_parts.push(sanitize_terminal_text(r));
    }
    if let Some(p) = str_field(request, "blocked_path").filter(|d| !d.is_empty()) {
        detail_parts.push(format!("Path: {p}"));
    }
    let detail = (!detail_parts.is_empty()).then(|| detail_parts.join("\n"));

    let mut options = Vec::new();
    let allow = ApprovalOption {
        id: OPT_ALLOW.into(),
        label: "Allow".into(),
        kind: ApprovalOptionKind::AllowOnce,
    };
    let deny = ApprovalOption {
        id: OPT_DENY.into(),
        label: "Deny".into(),
        kind: ApprovalOptionKind::Deny,
    };
    let default_to_no = request.get("default_to_no").and_then(Value::as_bool) == Some(true);
    if default_to_no {
        options.push(deny.clone());
    }
    options.push(allow);
    if !ask.suggestions.is_empty() {
        options.push(ApprovalOption {
            id: OPT_ALLOW_SESSION.into(),
            label: "Allow for this session".into(),
            kind: ApprovalOptionKind::AllowForSession,
        });
        if ask.has_persistent_suggestion() && !ask.suppress_always_allow {
            options.push(ApprovalOption {
                id: OPT_ALLOW_ALWAYS.into(),
                label: "Always allow".into(),
                kind: ApprovalOptionKind::AllowAlways,
            });
        }
    }
    if !default_to_no {
        options.push(deny);
    }
    options.push(ApprovalOption {
        id: OPT_DENY_FEEDBACK.into(),
        label: "Deny with feedback".into(),
        kind: ApprovalOptionKind::DenyWithFeedback,
    });
    options.push(ApprovalOption {
        id: OPT_ABORT.into(),
        label: "Deny and stop".into(),
        kind: ApprovalOptionKind::Abort,
    });
    InteractionRequest::Approval {
        title,
        detail,
        subject,
        options,
    }
}

/// Response body (`control_response.response.response`) for a resolved permission request.
/// Returns the body and whether the tool was denied.
pub fn permission_response(
    ask: &PermissionAsk,
    resolution: &InteractionResolution,
) -> Result<(Value, bool), String> {
    if ask.is_question() {
        return Ok(question_response(&ask.input, resolution));
    }
    let deny = |message: &str, interrupt: bool| {
        let mut v = json!({ "behavior": "deny", "message": message });
        if interrupt {
            v["interrupt"] = json!(true);
        }
        (v, true)
    };
    match resolution {
        InteractionResolution::Approval {
            option_id,
            feedback,
        } => match option_id.as_str() {
            OPT_ALLOW => Ok((
                json!({ "behavior": "allow", "updatedInput": ask.input }),
                false,
            )),
            OPT_ALLOW_SESSION => {
                let updates: Vec<Value> = ask
                    .suggestions
                    .iter()
                    .map(|s| {
                        let mut s = s.clone();
                        if let Some(obj) = s.as_object_mut() {
                            obj.insert("destination".into(), json!("session"));
                        }
                        s
                    })
                    .collect();
                Ok((
                    json!({ "behavior": "allow", "updatedInput": ask.input, "updatedPermissions": updates }),
                    false,
                ))
            }
            OPT_ALLOW_ALWAYS => Ok((
                json!({ "behavior": "allow", "updatedInput": ask.input, "updatedPermissions": ask.suggestions }),
                false,
            )),
            OPT_DENY => Ok(deny("The user denied this action.", false)),
            OPT_DENY_FEEDBACK => {
                let text = feedback.as_deref().map(str::trim).filter(|f| !f.is_empty());
                Ok(deny(text.unwrap_or("The user denied this action."), false))
            }
            OPT_ABORT => Ok(deny(
                "The user denied this action and stopped the turn.",
                true,
            )),
            other => Err(format!("unknown approval option {other}")),
        },
        InteractionResolution::Dismissed => Ok(deny("The user dismissed the request.", false)),
        InteractionResolution::Question { .. } => {
            Err("a question answer was given to a permission request".into())
        }
    }
}

/// The answer to a permission request or question that expired unanswered (the engine calls
/// `expire_request`): a denial whose message tells the model why.
pub fn expiry_response(reason: ExpireReason) -> Value {
    let message = match reason {
        ExpireReason::TurnEnded => {
            "The request expired unanswered: the turn it belonged to ended before the user answered."
        }
        ExpireReason::TaskEnded => {
            "The request expired unanswered: the background task that asked ended before the user answered."
        }
        ExpireReason::HarnessCancelled
        | ExpireReason::ProcessExited
        | ExpireReason::DaemonRestarted => "The request expired before the user answered.",
    };
    json!({ "behavior": "deny", "message": message })
}

// ---------------------------------------------------------------------------------------------
// AskUserQuestion
// ---------------------------------------------------------------------------------------------

/// The `question` interaction for an `AskUserQuestion` input. Question ids are `q<index>`,
/// choice ids `c<index>`.
pub fn question_interaction(input: &Value) -> Option<InteractionRequest> {
    let questions = input.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    let questions: Vec<Question> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| Question {
            id: format!("q{i}"),
            header: str_field(q, "header").map(str::to_owned),
            prompt: str_field(q, "question").unwrap_or_default().to_owned(),
            choices: q
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(j, o)| QuestionChoice {
                    id: format!("c{j}"),
                    label: str_field(o, "label").unwrap_or_default().to_owned(),
                    description: str_field(o, "description").map(str::to_owned),
                })
                .collect(),
            multi_select: q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            // Claude Code always offers "Other" (free text) in addition to the options.
            allow_free_text: true,
            placeholder: None,
        })
        .collect();
    let title = if questions.len() == 1 {
        questions[0].prompt.clone()
    } else {
        format!("{} questions from Claude", questions.len())
    };
    Some(InteractionRequest::Question { title, questions })
}

/// `allow` with `updatedInput.answers` (question text → answer; multi-select answers joined
/// with `, `; free text appended), as the Agent SDK documents for `AskUserQuestion`.
pub fn question_response(input: &Value, resolution: &InteractionResolution) -> (Value, bool) {
    let InteractionResolution::Question { answers } = resolution else {
        return (
            json!({ "behavior": "deny", "message": "The user dismissed the question." }),
            true,
        );
    };
    let questions = input
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut map = Map::new();
    for (i, q) in questions.iter().enumerate() {
        let qid = format!("q{i}");
        let Some(answer) = answers.iter().find(|a| a.question_id == qid) else {
            continue;
        };
        let options = q
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut parts: Vec<String> = answer
            .choice_ids
            .iter()
            .filter_map(|cid| cid.strip_prefix('c').and_then(|n| n.parse::<usize>().ok()))
            .filter_map(|j| options.get(j))
            .filter_map(|o| str_field(o, "label").map(str::to_owned))
            .collect();
        if let Some(text) = answer
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            parts.push(text.to_owned());
        }
        if parts.is_empty() {
            continue;
        }
        let key = str_field(q, "question").unwrap_or_default().to_owned();
        map.insert(key, Value::String(parts.join(", ")));
    }
    let mut updated = input.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("answers".into(), Value::Object(map));
    }
    (
        json!({ "behavior": "allow", "updatedInput": updated }),
        false,
    )
}

// ---------------------------------------------------------------------------------------------
// Results, usage
// ---------------------------------------------------------------------------------------------

/// `terminal_reason` values that mean the turn was aborted.
const ABORTED_TERMINAL_REASONS: &[&str] = &["aborted_streaming", "aborted_tools"];

/// Per-turn usage from a `result` message. Claude reports input tokens without cache reads
/// and cache writes; the normalized `inputTokens` is their sum and `cachedInputTokens` the
/// cache reads. `costUsd` is the difference of the process-cumulative `total_cost_usd`.
pub fn usage_from_result(result: &Value, cost_delta: Option<f64>) -> Option<Usage> {
    let u = result.get("usage")?;
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let input = n(u, "input_tokens")
        + n(u, "cache_creation_input_tokens")
        + n(u, "cache_read_input_tokens");
    Some(Usage {
        input_tokens: input,
        output_tokens: n(u, "output_tokens"),
        cached_input_tokens: n(u, "cache_read_input_tokens"),
        reasoning_tokens: u
            .get("output_tokens_details")
            .map(|d| n(d, "thinking_tokens"))
            .unwrap_or(0),
        cost_usd: cost_delta,
        context: None,
    })
}

/// Context-window occupancy from the response of the `get_context_usage` control request:
/// `totalTokens` out of `rawMaxTokens`, the two numbers Claude Code's `/context` shows.
/// `None` when either is missing or the window is zero.
pub fn context_from_usage_response(response: &Value) -> Option<ContextUsage> {
    let used = response.get("totalTokens").and_then(Value::as_u64)?;
    let window = response
        .get("rawMaxTokens")
        .and_then(Value::as_u64)
        .filter(|w| *w > 0)?;
    Some(ContextUsage {
        used_tokens: used,
        window_tokens: window,
    })
}

/// The text of a failed `result` that arrives before any turn: the CLI refused to start the
/// session (an unknown `--resume-session-at` anchor: "No message found with message.uuid of:
/// …", recording g6). Its `errors`, else its `result`, else its `subtype`; `None` for a result
/// that is not an error.
pub fn startup_error(result: &Value) -> Option<String> {
    if result.get("is_error").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let errors: Vec<&str> = result
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|e| !e.is_empty())
        .collect();
    let text = if !errors.is_empty() {
        errors.join("; ")
    } else if let Some(r) = str_field(result, "result").filter(|r| !r.is_empty()) {
        r.to_owned()
    } else {
        str_field(result, "subtype").unwrap_or("error").to_owned()
    };
    Some(sanitize_terminal_text(&text))
}

// ---------------------------------------------------------------------------------------------
// Status (`get_status`, `get_usage`, `get_plan`)
// ---------------------------------------------------------------------------------------------

/// The sections of a `get_status` response (`@internal` in Claude Code 2.1.284: "the rows of
/// the terminal's /status screen …, every value already rendered as text"), verbatim.
pub fn status_sections(response: &Value) -> Vec<StatusSection> {
    response
        .get("sections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|section| {
            let title = str_field(section, "title")?.to_owned();
            let rows = section
                .get("rows")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|row| {
                    Some(StatusRow {
                        label: str_field(row, "label")?.to_owned(),
                        value: match row.get("value") {
                            Some(Value::String(v)) => v.clone(),
                            Some(Value::Null) | None => String::new(),
                            Some(other) => other.to_string(),
                        },
                    })
                })
                .collect();
            Some(StatusSection { title, rows })
        })
        .collect()
}

/// Label of an entry of `get_usage.rate_limits.limits` by its `kind` (the windows Claude Code's
/// `/usage` screen shows; another kind is shown by its own name), with the model of a scoped
/// limit (`scope.model.display_name`).
fn limit_label(limit: &Value) -> String {
    let kind = str_field(limit, "kind").unwrap_or("limit");
    let model = limit
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str);
    match (kind, model) {
        ("session", _) => "Current session".to_owned(),
        ("weekly_all", _) => "Current week (all models)".to_owned(),
        ("weekly_scoped", Some(model)) => format!("Current week ({model})"),
        (kind, Some(model)) => format!("{kind} ({model})"),
        (kind, None) => kind.to_owned(),
    }
}

fn number(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The sections of a `get_usage` response (Claude Code 2.1.284: "Experimental — the response
/// shape may change"; requested with `skip_behaviors: true`, so `behaviors` is null):
/// * "Plan usage": the plan (`subscription_type`), each entry of the normalised
///   `rate_limits.limits` (`percent`, `resets_at`, `severity` when not normal) and the usage
///   credits (`rate_limits.spend`: `percent` when enabled, else `disabled_reason`);
/// * "Session usage" (`with_session`): the process's cost, durations, changed lines and tokens
///   per model (`session`). Left out for the status without a session (a probe process has no
///   conversation).
pub fn usage_sections(response: &Value, with_session: bool) -> Vec<StatusSection> {
    let mut out = Vec::new();
    let mut plan = Vec::new();
    if let Some(kind) = str_field(response, "subscription_type") {
        plan.push(StatusRow {
            label: "Plan".into(),
            value: kind.to_owned(),
        });
    }
    if response
        .get("rate_limits_available")
        .and_then(Value::as_bool)
        == Some(false)
    {
        plan.push(StatusRow {
            label: "Rate limits".into(),
            value: "not available".into(),
        });
    }
    let limits = response
        .pointer("/rate_limits/limits")
        .and_then(Value::as_array);
    for limit in limits.into_iter().flatten() {
        let mut value = match limit.get("percent").and_then(number) {
            Some(p) => format!("{p}% used"),
            None => "usage not reported".to_owned(),
        };
        if let Some(resets) = str_field(limit, "resets_at") {
            value.push_str(&format!(", resets {resets}"));
        }
        if let Some(severity) = str_field(limit, "severity").filter(|s| *s != "normal") {
            value.push_str(&format!(" ({severity})"));
        }
        plan.push(StatusRow {
            label: limit_label(limit),
            value,
        });
    }
    if let Some(spend) = response
        .pointer("/rate_limits/spend")
        .filter(|s| s.is_object())
    {
        let value = if spend.get("enabled").and_then(Value::as_bool) == Some(true) {
            match spend.get("percent").and_then(number) {
                Some(p) => format!("on, {p}% used"),
                None => "on".to_owned(),
            }
        } else {
            match str_field(spend, "disabled_reason") {
                Some(reason) => format!("off ({reason})"),
                None => "off".to_owned(),
            }
        };
        plan.push(StatusRow {
            label: "Usage credits".into(),
            value,
        });
    }
    if !plan.is_empty() {
        out.push(StatusSection {
            title: "Plan usage".into(),
            rows: plan,
        });
    }
    let session = response.get("session").filter(|s| s.is_object());
    if let (true, Some(session)) = (with_session, session) {
        let mut rows = Vec::new();
        if let Some(cost) = session.get("total_cost_usd").and_then(Value::as_f64) {
            rows.push(StatusRow {
                label: "Cost".into(),
                value: format!("${cost:.4}"),
            });
        }
        for (key, label) in [
            ("total_duration_ms", "Duration"),
            ("total_api_duration_ms", "API duration"),
        ] {
            if let Some(ms) = session.get(key).and_then(Value::as_u64) {
                rows.push(StatusRow {
                    label: label.into(),
                    value: format!("{:.1} s", ms as f64 / 1000.0),
                });
            }
        }
        if let (Some(added), Some(removed)) = (
            session.get("total_lines_added").and_then(Value::as_u64),
            session.get("total_lines_removed").and_then(Value::as_u64),
        ) {
            rows.push(StatusRow {
                label: "Lines changed".into(),
                value: format!("+{added} -{removed}"),
            });
        }
        if let Some(models) = session.get("model_usage").and_then(Value::as_object) {
            for (model, u) in models {
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                rows.push(StatusRow {
                    label: model.clone(),
                    value: format!(
                        "{} in, {} out, {} cache read, {} cache write",
                        n("inputTokens"),
                        n("outputTokens"),
                        n("cacheReadInputTokens"),
                        n("cacheCreationInputTokens")
                    ),
                });
            }
        }
        if !rows.is_empty() {
            out.push(StatusSection {
                title: "Session usage".into(),
                rows,
            });
        }
    }
    out
}

/// The section of a `get_plan` response (`@internal` in Claude Code 2.1.284: `{exists,
/// content?, path?}`): the current plan of plan mode, when there is one.
pub fn plan_section(response: &Value) -> Option<StatusSection> {
    if response.get("exists").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let mut rows = Vec::new();
    if let Some(path) = str_field(response, "path") {
        rows.push(StatusRow {
            label: "File".into(),
            value: path.to_owned(),
        });
    }
    rows.push(StatusRow {
        label: "Plan".into(),
        value: str_field(response, "content")
            .unwrap_or_default()
            .to_owned(),
    });
    Some(StatusSection {
        title: "Plan".into(),
        rows,
    })
}

/// The anchor of a turn (`AdapterEvent::TurnAnchor`): the `uuid` of its last main-thread
/// transcript entry, which `--resume-session-at` takes ("any chain element's UUID; normally
/// the last element of the turn to keep"). Copied transcript entries keep their uuids in a fork,
/// so an anchor stays valid in forks of forks (recording g2).
pub fn turn_anchor(uuid: &str) -> Value {
    json!({ "leafUuid": uuid })
}

/// The uuid of an anchor made by [`turn_anchor`].
pub fn anchor_uuid(anchor: &Value) -> Option<&str> {
    anchor.get("leafUuid").and_then(Value::as_str)
}

/// Why the CLI started a run by itself, from its `result.origin`: `task-notification` (the run
/// takes up background tasks that ended). Other origins and none give no trigger.
pub fn turn_trigger(result: &Value) -> Option<TurnTrigger> {
    match result.pointer("/origin/kind").and_then(Value::as_str) {
        Some("task-notification") => Some(TurnTrigger::BackgroundTask),
        _ => None,
    }
}

/// Status and error of a finished turn.
pub fn turn_outcome(result: &Value, interrupt_requested: bool) -> (TurnStatus, Option<TurnError>) {
    let terminal = str_field(result, "terminal_reason");
    if interrupt_requested || terminal.is_some_and(|t| ABORTED_TERMINAL_REASONS.contains(&t)) {
        return (TurnStatus::Interrupted, None);
    }
    if result.get("is_error").and_then(Value::as_bool) == Some(true) {
        let subtype = str_field(result, "subtype").unwrap_or("error");
        let errors: Vec<String> = result
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str().map(str::to_owned))
            .collect();
        let message = sanitize_terminal_text(&if !errors.is_empty() {
            format!("{subtype}: {}", errors.join("; "))
        } else if let Some(r) = str_field(result, "result").filter(|r| !r.is_empty()) {
            format!("{subtype}: {r}")
        } else {
            subtype.to_owned()
        });
        return (
            TurnStatus::Failed,
            Some(TurnError {
                message,
                kind: "harnessError".into(),
            }),
        );
    }
    (TurnStatus::Completed, None)
}

// ---------------------------------------------------------------------------------------------
// Initialize response: models, commands, permission modes
// ---------------------------------------------------------------------------------------------

/// The ultracode effort level: xhigh effort plus standing dynamic-workflow orchestration.
pub const ULTRACODE: &str = "ultracode";

/// Canonical order and labels of Claude Code's effort levels.
pub const EFFORT_LEVELS: &[(&str, &str)] = &[
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra high"),
    ("max", "Max"),
    (ULTRACODE, "Ultracode"),
];

/// `apply_flag_settings.settings` for the effort level `effort` (`None`: the CLI's default).
/// Ultracode is asked for as the effort level `ultracode` and left by setting another level
/// (both recorded with 2.1.283); leaving it for the default also clears the `ultracode` flag,
/// as the CLI's own effort picker does. `get_settings` confirms the outcome (see
/// `Inner::confirm_effort`).
pub fn effort_flag_settings(effort: Option<&str>, was_ultracode: bool) -> Value {
    match effort {
        Some(level) => json!({ "effortLevel": level }),
        None if was_ultracode => json!({ "effortLevel": null, "ultracode": false }),
        None => json!({ "effortLevel": null }),
    }
}

/// Models from the `initialize` response (`models[]`: `value`, `displayName`, `description`,
/// `supportedEffortLevels`). The pseudo-model `default` is the CLI's own default. Ultracode is
/// offered for the models that list `xhigh`: it runs at xhigh effort, and the CLI refuses it
/// for a model without it ("Ultracode runs at xhigh effort, which <model> doesn't support").
pub fn models_from_initialize(init: &Value) -> Vec<Model> {
    init.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = str_field(m, "value")?.to_owned();
            let supports_effort = m
                .get("supportsEffort")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let levels = m
                .get("supportedEffortLevels")
                .and_then(Value::as_array)
                .map(|l| {
                    let mut levels: Vec<String> = l
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                    if levels.iter().any(|x| x == "xhigh") && !levels.iter().any(|x| x == ULTRACODE)
                    {
                        levels.push(ULTRACODE.to_owned());
                    }
                    levels
                });
            Some(Model {
                display_name: str_field(m, "displayName").unwrap_or(&id).to_owned(),
                description: str_field(m, "description").map(str::to_owned),
                is_default: id == "default",
                effort_levels: if supports_effort {
                    levels
                } else {
                    Some(Vec::new())
                },
                id,
            })
        })
        .collect()
}

/// The models (`initialize.models[].value`) the CLI marks `supportsFastMode` (feature
/// `fastModeModels`).
pub fn fast_mode_models(init: &Value) -> Vec<String> {
    init.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("supportsFastMode").and_then(Value::as_bool) == Some(true))
        .filter_map(|m| str_field(m, "value").map(str::to_owned))
        .collect()
}

/// `apply_flag_settings.settings` for fast mode: `fastMode` is both the SDK's opt-in and the
/// switch (recording e1: `true` turns it on, `false` off again).
pub fn fast_mode_flag_settings(on: bool) -> Value {
    json!({ "fastMode": on })
}

/// Effort levels offered by at least one model, in canonical order. Ultracode only for a model
/// that lists xhigh (a model without a list of levels does not name it).
pub fn effort_levels(models: &[Model]) -> Vec<EffortLevel> {
    EFFORT_LEVELS
        .iter()
        .filter(|(id, _)| {
            let listed = |l: &Vec<String>| l.iter().any(|x| x == id);
            if *id == ULTRACODE {
                return models
                    .iter()
                    .any(|m| m.effort_levels.as_ref().is_some_and(listed));
            }
            models.is_empty()
                || models
                    .iter()
                    .any(|m| m.effort_levels.as_ref().is_none_or(listed))
        })
        .map(|(id, label)| EffortLevel {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
        })
        .collect()
}

/// Claude Code's permission mode for plan mode. It is not offered as a permission mode: plan
/// mode is the thread's `modes.plan` (feature `planMode`, the app's `/plan`), which the adapter
/// turns on and off with this mode over the thread's own permission mode.
pub const PLAN_MODE: &str = "plan";

/// Settings with the permission mode `plan`, which earlier versions of this adapter offered as a
/// permission mode (threads and project defaults keep it): plan mode now (`modes.plan`), over the
/// CLI's default permission mode (no `--permission-mode`), which is what the CLI returns to when
/// plan mode ends. Other settings are unchanged.
pub fn upgrade_settings(settings: ThreadSettings) -> UpgradedSettings {
    if settings.permission_mode.as_deref() != Some(PLAN_MODE) {
        return UpgradedSettings {
            settings,
            plan: false,
        };
    }
    UpgradedSettings {
        settings: ThreadSettings {
            permission_mode: None,
            ..settings
        },
        plan: true,
    }
}

/// Permission modes (fixed table). `bypassPermissions` is only offered when the adapter
/// option `allowBypassPermissions` is set; `plan` is the thread's plan mode ([`PLAN_MODE`]).
/// The default is the CLI's `current_permission_mode` (`default` when that is `plan`).
pub fn permission_modes(current: Option<&str>, allow_bypass: bool) -> Vec<PermissionMode> {
    let table: &[(&str, &str, &str)] = &[
        (
            "default",
            "Ask",
            "Ask before edits and non-read-only commands",
        ),
        (
            "acceptEdits",
            "Accept edits",
            "Apply file edits without asking; ask for commands",
        ),
        (
            "auto",
            "Auto",
            "A classifier approves or escalates each request",
        ),
        (
            "dontAsk",
            "Don't ask",
            "Deny anything that is not pre-approved by rules",
        ),
        (
            "bypassPermissions",
            "Bypass permissions",
            "Run every tool without asking",
        ),
    ];
    let current = current.filter(|m| *m != PLAN_MODE).unwrap_or("default");
    table
        .iter()
        .filter(|(id, _, _)| allow_bypass || *id != "bypassPermissions")
        .map(|(id, label, desc)| PermissionMode {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
            description: Some((*desc).to_owned()),
            is_default: *id == current,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aas_harness::protocol::QuestionAnswer;
    use pretty_assertions::assert_eq;

    /// The permission mode `plan` of earlier versions is plan mode over the CLI's default;
    /// other settings are kept as they are.
    #[test]
    fn a_plan_permission_mode_is_upgraded_to_plan_mode() {
        let legacy = ThreadSettings {
            model: Some("opus".into()),
            effort: Some("high".into()),
            permission_mode: Some(PLAN_MODE.into()),
        };
        assert_eq!(
            upgrade_settings(legacy),
            UpgradedSettings {
                settings: ThreadSettings {
                    model: Some("opus".into()),
                    effort: Some("high".into()),
                    permission_mode: None,
                },
                plan: true,
            }
        );
        let current = ThreadSettings {
            permission_mode: Some("acceptEdits".into()),
            ..ThreadSettings::default()
        };
        assert_eq!(
            upgrade_settings(current.clone()),
            UpgradedSettings {
                settings: current,
                plan: false,
            }
        );
    }

    #[test]
    fn tool_table() {
        assert_eq!(classify_tool("Bash"), ToolClass::Command);
        assert_eq!(classify_tool("PowerShell"), ToolClass::Command);
        assert_eq!(classify_tool("Write"), ToolClass::FileChange);
        assert_eq!(classify_tool("TaskCreate"), ToolClass::Plan);
        assert_eq!(
            classify_tool("Task"),
            ToolClass::Generic(ToolCategory::Subagent)
        );
        assert_eq!(
            classify_tool("WebFetch"),
            ToolClass::Generic(ToolCategory::Fetch)
        );
        assert_eq!(
            classify_tool("mcp__docs__search"),
            ToolClass::Generic(ToolCategory::Mcp)
        );
        assert_eq!(
            classify_tool("SomethingNew"),
            ToolClass::Generic(ToolCategory::Other)
        );
        assert_eq!(
            split_mcp_name("mcp__claude_ai_Docs__batch__x"),
            Some(("claude_ai_Docs", "batch__x"))
        );
        assert_eq!(tool_title("mcp__docs__search", &json!({})), "docs: search");
        assert_eq!(
            tool_title("Read", &json!({"file_path": "a.rs"})),
            "Read a.rs"
        );
        assert_eq!(
            tool_title(
                "Task",
                &json!({"description": "find", "subagent_type": "Explore"})
            ),
            "Explore: find"
        );
    }

    #[test]
    fn bash_completion_uses_structured_output() {
        let input = json!({"command": "echo hi"});
        let started = tool_started_body("Bash", &input).unwrap();
        let result = ToolResult {
            text: "hi".into(),
            is_error: false,
            structured: Some(json!({"stdout": "hi", "stderr": "warn", "interrupted": false})),
            denied_by_user: false,
        };
        let (body, status) = tool_completed("Bash", &input, &started, &result);
        assert_eq!(status, ItemStatus::Completed);
        match body {
            ItemBody::CommandExecution {
                command, output, ..
            } => {
                assert_eq!(command, "echo hi");
                assert_eq!(output, "hi\nwarn");
            }
            other => panic!("{other:?}"),
        }
        let denied = ToolResult {
            text: "Denied".into(),
            is_error: true,
            structured: None,
            denied_by_user: true,
        };
        assert_eq!(
            tool_completed("Bash", &input, &started, &denied).1,
            ItemStatus::Declined
        );
        let failed = ToolResult {
            text: "boom".into(),
            is_error: true,
            structured: None,
            denied_by_user: false,
        };
        assert_eq!(
            tool_completed("Bash", &input, &started, &failed).1,
            ItemStatus::Failed
        );
    }

    #[test]
    fn write_and_edit_completion_diffs() {
        let input = json!({"file_path": "C:\\w\\note.txt", "content": "hello"});
        let started = tool_started_body("Write", &input).unwrap();
        let result = ToolResult {
            text: "File created".into(),
            is_error: false,
            structured: Some(
                json!({"type": "create", "filePath": "C:\\w\\note.txt", "content": "hello", "structuredPatch": []}),
            ),
            denied_by_user: false,
        };
        let (body, _) = tool_completed("Write", &input, &started, &result);
        let ItemBody::FileChange { changes } = body else {
            panic!()
        };
        assert_eq!(changes[0].kind, FileChangeKind::Add);
        assert_eq!(
            changes[0].diff.as_deref(),
            Some("@@ -0,0 +1,1 @@\n+hello\n")
        );

        let input =
            json!({"file_path": "C:\\w\\note.txt", "old_string": "hello", "new_string": "world"});
        let started = tool_started_body("Edit", &input).unwrap();
        let ItemBody::FileChange { changes } = &started else {
            panic!()
        };
        assert_eq!(changes[0].diff.as_deref(), Some("@@\n-hello\n+world\n"));
        let result = ToolResult {
            text: "updated".into(),
            is_error: false,
            structured: Some(json!({"filePath": "C:\\w\\note.txt", "structuredPatch": [
                {"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1,
                 "lines": ["-hello", "\\ No newline at end of file", "+world", "\\ No newline at end of file"]}]})),
            denied_by_user: false,
        };
        let (body, _) = tool_completed("Edit", &input, &started, &result);
        let ItemBody::FileChange { changes } = body else {
            panic!()
        };
        assert_eq!(changes[0].kind, FileChangeKind::Update);
        assert_eq!(changes[0].added, Some(1));
        assert_eq!(changes[0].removed, Some(1));
        assert!(
            changes[0]
                .diff
                .as_deref()
                .unwrap()
                .starts_with("@@ -1,1 +1,1 @@\n-hello\n")
        );
    }

    #[test]
    fn task_list_tracks_plan_tools() {
        let mut list = TaskList::default();
        let ok = |s: Value| ToolResult {
            text: String::new(),
            is_error: false,
            structured: Some(s),
            denied_by_user: false,
        };
        assert!(list.apply(
            "TaskCreate",
            &json!({"subject": "A"}),
            &ok(json!({"task": {"id": "1", "subject": "A"}}))
        ));
        assert!(list.apply(
            "TaskCreate",
            &json!({"subject": "B"}),
            &ok(json!({"task": {"id": "2", "subject": "B"}}))
        ));
        assert!(list.apply(
            "TaskUpdate",
            &json!({"taskId": "1", "status": "in_progress"}),
            &ok(json!({"success": true}))
        ));
        assert_eq!(
            list.entries(),
            vec![
                PlanEntry {
                    text: "A".into(),
                    status: PlanEntryStatus::InProgress
                },
                PlanEntry {
                    text: "B".into(),
                    status: PlanEntryStatus::Pending
                },
            ]
        );
        assert!(!list.apply(
            "TaskUpdate",
            &json!({"taskId": "1", "status": "in_progress"}),
            &ok(json!({"success": true}))
        ));
        assert!(list.apply(
            "TaskUpdate",
            &json!({"taskId": "2", "status": "deleted"}),
            &ok(json!({"success": true}))
        ));
        assert_eq!(list.entries().len(), 1);
        assert!(list.apply(
            "TodoWrite",
            &json!({"todos": [{"content": "x", "status": "completed", "activeForm": "x"}]}),
            &ok(json!({}))
        ));
        assert_eq!(
            list.entries(),
            vec![PlanEntry {
                text: "x".into(),
                status: PlanEntryStatus::Completed
            }]
        );
    }

    #[test]
    fn permission_options_follow_suggestions() {
        let request = json!({
            "subtype": "can_use_tool", "tool_name": "Bash", "display_name": "Bash",
            "input": {"command": "mkdir x"}, "description": "Create x",
            "permission_suggestions": [
                {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "mkdir x *"}], "behavior": "allow", "destination": "localSettings"},
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
            ],
            "tool_use_id": "toolu_1"
        });
        let ask = PermissionAsk::from_request(&request);
        let InteractionRequest::Approval {
            title,
            subject,
            options,
            detail,
        } = permission_interaction(&request, &ask)
        else {
            panic!()
        };
        assert_eq!(title, "Run command?");
        assert_eq!(detail.as_deref(), Some("Create x"));
        assert_eq!(
            subject,
            Subject::Command {
                command: "mkdir x".into(),
                cwd: None
            }
        );
        let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "allow",
                "allow_session",
                "allow_always",
                "deny",
                "deny_feedback",
                "abort"
            ]
        );

        let (resp, denied) = permission_response(
            &ask,
            &InteractionResolution::Approval {
                option_id: "allow_session".into(),
                feedback: None,
            },
        )
        .unwrap();
        assert!(!denied);
        assert_eq!(resp["updatedPermissions"][0]["destination"], "session");
        assert_eq!(resp["updatedPermissions"][1]["destination"], "session");
        let (resp, _) = permission_response(
            &ask,
            &InteractionResolution::Approval {
                option_id: "allow_always".into(),
                feedback: None,
            },
        )
        .unwrap();
        assert_eq!(
            resp["updatedPermissions"][0]["destination"],
            "localSettings"
        );
        let (resp, denied) = permission_response(
            &ask,
            &InteractionResolution::Approval {
                option_id: "deny_feedback".into(),
                feedback: Some("use git".into()),
            },
        )
        .unwrap();
        assert!(denied);
        assert_eq!(resp, json!({"behavior": "deny", "message": "use git"}));
        let (resp, _) = permission_response(
            &ask,
            &InteractionResolution::Approval {
                option_id: "abort".into(),
                feedback: None,
            },
        )
        .unwrap();
        assert_eq!(resp["interrupt"], true);
        assert!(
            permission_response(
                &ask,
                &InteractionResolution::Approval {
                    option_id: "zzz".into(),
                    feedback: None
                }
            )
            .is_err()
        );
    }

    #[test]
    fn suppress_always_and_default_to_no() {
        let request = json!({
            "tool_name": "Bash", "input": {"command": "rm -rf x"},
            "permission_suggestions": [{"type": "addRules", "destination": "localSettings"}],
            "suppress_always_allow_rule": true, "default_to_no": true, "decision_reason": "\u{1b}[31mdangerous\u{1b}[0m"
        });
        let ask = PermissionAsk::from_request(&request);
        let InteractionRequest::Approval {
            options, detail, ..
        } = permission_interaction(&request, &ask)
        else {
            panic!()
        };
        let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["deny", "allow", "allow_session", "deny_feedback", "abort"]
        );
        assert_eq!(detail.as_deref(), Some("dangerous"));
    }

    #[test]
    fn ask_user_question_round_trip() {
        let input = json!({"questions": [
            {"question": "Which color?", "header": "Color", "multiSelect": false,
             "options": [{"label": "Red", "description": "r"}, {"label": "Blue", "description": "b"}]},
            {"question": "Which sizes?", "header": "Size", "multiSelect": true,
             "options": [{"label": "S", "description": ""}, {"label": "M", "description": ""}]}
        ]});
        let InteractionRequest::Question { title, questions } =
            question_interaction(&input).unwrap()
        else {
            panic!()
        };
        assert_eq!(title, "2 questions from Claude");
        assert_eq!(questions[0].choices[1].label, "Blue");
        assert!(questions[1].multi_select);
        let resolution = InteractionResolution::Question {
            answers: vec![
                QuestionAnswer {
                    question_id: "q0".into(),
                    choice_ids: vec!["c1".into()],
                    text: None,
                },
                QuestionAnswer {
                    question_id: "q1".into(),
                    choice_ids: vec!["c0".into(), "c1".into()],
                    text: Some("XL too".into()),
                },
            ],
        };
        let (resp, denied) = question_response(&input, &resolution);
        assert!(!denied);
        assert_eq!(resp["behavior"], "allow");
        assert_eq!(
            resp["updatedInput"]["answers"],
            json!({"Which color?": "Blue", "Which sizes?": "S, M, XL too"})
        );
        assert_eq!(resp["updatedInput"]["questions"], input["questions"]);
        let (resp, denied) = question_response(&input, &InteractionResolution::Dismissed);
        assert!(denied);
        assert_eq!(resp["behavior"], "deny");
    }

    #[test]
    fn turn_outcomes() {
        let ok = json!({"type": "result", "subtype": "success", "is_error": false, "terminal_reason": "completed",
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 100,
                      "output_tokens": 40, "output_tokens_details": {"thinking_tokens": 30}}});
        assert_eq!(turn_outcome(&ok, false), (TurnStatus::Completed, None));
        assert_eq!(turn_outcome(&ok, true).0, TurnStatus::Interrupted);
        let context = json!({"totalTokens": 35751, "maxTokens": 200000, "rawMaxTokens": 200000, "percentage": 18});
        assert_eq!(
            context_from_usage_response(&context),
            Some(ContextUsage {
                used_tokens: 35751,
                window_tokens: 200000
            })
        );
        assert_eq!(
            context_from_usage_response(&json!({"totalTokens": 1, "rawMaxTokens": 0})),
            None
        );
        assert_eq!(
            context_from_usage_response(&json!({"maxTokens": 200000})),
            None
        );
        let usage = usage_from_result(&ok, Some(0.5)).unwrap();
        assert_eq!(usage.input_tokens, 115);
        assert_eq!(usage.cached_input_tokens, 100);
        assert_eq!(usage.reasoning_tokens, 30);
        assert_eq!(usage.cost_usd, Some(0.5));
        let aborted = json!({"subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_streaming"});
        assert_eq!(turn_outcome(&aborted, false).0, TurnStatus::Interrupted);
        let failed =
            json!({"subtype": "error_max_turns", "is_error": true, "errors": ["too many turns"]});
        let (status, err) = turn_outcome(&failed, false);
        assert_eq!(status, TurnStatus::Failed);
        assert_eq!(err.unwrap().message, "error_max_turns: too many turns");
    }

    #[test]
    fn initialize_tables() {
        let init = json!({
            "models": [
                {"value": "default", "displayName": "Default (recommended)", "description": "Opus", "supportsEffort": true,
                 "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
                {"value": "haiku", "displayName": "Haiku", "supportsEffort": false}
            ],
            "commands": [{"name": "compact", "description": "Compact", "argumentHint": "[instructions]"}, {"name": "", "description": ""}]
        });
        let models = models_from_initialize(&init);
        assert_eq!(models.len(), 2);
        assert!(models[0].is_default);
        assert_eq!(models[1].effort_levels, Some(vec![]));
        // Ultracode for the model that lists xhigh.
        assert_eq!(
            models[0]
                .effort_levels
                .as_ref()
                .unwrap()
                .last()
                .map(String::as_str),
            Some(ULTRACODE)
        );
        assert_eq!(effort_levels(&models).len(), 6);
        let modes = permission_modes(Some("acceptEdits"), false);
        assert!(!modes.iter().any(|m| m.id == "bypassPermissions"));
        assert!(
            modes
                .iter()
                .find(|m| m.id == "acceptEdits")
                .unwrap()
                .is_default
        );
        assert!(
            permission_modes(None, true)
                .iter()
                .any(|m| m.id == "bypassPermissions")
        );
        // Plan mode is the thread's `modes.plan` (the app's `/plan`), not a permission mode.
        assert!(!permission_modes(None, true).iter().any(|m| m.id == "plan"));
        // A CLI whose default mode is plan: the thread's base mode is `default`.
        assert!(
            permission_modes(Some("plan"), false)
                .iter()
                .find(|m| m.id == "default")
                .unwrap()
                .is_default
        );
    }

    #[test]
    fn fast_mode_models_are_the_ones_the_cli_marks() {
        // The shape of Claude Code 2.1.284's `initialize.models` (recording e1).
        let init = json!({"models": [
            {"value": "default", "resolvedModel": "claude-opus-4-6", "supportsEffort": true, "supportsAutoMode": true},
            {"value": "opus", "resolvedModel": "claude-opus-5-5", "supportsEffort": true, "supportsFastMode": true},
            {"value": "haiku", "resolvedModel": "claude-haiku-4-5-20251001"},
            {"value": "claude-opus-4-8", "supportsFastMode": true},
            {"value": "claude-opus-4-7", "supportsFastMode": false}
        ]});
        assert_eq!(fast_mode_models(&init), ["opus", "claude-opus-4-8"]);
        assert!(fast_mode_models(&json!({})).is_empty());
    }

    #[test]
    fn terminal_control_is_removed_from_request_details() {
        let request = json!({"tool_name": "Bash", "input": {"command": "ls"},
            "title": "\u{1b}[1mRun\u{1b}[0m ls?", "decision_reason": "\u{1b}[1;31mred\u{1b}[0m plain \u{1b}]0;title\u{7}x"});
        let ask = PermissionAsk::from_request(&request);
        let InteractionRequest::Approval { title, detail, .. } =
            permission_interaction(&request, &ask)
        else {
            panic!("an approval")
        };
        assert_eq!(title, "Run ls?");
        assert_eq!(detail.as_deref(), Some("red plain x"));
    }

    fn done(structured: Value) -> ToolResult {
        ToolResult {
            text: String::new(),
            is_error: false,
            structured: Some(structured),
            denied_by_user: false,
        }
    }

    #[test]
    fn launches_come_from_the_structured_result() {
        // Agent (claude-live E1).
        let agent = done(json!({"isAsync": true, "status": "async_launched",
            "agentId": "a5462215479566a35", "description": "Sleep 40 seconds then respond",
            "resolvedModel": "claude-haiku-4-5-20251001", "prompt": "…", "outputFile": "x"}));
        assert_eq!(
            background_effect("Agent", &json!({}), &agent),
            Some(BackgroundEffect::Task(Launch {
                task_id: "a5462215479566a35".into(),
                kind: BackgroundTaskKind::Agent,
                title: "Sleep 40 seconds then respond".into()
            }))
        );
        // Workflow (claude-live E3a).
        let workflow = done(json!({"status": "async_launched", "taskId": "wia92dx1s",
            "taskType": "local_workflow", "workflowName": "simple-parallel-test",
            "runId": "wf_b93f111a-425", "summary": "Run two agents in parallel"}));
        assert_eq!(
            background_effect("Workflow", &json!({"script": "…"}), &workflow),
            Some(BackgroundEffect::Task(Launch {
                task_id: "wia92dx1s".into(),
                kind: BackgroundTaskKind::Workflow,
                title: "Run two agents in parallel".into()
            }))
        );
        // Bash in the background (claude-live E2).
        let bash = done(json!({"stdout": "", "stderr": "", "interrupted": false,
            "isImage": false, "noOutputExpected": false, "backgroundTaskId": "bkomsmz3d"}));
        let input = json!({"command": "sleep 30 && echo done-B",
            "description": "Sleep for 30 seconds then output done-B", "run_in_background": true});
        assert_eq!(
            background_effect("Bash", &input, &bash),
            Some(BackgroundEffect::Task(Launch {
                task_id: "bkomsmz3d".into(),
                kind: BackgroundTaskKind::Shell,
                title: "Sleep for 30 seconds then output done-B".into()
            }))
        );
        // A remote agent (the 2.1.283 bundle's result shape).
        let remote = done(json!({"status": "remote_launched", "taskId": "r1",
            "sessionUrl": "https://example.invalid", "description": "remote work"}));
        assert!(matches!(
            background_effect("Agent", &json!({}), &remote),
            Some(BackgroundEffect::Task(Launch {
                kind: BackgroundTaskKind::Remote,
                ..
            }))
        ));
        // A foreground command, a failed or denied call: nothing.
        let fg = done(json!({"stdout": "hi", "stderr": "", "interrupted": false}));
        assert_eq!(background_effect("Bash", &input, &fg), None);
        let failed = ToolResult {
            is_error: true,
            ..bash.clone()
        };
        assert_eq!(background_effect("Bash", &input, &failed), None);
        let denied = ToolResult {
            denied_by_user: true,
            ..bash
        };
        assert_eq!(background_effect("Bash", &input, &denied), None);
    }

    #[test]
    fn cron_tools_change_the_scheduled_wakeups() {
        // Recorded w4.
        let create = done(json!({"id": "47a7861e", "humanSchedule": "Every minute",
            "recurring": true, "durable": false}));
        let input = json!({"cron": "* * * * *", "prompt": "Reply with exactly: tick",
            "recurring": true, "durable": false});
        assert_eq!(
            background_effect("CronCreate", &input, &create),
            Some(BackgroundEffect::Cron(Cron {
                id: "47a7861e".into(),
                prompt: "Reply with exactly: tick".into(),
                schedule: Some("Every minute".into()),
                human_schedule: true,
                recurring: true
            }))
        );
        assert_eq!(
            background_effect(
                "CronDelete",
                &json!({"id": "47a7861e"}),
                &done(json!({"id": "47a7861e"}))
            ),
            Some(BackgroundEffect::CronDeleted("47a7861e".into()))
        );
        let list = done(json!({"jobs": [{"id": "47a7861e", "cron": "* * * * *",
            "humanSchedule": "Every minute", "prompt": "Reply with exactly: tick",
            "recurring": true, "durable": false}]}));
        assert!(matches!(
            background_effect("CronList", &json!({}), &list),
            Some(BackgroundEffect::CronList(jobs)) if jobs.len() == 1
        ));
        // ScheduleWakeup names no id (recorded w1): nothing to key a task by.
        let wakeup = done(
            json!({"scheduledFor": 1790596080000u64, "clampedDelaySeconds": 60,
            "wasClamped": false}),
        );
        assert_eq!(
            background_effect("ScheduleWakeup", &json!({}), &wakeup),
            None
        );
    }

    #[test]
    fn a_workflow_item_is_titled_with_the_workflow_name() {
        let input = json!({"script": "export const meta = {name: 'x'}"});
        let started = tool_started_body("Workflow", &input).unwrap();
        assert!(matches!(
            &started,
            ItemBody::ToolCall { category: ToolCategory::Subagent, title, .. } if title == "Workflow"
        ));
        let result = done(json!({"status": "async_launched", "taskId": "w1",
            "taskType": "local_workflow", "workflowName": "parallel-ping-test"}));
        let (body, _) = tool_completed("Workflow", &input, &started, &result);
        assert!(matches!(
            body,
            ItemBody::ToolCall { title, .. } if title == "Workflow: parallel-ping-test"
        ));
    }

    #[test]
    fn triggers_come_from_the_result_origin() {
        assert_eq!(
            turn_trigger(&json!({"origin": {"kind": "task-notification"}})),
            Some(TurnTrigger::BackgroundTask)
        );
        assert_eq!(turn_trigger(&json!({"origin": {"kind": "human"}})), None);
        assert_eq!(turn_trigger(&json!({"subtype": "success"})), None);
    }

    /// `get_usage` as Claude Code 2.1.284 answers it (recording b1, shortened).
    fn usage_response() -> Value {
        json!({
            "session": {"total_cost_usd": 0.012375899999999999, "total_api_duration_ms": 4742, "total_duration_ms": 33373,
                "total_lines_added": 3, "total_lines_removed": 1,
                "model_usage": {"claude-haiku-4-5-20251001": {"inputTokens": 10, "outputTokens": 330,
                    "cacheReadInputTokens": 19759, "cacheCreationInputTokens": 4370, "costUSD": 0.0123}}},
            "subscription_type": "max", "rate_limits_available": true,
            "rate_limits": {
                "five_hour": {"utilization": 43, "resets_at": "2026-09-28T20:20:00.469143+00:00"},
                "limits": [
                    {"kind": "session", "group": "session", "percent": 43, "severity": "normal",
                     "resets_at": "2026-09-28T20:20:00.469143+00:00", "scope": null, "is_active": false},
                    {"kind": "weekly_all", "group": "weekly", "percent": 54, "severity": "warning",
                     "resets_at": "2026-10-01T16:00:00.469173+00:00", "scope": null, "is_active": true},
                    {"kind": "weekly_scoped", "group": "weekly", "percent": 0, "severity": "normal",
                     "resets_at": "2026-10-01T16:00:00+00:00", "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}],
                "spend": {"percent": 0, "severity": "normal", "enabled": false, "disabled_reason": "out_of_credits"}},
            "behaviors": null
        })
    }

    fn rows(section: &StatusSection) -> Vec<(&str, &str)> {
        section
            .rows
            .iter()
            .map(|r| (r.label.as_str(), r.value.as_str()))
            .collect()
    }

    #[test]
    fn usage_becomes_the_plan_and_session_sections() {
        let sections = usage_sections(&usage_response(), true);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "Plan usage");
        assert_eq!(
            rows(&sections[0]),
            [
                ("Plan", "max"),
                (
                    "Current session",
                    "43% used, resets 2026-09-28T20:20:00.469143+00:00"
                ),
                (
                    "Current week (all models)",
                    "54% used, resets 2026-10-01T16:00:00.469173+00:00 (warning)"
                ),
                (
                    "Current week (Fable)",
                    "0% used, resets 2026-10-01T16:00:00+00:00"
                ),
                ("Usage credits", "off (out_of_credits)"),
            ]
        );
        assert_eq!(sections[1].title, "Session usage");
        assert_eq!(
            rows(&sections[1]),
            [
                ("Cost", "$0.0124"),
                ("Duration", "33.4 s"),
                ("API duration", "4.7 s"),
                ("Lines changed", "+3 -1"),
                (
                    "claude-haiku-4-5-20251001",
                    "10 in, 330 out, 19759 cache read, 4370 cache write"
                ),
            ]
        );
        // Without a session (a probe process): the plan only.
        let sections = usage_sections(&usage_response(), false);
        assert_eq!(sections.len(), 1);
        // No rate limits for this login; enabled credits.
        let sections = usage_sections(
            &json!({"rate_limits_available": false, "rate_limits": {"spend": {"enabled": true, "percent": 12}}}),
            false,
        );
        assert_eq!(
            rows(&sections[0]),
            [
                ("Rate limits", "not available"),
                ("Usage credits", "on, 12% used")
            ]
        );
        assert!(usage_sections(&json!({}), true).is_empty());
    }

    #[test]
    fn status_and_plan_sections_are_the_cli_rows() {
        let status = json!({"sections": [
            {"title": "Session", "rows": [{"label": "Version", "value": "2.1.284"}, {"label": "Session name", "value": null}]},
            {"title": "Environment", "rows": [{"label": "Model", "value": "claude-haiku-4-5-20251001"}, {"value": "no label"}]},
            {"rows": []}]});
        let sections = status_sections(&status);
        assert_eq!(sections.len(), 2);
        assert_eq!(
            rows(&sections[0]),
            [("Version", "2.1.284"), ("Session name", "")]
        );
        assert_eq!(rows(&sections[1]), [("Model", "claude-haiku-4-5-20251001")]);
        assert_eq!(plan_section(&json!({"exists": false})), None);
        let plan = plan_section(&json!({"exists": true, "content": "# Plan", "path": "C:\\p.md"}))
            .unwrap();
        assert_eq!(rows(&plan), [("File", "C:\\p.md"), ("Plan", "# Plan")]);
        // A connection that does not get the path.
        let plan = plan_section(&json!({"exists": true, "content": "# Plan"})).unwrap();
        assert_eq!(rows(&plan), [("Plan", "# Plan")]);
    }

    #[test]
    fn a_refused_start_is_the_result_text() {
        // Recordings g6 and g3a.
        assert_eq!(
            startup_error(
                &json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
                "errors": ["No message found with message.uuid of: 1111"]})
            )
            .as_deref(),
            Some("No message found with message.uuid of: 1111")
        );
        assert_eq!(
            startup_error(&json!({"subtype": "error_during_execution", "is_error": true, "errors": ["\u{1b}[31mbad\u{1b}[0m"]}))
                .as_deref(),
            Some("bad")
        );
        assert_eq!(
            startup_error(&json!({"subtype": "error_during_execution", "is_error": true}))
                .as_deref(),
            Some("error_during_execution")
        );
        assert_eq!(
            startup_error(&json!({"subtype": "success", "is_error": false})),
            None
        );
        assert_eq!(
            anchor_uuid(&turn_anchor("cffd151b-1a78-4509-882b-5c8460ad0617")),
            Some("cffd151b-1a78-4509-882b-5c8460ad0617")
        );
    }

    #[test]
    fn exit_plan_mode_is_the_proposed_plan() {
        let input = json!({"plan": "# Plan\n\nCreate hello.txt", "planFilePath": "C:\\p.md"});
        let started = tool_started_body("ExitPlanMode", &input).unwrap();
        assert_eq!(
            started,
            ItemBody::ProposedPlan {
                text: "# Plan\n\nCreate hello.txt".into()
            }
        );
        let approved = ToolResult {
            text: "User has approved your plan.".into(),
            is_error: false,
            structured: Some(
                json!({"plan": "# Plan\n\nCreate hello.txt", "isAgent": false, "filePath": "C:\\p.md"}),
            ),
            denied_by_user: false,
        };
        assert_eq!(
            tool_completed("ExitPlanMode", &input, &started, &approved),
            (started.clone(), ItemStatus::Completed)
        );
        let rejected = ToolResult {
            denied_by_user: true,
            ..approved
        };
        assert_eq!(
            tool_completed("ExitPlanMode", &input, &started, &rejected).1,
            ItemStatus::Declined
        );
    }

    #[test]
    fn expiry_answers_say_why() {
        for reason in [
            ExpireReason::TurnEnded,
            ExpireReason::TaskEnded,
            ExpireReason::HarnessCancelled,
            ExpireReason::ProcessExited,
            ExpireReason::DaemonRestarted,
        ] {
            let body = expiry_response(reason);
            assert_eq!(body["behavior"], "deny");
            assert!(
                body["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("The request expired")
            );
        }
    }

    #[test]
    fn effort_flags_for_ultracode() {
        assert_eq!(
            effort_flag_settings(Some(ULTRACODE), false),
            json!({"effortLevel": "ultracode"})
        );
        assert_eq!(
            effort_flag_settings(Some("high"), true),
            json!({"effortLevel": "high"})
        );
        assert_eq!(
            effort_flag_settings(None, true),
            json!({"effortLevel": null, "ultracode": false})
        );
        assert_eq!(
            effort_flag_settings(None, false),
            json!({"effortLevel": null})
        );
        // A model without effort levels (haiku) and one without a list are not offered it.
        let models = models_from_initialize(&json!({"models": [
            {"value": "haiku", "supportsEffort": false, "supportedEffortLevels": null},
            {"value": "x", "supportsEffort": true}
        ]}));
        assert!(!effort_levels(&models).iter().any(|l| l.id == ULTRACODE));
    }
}
