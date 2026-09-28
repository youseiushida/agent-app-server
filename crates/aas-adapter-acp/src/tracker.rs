//! Turns the stream of item-producing `session/update`s into normalized item events.
//!
//! Item boundaries in ACP are implicit, so the rules are fixed and documented:
//!
//! * A *run* of `agent_message_chunk` (or `agent_thought_chunk`) forms one item. The run ends
//!   when another item-producing update arrives (a different chunk kind, a tool call, a plan
//!   or a user chunk), when the chunk's `messageId` differs from the run's, or when the turn
//!   ends. State updates (config, modes, commands, usage, session info) do not end a run.
//! * A tool call is one item keyed by its `toolCallId`; `tool_call_update` fields replace the
//!   stored ones. When only the output of a command grew by appending, a delta is emitted;
//!   any other change emits the whole new body.
//! * The latest `plan` of a turn is one item that is replaced on every update.

use std::collections::HashMap;

use aas_harness::AdapterEvent;
use aas_harness::protocol::{DeltaField, ItemBody, ItemStatus, TurnStatus};

use crate::mapping::{self, ToolState};
use crate::wire::{ContentChunk, SessionUpdate, ToolCallFields};

/// Something the tracker produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Emit {
    Event(AdapterEvent),
    /// Text of a `user_message_chunk` (only meaningful when collecting history).
    UserText(String),
    /// A chunk carried content with no textual rendering (e.g. an image); forwarded raw.
    Unrendered(serde_json::Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Message,
    Thought,
}

#[derive(Debug, Clone)]
struct Run {
    kind: RunKind,
    key: String,
    message_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Tracker {
    run: Option<Run>,
    tools: HashMap<String, Tracked>,
    /// Tool ids in start order (for deterministic closing).
    order: Vec<String>,
    plan_key: Option<String>,
    seq: u64,
}

#[derive(Debug)]
struct Tracked {
    state: ToolState,
    key: String,
    last_body: ItemBody,
    open: bool,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_key(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}-{}", self.seq)
    }

    pub fn tool(&self, id: &str) -> Option<&ToolState> {
        self.tools.get(id).map(|t| &t.state)
    }

    /// Item key of a tool call seen in this session.
    pub fn tool_key(&self, id: &str) -> Option<String> {
        self.tools.get(id).map(|t| t.key.clone())
    }

