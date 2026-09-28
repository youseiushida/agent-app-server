//! Pure mapping of pi RPC events to [`AdapterEvent`]s.
//!
//! Turn lifecycle (`agent_start`, `agent_settled`, prompt responses) is handled by the
//! session; this module turns the content of a turn into items and notices, and keeps the
//! per-turn facts the session needs to decide the outcome (last stop reason, usage).
//!
//! Fixed mapping (see also `docs/adapters/pi.md`):
//!
//! | pi event                                   | adapter event                                   |
//! |--------------------------------------------|-------------------------------------------------|
//! | `message_update` `text_*` / `thinking_*`   | `agentMessage` / `reasoning` item (start/delta/end) |
//! | `message_end` (assistant)                  | closes its blocks with the authoritative content, usage |
//! | `tool_execution_start/update/end`          | tool item (see [`crate::tools`])                |
//! | `thinking_level_changed`                   | `SessionInfo { effort }`                        |
//! | `compaction_*`, `auto_retry_*`, `summarization_retry_scheduled`, `extension_error` | notices |
//! | `turn_start`, `turn_end`, `agent_end`, `queue_update`, non-assistant messages, `toolcall_*` deltas, `summarization_retry_attempt_start/finished`, `bash_execution_update` | ignored (covered elsewhere) |
//! | anything else                              | `Native`                                        |

use std::collections::HashMap;

use aas_harness::{
    AdapterEvent, ContextUsage, DeltaField, ItemBody, ItemStatus, NoticeLevel, TurnError,
    TurnStatus, Usage,
};
use serde_json::{Value, json};

