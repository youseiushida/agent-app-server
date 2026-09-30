//! Shapes of pi's RPC protocol (`pi --mode rpc`) used by the adapter.
//!
//! Source of truth: `docs/rpc.md` and `dist/modes/rpc/rpc-mode.js` of the installed
//! `@earendil-works/pi-coding-agent` (verified against 0.85.1). Only the fields the adapter
//! reads are modelled; everything else is ignored so newer pi versions keep working.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use aas_harness::{EffortLevel, Model, Usage};

/// Thinking levels in pi's canonical order (`EXTENDED_THINKING_LEVELS` in pi-ai).
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `{"type":"response",…}`
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub command: String,
    pub success: bool,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Response {
    pub fn error_message(&self) -> String {
        self.error
            .clone()
            .unwrap_or_else(|| format!("pi rejected `{}`", self.command))
    }
}

/// A model entry (`get_available_models`, `get_state.model`, `set_model`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub thinking_level_map: Option<Map<String, Value>>,
    #[serde(default)]
    pub context_window: Option<u64>,
}

impl PiModel {
    /// Protocol model id: `provider/id` (pi model ids may themselves contain `/`).
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// Port of pi-ai's `getSupportedThinkingLevels`: models without reasoning only support
    /// `off`; a level mapped to `null` is unsupported; `xhigh`/`max` must be mapped explicitly.
    pub fn supported_thinking_levels(&self) -> Vec<String> {
        if !self.reasoning {
            return vec!["off".to_owned()];
        }
        THINKING_LEVELS
            .iter()
            .filter(|level| {
                let mapped = self
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| m.get(**level));
                match mapped {
                    Some(Value::Null) => false,
                    Some(_) => true,
                    None => !matches!(**level, "xhigh" | "max"),
                }
            })
            .map(|l| (*l).to_owned())
            .collect()
    }

    pub fn to_protocol(&self, is_default: bool) -> Model {
        let name = self.name.clone().unwrap_or_else(|| self.id.clone());
        let description = self
            .context_window
            .map(|w| format!("{} · context {}", self.provider, w));
        Model {
            id: self.qualified_id(),
            display_name: name,
            description,
            is_default,
            effort_levels: Some(self.supported_thinking_levels()),
            permission_modes: None,
        }
    }
}

/// Splits a protocol model id (`provider/id`) at the first `/`.
pub fn split_model_id(id: &str) -> Option<(&str, &str)> {
    let (provider, model) = id.split_once('/')?;
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

/// `get_state` data.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiState {
    #[serde(default)]
    pub model: Option<PiModel>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub is_streaming: bool,
    #[serde(default)]
    pub is_compacting: bool,
    #[serde(default)]
    pub steering_mode: Option<String>,
    #[serde(default)]
    pub follow_up_mode: Option<String>,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub auto_compaction_enabled: Option<bool>,
    #[serde(default)]
    pub message_count: Option<u64>,
    #[serde(default)]
    pub pending_message_count: Option<u64>,
}

/// One entry of `get_commands`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// Where the command comes from (`sourceInfo`: the extension's file, or `<inline:…>` for an
    /// extension pi bundles).
    #[serde(default)]
    pub source_info: Option<PiSourceInfo>,
}

/// `sourceInfo` of a command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PiSourceInfo {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

impl PiCommand {
    /// `sourceInfo.path`, when pi gave one.
    pub fn source_path(&self) -> Option<&str> {
        self.source_info.as_ref()?.path.as_deref()
    }
}

/// `get_session_stats` data (pi 0.85.1 `AgentSession.getSessionStats`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiSessionStats {
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub user_messages: u64,
    #[serde(default)]
    pub assistant_messages: u64,
    #[serde(default)]
    pub tool_calls: u64,
    #[serde(default)]
    pub tool_results: u64,
    #[serde(default)]
    pub total_messages: u64,
    #[serde(default)]
    pub tokens: PiStatsTokens,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub context_usage: Option<PiContextUsage>,
}

/// `tokens` of the session stats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiStatsTokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub total: u64,
}

/// `contextUsage` of the session stats: `tokens` and `percent` are `null` right after a
/// compaction, until the next response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiContextUsage {
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub percent: Option<f64>,
}

/// `get_entries` data: the session's entries in file order (after `since` when it was given)
/// and the current leaf (`null` for a session without entries).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiEntries {
    #[serde(default)]
    pub entries: Vec<Value>,
    #[serde(default)]
    pub leaf_id: Option<String>,
}

impl PiEntries {
    /// The id of the first user message among the entries (file order).
    pub fn first_user_message(&self) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| {
                e.get("type").and_then(Value::as_str) == Some("message")
                    && e.pointer("/message/role").and_then(Value::as_str) == Some("user")
            })
            .and_then(|e| e.get("id"))
            .and_then(Value::as_str)
    }
}

