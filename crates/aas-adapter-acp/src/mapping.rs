//! Pure mappings from ACP messages to the normalized model. No I/O, no state beyond the
//! values passed in. Every table here is documented in `docs/adapters/acp.md`.

use aas_harness::protocol::{
    ApprovalOption, ApprovalOptionKind, Command, CommandAction, CommandSource, ContextUsage,
    EffortLevel, FileChange, FileChangeKind, InteractionRequest, ItemBody, ItemStatus, Model,
    NoticeLevel, PermissionMode, PlanEntry as ProtoPlanEntry, PlanEntryStatus, Subject,
    ToolCategory, TurnError, TurnStatus, Usage,
};
use serde_json::Value;

use crate::wire::{
    AvailableCommand, ConfigOption, PermissionOption, PlanEntry, PromptUsage, SessionModeState,
    ToolCallFields, UsageUpdate,
};

// ----- tool calls ---------------------------------------------------------------------------

/// Devin's tool that starts a sub-agent (its `cognition.ai/inferenceToolName`).
const DEVIN_RUN_SUBAGENT: &str = "run_subagent";

/// Accumulated state of one tool call (`tool_call` plus every `tool_call_update`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolState {
    pub id: String,
    pub title: Option<String>,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub content: Vec<Value>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    pub meta: Option<Value>,
}

