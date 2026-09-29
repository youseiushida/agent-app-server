//! Folds replayed updates (`session/load`) into a [`NativeHistory`].
//!
//! Turn boundaries: a `user_message_chunk` that follows anything other than another user
//! chunk starts a new turn; contiguous user chunks form one user message. Updates before the
//! first user chunk form a turn without a user message.
//!
//! Each turn keeps the id its first user chunk carried (Devin's
//! `_meta["cognition.ai/clientMessageId"]`, the prompt's step id), from which the turn's
//! anchor is found (`crate::revert::history_anchors`).

use std::collections::HashMap;

use aas_harness::protocol::{ItemBody, ItemStatus, UserMessageDelivery};
use aas_harness::{AdapterEvent, HistoryItem, HistoryTurn, NativeHistory};

use crate::tracker::Emit;

#[derive(Debug, Default)]
pub struct HistoryBuilder {
    turns: Vec<HistoryTurn>,
    /// The client message id of each turn's first user chunk, parallel to `turns`.
    message_ids: Vec<Option<String>>,
    /// The id of the user chunk being pushed ([`user_message_id`](Self::user_message_id)).
    pending_message_id: Option<String>,
    index: HashMap<String, (usize, usize)>,
    in_user_message: bool,
    title: Option<String>,
}

impl HistoryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_title(&mut self, title: Option<String>) {
        if title.is_some() {
            self.title = title;
        }
    }

    /// The client message id of the user chunk whose emits are pushed next (`None`: it has
    /// none). Cleared by [`user_chunk_done`](Self::user_chunk_done).
    pub fn user_message_id(&mut self, id: Option<String>) {
        self.pending_message_id = id;
    }

    /// The user chunk's emits were pushed.
    pub fn user_chunk_done(&mut self) {
        self.pending_message_id = None;
    }

    fn current_turn(&mut self) -> usize {
        if self.turns.is_empty() {
            self.turns.push(HistoryTurn::default());
            self.message_ids.push(None);
        }
        self.turns.len() - 1
    }

    fn item_mut(&mut self, key: &str) -> Option<&mut HistoryItem> {
        let (t, i) = *self.index.get(key)?;
        self.turns.get_mut(t)?.items.get_mut(i)
    }

    pub fn push(&mut self, emit: Emit) {
        match emit {
            Emit::UserText(text) => {
                if self.in_user_message {
                    let t = self.current_turn();
                    if let Some(HistoryItem {
                        body: ItemBody::UserMessage { text: existing, .. },
                        ..
                    }) = self.turns[t].items.last_mut()
                    {
                        existing.push_str(&text);
                    }
                } else {
                    self.message_ids.push(self.pending_message_id.clone());
                    self.turns.push(HistoryTurn {
                        started_at: None,
                        completed_at: None,
                        items: vec![HistoryItem {
                            body: ItemBody::UserMessage {
                                text,
                                attachments: Vec::new(),
                                mentions: Vec::new(),
                                delivery: UserMessageDelivery::Normal,
                            },
                            status: ItemStatus::Completed,
                        }],
                    });
                    self.in_user_message = true;
                }
            }
            Emit::Unrendered(_) => {}
            Emit::Event(event) => {
                self.in_user_message = false;
                match event {
                    AdapterEvent::ItemStarted { key, body } => {
                        let t = self.current_turn();
                        self.turns[t].items.push(HistoryItem {
                            body,
                            status: ItemStatus::InProgress,
                        });
                        let i = self.turns[t].items.len() - 1;
                        self.index.insert(key, (t, i));
                    }
                    AdapterEvent::ItemDelta { key, field, text } => {
                        if let Some(item) = self.item_mut(&key) {
                            item.body.append(field, &text);
                        }
                    }
                    AdapterEvent::ItemUpdated { key, body } => {
                        if let Some(item) = self.item_mut(&key) {
                            item.body = body;
                        }
                    }
                    AdapterEvent::ItemCompleted { key, body, status } => {
                        if let Some(item) = self.item_mut(&key) {
                            if let Some(body) = body {
                                item.body = body;
                            }
                            item.status = status;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// The history, and the client message id of each turn (same order).
    pub fn finish(self) -> (NativeHistory, Vec<Option<String>>) {
        (
            NativeHistory {
                title: self.title,
                turns: self.turns,
            },
            self.message_ids,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn message(text: &str) -> Emit {
        Emit::Event(AdapterEvent::ItemStarted {
            key: format!("msg-{text}"),
            body: ItemBody::AgentMessage { text: text.into() },
        })
    }

    #[test]
    fn each_turn_keeps_the_id_of_its_first_user_chunk() {
        let mut b = HistoryBuilder::new();
        // Updates before any user chunk: a turn without a user message (and without an id).
        b.push(message("hello"));
        b.user_message_id(Some("a".into()));
        b.push(Emit::UserText("one".into()));
        b.user_chunk_done();
        // A continued user message keeps the first chunk's id.
        b.user_message_id(Some("ignored".into()));
        b.push(Emit::UserText(" more".into()));
        b.user_chunk_done();
        b.push(message("ONE"));
        b.user_message_id(None);
        b.push(Emit::UserText("two".into()));
        b.user_chunk_done();
        let (history, ids) = b.finish();
        assert_eq!(history.turns.len(), 3);
        assert_eq!(ids, vec![None, Some("a".into()), None]);
        assert!(matches!(
            &history.turns[1].items[0].body,
            ItemBody::UserMessage { text, .. } if text == "one more"
        ));
    }
}
