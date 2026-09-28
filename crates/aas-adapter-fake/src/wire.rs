//! The fake agent's line protocol (JSON Lines on stdin/stdout).
//!
//! It mirrors [`aas_harness::AdapterEvent`] closely so the adapter is a thin translation and
//! tests exercise the engine rather than this protocol.

use aas_harness::BackgroundTaskInfo;
use aas_protocol::types::{
    Command, DeltaField, InteractionRequest, InteractionResolution, ItemBody, ItemStatus,
    NoticeLevel, TurnError, TurnStatus, TurnTrigger, Usage,
};
use serde::{Deserialize, Serialize};

/// Adapter → agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Op {
    /// Opens the session: a new one, `resume` an existing one, or `fork_from` a new one
    /// branched off an existing session. `sessions_dir` is the session store (see
    /// [`crate::store`]); without it nothing is stored.
    Hello {
        session_id: String,
        resume: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sessions_dir: Option<String>,
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
    },
    Interrupt,
    Respond {
        request_id: String,
        resolution: InteractionResolution,
    },
    SetModel {
        model: Option<String>,
    },
    /// Asks the agent to stop background task `key` (answered only by the task's end).
    StopBackground {
        key: String,
    },
}

/// Agent → adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Ev {
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
    Notice {
        level: NoticeLevel,
        message: String,
    },
}
