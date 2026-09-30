//! The fake agent's line protocol (JSON Lines on stdin/stdout).
//!
//! It mirrors [`aas_harness::AdapterEvent`] closely so the adapter is a thin translation and
//! tests exercise the engine rather than this protocol.

use aas_harness::BackgroundTaskInfo;
use aas_protocol::types::{
    Command, DeltaField, InteractionRequest, InteractionResolution, ItemBody, ItemStatus,
    NoticeLevel, StatusSection, TurnError, TurnStatus, TurnTrigger, Usage,
};
use serde::{Deserialize, Serialize};

/// Where a fork at a turn branches its source: after (`before: false`) or before the stored
/// turn `turn` (its index in the source's transcript, the fake agent's turn anchor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkAt {
    pub turn: usize,
    pub before: bool,
}

/// The modes the agent runs in (plan mode, fast mode).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Modes {
    #[serde(default)]
    pub plan: bool,
    #[serde(default)]
    pub fast: bool,
}

/// A request answered right away, beside the turn (`Op::Query`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Query {
    /// The agent's own status.
    Status,
    /// A question answered beside the conversation.
    SideQuestion { question: String },
}

/// Adapter → agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Op {
    /// Opens the session: a new one, `resume` an existing one, or `fork_from` a new one
    /// branched off an existing session (at `fork_at` when given). `sessions_dir` is the
    /// session store (see [`crate::store`]); without it nothing is stored. `modes` are the
    /// modes to start in; `project_trusted` the user's decision about the project.
    Hello {
        session_id: String,
        resume: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork_at: Option<ForkAt>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sessions_dir: Option<String>,
        #[serde(default)]
        modes: Modes,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_trusted: Option<bool>,
    },
    Prompt {
        text: String,
        /// Files of the attached images (the agent acknowledges them).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    Steer {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
        /// The engine's id of the steered message (returned with `steerReturned`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    Interrupt,
    Respond {
        request_id: String,
        resolution: InteractionResolution,
    },
    SetModel {
        model: Option<String>,
    },
    /// Switches plan mode and fast mode (answered with `modes`).
    SetModes {
        modes: Modes,
    },
    /// Names the session (answered with `title`, like the echo of a real CLI).
    Rename {
        title: String,
    },
    /// Asks something beside the turn (answered with `queryResult` of the same id).
    Query {
        id: String,
        query: Query,
    },
    /// Asks the agent to stop background task `key` (answered only by the task's end).
    StopBackground {
        key: String,
    },
    /// Asks the agent to move the running item `key` (`@tool`) to the background.
    Background {
        key: String,
    },
}

/// Agent → adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Ev {
    /// The session the agent works in: the answer to `hello`, and again when it switches to
    /// another session by itself (`@switch-session`).
    Ready {
        session_id: String,
    },
    /// Answers a `hello` whose session could not be opened (instead of `ready`); the agent
    /// exits afterwards.
    Rejected {
        message: String,
    },
    SessionInfo {
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// Plan mode and fast mode as the agent runs them.
    Modes {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fast_state: Option<String>,
    },
    /// The session's name (after `rename`, or when the agent names it itself).
    Title {
        title: String,
    },
    Commands {
        commands: Vec<Command>,
    },
    /// Answers every `prompt`: taken (a turn follows), or refused because a turn runs;
    /// `own_run` says that the running turn is one the agent started by itself (whose
    /// `turnStarted` came before this answer).
    PromptAck {
        accepted: bool,
        #[serde(default)]
        own_run: bool,
    },
    TurnStarted,
    /// The anchor of the running turn: its index in the session's transcript.
    Anchor {
        turn: usize,
    },
    /// A provisional anchor of the running turn (`@late-anchor`): it cannot be branched at
    /// until `anchorSettled` names the same turn.
    ProvisionalAnchor {
        turn: usize,
    },
    /// The provisional anchor of turn `turn` has settled (at the start of the next turn, or at
    /// `@settle-anchor`).
    AnchorSettled {
        turn: usize,
    },
    ItemStarted {
        key: String,
        body: ItemBody,
    },
    Delta {
        key: String,
        field: DeltaField,
        text: String,
    },
    ItemUpdated {
        key: String,
        body: ItemBody,
    },
    ItemCompleted {
        key: String,
        status: ItemStatus,
        body: Option<ItemBody>,
    },
    /// Whether the running item `key` can be moved to the background now.
    Backgroundable {
        key: String,
        backgroundable: bool,
    },
    Request {
        request_id: String,
        request: InteractionRequest,
        item_key: Option<String>,
        /// The background task that asks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background_key: Option<String>,
    },
    Withdraw {
        request_id: String,
    },
    /// Answers every `steer`: taken into the running turn (which takes it in, or returns it
    /// with `steerReturned` before its completion), or refused because no turn runs any more
    /// (the turn completed before the steer arrived).
    SteerAck {
        accepted: bool,
    },
    /// A steered message the running turn did not take (`@refuse-steers`, or one still unread
    /// when the turn ended), reported before the turn's completion.
    SteerReturned {
        message_id: String,
    },
    /// Text for the composer (`@editor`).
    EditorText {
        text: String,
    },
    Usage {
        usage: Usage,
    },
    TurnCompleted {
        status: TurnStatus,
        usage: Option<Usage>,
        error: Option<TurnError>,
        /// Why the agent started this turn by itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<TurnTrigger>,
    },
    /// The whole state of one background task.
    Background {
        task: BackgroundTaskInfo,
    },
    /// Output of the current run of background task `key`: appended to what came before, or
    /// (`replace`) the whole output so far (`@bg … snapshots`, like a harness that reports
    /// snapshots).
    BackgroundOutput {
        key: String,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replace: bool,
    },
    /// The answer to `query` `id`.
    QueryResult {
        id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sections: Vec<StatusSection>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
    },
    Notice {
        level: NoticeLevel,
        message: String,
    },
}