    /// Handles an item-producing update. Returns `false` (and emits nothing) for updates that
    /// are not item-producing, so the caller can handle them as session state.
    pub fn on_update(&mut self, update: &SessionUpdate, out: &mut Vec<Emit>) -> bool {
        match update {
            SessionUpdate::AgentMessageChunk(c) => self.chunk(RunKind::Message, c, out),
            SessionUpdate::AgentThoughtChunk(c) => self.chunk(RunKind::Thought, c, out),
            SessionUpdate::UserMessageChunk(c) => {
                self.close_run(out);
                self.close_plan(out);
                match mapping::content_text(&c.content) {
                    Some(text) => out.push(Emit::UserText(text)),
                    None => out.push(Emit::Unrendered(c.content.clone())),
                }
            }
            SessionUpdate::ToolCall(f) | SessionUpdate::ToolCallUpdate(f) => {
                self.close_run(out);
                self.apply_tool(f, out);
            }
            SessionUpdate::Plan(p) => {
                self.close_run(out);
                let body = ItemBody::Plan {
                    entries: mapping::plan_entries(&p.entries),
                };
                match self.plan_key.clone() {
                    Some(key) => out.push(Emit::Event(AdapterEvent::ItemUpdated { key, body })),
                    None => {
                        let key = self.next_key("plan");
                        self.plan_key = Some(key.clone());
                        out.push(Emit::Event(AdapterEvent::ItemStarted { key, body }));
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// Applies a tool call's fields as an item of the turn (starting the item when the call is
    /// new) and returns its item key. Used for the tool call a permission request carries and
    /// for a command that moves to the background.
    pub fn tool_item(&mut self, fields: &ToolCallFields, out: &mut Vec<Emit>) -> String {
        self.close_run(out);
        self.apply_tool(fields, out);
        self.tools[&fields.tool_call_id].key.clone()
    }

    /// The state a tool call has with `fields` applied, without changing anything (for a
    /// request about a tool call that is not an item of the running turn).
    pub fn peek_tool(&self, fields: &ToolCallFields) -> ToolState {
        let mut state = self
            .tools
            .get(&fields.tool_call_id)
            .map(|t| t.state.clone())
            .unwrap_or_else(|| ToolState::new(&fields.tool_call_id));
        state.merge(fields);
        state
    }

    /// Whether the tool call is an item that is still open.
    pub fn is_open(&self, id: &str) -> bool {
        self.tools.get(id).is_some_and(|t| t.open)
    }

    /// Closes an open tool call item as [`ItemStatus::Backgrounded`]: its work goes on as a
    /// background task (which the caller has already reported, naming this item).
    pub fn close_backgrounded(&mut self, id: &str, out: &mut Vec<Emit>) {
        if let Some(t) = self.tools.get_mut(id)
            && t.open
        {
            t.open = false;
            out.push(Emit::Event(AdapterEvent::ItemCompleted {
                key: t.key.clone(),
                body: Some(t.last_body.clone()),
                status: ItemStatus::Backgrounded,
            }));
        }
    }

    fn chunk(&mut self, kind: RunKind, chunk: &ContentChunk, out: &mut Vec<Emit>) {
        let Some(text) = mapping::content_text(&chunk.content) else {
            out.push(Emit::Unrendered(chunk.content.clone()));
            return;
        };
        let continues = self.run.as_ref().is_some_and(|r| {
            r.kind == kind && (chunk.message_id.is_none() || r.message_id == chunk.message_id)
        });
        if continues {
            let key = self.run.as_ref().expect("run").key.clone();
            if !text.is_empty() {
                out.push(Emit::Event(AdapterEvent::ItemDelta {
                    key,
                    field: DeltaField::Text,
                    text,
                }));
            }
            return;
        }
        self.close_run(out);
        let key = self.next_key(match kind {
            RunKind::Message => "msg",
            RunKind::Thought => "thought",
        });
        let body = match kind {
            RunKind::Message => ItemBody::AgentMessage { text },
            RunKind::Thought => ItemBody::Reasoning { text },
        };
        out.push(Emit::Event(AdapterEvent::ItemStarted {
            key: key.clone(),
            body,
        }));
        self.run = Some(Run {
            kind,
            key,
            message_id: chunk.message_id.clone(),
        });
    }

    fn apply_tool(&mut self, fields: &ToolCallFields, out: &mut Vec<Emit>) {
        let id = fields.tool_call_id.clone();
        if let Some(tracked) = self.tools.get_mut(&id) {
            tracked.state.merge(fields);
            let body = tracked.state.body();
            if !tracked.open {
                // An update for an already closed call: publish the new body so the item stays
                // accurate, but it stays closed.
                if body != tracked.last_body {
                    tracked.last_body = body.clone();
                    out.push(Emit::Event(AdapterEvent::ItemUpdated {
                        key: tracked.key.clone(),
                        body,
                    }));
                }
                return;
            }
            if body != tracked.last_body {
                match output_suffix(&tracked.last_body, &body) {
                    Some(suffix) => out.push(Emit::Event(AdapterEvent::ItemDelta {
                        key: tracked.key.clone(),
                        field: DeltaField::Output,
                        text: suffix,
                    })),
                    None => out.push(Emit::Event(AdapterEvent::ItemUpdated {
                        key: tracked.key.clone(),
                        body: body.clone(),
                    })),
                }
                tracked.last_body = body;
            }
            if let Some(status) = tracked.state.terminal_status() {
                tracked.open = false;
                out.push(Emit::Event(AdapterEvent::ItemCompleted {
                    key: tracked.key.clone(),
                    body: Some(tracked.last_body.clone()),
                    status,
                }));
            }
            return;
        }
        let mut state = ToolState::new(&id);
        state.merge(fields);
        let key = format!("tool-{id}");
        let body = state.body();
        out.push(Emit::Event(AdapterEvent::ItemStarted {
            key: key.clone(),
            body: body.clone(),
        }));
        let status = state.terminal_status();
        if let Some(status) = status {
            out.push(Emit::Event(AdapterEvent::ItemCompleted {
                key: key.clone(),
                body: Some(body.clone()),
                status,
            }));
        }
        self.order.push(id.clone());
        self.tools.insert(
            id,
            Tracked {
                state,
                key,
                last_body: body,
                open: status.is_none(),
            },
        );
    }

    /// Ends the current message/thought run.
    pub fn close_run(&mut self, out: &mut Vec<Emit>) {
        if let Some(run) = self.run.take() {
            out.push(Emit::Event(AdapterEvent::ItemCompleted {
                key: run.key,
                body: None,
                status: ItemStatus::Completed,
            }));
        }
    }

    fn close_plan(&mut self, out: &mut Vec<Emit>) {
        if let Some(key) = self.plan_key.take() {
            out.push(Emit::Event(AdapterEvent::ItemCompleted {
                key,
                body: None,
                status: ItemStatus::Completed,
            }));
        }
    }

    /// Closes everything at the end of a turn. Tool calls the agent never finished are closed
    /// as `interrupted` (or `failed` when the turn failed).
    pub fn end_turn(&mut self, status: TurnStatus, out: &mut Vec<Emit>) {
        self.close_run(out);
        self.close_plan(out);
        let unfinished = if status == TurnStatus::Failed {
            ItemStatus::Failed
        } else {
            ItemStatus::Interrupted
        };
        for id in std::mem::take(&mut self.order) {
            if let Some(t) = self.tools.get_mut(&id)
                && t.open
            {
                t.open = false;
                out.push(Emit::Event(AdapterEvent::ItemCompleted {
                    key: t.key.clone(),
                    body: Some(t.last_body.clone()),
                    status: unfinished,
                }));
            }
        }
    }
}

/// When `new` is a command execution whose output only grew by appending (and nothing else
/// changed), returns the appended text.
fn output_suffix(old: &ItemBody, new: &ItemBody) -> Option<String> {
    match (old, new) {
        (
            ItemBody::CommandExecution {
                command: c1,
                cwd: w1,
                output: o1,
                exit_code: e1,
                ..
            },
            ItemBody::CommandExecution {
                command: c2,
                cwd: w2,
                output: o2,
                exit_code: e2,
                ..
            },
        ) if c1 == c2
            && w1 == w2
            && e1 == e2
            && o2.len() > o1.len()
            && o2.starts_with(o1.as_str()) =>
        {
            Some(o2[o1.len()..].to_owned())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn upd(v: serde_json::Value) -> SessionUpdate {
        SessionUpdate::parse(v)
    }

    fn events(out: Vec<Emit>) -> Vec<AdapterEvent> {
        out.into_iter()
            .filter_map(|e| match e {
                Emit::Event(ev) => Some(ev),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn chunk_runs_form_items_and_state_updates_do_not_break_them() {
        let mut t = Tracker::new();
        let mut out = Vec::new();
        for u in [
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Th"}}),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "ink"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "O"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "K"}}),
        ] {
            assert!(t.on_update(&upd(u), &mut out));
        }
        assert!(!t.on_update(
            &upd(json!({"sessionUpdate": "usage_update", "used": 1, "size": 2})),
            &mut out
        ));
        t.on_update(&upd(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "!"}})), &mut out);
        t.end_turn(TurnStatus::Completed, &mut out);
        assert_eq!(
            events(out),
            vec![
                AdapterEvent::ItemStarted {
                    key: "thought-1".into(),
                    body: ItemBody::Reasoning { text: "Th".into() }
                },
                AdapterEvent::ItemDelta {
                    key: "thought-1".into(),
                    field: DeltaField::Text,
                    text: "ink".into()
                },
                AdapterEvent::ItemCompleted {
                    key: "thought-1".into(),
                    body: None,
                    status: ItemStatus::Completed
                },
                AdapterEvent::ItemStarted {
                    key: "msg-2".into(),
                    body: ItemBody::AgentMessage { text: "O".into() }
                },
                AdapterEvent::ItemDelta {
                    key: "msg-2".into(),
                    field: DeltaField::Text,
                    text: "K".into()
                },
                AdapterEvent::ItemDelta {
                    key: "msg-2".into(),
                    field: DeltaField::Text,
                    text: "!".into()
                },
                AdapterEvent::ItemCompleted {
                    key: "msg-2".into(),
                    body: None,
                    status: ItemStatus::Completed
                },
            ]
        );
    }

    #[test]
    fn message_id_change_starts_a_new_item() {
        let mut t = Tracker::new();
        let mut out = Vec::new();
        t.on_update(&upd(json!({"sessionUpdate": "agent_message_chunk", "messageId": "a", "content": {"type": "text", "text": "1"}})), &mut out);
        t.on_update(&upd(json!({"sessionUpdate": "agent_message_chunk", "messageId": "b", "content": {"type": "text", "text": "2"}})), &mut out);
        let ev = events(out);
        assert!(matches!(&ev[1], AdapterEvent::ItemCompleted { key, .. } if key == "msg-1"));
        assert!(matches!(&ev[2], AdapterEvent::ItemStarted { key, .. } if key == "msg-2"));
    }

    #[test]
    fn command_output_growth_becomes_deltas() {
        let mut t = Tracker::new();
        let mut out = Vec::new();
        let id = "functions.exec:3#x";
        t.on_update(&upd(json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": "Ran ping", "kind": "execute", "rawInput": {"command": "ping"}})), &mut out);
        t.on_update(&upd(json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "in_progress"})), &mut out);
        t.on_update(&upd(json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "content": [{"type": "content", "content": {"type": "text", "text": "a\n"}}]})), &mut out);
        t.on_update(&upd(json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "content": [{"type": "content", "content": {"type": "text", "text": "a\nb\n"}}]})), &mut out);
        t.on_update(&upd(json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed", "content": [{"type": "content", "content": {"type": "text", "text": "Canceled"}}]})), &mut out);
        let ev = events(out);
        let key = format!("tool-{id}");
        assert!(matches!(&ev[0], AdapterEvent::ItemStarted { key: k, .. } if *k == key));
        assert_eq!(
            ev[1],
            AdapterEvent::ItemDelta {
                key: key.clone(),
                field: DeltaField::Output,
                text: "a\n".into()
            }
        );
        assert_eq!(
            ev[2],
            AdapterEvent::ItemDelta {
                key: key.clone(),
                field: DeltaField::Output,
                text: "b\n".into()
            }
        );
        assert!(
            matches!(&ev[3], AdapterEvent::ItemUpdated { body: ItemBody::CommandExecution { output, .. }, .. } if output == "Canceled")
        );
        assert!(matches!(
            &ev[4],
            AdapterEvent::ItemCompleted {
                status: ItemStatus::Failed,
                ..
            }
        ));
        assert_eq!(ev.len(), 5);
    }

    #[test]
    fn unfinished_tools_close_with_turn() {
        let mut t = Tracker::new();
        let mut out = Vec::new();
        t.on_update(&upd(json!({"sessionUpdate": "tool_call", "toolCallId": "a", "title": "Read", "kind": "read"})), &mut out);
        t.on_update(&upd(json!({"sessionUpdate": "plan", "entries": [{"content": "x", "priority": "high", "status": "pending"}]})), &mut out);
        t.on_update(&upd(json!({"sessionUpdate": "plan", "entries": [{"content": "x", "priority": "high", "status": "completed"}]})), &mut out);
        out.clear();
        t.end_turn(TurnStatus::Interrupted, &mut out);
        let ev = events(out);
        assert!(
            matches!(&ev[0], AdapterEvent::ItemCompleted { key, status: ItemStatus::Completed, .. } if key == "plan-1")
        );
        assert!(
            matches!(&ev[1], AdapterEvent::ItemCompleted { key, status: ItemStatus::Interrupted, .. } if key == "tool-a")
        );
    }

    #[test]
    fn tool_call_update_for_unknown_id_starts_an_item() {
        let mut t = Tracker::new();
        let mut out = Vec::new();
        t.on_update(&upd(json!({"sessionUpdate": "tool_call_update", "toolCallId": "z", "status": "completed", "title": "Done"})), &mut out);
        let ev = events(out);
        assert!(matches!(&ev[0], AdapterEvent::ItemStarted { .. }));
        assert!(matches!(
            &ev[1],
            AdapterEvent::ItemCompleted {
                status: ItemStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn image_chunks_are_unrendered() {
        let mut t = Tracker::new();
        let mut out = Vec::new();
        t.on_update(&upd(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "data": "AA", "mimeType": "image/png"}})), &mut out);
        assert!(matches!(&out[0], Emit::Unrendered(_)));
    }
}