impl ToolState {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            ..Default::default()
        }
    }

    /// Applies `tool_call` / `tool_call_update` fields: every present field replaces the
    /// stored one (ACP semantics). `_meta` objects are merged key by key.
    pub fn merge(&mut self, f: &ToolCallFields) {
        if f.title.is_some() {
            self.title = f.title.clone();
        }
        if f.name.is_some() {
            self.name = f.name.clone();
        }
        if f.kind.is_some() {
            self.kind = f.kind.clone();
        }
        if f.status.is_some() {
            self.status = f.status.clone();
        }
        if let Some(content) = &f.content {
            self.content = content.clone();
        }
        if f.raw_input.is_some() {
            self.raw_input = f.raw_input.clone();
        }
        if f.raw_output.is_some() {
            self.raw_output = f.raw_output.clone();
        }
        if let Some(Value::Object(new)) = &f.meta {
            match &mut self.meta {
                Some(Value::Object(old)) => {
                    for (k, v) in new {
                        old.insert(k.clone(), v.clone());
                    }
                }
                _ => self.meta = Some(Value::Object(new.clone())),
            }
        }
    }

    /// Terminal status reported by the agent, if any.
    pub fn terminal_status(&self) -> Option<ItemStatus> {
        match self.status.as_deref() {
            Some("completed") => Some(ItemStatus::Completed),
            Some("failed") => Some(ItemStatus::Failed),
            _ => None,
        }
    }

    fn diffs(&self) -> Vec<(String, Option<String>, String)> {
        self.content
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("diff"))
            .filter_map(|c| {
                let path = c.get("path")?.as_str()?.to_owned();
                let new = c
                    .get("newText")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let old = c.get("oldText").and_then(Value::as_str).map(str::to_owned);
                Some((path, old, new))
            })
            .collect()
    }

    /// Text shown as the tool's output: the `text` blocks of `content` entries, in order.
    /// Embedded resources (e.g. a preview of the command itself) and terminals are not output.
    /// Falls back to `rawOutput` when it is a string.
    pub fn output_text(&self) -> String {
        let mut out = String::new();
        for entry in &self.content {
            if entry.get("type").and_then(Value::as_str) != Some("content") {
                continue;
            }
            let Some(block) = entry.get("content") else {
                continue;
            };
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = block.get("text").and_then(Value::as_str)
            {
                out.push_str(t);
            }
        }
        if out.is_empty()
            && let Some(Value::String(s)) = &self.raw_output
        {
            out.push_str(s);
        }
        out
    }

    /// The command of an `execute` tool: `rawInput.command` when it is a string, otherwise the
    /// title.
    pub fn command(&self) -> String {
        match self.raw_input.as_ref().and_then(|v| v.get("command")) {
            Some(Value::String(s)) => s.clone(),
            _ => self.title.clone().unwrap_or_else(|| self.id.clone()),
        }
    }

    /// Exit code from Devin's `_meta.terminal_exit.exit_code` extension (documented in
    /// docs/adapters/acp.md). ACP itself carries no exit code for agent-run commands.
    pub fn exit_code(&self) -> Option<i32> {
        self.meta
            .as_ref()?
            .get("terminal_exit")?
            .get("exit_code")?
            .as_i64()
            .and_then(|c| i32::try_from(c).ok())
    }

    /// Devin's own name of the tool (`_meta["cognition.ai/inferenceToolName"]`, e.g. `exec`,
    /// `run_subagent`), documented in docs/adapters/acp.md.
    pub fn inference_tool_name(&self) -> Option<&str> {
        self.meta
            .as_ref()?
            .get("cognition.ai/inferenceToolName")?
            .as_str()
    }

    fn display_title(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.name.clone())
            .or_else(|| self.kind.clone())
            .unwrap_or_else(|| "tool".to_owned())
    }

    fn file_changes(&self) -> Vec<FileChange> {
        let kind = self.kind.as_deref();
        self.diffs()
            .into_iter()
            .map(|(path, old, new)| {
                let change_kind = match (kind, &old) {
                    (Some("delete"), _) => FileChangeKind::Delete,
                    (Some("move"), _) => FileChangeKind::Move,
                    (_, None) => FileChangeKind::Add,
                    (_, Some(_)) => FileChangeKind::Update,
                };
                let (diff, added, removed) = unified_diff(&path, old.as_deref(), &new);
                FileChange {
                    path,
                    kind: change_kind,
                    move_path: None,
                    diff: Some(diff),
                    added: Some(added),
                    removed: Some(removed),
                }
            })
            .collect()
    }

    /// The normalized item body (mapping table in docs/adapters/acp.md).
    pub fn body(&self) -> ItemBody {
        let kind = self.kind.as_deref().unwrap_or("other");
        match kind {
            "execute" => ItemBody::CommandExecution {
                command: self.command(),
                cwd: None,
                output: self.output_text(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: self.exit_code(),
                duration_ms: None,
            },
            "edit" | "delete" | "move" if !self.diffs().is_empty() => ItemBody::FileChange {
                changes: self.file_changes(),
            },
            _ => {
                let category = match kind {
                    "read" => ToolCategory::Read,
                    "search" => ToolCategory::Search,
                    "fetch" => ToolCategory::Fetch,
                    "think" => ToolCategory::Think,
                    "edit" | "delete" | "move" => ToolCategory::Edit,
                    _ if self.inference_tool_name() == Some(DEVIN_RUN_SUBAGENT) => {
                        ToolCategory::Subagent
                    }
                    _ => ToolCategory::Other,
                };
                let output = self.output_text();
                let output = if output.is_empty() {
                    self.raw_output
                        .as_ref()
                        .filter(|v| !v.is_null())
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => serde_json::to_string_pretty(other).unwrap_or_default(),
                        })
                } else {
                    Some(output)
                };
                ItemBody::ToolCall {
                    category,
                    name: self.name.clone().unwrap_or_else(|| kind.to_owned()),
                    title: self.display_title(),
                    server: None,
                    input: self.raw_input.clone().filter(|v| !v.is_null()),
                    output,
                    output_truncated: false,
                    output_blob_id: None,
                }
            }
        }
    }

    /// Subject of a permission request about this tool.
    pub fn subject(&self) -> Subject {
        match self.body() {
            ItemBody::CommandExecution { command, cwd, .. } => Subject::Command { command, cwd },
            ItemBody::FileChange { changes } => Subject::FileChange { changes },
            _ => Subject::Tool {
                name: self.name.clone().unwrap_or_else(|| self.display_title()),
                input: self.raw_input.clone().filter(|v| !v.is_null()),
            },
        }
    }
}

/// Unified diff of one file plus added/removed line counts.
pub fn unified_diff(path: &str, old: Option<&str>, new: &str) -> (String, u64, u64) {
    let old_text = old.unwrap_or("");
    let diff = similar::TextDiff::from_lines(old_text, new);
    let mut added = 0u64;
    let mut removed = 0u64;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let a = if old.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_owned()
    };
    let b = format!("b/{path}");
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(&a, &b)
        .to_string();
    (text, added, removed)
}

// ----- permissions --------------------------------------------------------------------------

