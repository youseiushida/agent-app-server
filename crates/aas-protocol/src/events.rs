//! Stream events (`stream/batch` payload).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::*;
use crate::types::*;

/// One event in a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub seq: u64,
    /// Present when this event is the concatenation of the deltas `seqFrom..=seq`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub seq_from: Option<u64>,
    pub ts: Millis,
    #[serde(flatten)]
    pub event: Event,
}

/// Every event type of both streams. The server decides which stream an event belongs to:
/// the workspace stream carries the variants listed under "workspace", the thread streams
/// carry the others.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all_fields = "camelCase")]
pub enum Event {
    // ----- workspace -----
    #[serde(rename = "project/upserted")]
    ProjectUpserted { project: Project },
    #[serde(rename = "project/removed")]
    ProjectRemoved { project_id: ProjectId },
    #[serde(rename = "thread/upserted")]
    ThreadUpserted { thread: Thread },
    #[serde(rename = "thread/removed")]
    ThreadRemoved { thread_id: ThreadId },
    #[serde(rename = "interaction/pending")]
    InteractionPending { interaction: Interaction },
    #[serde(rename = "interaction/closed")]
    InteractionClosed {
        interaction_id: InteractionId,
        thread_id: ThreadId,
        status: InteractionStatus,
    },
    #[serde(rename = "harness/updated")]
    HarnessUpdated { harness: Harness },
    #[serde(rename = "operation/updated")]
    OperationUpdated { operation: Operation },

    // ----- thread -----
    #[serde(rename = "thread/updated")]
    ThreadUpdated { thread: Thread },
    #[serde(rename = "turn/started")]
    TurnStarted { turn: Turn },
    #[serde(rename = "turn/completed")]
    TurnCompleted { turn: Turn },
    #[serde(rename = "turn/diffUpdated")]
    TurnDiffUpdated { turn_id: TurnId, diff: DiffSummary },
    /// Usage of a running turn so far, as the harness reported it (cumulative within the
    /// turn; `usage.context` is the latest context-window occupancy when reported).
    #[serde(rename = "turn/usageUpdated")]
    TurnUsageUpdated { turn_id: TurnId, usage: Usage },
    #[serde(rename = "item/started")]
    ItemStarted { item: Item },
    #[serde(rename = "item/delta")]
    ItemDelta {
        item_id: ItemId,
        field: DeltaField,
        text: String,
    },
    #[serde(rename = "item/updated")]
    ItemUpdated { item: Item },
    #[serde(rename = "item/completed")]
    ItemCompleted { item: Item },
    #[serde(rename = "interaction/requested")]
    InteractionRequested { interaction: Interaction },
    #[serde(rename = "interaction/resolved")]
    InteractionResolved { interaction: Interaction },
    #[serde(rename = "interaction/expired")]
    InteractionExpired { interaction: Interaction },
    #[serde(rename = "queue/updated")]
    QueueUpdated { queued: Vec<QueuedInput> },
    #[serde(rename = "commands/changed")]
    CommandsChanged {},
    /// A background task started, progressed or ended (always the whole task).
    #[serde(rename = "backgroundTask/updated")]
    BackgroundTaskUpdated { task: BackgroundTask },
    /// The harness reported that the thread's agent now works in another native session than
    /// the one the thread had (the CLI switched sessions by itself). The thread's
    /// `nativeSessionId` follows it.
    #[serde(rename = "thread/nativeSessionChanged")]
    NativeSessionChanged {
        previous_native_session_id: String,
        native_session_id: String,
    },
    /// The harness asks to put text into the thread's composer (e.g. a pi extension's
    /// `setEditorText`). The client inserts it into the composer of the open thread; it never
    /// sends it by itself.
    #[serde(rename = "composer/insert")]
    ComposerInsert { text: String },
    #[serde(rename = "native")]
    Native { harness_id: String, payload: Value },
}

impl Event {
    /// The wire name of the event (`type` field).
    pub fn type_name(&self) -> &'static str {
        match self {
            Event::ProjectUpserted { .. } => "project/upserted",
            Event::ProjectRemoved { .. } => "project/removed",
            Event::ThreadUpserted { .. } => "thread/upserted",
            Event::ThreadRemoved { .. } => "thread/removed",
            Event::InteractionPending { .. } => "interaction/pending",
            Event::InteractionClosed { .. } => "interaction/closed",
            Event::HarnessUpdated { .. } => "harness/updated",
            Event::OperationUpdated { .. } => "operation/updated",
            Event::ThreadUpdated { .. } => "thread/updated",
            Event::TurnStarted { .. } => "turn/started",
            Event::TurnCompleted { .. } => "turn/completed",
            Event::TurnDiffUpdated { .. } => "turn/diffUpdated",
            Event::TurnUsageUpdated { .. } => "turn/usageUpdated",
            Event::ItemStarted { .. } => "item/started",
            Event::ItemDelta { .. } => "item/delta",
            Event::ItemUpdated { .. } => "item/updated",
            Event::ItemCompleted { .. } => "item/completed",
            Event::InteractionRequested { .. } => "interaction/requested",
            Event::InteractionResolved { .. } => "interaction/resolved",
            Event::InteractionExpired { .. } => "interaction/expired",
            Event::QueueUpdated { .. } => "queue/updated",
            Event::CommandsChanged {} => "commands/changed",
            Event::BackgroundTaskUpdated { .. } => "backgroundTask/updated",
            Event::NativeSessionChanged { .. } => "thread/nativeSessionChanged",
            Event::ComposerInsert { .. } => "composer/insert",
            Event::Native { .. } => "native",
        }
    }

    /// Whether the event belongs on the workspace stream.
    pub fn is_workspace_event(&self) -> bool {
        matches!(
            self,
            Event::ProjectUpserted { .. }
                | Event::ProjectRemoved { .. }
                | Event::ThreadUpserted { .. }
                | Event::ThreadRemoved { .. }
                | Event::InteractionPending { .. }
                | Event::InteractionClosed { .. }
                | Event::HarnessUpdated { .. }
                | Event::OperationUpdated { .. }
        )
    }

    /// The item an event refers to, used to compact deltas of completed items.
    pub fn item_id(&self) -> Option<&ItemId> {
        match self {
            Event::ItemStarted { item }
            | Event::ItemUpdated { item }
            | Event::ItemCompleted { item } => Some(&item.id),
            Event::ItemDelta { item_id, .. } => Some(item_id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_envelope_shape() {
        let env = EventEnvelope {
            seq: 10,
            seq_from: Some(7),
            ts: 1,
            event: Event::ItemDelta {
                item_id: ItemId::from("itm_1"),
                field: DeltaField::Text,
                text: "abc".into(),
            },
        };
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "seq": 10, "seqFrom": 7, "ts": 1, "type": "item/delta",
                "data": {"itemId": "itm_1", "field": "text", "text": "abc"}
            })
        );
        let back: EventEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn empty_struct_variant_serializes_to_empty_object() {
        let json = serde_json::to_value(Event::CommandsChanged {}).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "commands/changed", "data": {}})
        );
    }
}
