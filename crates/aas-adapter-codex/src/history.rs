//! Native sessions: `thread/list` summaries and `thread/read` history.

use std::path::Path;

use aas_harness::protocol::{ItemBody, ItemStatus, Mention, UserMessageDelivery};
use aas_harness::{AdapterPolicy, HistoryItem, HistoryTurn, NativeHistory, NativeSessionSummary};
use serde_json::Value;

use crate::mapping::{MappedItem, map_item};
use crate::wire::{WireItem, WireThread};

/// Title of a Codex thread: its name (cut to `policy.harness_title_chars`), else the first line
/// of its preview (the first user message), cut to `policy.first_message_title_chars` like the
/// engine's titles from a first message.
pub fn thread_title(thread: &WireThread, policy: &AdapterPolicy) -> Option<String> {
    thread
        .name
        .as_deref()
        .and_then(|name| policy.harness_title(name))
        .or_else(|| policy.prompt_title(&thread.preview))
}

pub fn summary(thread: &WireThread, policy: &AdapterPolicy) -> NativeSessionSummary {
    NativeSessionSummary {
        native_session_id: thread.id.clone(),
        title: thread_title(thread, policy),
        updated_at: thread.updated_at.or(thread.created_at).map(|s| s * 1000),
        cwd: thread.cwd.clone(),
    }
}

/// A Codex user message as a protocol item body: text parts joined, `mention` inputs listed.
/// Images cannot be carried over (the engine stores images as blobs it created itself).
pub fn user_message_body(content: &[Value]) -> ItemBody {
    let mut texts = Vec::new();
    let mut mentions = Vec::new();
    for part in content {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    texts.push(t.to_owned());
                }
            }
            Some("mention") => {
                if let Some(p) = part.get("path").and_then(Value::as_str) {
                    mentions.push(Mention { path: p.to_owned() });
                }
            }
            Some("localImage") | Some("image") => texts.push("[image]".to_owned()),
            _ => {}
        }
    }
    ItemBody::UserMessage {
        text: texts.join("\n"),
        attachments: Vec::new(),
        mentions,
        delivery: UserMessageDelivery::Normal,
    }
}

pub fn history(thread: &WireThread, cwd: &Path, policy: &AdapterPolicy) -> NativeHistory {
    let turns = thread
        .turns
        .iter()
        .map(|turn| {
            let items = turn
                .items
                .iter()
                .filter_map(|raw| {
                    let item: WireItem =
                        serde_json::from_value(raw.clone()).unwrap_or(WireItem::Unknown);
                    match item {
                        WireItem::UserMessage { content, .. } => Some(HistoryItem {
                            body: user_message_body(&content),
                            status: ItemStatus::Completed,
                        }),
                        other => match map_item(&other, cwd, None) {
                            MappedItem::Item { body, status, .. } => {
                                Some(HistoryItem { body, status })
                            }
                            MappedItem::Skip | MappedItem::Unknown => None,
                        },
                    }
                })
                .collect();
            HistoryTurn {
                started_at: turn.started_at.map(|s| s * 1000),
                completed_at: turn.completed_at.map(|s| s * 1000),
                items,
            }
        })
        .collect();
    NativeHistory {
        title: thread_title(thread, policy),
        turns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles_prefer_name_then_preview() {
        let mut t: WireThread = serde_json::from_value(
            json!({"id":"a","preview":"Reply with exactly: OK\nmore","name":null}),
        )
        .unwrap();
        let policy = AdapterPolicy::default();
        assert_eq!(
            thread_title(&t, &policy).as_deref(),
            Some("Reply with exactly: OK")
        );
        t.name = Some("  Named  ".into());
        assert_eq!(thread_title(&t, &policy).as_deref(), Some("Named"));
        // A blank name falls back to the preview.
        t.name = Some("   ".into());
        assert_eq!(
            thread_title(&t, &policy).as_deref(),
            Some("Reply with exactly: OK")
        );
        // The lengths are the engine's policy values.
        let long: WireThread =
            serde_json::from_value(json!({"id":"b","preview":"x".repeat(130),"name":null}))
                .unwrap();
        let short = AdapterPolicy {
            first_message_title_chars: 100,
            harness_title_chars: 10,
            ..AdapterPolicy::default()
        };
        assert_eq!(thread_title(&long, &short).unwrap().chars().count(), 101);
        let named: WireThread =
            serde_json::from_value(json!({"id":"c","preview":"p","name":"n".repeat(30)})).unwrap();
        assert_eq!(thread_title(&named, &short).unwrap(), "n".repeat(10));
    }

    #[test]
    fn history_maps_turns_and_user_messages() {
        let thread: WireThread = serde_json::from_value(json!({
            "id":"t","preview":"hi","updatedAt":10,"createdAt":5,
            "turns":[{"id":"u1","status":"completed","startedAt":100,"completedAt":101,"items":[
                {"type":"userMessage","id":"m","clientId":null,"content":[{"type":"text","text":"hi","text_elements":[]}]},
                {"type":"reasoning","id":"r","summary":[],"content":["thinking"]},
                {"type":"agentMessage","id":"a","text":"hello","phase":"final_answer","memoryCitation":null},
                {"type":"totallyNew","id":"z"}
            ]}]
        }))
        .unwrap();
        let h = history(&thread, Path::new("/p"), &AdapterPolicy::default());
        assert_eq!(h.title.as_deref(), Some("hi"));
        assert_eq!(h.turns.len(), 1);
        let turn = &h.turns[0];
        assert_eq!(turn.started_at, Some(100_000));
        assert_eq!(turn.items.len(), 3);
        assert!(matches!(&turn.items[0].body, ItemBody::UserMessage { text, .. } if text == "hi"));
        assert!(matches!(&turn.items[1].body, ItemBody::Reasoning { text } if text == "thinking"));
        assert!(matches!(&turn.items[2].body, ItemBody::AgentMessage { text } if text == "hello"));
        assert_eq!(
            summary(&thread, &AdapterPolicy::default()).updated_at,
            Some(10_000)
        );
    }
}