/// Maps an ACP permission option kind. Unknown kinds return `None`: such options are not
/// offered to the user (mislabelling an "allow" as something else would be unsafe).
pub fn option_kind(kind: &str) -> Option<ApprovalOptionKind> {
    match kind {
        "allow_once" => Some(ApprovalOptionKind::AllowOnce),
        "allow_always" => Some(ApprovalOptionKind::AllowAlways),
        "reject_once" | "reject_always" => Some(ApprovalOptionKind::Deny),
        _ => None,
    }
}

/// Builds the approval request for `session/request_permission`. Returns the request and
/// the ids of options that were dropped because their kind is unknown.
pub fn permission_request(
    tool: &ToolState,
    options: &[PermissionOption],
) -> (InteractionRequest, Vec<String>) {
    let mut mapped = Vec::new();
    let mut dropped = Vec::new();
    for opt in options {
        match option_kind(&opt.kind) {
            Some(kind) => mapped.push(ApprovalOption {
                id: opt.option_id.clone(),
                label: if opt.name.is_empty() {
                    opt.option_id.clone()
                } else {
                    opt.name.clone()
                },
                kind,
            }),
            None => dropped.push(opt.option_id.clone()),
        }
    }
    let request = InteractionRequest::Approval {
        title: tool.display_title(),
        detail: None,
        subject: tool.subject(),
        options: mapped,
    };
    (request, dropped)
}

/// Option chosen when the user dismisses an approval: the first `reject_once`, else the
/// first `reject_always`. `None` means "answer `cancelled`".
pub fn dismiss_option(options: &[PermissionOption]) -> Option<String> {
    options
        .iter()
        .find(|o| o.kind == "reject_once")
        .or_else(|| options.iter().find(|o| o.kind == "reject_always"))
        .map(|o| o.option_id.clone())
}

// ----- turns --------------------------------------------------------------------------------

/// Outcome of a `session/prompt` response.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub status: TurnStatus,
    pub error: Option<TurnError>,
    pub notice: Option<(NoticeLevel, String, String)>,
}

/// Maps `stopReason` (mapping table in docs/adapters/acp.md).
pub fn stop_reason(reason: &str) -> TurnOutcome {
    let ok = |notice: Option<(NoticeLevel, String, String)>| TurnOutcome {
        status: TurnStatus::Completed,
        error: None,
        notice,
    };
    match reason {
        "end_turn" => ok(None),
        "max_tokens" => ok(Some((
            NoticeLevel::Warning,
            "The agent stopped because it reached its token limit.".to_owned(),
            "maxTokens".to_owned(),
        ))),
        "max_turn_requests" => ok(Some((
            NoticeLevel::Warning,
            "The agent stopped because it reached its limit of model requests for this turn."
                .to_owned(),
            "maxTurnRequests".to_owned(),
        ))),
        "cancelled" => TurnOutcome {
            status: TurnStatus::Interrupted,
            error: None,
            notice: None,
        },
        "refusal" => TurnOutcome {
            status: TurnStatus::Failed,
            error: Some(TurnError {
                message: "The agent refused to continue.".to_owned(),
                kind: "refusal".to_owned(),
            }),
            notice: None,
        },
        other => TurnOutcome {
            status: TurnStatus::Completed,
            error: None,
            notice: Some((
                NoticeLevel::Info,
                format!("The agent ended the turn with an unknown stop reason `{other}`."),
                "unknownStopReason".to_owned(),
            )),
        },
    }
}

/// Usage of a turn from the (unstable) `PromptResponse.usage`, plus the turn's cost when
/// known and the context-window occupancy of the last `usage_update` of the turn. A turn with
/// a context but no token counts gets zero counts so the context still reaches the engine.
pub fn turn_usage(
    usage: Option<&PromptUsage>,
    cost_usd: Option<f64>,
    context: Option<ContextUsage>,
) -> Option<Usage> {
    if usage.is_none() && cost_usd.is_none() && context.is_none() {
        return None;
    }
    let mut out = Usage::default();
    if let Some(u) = usage {
        out.input_tokens = u.input_tokens;
        out.output_tokens = u.output_tokens;
        out.cached_input_tokens = u.cached_read_tokens.unwrap_or(0);
        out.reasoning_tokens = u.thought_tokens.unwrap_or(0);
    }
    out.cost_usd = cost_usd;
    out.context = context;
    Some(out)
}