use crate::tools::{self, ToolKind};
use crate::wire::{PiUsage, content_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

#[derive(Debug)]
struct Block {
    key: String,
    kind: BlockKind,
    open: bool,
    /// Final text announced by the block's `_end` event.
    ended: Option<String>,
}

#[derive(Debug, Default)]
struct Message {
    prefix: String,
    blocks: HashMap<u64, Block>,
}

#[derive(Debug)]
struct Tool {
    key: String,
    kind: ToolKind,
    start: ItemBody,
    /// Output already emitted as deltas.
    emitted: String,
}

/// Facts about the running turn needed to decide its outcome.
#[derive(Debug, Default, Clone)]
struct TurnFacts {
    usage: Option<Usage>,
    /// Context-window occupancy pi reported during the turn (`get_session_stats`).
    context: Option<ContextUsage>,
    last_stop_reason: Option<String>,
    last_error: Option<String>,
    retry_failed: Option<String>,
}

/// Stateful mapper for one session.
#[derive(Debug, Default)]
pub struct Mapper {
    message_counter: u64,
    message: Option<Message>,
    tools: HashMap<String, Tool>,
    turn: TurnFacts,
}

/// Whether an event type produces turn-scoped items (and therefore needs a running turn).
pub fn is_turn_scoped(kind: &str) -> bool {
    matches!(
        kind,
        "message_start"
            | "message_update"
            | "message_end"
            | "tool_execution_start"
            | "tool_execution_update"
            | "tool_execution_end"
    )
}

fn notice(level: NoticeLevel, message: String, code: &str) -> AdapterEvent {
    AdapterEvent::Notice {
        level,
        message,
        code: Some(code.to_owned()),
    }
}

fn block_body(kind: BlockKind, text: String) -> ItemBody {
    match kind {
        BlockKind::Text => ItemBody::AgentMessage { text },
        BlockKind::Thinking => ItemBody::Reasoning { text },
    }
}

impl Mapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resets the per-turn facts. Item keys stay unique for the whole session.
    pub fn begin_turn(&mut self) {
        self.turn = TurnFacts::default();
        self.message = None;
        self.tools.clear();
    }

    /// Usage of the turn so far (sum of finished assistant messages), with the context
    /// occupancy last reported during the turn. A turn with a context but no finished message
    /// gets zero counts so the context still reaches the engine.
    pub fn turn_usage(&self) -> Option<Usage> {
        match (self.turn.usage, self.turn.context) {
            (Some(usage), context) => Some(Usage { context, ..usage }),
            (None, Some(context)) => Some(Usage {
                context: Some(context),
                ..Usage::default()
            }),
            (None, None) => None,
        }
    }

    /// Records the context occupancy pi reported for the running turn.
    pub fn set_context(&mut self, context: ContextUsage) {
        self.turn.context = Some(context);
    }

    /// Outcome of the turn from explicit facts only:
    /// abort requested or last assistant stop reason `aborted` → interrupted;
    /// last stop reason `error` or exhausted retries → failed; otherwise completed.
    pub fn outcome(&self, abort_requested: bool) -> (TurnStatus, Option<TurnError>) {
        if abort_requested || self.turn.last_stop_reason.as_deref() == Some("aborted") {
            return (TurnStatus::Interrupted, None);
        }
        if let Some(err) = &self.turn.retry_failed {
            return (
                TurnStatus::Failed,
                Some(TurnError {
                    message: err.clone(),
                    kind: "harnessError".into(),
                }),
            );
        }
        if self.turn.last_stop_reason.as_deref() == Some("error") {
            let message = self
                .turn
                .last_error
                .clone()
                .unwrap_or_else(|| "the model request failed".into());
            return (
                TurnStatus::Failed,
                Some(TurnError {
                    message,
                    kind: "harnessError".into(),
                }),
            );
        }
        (TurnStatus::Completed, None)
    }

    /// Maps one pi event. `declined` tells whether the user denied a tool call through the
    /// approval gate (so its failed result is reported as `declined`).
    pub fn map(&mut self, ev: &Value, declined: &dyn Fn(&str) -> bool) -> Vec<AdapterEvent> {
        let kind = ev.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "message_start" => {
                if role(ev) == Some("assistant") {
                    self.message_counter += 1;
                    self.message = Some(Message {
                        prefix: format!("m{}", self.message_counter),
                        blocks: HashMap::new(),
                    });
                }
                Vec::new()
            }
            "message_update" => self.message_update(ev),
            "message_end" => self.message_end(ev),
            "tool_execution_start" => self.tool_start(ev),
            "tool_execution_update" => self.tool_update(ev),
            "tool_execution_end" => self.tool_end(ev, declined),
            "thinking_level_changed" => vec![AdapterEvent::SessionInfo {
                model: None,
                permission_mode: None,
                effort: ev.get("level").and_then(Value::as_str).map(str::to_owned),
            }],
            "compaction_start" => {
                let reason = ev.get("reason").and_then(Value::as_str).unwrap_or("manual");
                vec![notice(
                    NoticeLevel::Info,
                    format!("Compacting the conversation ({reason})"),
                    "compaction",
                )]
            }
            "compaction_end" => {
                if ev.get("aborted").and_then(Value::as_bool) == Some(true) {
                    vec![notice(
                        NoticeLevel::Warning,
                        "Compaction was aborted".into(),
                        "compaction",
                    )]
                } else if let Some(err) = ev.get("errorMessage").and_then(Value::as_str) {
                    // pi's own text (e.g. "Compaction failed: Nothing to compact (session too small)").
                    vec![notice(NoticeLevel::Error, err.to_owned(), "compaction")]
                } else {
                    let before = ev.pointer("/result/tokensBefore").and_then(Value::as_u64);
                    let message = match before {
                        Some(n) => format!("Conversation compacted ({n} tokens before)"),
                        None => "Conversation compacted".into(),
                    };
                    vec![notice(NoticeLevel::Info, message, "compaction")]
                }
            }
            "auto_retry_start" => {
                let attempt = ev.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                let max = ev.get("maxAttempts").and_then(Value::as_u64).unwrap_or(0);
                let err = ev
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                vec![notice(
                    NoticeLevel::Warning,
                    format!("Retrying after a transient error (attempt {attempt}/{max}): {err}"),
                    "autoRetry",
                )]
            }
            "auto_retry_end" => {
                if ev.get("success").and_then(Value::as_bool) == Some(false) {
                    let err = ev
                        .get("finalError")
                        .and_then(Value::as_str)
                        .unwrap_or("retries exhausted")
                        .to_owned();
                    self.turn.retry_failed = Some(err.clone());
                    vec![notice(
                        NoticeLevel::Error,
                        format!("Giving up after retries: {err}"),
                        "autoRetry",
                    )]
                } else {
                    Vec::new()
                }
            }
            "summarization_retry_scheduled" => {
                let err = ev
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                vec![notice(
                    NoticeLevel::Warning,
                    format!("Retrying summarization: {err}"),
                    "summarizationRetry",
                )]
            }
            "extension_error" => {
                let path = ev
                    .get("extensionPath")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let event = ev.get("event").and_then(Value::as_str).unwrap_or("?");
                let err = ev.get("error").and_then(Value::as_str).unwrap_or_default();
                vec![notice(
                    NoticeLevel::Error,
                    format!("Extension error in {path} ({event}): {err}"),
                    "extensionError",
                )]
            }
            "turn_start"
            | "turn_end"
            | "agent_end"
            | "queue_update"
            | "summarization_retry_attempt_start"
            | "summarization_retry_finished"
            | "bash_execution_update" => Vec::new(),
            _ => vec![AdapterEvent::Native {
                payload: ev.clone(),
            }],
        }
    }

    fn message_update(&mut self, ev: &Value) -> Vec<AdapterEvent> {
        let Some(delta) = ev.get("assistantMessageEvent") else {
            return Vec::new();
        };
        let dkind = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = delta
            .get("contentIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let block_kind = match dkind {
            "text_start" | "text_delta" | "text_end" => BlockKind::Text,
            "thinking_start" | "thinking_delta" | "thinking_end" => BlockKind::Thinking,
            _ => return Vec::new(),
        };
        let message = self.message.get_or_insert_with(|| {
            // A delta without message_start: open an implicit message so nothing is lost.
            self.message_counter += 1;
            Message {
                prefix: format!("m{}", self.message_counter),
                blocks: HashMap::new(),
            }
        });
        let mut out = Vec::new();
        let block = message.blocks.entry(index).or_insert_with(|| {
            let block = Block {
                key: format!("{}.{}", message.prefix, index),
                kind: block_kind,
                open: true,
                ended: None,
            };
            out.push(AdapterEvent::ItemStarted {
                key: block.key.clone(),
                body: block_body(block.kind, String::new()),
            });
            block
        });
        if block.ended.is_some() {
            // The block already ended; later events for it are ignored.
            return out;
        }
        match dkind {
            "text_delta" | "thinking_delta" => {
                if let Some(text) = delta
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    out.push(AdapterEvent::ItemDelta {
                        key: block.key.clone(),
                        field: DeltaField::Text,
                        text: text.to_owned(),
                    });
                }
            }
            "text_end" | "thinking_end" => {
                // Completion waits for message_end, which is authoritative and tells whether
                // the message was aborted; `_end` only carries the block's final text.
                block.ended = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
        out
    }

    fn message_end(&mut self, ev: &Value) -> Vec<AdapterEvent> {
        if role(ev) != Some("assistant") {
            return Vec::new();
        }
        let msg = &ev["message"];
        let mut out = Vec::new();
        let stop = msg
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let aborted = stop.as_deref() == Some("aborted");
        let mut message = self.message.take().unwrap_or_else(|| {
            self.message_counter += 1;
            Message {
                prefix: format!("m{}", self.message_counter),
                blocks: HashMap::new(),
            }
        });
        if let Some(Value::Array(content)) = msg.get("content") {
            for (index, part) in content.iter().enumerate() {
                let index = index as u64;
                let (kind, text) = match part.get("type").and_then(Value::as_str) {
                    Some("text") => (
                        BlockKind::Text,
                        part.get("text").and_then(Value::as_str).unwrap_or_default(),
                    ),
                    Some("thinking") => (
                        BlockKind::Thinking,
                        part.get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    _ => continue,
                };
                let status = if aborted {
                    ItemStatus::Interrupted
                } else {
                    ItemStatus::Completed
                };
                match message.blocks.get_mut(&index) {
                    Some(block) if block.open => {
                        block.open = false;
                        out.push(AdapterEvent::ItemCompleted {
                            key: block.key.clone(),
                            body: Some(block_body(block.kind, text.to_owned())),
                            status,
                        });
                    }
                    Some(_) => {}
                    None if text.is_empty() => {}
                    None => {
                        // Block never streamed (non-streaming provider): report it whole.
                        let key = format!("{}.{}", message.prefix, index);
                        out.push(AdapterEvent::ItemStarted {
                            key: key.clone(),
                            body: block_body(kind, String::new()),
                        });
                        out.push(AdapterEvent::ItemCompleted {
                            key,
                            body: Some(block_body(kind, text.to_owned())),
                            status,
                        });
                    }
                }
            }
        }
        // Blocks not present in the final content: close them with what `_end` announced
        // (or what was streamed).
        let mut leftover: Vec<&Block> = message.blocks.values().filter(|b| b.open).collect();
        leftover.sort_by_key(|b| b.key.clone());
        for block in leftover {
            out.push(AdapterEvent::ItemCompleted {
                key: block.key.clone(),
                body: block.ended.clone().map(|text| block_body(block.kind, text)),
                status: if aborted {
                    ItemStatus::Interrupted
                } else {
                    ItemStatus::Completed
                },
            });
        }
        if let Some(usage) = msg
            .get("usage")
            .and_then(|u| serde_json::from_value::<PiUsage>(u.clone()).ok())
        {
            let usage = usage.to_protocol();
            let total = self.turn.usage.get_or_insert_with(Usage::default);
            total.accumulate(&usage);
            out.push(AdapterEvent::TurnUsage {
                usage: Usage {
                    context: self.turn.context,
                    ..*total
                },
            });
        }
        let error = msg
            .get("errorMessage")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match stop.as_deref() {
            Some("error") => out.push(notice(
                NoticeLevel::Error,
                error
                    .clone()
                    .unwrap_or_else(|| "the model request failed".into()),
                "assistantError",
            )),
            Some("length") => out.push(notice(
                NoticeLevel::Warning,
                "The response stopped at the output token limit".into(),
                "maxTokens",
            )),
            _ => {}
        }
        self.turn.last_stop_reason = stop;
        self.turn.last_error = error;
        out
    }

    fn tool_start(&mut self, ev: &Value) -> Vec<AdapterEvent> {
        let Some(id) = ev.get("toolCallId").and_then(Value::as_str) else {
            return vec![native(ev)];
        };
        let name = ev.get("toolName").and_then(Value::as_str).unwrap_or("tool");
        let args = ev.get("args").cloned().unwrap_or(Value::Null);
        let (body, kind) = tools::start_body(name, &args);
        let key = tool_key(id);
        self.tools.insert(
            id.to_owned(),
            Tool {
                key: key.clone(),
                kind,
                start: body.clone(),
                emitted: String::new(),
            },
        );
        vec![AdapterEvent::ItemStarted { key, body }]
    }

    fn tool_update(&mut self, ev: &Value) -> Vec<AdapterEvent> {
        let Some(id) = ev.get("toolCallId").and_then(Value::as_str) else {
            return vec![native(ev)];
        };
        let Some(tool) = self.tools.get_mut(id) else {
            return Vec::new();
        };
        if tool.kind == ToolKind::FileChange {
            return Vec::new();
        }
        // partialResult holds the accumulated output so far.
        let text = ev
            .pointer("/partialResult/content")
            .map(content_text)
            .unwrap_or_default();
        if text == tool.emitted {
            return Vec::new();
        }
        let event = if let Some(delta) = text.strip_prefix(tool.emitted.as_str()) {
            AdapterEvent::ItemDelta {
                key: tool.key.clone(),
                field: DeltaField::Output,
                text: delta.to_owned(),
            }
        } else {
            // Output was rewritten (e.g. truncated window): replace it whole.
            AdapterEvent::ItemUpdated {
                key: tool.key.clone(),
                body: tools::with_output(&tool.start, &text),
            }
        };
        tool.emitted = text;
        vec![event]
    }

    fn tool_end(&mut self, ev: &Value, declined: &dyn Fn(&str) -> bool) -> Vec<AdapterEvent> {
        let Some(id) = ev.get("toolCallId").and_then(Value::as_str) else {
            return vec![native(ev)];
        };
        let mut out = Vec::new();
        let tool = match self.tools.remove(id) {
            Some(tool) => tool,
            None => {
                // End without start: synthesize the start so the item exists.
                let name = ev.get("toolName").and_then(Value::as_str).unwrap_or("tool");
                let (body, kind) =
                    tools::start_body(name, &ev.get("args").cloned().unwrap_or(Value::Null));
                out.push(AdapterEvent::ItemStarted {
                    key: tool_key(id),
                    body: body.clone(),
                });
                Tool {
                    key: tool_key(id),
                    kind,
                    start: body,
                    emitted: String::new(),
                }
            }
        };
        let result = ev.get("result").cloned().unwrap_or(Value::Null);
        let body = tools::final_body(&tool.start, tool.kind, &result);
        let is_error = ev.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let status = if declined(id) {
            ItemStatus::Declined
        } else if is_error {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        out.push(AdapterEvent::ItemCompleted {
            key: tool.key,
            body: Some(body),
            status,
        });
        out
    }
}

fn role(ev: &Value) -> Option<&str> {
    ev.pointer("/message/role").and_then(Value::as_str)
}

fn native(ev: &Value) -> AdapterEvent {
    AdapterEvent::Native {
        payload: ev.clone(),
    }
}

pub fn tool_key(tool_call_id: &str) -> String {
    format!("tool:{tool_call_id}")
}

/// Text of a pi `custom` message meant for display (used for extension-injected messages).
pub fn custom_message_notice(ev: &Value) -> Option<AdapterEvent> {
    let msg = ev.get("message")?;
    if msg.get("role").and_then(Value::as_str) != Some("custom")
        || msg.get("display").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    let text = content_text(msg.get("content").unwrap_or(&Value::Null));
    (!text.is_empty()).then(|| notice(NoticeLevel::Info, text, "extensionMessage"))
}

/// Payload used when pi prints a line that is not JSON.
pub fn non_json_payload(line: &str) -> Value {
    json!({ "type": "aas.nonJsonLine", "line": line })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn no(_: &str) -> bool {
        false
    }

    fn upd(ev: Value) -> Value {
        json!({"type": "message_update", "usage": {}, "assistantMessageEvent": ev})
    }

    #[test]
    fn streamed_text_and_thinking_become_items() {
        let mut m = Mapper::new();
        m.begin_turn();
        let mut all = Vec::new();
        all.extend(m.map(
            &json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
            &no,
        ));
        all.extend(m.map(&upd(json!({"type":"thinking_start","contentIndex":0})), &no));
        all.extend(m.map(
            &upd(json!({"type":"thinking_delta","contentIndex":0,"delta":"hmm"})),
            &no,
        ));
        all.extend(m.map(&upd(json!({"type":"text_start","contentIndex":1})), &no));
        all.extend(m.map(
            &upd(json!({"type":"text_delta","contentIndex":1,"delta":"O"})),
            &no,
        ));
        all.extend(m.map(
            &upd(json!({"type":"thinking_end","contentIndex":0,"content":"hmm."})),
            &no,
        ));
        all.extend(m.map(
            &upd(json!({"type":"text_delta","contentIndex":1,"delta":"K"})),
            &no,
        ));
        all.extend(m.map(
            &upd(json!({"type":"text_end","contentIndex":1,"content":"OK"})),
            &no,
        ));
        all.extend(m.map(
            &json!({"type":"message_end","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"hmm."},{"type":"text","text":"OK"}],
                "stopReason":"stop","usage":{"input":10,"output":2,"cacheRead":5,"cacheWrite":0,"cost":{"total":0.5}}}}),
            &no,
        ));
        assert_eq!(
            all,
            vec![
                AdapterEvent::ItemStarted {
                    key: "m1.0".into(),
                    body: ItemBody::Reasoning {
                        text: String::new()
                    }
                },
                AdapterEvent::ItemDelta {
                    key: "m1.0".into(),
                    field: DeltaField::Text,
                    text: "hmm".into()
                },
                AdapterEvent::ItemStarted {
                    key: "m1.1".into(),
                    body: ItemBody::AgentMessage {
                        text: String::new()
                    }
                },
                AdapterEvent::ItemDelta {
                    key: "m1.1".into(),
                    field: DeltaField::Text,
                    text: "O".into()
                },
                AdapterEvent::ItemDelta {
                    key: "m1.1".into(),
                    field: DeltaField::Text,
                    text: "K".into()
                },
                AdapterEvent::ItemCompleted {
                    key: "m1.0".into(),
                    body: Some(ItemBody::Reasoning {
                        text: "hmm.".into()
                    }),
                    status: ItemStatus::Completed
                },
                AdapterEvent::ItemCompleted {
                    key: "m1.1".into(),
                    body: Some(ItemBody::AgentMessage { text: "OK".into() }),
                    status: ItemStatus::Completed
                },
                AdapterEvent::TurnUsage {
                    usage: Usage {
                        input_tokens: 15,
                        output_tokens: 2,
                        cached_input_tokens: 5,
                        reasoning_tokens: 0,
                        cost_usd: Some(0.5),
                        context: None
                    }
                },
            ]
        );
        assert_eq!(m.outcome(false), (TurnStatus::Completed, None));
    }

    #[test]
    fn aborted_message_closes_open_blocks_as_interrupted() {
        let mut m = Mapper::new();
        m.begin_turn();
        m.map(
            &json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
            &no,
        );
        m.map(&upd(json!({"type":"text_start","contentIndex":0})), &no);
        m.map(
            &upd(json!({"type":"text_delta","contentIndex":0,"delta":"1\n2"})),
            &no,
        );
        let out = m.map(
            &json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"1\n2"}],
                "stopReason":"aborted","errorMessage":"Request was aborted","usage":{"input":0,"output":0}}}),
            &no,
        );
        assert!(out.contains(&AdapterEvent::ItemCompleted {
            key: "m1.0".into(),
            body: Some(ItemBody::AgentMessage {
                text: "1\n2".into()
            }),
            status: ItemStatus::Interrupted
        }));
        assert_eq!(m.outcome(false).0, TurnStatus::Interrupted);
    }

    #[test]
    fn unstreamed_blocks_are_reported_whole() {
        let mut m = Mapper::new();
        m.begin_turn();
        let out = m.map(
            &json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"stopReason":"stop"}}),
            &no,
        );
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], AdapterEvent::ItemStarted { key, .. } if key == "m1.0"));
    }

    #[test]
    fn tool_lifecycle_streams_output_and_honours_declines() {
        let mut m = Mapper::new();
        m.begin_turn();
        let start = m.map(&json!({"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{"command":"echo x"}}), &no);
        assert!(matches!(&start[0], AdapterEvent::ItemStarted { key, .. } if key == "tool:c1"));
        let u1 = m.map(&json!({"type":"tool_execution_update","toolCallId":"c1","partialResult":{"content":[]}}), &no);
        assert!(u1.is_empty());
        let u2 = m.map(&json!({"type":"tool_execution_update","toolCallId":"c1","partialResult":{"content":[{"type":"text","text":"x\n"}]}}), &no);
        assert_eq!(
            u2,
            vec![AdapterEvent::ItemDelta {
                key: "tool:c1".into(),
                field: DeltaField::Output,
                text: "x\n".into()
            }]
        );
        let u3 = m.map(&json!({"type":"tool_execution_update","toolCallId":"c1","partialResult":{"content":[{"type":"text","text":"y\n"}]}}), &no);
        assert!(matches!(&u3[0], AdapterEvent::ItemUpdated { .. }));
        let end = m.map(
            &json!({"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","result":{"content":[{"type":"text","text":"y\n"}]},"isError":false}),
            &no,
        );
        assert!(matches!(
            &end[0],
            AdapterEvent::ItemCompleted {
                status: ItemStatus::Completed,
                ..
            }
        ));

        m.map(&json!({"type":"tool_execution_start","toolCallId":"c2","toolName":"bash","args":{"command":"rm -rf /"}}), &no);
        let declined = |id: &str| id == "c2";
        let end = m.map(
            &json!({"type":"tool_execution_end","toolCallId":"c2","toolName":"bash","result":{"content":[{"type":"text","text":"denied"}]},"isError":true}),
            &declined,
        );
        assert!(matches!(
            &end[0],
            AdapterEvent::ItemCompleted {
                status: ItemStatus::Declined,
                ..
            }
        ));
    }

    #[test]
    fn errors_and_retries_decide_the_outcome() {
        let mut m = Mapper::new();
        m.begin_turn();
        m.map(
            &json!({"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"529 overloaded"}}),
            &no,
        );
        assert_eq!(
            m.outcome(false),
            (
                TurnStatus::Failed,
                Some(TurnError {
                    message: "529 overloaded".into(),
                    kind: "harnessError".into()
                })
            )
        );
        m.begin_turn();
        m.map(
            &json!({"type":"auto_retry_end","success":false,"attempt":3,"finalError":"overloaded"}),
            &no,
        );
        assert_eq!(m.outcome(false).0, TurnStatus::Failed);
        m.begin_turn();
        assert_eq!(m.outcome(true).0, TurnStatus::Interrupted);
    }

    #[test]
    fn session_level_and_unknown_events() {
        let mut m = Mapper::new();
        assert_eq!(
            m.map(
                &json!({"type":"thinking_level_changed","level":"high"}),
                &no
            ),
            vec![AdapterEvent::SessionInfo {
                model: None,
                permission_mode: None,
                effort: Some("high".into())
            }]
        );
        assert!(m.map(&json!({"type":"turn_start"}), &no).is_empty());
        let unknown = json!({"type":"entry_appended","entry":{}});
        assert_eq!(
            m.map(&unknown, &no),
            vec![AdapterEvent::Native {
                payload: unknown.clone()
            }]
        );
        let notes = m.map(&json!({"type":"extension_error","extensionPath":"x.ts","event":"tool_call","error":"boom"}), &no);
        assert!(matches!(
            &notes[0],
            AdapterEvent::Notice {
                level: NoticeLevel::Error,
                ..
            }
        ));
    }
}