/// One user message of `get_fork_messages`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiForkMessage {
    pub entry_id: String,
    #[serde(default)]
    pub text: String,
}

/// pi's `Usage` object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cost: PiCost,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct PiCost {
    #[serde(default)]
    pub total: f64,
}

impl PiUsage {
    /// Protocol usage. `input_tokens` counts every prompt token (pi reports cache reads and
    /// writes separately from `input`); `cached_input_tokens` is the cache-read subset.
    pub fn to_protocol(self) -> Usage {
        Usage {
            input_tokens: self.input + self.cache_read + self.cache_write,
            output_tokens: self.output,
            cached_input_tokens: self.cache_read,
            reasoning_tokens: self.reasoning,
            cost_usd: Some(self.cost.total),
            context: None,
        }
    }
}

/// Protocol effort levels for pi's thinking levels.
pub fn effort_levels() -> Vec<EffortLevel> {
    THINKING_LEVELS
        .iter()
        .map(|id| EffortLevel {
            id: (*id).to_owned(),
            label: match *id {
                "off" => "Off",
                "minimal" => "Minimal",
                "low" => "Low",
                "medium" => "Medium",
                "high" => "High",
                "xhigh" => "Extra high",
                "max" => "Max",
                _ => id,
            }
            .to_owned(),
        })
        .collect()
}

/// Text content of a pi `content` value (string or array of blocks).
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

// ----- command builders -----

/// Context-window occupancy from the data of a `get_session_stats` response
/// (`contextUsage.tokens` of `contextUsage.contextWindow`). `None` when pi does not report it:
/// no model or window, or `tokens: null` right after a compaction until the next response.
pub fn context_usage(stats: &Value) -> Option<aas_harness::ContextUsage> {
    let usage = stats.get("contextUsage")?;
    let used = usage.get("tokens").and_then(Value::as_u64)?;
    let window = usage
        .get("contextWindow")
        .and_then(Value::as_u64)
        .filter(|w| *w > 0)?;
    Some(aas_harness::ContextUsage {
        used_tokens: used,
        window_tokens: window,
    })
}

pub fn cmd(id: &str, kind: &str) -> Value {
    json!({ "id": id, "type": kind })
}

pub fn cmd_with(id: &str, kind: &str, fields: Value) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(id));
    obj.insert("type".into(), json!(kind));
    if let Value::Object(extra) = fields {
        obj.extend(extra);
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(reasoning: bool, map: Option<Value>) -> PiModel {
        PiModel {
            id: "m".into(),
            name: None,
            provider: "p".into(),
            reasoning,
            input: vec![],
            thinking_level_map: map.and_then(|v| v.as_object().cloned()),
            context_window: None,
        }
    }

    #[test]
    fn thinking_levels_follow_pi_rule() {
        assert_eq!(model(false, None).supported_thinking_levels(), vec!["off"]);
        assert_eq!(
            model(true, None).supported_thinking_levels(),
            vec!["off", "minimal", "low", "medium", "high"]
        );
        // Map observed from DeepSeek V4.1 Flash on pi 0.85.1.
        let map = json!({"minimal":null,"low":"low","medium":null,"high":"high","xhigh":null,"max":"max"});
        assert_eq!(
            model(true, Some(map)).supported_thinking_levels(),
            vec!["off", "low", "high", "max"]
        );
    }

    #[test]
    fn model_ids_are_provider_qualified() {
        let m = PiModel {
            id: "deepseek/deepseek-v4.1-flash".into(),
            ..model(true, None)
        };
        assert_eq!(m.qualified_id(), "p/deepseek/deepseek-v4.1-flash");
        assert_eq!(
            split_model_id("p/deepseek/deepseek-v4.1-flash"),
            Some(("p", "deepseek/deepseek-v4.1-flash"))
        );
        assert_eq!(split_model_id("nope"), None);
        assert_eq!(split_model_id("/x"), None);
    }

    #[test]
    fn usage_counts_cache_as_input() {
        let u: PiUsage = serde_json::from_value(json!({
            "input": 381, "output": 3, "cacheRead": 1536, "cacheWrite": 0, "reasoning": 0,
            "totalTokens": 1920, "cost": {"total": 0.25}
        }))
        .unwrap();
        let p = u.to_protocol();
        assert_eq!(p.input_tokens, 1917);
        assert_eq!(p.cached_input_tokens, 1536);
        assert_eq!(p.output_tokens, 3);
        assert_eq!(p.cost_usd, Some(0.25));
    }

    #[test]
    fn content_text_handles_strings_and_blocks() {
        assert_eq!(content_text(&json!("hi")), "hi");
        assert_eq!(
            content_text(
                &json!([{"type":"text","text":"a"},{"type":"image","data":"x"},{"type":"text","text":"b"}])
            ),
            "ab"
        );
    }
}