/// Context-window occupancy of a `usage_update` (`used` out of `size`); `None` when the agent
/// reports no window size.
pub fn context_usage(update: &UsageUpdate) -> Option<ContextUsage> {
    (update.size > 0).then_some(ContextUsage {
        used_tokens: update.used,
        window_tokens: update.size,
    })
}

// ----- plan, commands, content --------------------------------------------------------------

pub fn plan_entries(entries: &[PlanEntry]) -> Vec<ProtoPlanEntry> {
    entries
        .iter()
        .map(|e| ProtoPlanEntry {
            text: e.content.clone(),
            status: match e.status.as_str() {
                "completed" => PlanEntryStatus::Completed,
                "in_progress" => PlanEntryStatus::InProgress,
                _ => PlanEntryStatus::Pending,
            },
        })
        .collect()
}

/// A harness command for the composer: inserting `/name ` sends it as prompt text, which is
/// how ACP invokes commands.
pub fn command(c: &AvailableCommand) -> Command {
    Command {
        name: c.name.clone(),
        description: c.description.clone().filter(|d| !d.is_empty()),
        source: CommandSource::Harness,
        argument_hint: c
            .input
            .as_ref()
            .and_then(|i| i.hint.clone())
            .filter(|h| !h.is_empty()),
        action: CommandAction::InsertText {
            text: format!("/{} ", c.name),
        },
    }
}

/// Markdown rendering of a content block inside a message chunk. `None` for blocks with no
/// textual rendering (images, audio, unknown types).
pub fn content_text(block: &Value) -> Option<String> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => block.get("text").and_then(Value::as_str).map(str::to_owned),
        "resource_link" => {
            let uri = block.get("uri").and_then(Value::as_str)?;
            let label = block
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| block.get("name").and_then(Value::as_str))
                .unwrap_or(uri);
            Some(format!("[{label}]({uri})"))
        }
        "resource" => {
            let res = block.get("resource")?;
            let text = res.get("text").and_then(Value::as_str)?;
            let uri = res.get("uri").and_then(Value::as_str).unwrap_or_default();
            Some(format!("\n```\n{text}\n```\n<!-- {uri} -->\n"))
        }
        _ => None,
    }
}

// ----- session options (models, modes, thought levels) -------------------------------------

/// Config options / modes as last reported by the agent.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionOptions {
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
    #[serde(default)]
    pub modes: Option<SessionModeState>,
}

/// Whether the selectable lists (not the current selections) of two option sets differ.
pub fn option_lists_differ(a: &SessionOptions, b: &SessionOptions) -> bool {
    fn strip(o: &SessionOptions) -> SessionOptions {
        let mut o = o.clone();
        for c in &mut o.config_options {
            c.current_value = serde_json::Value::Null;
        }
        if let Some(m) = &mut o.modes {
            m.current_mode_id.clear();
        }
        o
    }
    strip(a) != strip(b)
}

/// Which setting a config option controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Model,
    Mode,
    Effort,
}

impl SettingKind {
    pub fn category(self) -> &'static str {
        match self {
            SettingKind::Model => "model",
            SettingKind::Mode => "mode",
            SettingKind::Effort => "thought_level",
        }
    }
}

impl SessionOptions {
    /// The select option of a category (first one wins when an agent sends several).
    pub fn option(&self, kind: SettingKind) -> Option<&ConfigOption> {
        self.config_options
            .iter()
            .find(|o| o.is_select() && o.category.as_deref() == Some(kind.category()))
    }

    pub fn current(&self, kind: SettingKind) -> Option<String> {
        if let Some(opt) = self.option(kind) {
            return opt.current().map(str::to_owned);
        }
        match kind {
            SettingKind::Mode => self
                .modes
                .as_ref()
                .map(|m| m.current_mode_id.clone())
                .filter(|m| !m.is_empty()),
            _ => None,
        }
    }

    /// Whether `value` is selectable for `kind`.
    pub fn accepts(&self, kind: SettingKind, value: &str) -> bool {
        if let Some(opt) = self.option(kind) {
            return opt.values().iter().any(|v| v.value == value);
        }
        match kind {
            SettingKind::Mode => self
                .modes
                .as_ref()
                .is_some_and(|m| m.available_modes.iter().any(|mode| mode.id == value)),
            _ => false,
        }
    }

