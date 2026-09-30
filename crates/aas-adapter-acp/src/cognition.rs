//! Cognition's ACP extensions for background work, as Devin CLI implements them (shapes
//! recorded from Devin CLI 3000.11.3; mapping tables in docs/adapters/acp.md §16).
//!
//! Devin runs two kinds of work outside the prompt turn and reports both only through its own
//! `_meta` keys:
//!
//! * **Background sub-agents** (`run_subagent` with `is_background`). A `tool_call_update`
//!   whose `toolCallId` is the agent id carries `_meta["cognition.ai/subagent_started"]`
//!   (`agentId`, `title`, `isBackground`), and later one carrying
//!   `_meta["cognition.ai/subagent_completed"]` (`agentId`, `success`, `summary`). Every update
//!   the sub-agent itself produces carries `_meta["cognition.ai/subagent_context"]
//!   .parentAgentId` = its own agent id (the root agent tags a copy of its usage `"root"`).
//! * **Background shells**. An `exec` tool call whose update carries
//!   `_meta["cognition.ai/backgroundShellId"]` keeps running after its turn; it ends with an
//!   update of that tool call carrying `_meta.terminal_exit` whose `terminal_id` is that id
//!   (with `exit_code`). While it runs, updates marked `_meta["cognition.ai/terminalPreview"]`
//!   carry the whole output so far (about once a second); they are relayed as the task's output
//!   (appended text when a preview continues the last one, the whole preview otherwise).
//!
//! They are stopped with `_cognition.ai/subagent/cancel {sessionId, agentId}` and
//! `_cognition.ai/terminal/killBackgroundShell {sessionId, shellId}`. Both answer `{}` for any
//! id; the end arrives as the signals above.
//!
//! The client asks for these signals by declaring `cognition.ai/subagentSupport` and
//! `cognition.ai/subagentControl` in `clientCapabilities._meta` (ACP's rule for custom
//! capabilities; agents that do not know them ignore them). Everything else here is used only
//! when the agent confirms the extension in its `initialize` answer
//! (`agentCapabilities._meta["cognition.ai/subagentControl"] = true`), never because of the
//! executable's name. Only explicit fields are read: no text written for people is parsed,
//! and nothing ends because time passed.
//!
//! [`Extensions`] is what the agent confirmed of Cognition's extensions as a whole: background
//! work (here), steps and forks at a step (`crate::revert`), the session's name, and whether the
//! agent is one of Cognition's at all (its own commands and notifications, `crate::stats`).

use std::collections::HashMap;

use aas_harness::protocol::{BackgroundProgress, BackgroundTaskKind, ItemStatus};
use aas_harness::{
    AdapterError, AdapterEvent, BackgroundOutcome, BackgroundState, BackgroundTaskInfo,
    BackgroundTasks, OutputUpdate,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::mapping::ToolState;
use crate::tracker::{Emit, Tracker};
use crate::wire::{AvailableCommand, InitializeResponse, SessionUpdate, ToolCallFields};

/// Client capability: send the structured sub-agent signals.
const SUBAGENT_SUPPORT: &str = "cognition.ai/subagentSupport";
/// Client capability (and the agent's confirmation of the extension): sub-agent control.
const SUBAGENT_CONTROL: &str = "cognition.ai/subagentControl";
/// Client capability (and the agent's confirmation): the steps of a session and forks at a
/// step (`crate::revert`). Recorded with Devin CLI 3000.11.3: declaring it changes nothing
/// else (the same `session/new` answer, commands and per-turn updates), and the agent then also
/// confirms `cognition.ai/revertHistoryRewound`, which is only about `revert/execute` (not
/// used).
const REVERT: &str = "cognition.ai/revert";
/// The agent's capability: `_cognition.ai/session/rename`.
const SESSION_RENAME: &str = "cognition.ai/sessionRename";
/// The namespace of Cognition's capabilities.
const NAMESPACE: &str = "cognition.ai/";
/// `_cognition.ai/session/rename {sessionId, title}`: gives the session a title (answers `{}`
/// and echoes it as `session_info_update`).
pub const RENAME_SESSION: &str = "_cognition.ai/session/rename";

/// Commands of Cognition's agents this adapter does not offer, by name as Devin CLI 3000.11.3
/// lists them (`available_commands_update`, `_meta["cognition.ai/category"] = "Account"`):
///
/// * `login`, `logout`: the CLI's own sign-in. Signing in from the phone is out of scope
///   (design.md §1, the harness's login), and `/logout` signs the PC's Devin out, which breaks
///   every Devin session of the daemon and of the user's own terminals. `status` (the
///   sign-in status) stays: it changes nothing.
///
/// The list is only applied to an agent that declares Cognition's capabilities
/// ([`Extensions::cognition`]); another ACP agent's commands of the same names are its own.
pub const HIDDEN_COMMANDS: &[&str] = &["login", "logout"];

/// Cognition's extensions the agent confirmed in its `initialize` answer
/// (`agentCapabilities._meta`), never because of the executable's name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Extensions {
    /// The agent declares capabilities in Cognition's namespace (`cognition.ai/…` set to a
    /// value other than `false` or `null`): it is one of Cognition's agents, whose own commands
    /// ([`HIDDEN_COMMANDS`]) and notifications (`crate::stats`) this adapter knows.
    pub cognition: bool,
    /// `cognition.ai/subagentControl`: background sub-agents and shells (this module).
    pub background: bool,
    /// `cognition.ai/revert`: steps and forks at a step (`crate::revert`).
    pub revert: bool,
    /// `cognition.ai/sessionRename`: [`RENAME_SESSION`].
    pub rename: bool,
}

