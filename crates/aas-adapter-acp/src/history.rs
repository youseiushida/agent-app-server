//! Folds replayed updates (`session/load`) into a [`NativeHistory`].
//!
//! Turn boundaries: a `user_message_chunk` that follows anything other than another user
//! chunk starts a new turn; contiguous user chunks form one user message. Updates before the
//! first user chunk form a turn without a user message.

use std::collections::HashMap;

use aas_harness::protocol::{ItemBody, ItemStatus, UserMessageDelivery};
use aas_harness::{AdapterEvent, HistoryItem, HistoryTurn, NativeHistory};

use crate::tracker::Emit;

#[derive(Debug, Default)]
pub struct HistoryBuilder {
    turns: Vec<HistoryTurn>,
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

    fn current_turn(&mut self) -> usize {
        if self.turns.is_empty() {
            self.turns.push(HistoryTurn::default());
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

    pub fn finish(self) -> NativeHistory {
        NativeHistory {
            title: self.title,
            turns: self.turns,
        }
    }
}