    /// Whether the agent exposes a selector for `kind`.
    pub fn has(&self, kind: SettingKind) -> bool {
        self.option(kind).is_some() || (kind == SettingKind::Mode && self.modes.is_some())
    }

    pub fn set_current(&mut self, kind: SettingKind, value: &str) {
        if let Some(opt) = self
            .config_options
            .iter_mut()
            .find(|o| o.is_select() && o.category.as_deref() == Some(kind.category()))
        {
            opt.current_value = Value::String(value.to_owned());
        }
        if kind == SettingKind::Mode
            && let Some(m) = &mut self.modes
        {
            m.current_mode_id = value.to_owned();
        }
    }

    pub fn models(&self, default: Option<&str>) -> Vec<Model> {
        self.option(SettingKind::Model)
            .map(|o| {
                o.values()
                    .into_iter()
                    .map(|v| Model {
                        is_default: Some(v.value.as_str()) == default,
                        id: v.value,
                        display_name: v.name,
                        description: v.description,
                        effort_levels: None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn effort_levels(&self) -> Vec<EffortLevel> {
        self.option(SettingKind::Effort)
            .map(|o| {
                o.values()
                    .into_iter()
                    .map(|v| EffortLevel {
                        id: v.value,
                        label: v.name,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn permission_modes(&self, default: Option<&str>) -> Vec<PermissionMode> {
        if let Some(opt) = self.option(SettingKind::Mode) {
            return opt
                .values()
                .into_iter()
                .map(|v| PermissionMode {
                    is_default: Some(v.value.as_str()) == default,
                    id: v.value,
                    label: v.name,
                    description: v.description,
                })
                .collect();
        }
        self.modes
            .as_ref()
            .map(|m| {
                m.available_modes
                    .iter()
                    .map(|mode| PermissionMode {
                        id: mode.id.clone(),
                        label: mode.name.clone(),
                        description: mode.description.clone(),
                        is_default: Some(mode.id.as_str()) == default,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn fields(v: Value) -> ToolCallFields {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn execute_tool_uses_raw_input_command_and_text_output() {
        let mut t = ToolState::new("t1");
        t.merge(&fields(json!({
            "toolCallId": "t1", "title": "Ran echo", "kind": "execute",
            "content": [{"type": "content", "content": {"type": "resource", "resource": {"mimeType": "text/x-shellscript", "text": "echo hi", "uri": "tool://preview"}}}],
            "rawInput": {"command": "echo hi"}
        })));
        assert_eq!(
            t.body(),
            ItemBody::CommandExecution {
                command: "echo hi".into(),
                cwd: None,
                output: String::new(),
                output_truncated: false,
                output_blob_id: None,
                exit_code: None,
                duration_ms: None
            }
        );
        t.merge(&fields(json!({
            "toolCallId": "t1", "status": "in_progress",
            "content": [{"type": "content", "content": {"type": "text", "text": "hi\n"}}],
            "_meta": {"terminal_exit": {"terminal_id": "x", "exit_code": 0, "signal": null}}
        })));
        match t.body() {
            ItemBody::CommandExecution {
                output, exit_code, ..
            } => {
                assert_eq!(output, "hi\n");
                assert_eq!(exit_code, Some(0));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.terminal_status(), None);
        t.merge(&fields(json!({"toolCallId": "t1", "status": "completed"})));
        assert_eq!(t.terminal_status(), Some(ItemStatus::Completed));
    }

    #[test]
    fn execute_without_string_command_falls_back_to_title() {
        let mut t = ToolState::new("t");
        t.merge(&fields(json!({"toolCallId": "t", "title": "Run tests", "kind": "execute", "rawInput": {"argv": ["cargo", "test"]}})));
        assert!(
            matches!(t.body(), ItemBody::CommandExecution { command, .. } if command == "Run tests")
        );
    }

    #[test]
    fn edit_with_diff_becomes_file_change() {
        let mut t = ToolState::new("w");
        t.merge(&fields(json!({
            "toolCallId": "w", "title": "Wrote hello.txt", "kind": "edit",
            "content": [{"type": "diff", "path": "hello.txt", "newText": "hello from aas"}]
        })));
        match t.body() {
            ItemBody::FileChange { changes } => {
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].kind, FileChangeKind::Add);
                assert_eq!(changes[0].added, Some(1));
                assert_eq!(changes[0].removed, Some(0));
                let diff = changes[0].diff.as_deref().unwrap();
                assert!(diff.contains("--- /dev/null"), "{diff}");
                assert!(diff.contains("+++ b/hello.txt"), "{diff}");
                assert!(diff.contains("+hello from aas"), "{diff}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn update_diff_counts_lines() {
        let (diff, added, removed) = unified_diff("a.rs", Some("a\nb\nc\n"), "a\nB\nc\nd\n");
        assert_eq!((added, removed), (2, 1));
        assert!(diff.starts_with("--- a/a.rs\n+++ b/a.rs\n"), "{diff}");
    }

    #[test]
    fn edit_without_diff_and_other_kinds_are_tool_calls() {
        let mut t = ToolState::new("e");
        t.merge(&fields(
            json!({"toolCallId": "e", "title": "Edit x", "kind": "edit"}),
        ));
        assert!(matches!(
            t.body(),
            ItemBody::ToolCall {
                category: ToolCategory::Edit,
                ..
            }
        ));
        for (kind, cat) in [
            ("read", ToolCategory::Read),
            ("search", ToolCategory::Search),
            ("fetch", ToolCategory::Fetch),
            ("think", ToolCategory::Think),
            ("switch_mode", ToolCategory::Other),
            ("other", ToolCategory::Other),
            ("something_new", ToolCategory::Other),
        ] {
            let mut t = ToolState::new("x");
            t.merge(&fields(
                json!({"toolCallId": "x", "title": "T", "kind": kind, "rawOutput": {"n": 1}}),
            ));
            match t.body() {
                ItemBody::ToolCall {
                    category,
                    output,
                    name,
                    ..
                } => {
                    assert_eq!(category, cat, "{kind}");
                    assert_eq!(name, kind);
                    assert_eq!(output.as_deref(), Some("{\n  \"n\": 1\n}"));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn permission_options_map_by_kind_and_unknown_kinds_are_dropped() {
        let mut t = ToolState::new("p");
        t.merge(&fields(json!({"toolCallId": "p", "title": "Ran ping", "kind": "execute", "rawInput": {"command": "ping x"}})));
        let options: Vec<PermissionOption> = serde_json::from_value(json!([
            {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
            {"optionId": "allow_session", "name": "Yes, allow `ping` (this session)", "kind": "allow_always"},
            {"optionId": "weird", "name": "Weird", "kind": "allow_forever_and_ever"},
            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
            {"optionId": "reject_always", "name": "Never", "kind": "reject_always"}
        ]))
        .unwrap();
        let (req, dropped) = permission_request(&t, &options);
        assert_eq!(dropped, vec!["weird".to_owned()]);
        match req {
            InteractionRequest::Approval {
                title,
                subject,
                options,
                ..
            } => {
                assert_eq!(title, "Ran ping");
                assert_eq!(
                    subject,
                    Subject::Command {
                        command: "ping x".into(),
                        cwd: None
                    }
                );
                let kinds: Vec<_> = options.iter().map(|o| (o.id.as_str(), o.kind)).collect();
                assert_eq!(
                    kinds,
                    vec![
                        ("allow_once", ApprovalOptionKind::AllowOnce),
                        ("allow_session", ApprovalOptionKind::AllowAlways),
                        ("reject_once", ApprovalOptionKind::Deny),
                        ("reject_always", ApprovalOptionKind::Deny),
                    ]
                );
                assert_eq!(options[3].label, "Never");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(dismiss_option(&options), Some("reject_once".to_owned()));
        assert_eq!(dismiss_option(&options[..2]), None);
    }

    #[test]
    fn stop_reasons() {
        assert_eq!(stop_reason("end_turn").status, TurnStatus::Completed);
        assert_eq!(stop_reason("cancelled").status, TurnStatus::Interrupted);
        let refusal = stop_reason("refusal");
        assert_eq!(refusal.status, TurnStatus::Failed);
        assert_eq!(refusal.error.unwrap().kind, "refusal");
        let max = stop_reason("max_tokens");
        assert_eq!(max.status, TurnStatus::Completed);
        assert_eq!(max.notice.unwrap().2, "maxTokens");
        assert_eq!(
            stop_reason("max_turn_requests").notice.unwrap().2,
            "maxTurnRequests"
        );
        assert_eq!(
            stop_reason("new_reason").notice.unwrap().2,
            "unknownStopReason"
        );
    }

    #[test]
    fn usage_maps_prompt_usage() {
        let u = PromptUsage {
            input_tokens: 10,
            output_tokens: 2,
            thought_tokens: Some(3),
            cached_read_tokens: Some(7),
        };
        assert_eq!(
            turn_usage(Some(&u), Some(0.5), None),
            Some(Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 7,
                reasoning_tokens: 3,
                cost_usd: Some(0.5),
                context: None
            })
        );
        assert_eq!(turn_usage(None, None, None), None);
        // A reported context reaches the engine even without token counts.
        let ctx = ContextUsage {
            used_tokens: 11646,
            window_tokens: 202752,
        };
        assert_eq!(
            turn_usage(None, None, Some(ctx)),
            Some(Usage {
                context: Some(ctx),
                ..Usage::default()
            })
        );
    }

    #[test]
    fn context_needs_a_window_size() {
        let update: UsageUpdate = serde_json::from_value(
            json!({"sessionUpdate": "usage_update", "used": 11646, "size": 202752}),
        )
        .unwrap();
        assert_eq!(
            context_usage(&update),
            Some(ContextUsage {
                used_tokens: 11646,
                window_tokens: 202752
            })
        );
        let no_size: UsageUpdate =
            serde_json::from_value(json!({"sessionUpdate": "usage_update", "used": 5})).unwrap();
        assert_eq!(context_usage(&no_size), None);
    }

    #[test]
    fn commands_insert_slash_text() {
        let c: AvailableCommand = serde_json::from_value(
            json!({"name": "plan", "description": "Plan it", "input": {"hint": "[prompt]"}}),
        )
        .unwrap();
        let cmd = command(&c);
        assert_eq!(
            cmd.action,
            CommandAction::InsertText {
                text: "/plan ".into()
            }
        );
        assert_eq!(cmd.argument_hint.as_deref(), Some("[prompt]"));
        assert_eq!(cmd.source, CommandSource::Harness);
    }

    #[test]
    fn content_blocks_render_to_markdown() {
        assert_eq!(
            content_text(&json!({"type": "text", "text": "hi"})),
            Some("hi".into())
        );
        assert_eq!(
            content_text(&json!({"type": "resource_link", "uri": "file:///x.rs", "name": "x.rs"})),
            Some("[x.rs](file:///x.rs)".into())
        );
        assert_eq!(
            content_text(&json!({"type": "image", "data": "AAAA", "mimeType": "image/png"})),
            None
        );
    }

    #[test]
    fn session_options_prefer_config_options_over_modes() {
        let opts: SessionOptions = serde_json::from_value(json!({
            "config_options": [
                {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": "code",
                 "options": [{"value": "code", "name": "Code", "description": "Write"}, {"value": "ask", "name": "Ask"}]},
                {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "m2",
                 "options": [{"value": "m1", "name": "M1"}, {"value": "m2", "name": "M2"}]}
            ],
            "modes": {"currentModeId": "legacy", "availableModes": [{"id": "legacy", "name": "Legacy"}]}
        }))
        .unwrap();
        assert_eq!(opts.current(SettingKind::Mode).as_deref(), Some("code"));
        assert_eq!(opts.current(SettingKind::Model).as_deref(), Some("m2"));
        assert_eq!(opts.current(SettingKind::Effort), None);
        assert!(opts.accepts(SettingKind::Model, "m1"));
        assert!(!opts.accepts(SettingKind::Model, "m9"));
        assert!(!opts.has(SettingKind::Effort));
        let modes = opts.permission_modes(Some("code"));
        assert_eq!(modes.len(), 2);
        assert!(modes[0].is_default);
        let models = opts.models(Some("m2"));
        assert!(models[1].is_default && !models[0].is_default);

        let only_modes: SessionOptions = serde_json::from_value(json!({
            "modes": {"currentModeId": "a", "availableModes": [{"id": "a", "name": "A"}, {"id": "b", "name": "B"}]}
        }))
        .unwrap();
        assert_eq!(only_modes.current(SettingKind::Mode).as_deref(), Some("a"));
        assert!(only_modes.accepts(SettingKind::Mode, "b"));
        assert_eq!(only_modes.permission_modes(None).len(), 2);
    }
}