/// What the agent confirmed of Cognition's extensions.
pub fn extensions(init: &InitializeResponse) -> Extensions {
    let meta = init
        .agent_capabilities
        .meta
        .as_ref()
        .and_then(Value::as_object);
    let flag = |key: &str| meta.and_then(|m| m.get(key)).and_then(Value::as_bool) == Some(true);
    Extensions {
        cognition: meta.is_some_and(|m| {
            m.iter().any(|(k, v)| {
                k.starts_with(NAMESPACE) && !matches!(v, Value::Null | Value::Bool(false))
            })
        }),
        background: flag(SUBAGENT_CONTROL),
        revert: flag(REVERT),
        rename: flag(SESSION_RENAME),
    }
}

/// The agent's commands without [`HIDDEN_COMMANDS`] when the agent is one of Cognition's.
pub fn visible_commands(commands: &[AvailableCommand], ext: Extensions) -> Vec<AvailableCommand> {
    commands
        .iter()
        .filter(|c| !(ext.cognition && HIDDEN_COMMANDS.contains(&c.name.as_str())))
        .cloned()
        .collect()
}
const SUBAGENT_CONTEXT: &str = "cognition.ai/subagent_context";
const SUBAGENT_STARTED: &str = "cognition.ai/subagent_started";
const SUBAGENT_COMPLETED: &str = "cognition.ai/subagent_completed";
const BACKGROUND_SHELL_ID: &str = "cognition.ai/backgroundShellId";
const BACKGROUND_COMMAND: &str = "cognition.ai/backgroundCommand";
const TERMINAL_EXIT: &str = "terminal_exit";
/// `_meta` flag of an update whose content is the running command's whole output so far.
const TERMINAL_PREVIEW: &str = "cognition.ai/terminalPreview";
const INPUT_TOKENS: &str = "cognition.ai/inputTokens";
const OUTPUT_TOKENS: &str = "cognition.ai/outputTokens";
/// `parentAgentId` of the root agent's own updates.
const ROOT_AGENT: &str = "root";
const CANCEL_SUBAGENT: &str = "_cognition.ai/subagent/cancel";
const KILL_BACKGROUND_SHELL: &str = "_cognition.ai/terminal/killBackgroundShell";
/// Task keys carry the kind of id, so that a sub-agent id and a shell id can never collide
/// (the port requires keys unique within the session).
const AGENT_KEY_PREFIX: &str = "subagent:";
const SHELL_KEY_PREFIX: &str = "shell:";

/// `clientCapabilities._meta` every session declares. Devin 3000.11.3 confirms
/// `cognition.ai/subagentControl` and `cognition.ai/revert` only when the client declared them.
pub fn client_meta() -> Value {
    json!({ SUBAGENT_SUPPORT: true, SUBAGENT_CONTROL: true, REVERT: true })
}

/// Task key of a background sub-agent.
pub fn agent_key(agent_id: &str) -> String {
    format!("{AGENT_KEY_PREFIX}{agent_id}")
}

/// Task key of a background shell.
pub fn shell_key(shell_id: &str) -> String {
    format!("{SHELL_KEY_PREFIX}{shell_id}")
}

/// The agent id a `subagent_context` tag names, when it is not the root agent.
fn sub_agent_tag(meta: Option<&Value>) -> Option<&str> {
    meta?
        .get(SUBAGENT_CONTEXT)?
        .get("parentAgentId")?
        .as_str()
        .filter(|id| !id.is_empty() && *id != ROOT_AGENT)
}

/// Decodes one `_meta` entry; an entry that is absent or has another shape is `None`.
fn meta_entry<T: DeserializeOwned>(meta: Option<&Value>, key: &str) -> Option<T> {
    serde_json::from_value(meta?.get(key)?.clone()).ok()
}

fn meta_str<'a>(meta: Option<&'a Value>, key: &str) -> Option<&'a str> {
    meta?.get(key)?.as_str().filter(|s| !s.is_empty())
}

