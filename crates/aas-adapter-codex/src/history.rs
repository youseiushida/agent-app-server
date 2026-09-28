//! Native sessions: `thread/list` summaries and `thread/read` history.

use std::path::Path;

use aas_harness::protocol::{ItemBody, ItemStatus, Mention, UserMessageDelivery};
use aas_harness::{HistoryItem, HistoryTurn, NativeHistory, NativeSessionSummary};
use serde_json::Value;

use crate::mapping::{MappedItem, map_item};
use crate::wire::{WireItem, WireThread};

/// Most characters of a title made from a thread's preview; a longer first line is cut and ends
/// with `…`. A display rule: long enough to tell threads apart in a phone's list.
const PREVIEW_TITLE_MAX_CHARS: usize = 120;

/// Title of a Codex thread: its name, else the first line of its preview (the first user
/// message), truncated to [`PREVIEW_TITLE_MAX_CHARS`] characters.
pub fn thread_title(thread: &WireThread) -> Option<String> {
    if let Some(name) = thread.name.as_ref().filter(|n| !n.trim().is_empty()) {
        return Some(name.trim().to_owned());
    }
    let first = thread.preview.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return None;
    }
    let mut title: String = first.chars().take(PREVIEW_TITLE_MAX_CHARS).collect();
    if first.chars().count() > PREVIEW_TITLE_MAX_CHARS {
        title.push('…');
    }
    Some(title)
}

pub fn summary(thread: &WireThread) -> NativeSessionSummary {
    NativeSessionSummary {
        native_session_id: thread.id.clone(),
        title: thread_title(thread),
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

pub fn history(thread: &WireThread, cwd: &Path) -> NativeHistory {
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
        title: thread_title(thread),
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
        assert_eq!(thread_title(&t).as_deref(), Some("Reply with exactly: OK"));
        t.name = Some("Named".into());
        assert_eq!(thread_title(&t).as_deref(), Some("Named"));
        let long: WireThread =
            serde_json::from_value(json!({"id":"b","preview":"x".repeat(130)})).unwrap();
        assert_eq!(thread_title(&long).unwrap().chars().count(), 121);
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
        let h = history(&thread, Path::new("/p"));
        assert_eq!(h.title.as_deref(), Some("hi"));
        assert_eq!(h.turns.len(), 1);
        let turn = &h.turns[0];
        assert_eq!(turn.started_at, Some(100_000));
        assert_eq!(turn.items.len(), 3);
        assert!(matches!(&turn.items[0].body, ItemBody::UserMessage { text, .. } if text == "hi"));
        assert!(matches!(&turn.items[1].body, ItemBody::Reasoning { text } if text == "thinking"));
        assert!(matches!(&turn.items[2].body, ItemBody::AgentMessage { text } if text == "hello"));
        assert_eq!(summary(&thread).updated_at, Some(10_000));
    }
}