/// Whether a replayed update belongs to a sub-agent (or announces one) rather than to the
/// root agent's conversation. Used when a session's history is read.
pub fn is_sub_agent_update(meta: Option<&Value>) -> bool {
    sub_agent_tag(meta).is_some()
        || meta.is_some_and(|m| {
            m.get(SUBAGENT_STARTED).is_some() || m.get(SUBAGENT_COMPLETED).is_some()
        })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentStarted {
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    is_background: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentCompleted {
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TerminalExit {
    #[serde(default)]
    terminal_id: Option<String>,
    #[serde(default)]
    exit_code: Option<i64>,
}

/// What a stop request for a task asks of the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopTarget {
    SubAgent(String),
    Shell(String),
}

impl StopTarget {
    /// Method and params of the request.
    pub fn request(&self, session_id: &str) -> (&'static str, Value) {
        match self {
            StopTarget::SubAgent(id) => (
                CANCEL_SUBAGENT,
                json!({ "sessionId": session_id, "agentId": id }),
            ),
            StopTarget::Shell(id) => (
                KILL_BACKGROUND_SHELL,
                json!({ "sessionId": session_id, "shellId": id }),
            ),
        }
    }
}

/// Where an item-producing update goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    /// The root agent's (or a foreground sub-agent's tool call): an item of the turn.
    Root,
    /// Text or a plan of a sub-agent: part of its own conversation, not of the turn's.
    Hidden,
    /// Background work: handled here, its events are in the output.
    Handled,
    /// About a sub-agent but carrying nothing this extension maps: forwarded as `Native`.
    Unmapped,
}

#[derive(Debug, Default)]
struct SubAgent {
    background: bool,
    ended: bool,
    tool_uses: u64,
    last_tool: Option<String>,
    tokens: u64,
}

#[derive(Debug)]
struct Shell {
    shell_id: String,
    state: ToolState,
    /// `terminal_exit` for this shell arrived.
    exited: bool,
    exit_code: Option<i32>,
    /// The output last relayed from a preview (`None` before the first one).
    streamed: Option<String>,
}

/// Devin's background work in one session.
#[derive(Debug, Default)]
pub struct Background {
    tasks: BackgroundTasks,
    /// Every sub-agent the agent announced (background or not), by agent id.
    agents: HashMap<String, SubAgent>,
    /// Tool calls a sub-agent made: tool call id → agent id. Later updates of a tool call do
    /// not always repeat the tag (e.g. its `terminal_exit`).
    tool_owner: HashMap<String, String>,
    /// Tool calls of background sub-agents, as accumulated (they are not items of a turn).
    agent_tools: HashMap<String, ToolState>,
    /// Background shells by the tool call that runs them.
    shells: HashMap<String, Shell>,
}

impl Background {
    pub fn new() -> Self {
        Self::default()
    }

    /// Routes one item-producing `session/update` (`meta` is the update's `_meta`). `root` is
    /// the tracker of the turn's items; `in_turn` whether a turn runs.
    pub fn on_update(
        &mut self,
        update: &SessionUpdate,
        meta: Option<&Value>,
        root: &mut Tracker,
        in_turn: bool,
        out: &mut Vec<Emit>,
    ) -> Routed {
        match update {
            SessionUpdate::ToolCall(f) => self.on_tool(f, true, root, in_turn, out),
            SessionUpdate::ToolCallUpdate(f) => self.on_tool(f, false, root, in_turn, out),
            SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::Plan(_) => {
                if sub_agent_tag(meta).is_some() {
                    Routed::Hidden
                } else {
                    Routed::Root
                }
            }
            _ => Routed::Root,
        }
    }

    /// A `usage_update`. One tagged with a sub-agent is that sub-agent's (it counts towards
    /// its progress and never towards the root agent's context): returns `true`.
    pub fn on_usage(&mut self, meta: Option<&Value>, out: &mut Vec<Emit>) -> bool {
        let Some(agent) = sub_agent_tag(meta) else {
            return false;
        };
        let agent = agent.to_owned();
        let tokens: u64 = [INPUT_TOKENS, OUTPUT_TOKENS]
            .iter()
            .filter_map(|k| meta.and_then(|m| m.get(*k)).and_then(Value::as_u64))
            .sum();
        if let Some(a) = self.agents.get_mut(&agent)
            && a.background
            && !a.ended
        {
            a.tokens += tokens;
            self.publish_progress(&agent, out);
        }
        true
    }

    /// The background task a request about tool call `fields` belongs to: the background
    /// shell it runs, or the background sub-agent that made it (by its explicit tag). `None`
    /// for the root agent's own tool calls and for foreground sub-agents.
    pub fn task_of_tool(&mut self, fields: &ToolCallFields) -> Option<String> {
        if let Some(shell) = self.shells.get(&fields.tool_call_id) {
            let key = shell_key(&shell.shell_id);
            return self.running(&key).then_some(key);
        }
        let owner = self.owner(fields)?;
        let key = agent_key(&owner);
        (self.is_background_agent(&owner) && self.running(&key)).then_some(key)
    }

    /// The tool call as this extension knows it, with `fields` applied (for the subject of a
    /// request): a background shell's or a background sub-agent's. `None` otherwise.
    pub fn tool_view(&self, fields: &ToolCallFields) -> Option<ToolState> {
        let known = self
            .shells
            .get(&fields.tool_call_id)
            .map(|s| &s.state)
            .or_else(|| self.agent_tools.get(&fields.tool_call_id))?;
        let mut state = known.clone();
        state.merge(fields);
        Some(state)
    }

    /// The task `key` names, when it can be stopped now.
    pub fn stop_target(&self, key: &str) -> Result<StopTarget, AdapterError> {
        let task = self
            .tasks
            .get(key)
            .ok_or_else(|| AdapterError::Other(format!("unknown background task `{key}`")))?;
        if task.state.is_ended() {
            return Err(AdapterError::Other(format!(
                "background task `{key}` has already ended"
            )));
        }
        if let Some(id) = key.strip_prefix(AGENT_KEY_PREFIX) {
            Ok(StopTarget::SubAgent(id.to_owned()))
        } else if let Some(id) = key.strip_prefix(SHELL_KEY_PREFIX) {
            Ok(StopTarget::Shell(id.to_owned()))
        } else {
            Err(AdapterError::Other(format!(
                "unknown background task `{key}`"
            )))
        }
    }

    fn running(&self, key: &str) -> bool {
        self.tasks.get(key).is_some_and(|t| !t.state.is_ended())
    }

    fn is_background_agent(&self, agent: &str) -> bool {
        self.agents.get(agent).is_some_and(|a| a.background)
    }

    /// The sub-agent that made a tool call: remembered from its first tagged update.
    fn owner(&mut self, fields: &ToolCallFields) -> Option<String> {
        if let Some(agent) = self.tool_owner.get(&fields.tool_call_id) {
            return Some(agent.clone());
        }
        let agent = sub_agent_tag(fields.meta.as_ref())?.to_owned();
        self.tool_owner
            .insert(fields.tool_call_id.clone(), agent.clone());
        Some(agent)
    }

    fn on_tool(
        &mut self,
        f: &ToolCallFields,
        is_start: bool,
        root: &mut Tracker,
        in_turn: bool,
        out: &mut Vec<Emit>,
    ) -> Routed {
        let meta = f.meta.as_ref();
        if let Some(started) = meta_entry::<SubagentStarted>(meta, SUBAGENT_STARTED) {
            self.agent_started(&f.tool_call_id, started, meta, out);
            return Routed::Handled;
        }
        if let Some(done) = meta_entry::<SubagentCompleted>(meta, SUBAGENT_COMPLETED) {
            return self.agent_completed(&f.tool_call_id, done, f.status.as_deref(), out);
        }
        if self.agents.contains_key(&f.tool_call_id) {
            // The sub-agent's own pseudo tool call, without a start or an end.
            return Routed::Unmapped;
        }
        if self.shells.contains_key(&f.tool_call_id) {
            self.shell_update(f, out);
            return Routed::Handled;
        }
        let owner = self.owner(f);
        if let Some(shell_id) = meta_str(meta, BACKGROUND_SHELL_ID) {
            let shell_id = shell_id.to_owned();
            self.shell_started(f, &shell_id, owner, root, in_turn, out);
            return Routed::Handled;
        }
        match owner {
            Some(agent) if self.is_background_agent(&agent) => {
                self.agent_tool(&agent, f, is_start, out);
                Routed::Handled
            }
            // The root agent's, or a foreground sub-agent's: part of the turn.
            _ => Routed::Root,
        }
    }

    fn agent_started(
        &mut self,
        tool_call_id: &str,
        s: SubagentStarted,
        meta: Option<&Value>,
        out: &mut Vec<Emit>,
    ) {
        let id = if s.agent_id.is_empty() {
            tool_call_id.to_owned()
        } else {
            s.agent_id
        };
        let background = s.is_background == Some(true);
        let agent = self.agents.entry(id.clone()).or_default();
        if agent.ended {
            // A new run of the same agent starts from scratch.
            *agent = SubAgent::default();
        }
        agent.background = background;
        if !background {
            return;
        }
        let parent_key = sub_agent_tag(meta)
            .filter(|p| self.is_background_agent(p))
            .map(agent_key);
        let title = s
            .title
            .filter(|t| !t.is_empty())
            .or(s.task.filter(|t| !t.is_empty()))
            .unwrap_or_else(|| id.clone());
        let mut task = BackgroundTaskInfo::new(agent_key(&id), BackgroundTaskKind::Agent, title);
        task.parent_key = parent_key;
        task.stoppable = true;
        self.start(task, out);
    }

    fn agent_completed(
        &mut self,
        tool_call_id: &str,
        c: SubagentCompleted,
        status: Option<&str>,
        out: &mut Vec<Emit>,
    ) -> Routed {
        let id = if c.agent_id.is_empty() {
            tool_call_id.to_owned()
        } else {
            c.agent_id
        };
        let Some(agent) = self.agents.get_mut(&id) else {
            return Routed::Unmapped;
        };
        agent.ended = true;
        if !agent.background {
            return Routed::Handled;
        }
        // `success` says whether the sub-agent's run ended normally (a cancel is `false`).
        let state = match (c.success, status) {
            (Some(true), _) => BackgroundState::Completed,
            (Some(false), _) | (None, Some("failed")) => BackgroundState::Failed,
            (None, _) => BackgroundState::Completed,
        };
        let result = c
            .summary
            .filter(|s| !s.is_empty())
            .map(|summary| BackgroundOutcome {
                summary: Some(summary),
                ..BackgroundOutcome::default()
            });
        self.end(&agent_key(&id), state, result, out);
        Routed::Handled
    }

    /// A tool call of a background sub-agent: its progress.
    fn agent_tool(&mut self, agent: &str, f: &ToolCallFields, is_start: bool, out: &mut Vec<Emit>) {
        let state = self.agent_tool_state(f);
        if is_start && let Some(a) = self.agents.get_mut(agent) {
            a.tool_uses += 1;
            a.last_tool = Some(tool_name(&state));
        }
        self.publish_progress(agent, out);
    }

    fn agent_tool_state(&mut self, f: &ToolCallFields) -> ToolState {
        let state = self
            .agent_tools
            .entry(f.tool_call_id.clone())
            .or_insert_with(|| ToolState::new(&f.tool_call_id));
        state.merge(f);
        state.clone()
    }

    fn shell_started(
        &mut self,
        f: &ToolCallFields,
        shell_id: &str,
        owner: Option<String>,
        root: &mut Tracker,
        in_turn: bool,
        out: &mut Vec<Emit>,
    ) {
        let (state, origin) = match &owner {
            None if in_turn => {
                // The root agent's command of this turn: its item is where the shell came from.
                let key = root.tool_item(f, out);
                let origin = root.is_open(&f.tool_call_id).then_some(key);
                let state = root
                    .tool(&f.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| root.peek_tool(f));
                (state, origin)
            }
            None => (root.peek_tool(f), None),
            Some(_) => (self.agent_tool_state(f), None),
        };
        let parent_key = owner
            .as_deref()
            .filter(|a| self.is_background_agent(a))
            .map(agent_key);
        let title = meta_str(f.meta.as_ref(), BACKGROUND_COMMAND)
            .map(str::to_owned)
            .unwrap_or_else(|| state.command());
        let mut task =
            BackgroundTaskInfo::new(shell_key(shell_id), BackgroundTaskKind::Shell, title);
        task.origin_item_key = origin.clone();
        task.parent_key = parent_key;
        task.stoppable = true;
        self.start(task, out);
        if origin.is_some() {
            root.close_backgrounded(&f.tool_call_id, out);
        }
        self.shells.insert(
            f.tool_call_id.clone(),
            Shell {
                shell_id: shell_id.to_owned(),
                state,
                exited: false,
                exit_code: None,
                streamed: None,
            },
        );
        // The same update may already report the end.
        self.shell_update(f, out);
    }

    /// An update of a background shell's tool call. Its end is the shell's `terminal_exit`, or
    /// a terminal `status` of the tool call, whichever comes first; a later terminal report
    /// only adds explicit fields the result does not have yet.
    fn shell_update(&mut self, f: &ToolCallFields, out: &mut Vec<Emit>) {
        let Some(shell) = self.shells.get_mut(&f.tool_call_id) else {
            return;
        };
        shell.state.merge(f);
        if let Some(exit) = meta_entry::<TerminalExit>(f.meta.as_ref(), TERMINAL_EXIT)
            && exit.terminal_id.as_deref() == Some(shell.shell_id.as_str())
        {
            shell.exited = true;
            shell.exit_code = exit.exit_code.and_then(|c| i32::try_from(c).ok());
        }
        let status = shell.state.terminal_status();
        if !shell.exited && status.is_none() {
            // A preview of the running shell: what it printed since the last one (the first
            // preview, or one that does not continue the last, replaces the output).
            if f.meta
                .as_ref()
                .and_then(|m| m.get(TERMINAL_PREVIEW))
                .and_then(Value::as_bool)
                == Some(true)
            {
                let text = shell.state.output_text();
                let update = match &shell.streamed {
                    Some(last) if *last == text => None,
                    Some(last) if text.starts_with(last.as_str()) => {
                        Some(OutputUpdate::Append(text[last.len()..].to_owned()))
                    }
                    _ => Some(OutputUpdate::Replace(text.clone())),
                };
                shell.streamed = Some(text);
                let key = shell_key(&shell.shell_id);
                if let Some(output) = update
                    && self.running(&key)
                {
                    out.push(Emit::Event(AdapterEvent::BackgroundOutput { key, output }));
                }
            }
            return;
        }
        let state = if status == Some(ItemStatus::Failed) {
            BackgroundState::Failed
        } else {
            BackgroundState::Completed
        };
        let output = shell.state.output_text();
        let outcome = BackgroundOutcome {
            summary: None,
            exit_code: shell.exit_code,
            output: (!output.is_empty()).then_some(output),
            output_omitted_bytes: None,
        };
        let key = shell_key(&shell.shell_id);
        if self.running(&key) {
            self.end(&key, state, Some(outcome), out);
        } else if let Some(task) = self.tasks.update(&key, |t| match t.result.as_mut() {
            Some(r) => {
                if r.exit_code.is_none() {
                    r.exit_code = outcome.exit_code;
                }
                if r.output.is_none() {
                    r.output = outcome.output;
                }
            }
            None => t.result = Some(outcome),
        }) {
            out.push(task_event(task));
        }
    }

    /// Reports a start: a new task, or a new run of an ended one. Live while it runs (Devin
    /// has no live-set report; the start and end signals are the set).
    fn start(&mut self, task: BackgroundTaskInfo, out: &mut Vec<Emit>) {
        let key = task.key.clone();
        let started = self.tasks.started(task).is_some();
        let live = self.tasks.update(&key, |t| t.live = true).is_some();
        if (started || live)
            && let Some(task) = self.tasks.get(&key)
        {
            out.push(task_event(task.clone()));
        }
    }

    /// Reports the end of a running task (the first end wins).
    fn end(
        &mut self,
        key: &str,
        state: BackgroundState,
        result: Option<BackgroundOutcome>,
        out: &mut Vec<Emit>,
    ) {
        let changed = self.tasks.update(key, |t| {
            if t.state.is_ended() {
                return;
            }
            t.state = state;
            t.live = false;
            if result.is_some() {
                t.result = result;
            }
        });
        if let Some(task) = changed {
            out.push(task_event(task));
        }
    }

    fn publish_progress(&mut self, agent: &str, out: &mut Vec<Emit>) {
        let Some(a) = self.agents.get(agent) else {
            return;
        };
        let progress = BackgroundProgress {
            last_tool_name: a.last_tool.clone(),
            tool_uses: (a.tool_uses > 0).then_some(a.tool_uses),
            tokens: (a.tokens > 0).then_some(a.tokens),
            ..BackgroundProgress::default()
        };
        let changed = self.tasks.update(&agent_key(agent), |t| {
            if !t.state.is_ended() {
                t.progress = Some(progress);
            }
        });
        if let Some(task) = changed {
            out.push(task_event(task));
        }
    }
}

fn task_event(task: BackgroundTaskInfo) -> Emit {
    Emit::Event(AdapterEvent::BackgroundTask {
        task: Box::new(task),
    })
}

/// The tool's own name (`_meta["cognition.ai/inferenceToolName"]`, e.g. `exec`), else its
/// title.
fn tool_name(state: &ToolState) -> String {
    state
        .inference_tool_name()
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .or_else(|| state.name.clone())
        .or_else(|| state.title.clone())
        .or_else(|| state.kind.clone())
        .unwrap_or_else(|| "tool".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn upd(v: Value) -> (SessionUpdate, Option<Value>) {
        let meta = v.get("_meta").cloned();
        (SessionUpdate::parse(v), meta)
    }

    /// Routes `v` and returns the route, the task reports and the other events it produced.
    fn route(
        bg: &mut Background,
        root: &mut Tracker,
        in_turn: bool,
        v: Value,
    ) -> (Routed, Vec<BackgroundTaskInfo>, Vec<AdapterEvent>) {
        let (u, meta) = upd(v);
        let mut out = Vec::new();
        let routed = bg.on_update(&u, meta.as_ref(), root, in_turn, &mut out);
        let mut tasks = Vec::new();
        let mut others = Vec::new();
        for e in out {
            match e {
                Emit::Event(AdapterEvent::BackgroundTask { task }) => tasks.push(*task),
                Emit::Event(ev) => others.push(ev),
                other => panic!("{other:?}"),
            }
        }
        (routed, tasks, others)
    }

    fn started(id: &str, background: bool, parent: Option<&str>) -> Value {
        let mut meta = json!({SUBAGENT_STARTED: {"agentId": id, "title": format!("agent {id}"), "task": "t", "isBackground": background}});
        if let Some(p) = parent {
            meta[SUBAGENT_CONTEXT] = json!({"parentAgentId": p});
        }
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "in_progress", "_meta": meta})
    }

    fn tagged_tool(id: &str, agent: &str) -> Value {
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": "Ran x", "kind": "execute",
            "rawInput": {"command": "x"},
            "_meta": {"cognition.ai/inferenceToolName": "exec", SUBAGENT_CONTEXT: {"parentAgentId": agent}}})
    }

    #[test]
    fn the_extensions_are_confirmed_only_by_the_agent() {
        let init = |caps: Value| -> InitializeResponse {
            serde_json::from_value(json!({"protocolVersion": 1, "agentCapabilities": caps}))
                .unwrap()
        };
        let all = extensions(&init(json!({"_meta": {
            SUBAGENT_CONTROL: true, REVERT: true, SESSION_RENAME: true,
            "cognition.ai/sessionListOrderBy": ["updated_at"]}})));
        assert_eq!(
            all,
            Extensions {
                cognition: true,
                background: true,
                revert: true,
                rename: true
            }
        );
        let off = extensions(&init(json!({"_meta": {SUBAGENT_CONTROL: false}})));
        assert_eq!(off, Extensions::default());
        // Another capability of the namespace makes it Cognition's agent, nothing more.
        let other = extensions(&init(
            json!({"_meta": {"cognition.ai/terminalLifecycle": true}}),
        ));
        assert_eq!(
            other,
            Extensions {
                cognition: true,
                ..Extensions::default()
            }
        );
        // Other namespaces and no `_meta` at all: a plain ACP agent.
        assert_eq!(
            extensions(&init(json!({"_meta": {"example.com/x": true}}))),
            Extensions::default()
        );
        assert_eq!(extensions(&init(json!({}))), Extensions::default());
        assert_eq!(
            client_meta(),
            json!({"cognition.ai/subagentSupport": true, "cognition.ai/subagentControl": true,
                   "cognition.ai/revert": true})
        );
    }

    #[test]
    fn account_commands_are_hidden_only_for_cognitions_agents() {
        let commands: Vec<AvailableCommand> = ["login", "logout", "status", "plan"]
            .iter()
            .map(|name| AvailableCommand {
                name: (*name).into(),
                description: None,
                input: None,
            })
            .collect();
        let names = |ext| -> Vec<String> {
            visible_commands(&commands, ext)
                .into_iter()
                .map(|c| c.name)
                .collect()
        };
        assert_eq!(
            names(Extensions {
                cognition: true,
                ..Extensions::default()
            }),
            vec!["status", "plan"]
        );
        assert_eq!(
            names(Extensions::default()),
            vec!["login", "logout", "status", "plan"]
        );
    }

    #[test]
    fn nested_background_sub_agents_name_their_parent() {
        let (mut bg, mut root) = (Background::new(), Tracker::new());
        let (r, tasks, _) = route(&mut bg, &mut root, true, started("a1", true, None));
        assert_eq!(r, Routed::Handled);
        assert_eq!(tasks[0].key, "subagent:a1");
        assert_eq!(tasks[0].title, "agent a1");
        assert!(tasks[0].live && tasks[0].stoppable);
        let (_, tasks, _) = route(&mut bg, &mut root, false, started("a2", true, Some("a1")));
        assert_eq!(tasks[0].parent_key.as_deref(), Some("subagent:a1"));
        // A shell of the nested agent names that agent.
        route(&mut bg, &mut root, false, tagged_tool("c1", "a2"));
        let (r, tasks, others) = route(
            &mut bg,
            &mut root,
            false,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "in_progress",
                "_meta": {BACKGROUND_SHELL_ID: "s9", SUBAGENT_CONTEXT: {"parentAgentId": "a2"}}}),
        );
        assert_eq!(r, Routed::Handled);
        assert!(others.is_empty(), "no item for a sub-agent's command");
        assert_eq!(tasks[0].key, "shell:s9");
        assert_eq!(
            tasks[0].title, "x",
            "the tool's command without a backgroundCommand"
        );
        assert_eq!(tasks[0].parent_key.as_deref(), Some("subagent:a2"));
        assert_eq!(tasks[0].origin_item_key, None);
    }

    #[test]
    fn foreground_sub_agents_are_part_of_the_turn() {
        let (mut bg, mut root) = (Background::new(), Tracker::new());
        let (r, tasks, _) = route(&mut bg, &mut root, true, started("f1", false, None));
        assert_eq!((r, tasks.len()), (Routed::Handled, 0));
        // Its tool calls are items of the turn; its text is not.
        let (r, _, _) = route(&mut bg, &mut root, true, tagged_tool("c1", "f1"));
        assert_eq!(r, Routed::Root);
        let (r, _, _) = route(
            &mut bg,
            &mut root,
            true,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"},
                "_meta": {SUBAGENT_CONTEXT: {"parentAgentId": "f1"}}}),
        );
        assert_eq!(r, Routed::Hidden);
        // Requests about its tool calls belong to the turn.
        let fields: ToolCallFields = serde_json::from_value(json!({"toolCallId": "c1"})).unwrap();
        assert_eq!(bg.task_of_tool(&fields), None);
        // Its end reports nothing.
        let (r, tasks, _) = route(
            &mut bg,
            &mut root,
            true,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "f1", "status": "completed",
                "_meta": {SUBAGENT_COMPLETED: {"agentId": "f1", "success": true, "summary": "done"}}}),
        );
        assert_eq!((r, tasks.len()), (Routed::Handled, 0));
    }

    #[test]
    fn requests_of_background_work_belong_to_its_task() {
        let (mut bg, mut root) = (Background::new(), Tracker::new());
        route(&mut bg, &mut root, true, started("a1", true, None));
        let (r, tasks, _) = route(&mut bg, &mut root, true, tagged_tool("c1", "a1"));
        assert_eq!(r, Routed::Handled);
        let progress = tasks[0].progress.clone().unwrap();
        assert_eq!(
            (progress.last_tool_name.as_deref(), progress.tool_uses),
            (Some("exec"), Some(1))
        );
        let fields: ToolCallFields = serde_json::from_value(json!({"toolCallId": "c1"})).unwrap();
        assert_eq!(bg.task_of_tool(&fields).as_deref(), Some("subagent:a1"));
        let view = bg.tool_view(&fields).expect("known");
        assert_eq!(view.command(), "x");
        // A root tool call is the turn's.
        let root_fields: ToolCallFields =
            serde_json::from_value(json!({"toolCallId": "r1"})).unwrap();
        assert_eq!(bg.task_of_tool(&root_fields), None);
        assert!(bg.tool_view(&root_fields).is_none());
    }

    #[test]
    fn a_shell_ends_at_its_first_terminal_report_and_later_reports_only_add_fields() {
        let (mut bg, mut root) = (Background::new(), Tracker::new());
        let mut out = Vec::new();
        root.on_update(
            &SessionUpdate::parse(
                json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Ran ping", "kind": "execute", "rawInput": {"command": "ping"}}),
            ),
            &mut out,
        );
        let (_, tasks, items) = route(
            &mut bg,
            &mut root,
            true,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress",
                "_meta": {BACKGROUND_SHELL_ID: "s1", BACKGROUND_COMMAND: "ping -n 5 x"}}),
        );
        assert_eq!(tasks[0].origin_item_key.as_deref(), Some("tool-t1"));
        assert_eq!(tasks[0].title, "ping -n 5 x");
        assert!(matches!(
            items.last(),
            Some(AdapterEvent::ItemCompleted { key, status: ItemStatus::Backgrounded, .. }) if key == "tool-t1"
        ));
        // Output previews are the shell's, not the item's (which is closed): the first one is
        // the whole output, a continuing one what it added, one that does not continue the
        // last replaces it, and the same preview again adds nothing.
        let preview = |text: &str| {
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1",
                "content": [{"type": "content", "content": {"type": "text", "text": text}}],
                "_meta": {TERMINAL_PREVIEW: true}})
        };
        let output = |events: Vec<AdapterEvent>| -> Vec<OutputUpdate> {
            events
                .into_iter()
                .map(|e| match e {
                    AdapterEvent::BackgroundOutput { key, output } if key == "shell:s1" => output,
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        let (r, tasks, events) = route(&mut bg, &mut root, false, preview("\nPING 1"));
        assert_eq!((r, tasks.len()), (Routed::Handled, 0));
        assert_eq!(output(events), [OutputUpdate::Replace("\nPING 1".into())]);
        let (_, _, events) = route(&mut bg, &mut root, false, preview("\nPING 1\nPING 2"));
        assert_eq!(output(events), [OutputUpdate::Append("\nPING 2".into())]);
        let (_, _, events) = route(&mut bg, &mut root, false, preview("\nPING 1\nPING 2"));
        assert!(events.is_empty());
        let (_, _, events) = route(&mut bg, &mut root, false, preview("\r\nPING 1"));
        assert_eq!(output(events), [OutputUpdate::Replace("\r\nPING 1".into())]);
        // An update that is not a preview is not output.
        let (_, _, events) = route(
            &mut bg,
            &mut root,
            false,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1",
                "content": [{"type": "content", "content": {"type": "text", "text": "a"}}]}),
        );
        assert!(events.is_empty());
        let (_, tasks, _) = route(
            &mut bg,
            &mut root,
            false,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed",
                "content": [{"type": "content", "content": {"type": "text", "text": "ab"}}]}),
        );
        assert_eq!(
            (tasks[0].state, tasks[0].live),
            (BackgroundState::Failed, false)
        );
        let result = tasks[0].result.clone().unwrap();
        assert_eq!(
            (result.exit_code, result.output.as_deref()),
            (None, Some("ab"))
        );
        // A later `terminal_exit` of the same shell adds the exit code; the state stays.
        let (_, tasks, _) = route(
            &mut bg,
            &mut root,
            false,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                "_meta": {TERMINAL_EXIT: {"terminal_id": "s1", "exit_code": 137, "signal": null}}}),
        );
        assert_eq!(tasks[0].state, BackgroundState::Failed);
        assert_eq!(tasks[0].result.as_ref().unwrap().exit_code, Some(137));
        // Stopping an ended task is refused.
        assert!(bg.stop_target("shell:s1").is_err());
    }

    #[test]
    fn a_terminal_exit_of_another_terminal_does_not_end_a_shell() {
        let (mut bg, mut root) = (Background::new(), Tracker::new());
        route(
            &mut bg,
            &mut root,
            false,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress",
                "_meta": {BACKGROUND_SHELL_ID: "s1"}}),
        );
        let (_, tasks, _) = route(
            &mut bg,
            &mut root,
            false,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1",
                "_meta": {TERMINAL_EXIT: {"terminal_id": "other", "exit_code": 0}}}),
        );
        assert!(tasks.is_empty());
        assert_eq!(
            bg.stop_target("shell:s1"),
            Ok(StopTarget::Shell("s1".into()))
        );
    }

    #[test]
    fn a_sub_agent_that_starts_again_is_a_new_run() {
        let (mut bg, mut root) = (Background::new(), Tracker::new());
        route(&mut bg, &mut root, true, started("a1", true, None));
        route(&mut bg, &mut root, true, tagged_tool("c1", "a1"));
        let (_, tasks, _) = route(
            &mut bg,
            &mut root,
            true,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "a1", "status": "failed",
                "_meta": {SUBAGENT_COMPLETED: {"agentId": "a1", "success": false, "summary": "[Error] Canceled by user"}}}),
        );
        assert_eq!(
            (tasks[0].state, tasks[0].live),
            (BackgroundState::Failed, false)
        );
        let (_, tasks, _) = route(&mut bg, &mut root, true, started("a1", true, None));
        assert_eq!(
            (tasks[0].state, tasks[0].live, tasks[0].runs),
            (BackgroundState::Running, true, 2)
        );
        assert_eq!(tasks[0].progress, None, "a new run starts from scratch");
        assert_eq!(
            bg.stop_target("subagent:a1"),
            Ok(StopTarget::SubAgent("a1".into()))
        );
        assert_eq!(
            StopTarget::SubAgent("a1".into()).request("s"),
            (CANCEL_SUBAGENT, json!({"sessionId": "s", "agentId": "a1"}))
        );
        assert_eq!(
            StopTarget::Shell("x".into()).request("s"),
            (
                KILL_BACKGROUND_SHELL,
                json!({"sessionId": "s", "shellId": "x"})
            )
        );
        assert!(bg.stop_target("subagent:none").is_err());
    }

    #[test]
    fn sub_agent_usage_is_its_progress_and_never_the_root_agents() {
        let (mut bg, mut root) = (Background::new(), Tracker::new());
        route(&mut bg, &mut root, true, started("a1", true, None));
        let mut out = Vec::new();
        let usage = |agent: &str| json!({INPUT_TOKENS: 100, OUTPUT_TOKENS: 20, SUBAGENT_CONTEXT: {"parentAgentId": agent}});
        assert!(bg.on_usage(Some(&usage("a1")), &mut out));
        assert!(bg.on_usage(Some(&usage("a1")), &mut out));
        let Some(Emit::Event(AdapterEvent::BackgroundTask { task })) = out.last() else {
            panic!("{out:?}")
        };
        assert_eq!(task.progress.as_ref().unwrap().tokens, Some(240));
        // An unknown (or foreground) agent's usage is not the root agent's either.
        assert!(bg.on_usage(Some(&usage("zz")), &mut out));
        // The root agent's own (untagged, or tagged `root`) is.
        assert!(!bg.on_usage(Some(&usage("root")), &mut out));
        assert!(!bg.on_usage(None, &mut out));
    }

    #[test]
    fn updates_about_a_sub_agent_that_map_to_nothing_are_unmapped() {
        let (mut bg, mut root) = (Background::new(), Tracker::new());
        route(&mut bg, &mut root, true, started("a1", true, None));
        let (r, tasks, _) = route(
            &mut bg,
            &mut root,
            true,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "a1", "status": "in_progress"}),
        );
        assert_eq!((r, tasks.len()), (Routed::Unmapped, 0));
        // An end for an agent never announced.
        let (r, _, _) = route(
            &mut bg,
            &mut root,
            true,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "zz", "status": "completed",
                "_meta": {SUBAGENT_COMPLETED: {"agentId": "zz", "success": true}}}),
        );
        assert_eq!(r, Routed::Unmapped);
    }

    #[test]
    fn replayed_sub_agent_updates_are_recognised() {
        assert!(is_sub_agent_update(Some(
            &json!({SUBAGENT_CONTEXT: {"parentAgentId": "a1"}})
        )));
        assert!(is_sub_agent_update(Some(
            &json!({SUBAGENT_STARTED: {"agentId": "a1"}})
        )));
        assert!(!is_sub_agent_update(Some(
            &json!({SUBAGENT_CONTEXT: {"parentAgentId": "root"}})
        )));
        assert!(!is_sub_agent_update(None));
    }
}
