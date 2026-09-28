//! Hand-written serde types for the subset of ACP v1 this adapter speaks.
//!
//! Every field is tolerant (`#[serde(default)]`, enums kept as strings) in the same spirit as
//! the official schema's `x-deserialize-default-on-error`: a single unexpected value must not
//! make a whole notification unreadable. Unknown variants are handled explicitly by
//! [`crate::mapping`] instead of failing deserialization.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version this adapter implements.
pub const PROTOCOL_VERSION: u16 = 1;

/// JSON-RPC error code ACP uses for "authentication required".
pub const AUTH_REQUIRED: i64 = -32000;
/// JSON-RPC error code ACP uses for "request cancelled".
pub const REQUEST_CANCELLED: i64 = -32800;

// ----- initialize ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u16,
    pub client_capabilities: ClientCapabilities,
    pub client_info: Implementation,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FileSystemCapabilities,
    pub terminal: bool,
    /// Both elicitation modes are relayed to the user as questions (see `crate::elicitation`).
    pub elicitation: ElicitationCapabilities,
    /// Custom capabilities (ACP extensibility: advertised in the capability object's `_meta`,
    /// ignored by agents that do not know them). See `crate::cognition::client_meta`.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// `{"form": {}, "url": {}}`: an empty object advertises a mode.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ElicitationCapabilities {
    pub form: Advertised,
    pub url: Advertised,
}

/// Serializes as `{}` (ACP advertises a capability with an empty object).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Advertised {}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    #[serde(default)]
    pub protocol_version: u16,
    #[serde(default)]
    pub agent_capabilities: AgentCapabilities,
    #[serde(default)]
    pub auth_methods: Vec<AuthMethod>,
    #[serde(default)]
    pub agent_info: Option<Implementation>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
    #[serde(default)]
    pub prompt_capabilities: PromptCapabilities,
    #[serde(default)]
    pub session_capabilities: SessionCapabilities,
    /// The agent's custom capabilities (ACP extensibility). Read only for the extensions this
    /// adapter implements (`crate::cognition::confirmed`).
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub embedded_context: bool,
}

/// Each capability is advertised by an object (`{}`); absence or `null` means unsupported.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    #[serde(default)]
    pub list: Option<Value>,
    #[serde(default)]
    pub resume: Option<Value>,
    /// UNSTABLE in ACP v1 (`session/fork`).
    #[serde(default)]
    pub fork: Option<Value>,
}

impl SessionCapabilities {
    fn advertised(v: &Option<Value>) -> bool {
        matches!(v, Some(Value::Object(_)))
    }
    pub fn list(&self) -> bool {
        Self::advertised(&self.list)
    }
    pub fn resume(&self) -> bool {
        Self::advertised(&self.resume)
    }
    pub fn fork(&self) -> bool {
        Self::advertised(&self.fork)
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethod {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `terminal` for methods the user runs in a terminal; absent means `agent`.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

// ----- sessions -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    pub mcp_servers: Vec<Value>,
}

/// Parameters of `session/load`, `session/resume` and `session/fork` (same shape).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingSessionRequest {
    pub session_id: String,
    pub cwd: String,
    pub mcp_servers: Vec<Value>,
}

/// Response of `session/new`, `session/load`, `session/resume`, `session/fork`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetupResponse {
    /// Present for `session/new` and `session/fork`.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub modes: Option<SessionModeState>,
    #[serde(default)]
    pub config_options: Option<Vec<ConfigOption>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionModeState {
    #[serde(default)]
    pub current_mode_id: String,
    #[serde(default)]
    pub available_modes: Vec<SessionMode>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMode {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A session configuration option. Only `select` options are modelled; `boolean` options are
/// kept (so the full list round-trips in the cache) but never mapped to settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub current_value: Value,
    /// Either a flat list of values or a list of groups; see [`ConfigOption::values`].
    #[serde(default)]
    pub options: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigValue {
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

impl ConfigOption {
    /// Whether this is a single-value selector (the default when `type` is absent).
    pub fn is_select(&self) -> bool {
        matches!(self.kind.as_deref(), None | Some("select"))
    }

    /// Current value of a select option.
    pub fn current(&self) -> Option<&str> {
        self.current_value.as_str()
    }

    /// Flattened selectable values (groups are expanded in order).
    pub fn values(&self) -> Vec<ConfigValue> {
        let Value::Array(entries) = &self.options else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries {
            if let Some(Value::Array(group)) = entry.get("options") {
                out.extend(
                    group
                        .iter()
                        .filter_map(|v| serde_json::from_value::<ConfigValue>(v.clone()).ok()),
                );
            } else if let Ok(v) = serde_json::from_value::<ConfigValue>(entry.clone())
                && !v.value.is_empty()
            {
                out.push(v);
            }
        }
        out
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionRequest {
    pub session_id: String,
    pub config_id: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionResponse {
    #[serde(default)]
    pub config_options: Option<Vec<ConfigOption>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeRequest {
    pub session_id: String,
    pub mode_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsRequest {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    pub session_id: String,
}

// ----- prompt -------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    #[serde(default)]
    pub stop_reason: String,
    /// UNSTABLE in ACP v1: token usage of the turn.
    #[serde(default)]
    pub usage: Option<PromptUsage>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub thought_tokens: Option<u64>,
    #[serde(default)]
    pub cached_read_tokens: Option<u64>,
}

// ----- session/update -----------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    #[serde(default)]
    pub session_id: String,
    pub update: Value,
}

/// A parsed `session/update` payload. Unknown or malformed updates stay raw.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdate {
    UserMessageChunk(ContentChunk),
    AgentMessageChunk(ContentChunk),
    AgentThoughtChunk(ContentChunk),
    ToolCall(ToolCallFields),
    ToolCallUpdate(ToolCallFields),
    Plan(PlanUpdate),
    AvailableCommands(AvailableCommandsUpdate),
    CurrentMode(CurrentModeUpdate),
    ConfigOptions(ConfigOptionUpdate),
    SessionInfo(SessionInfoUpdate),
    Usage(UsageUpdate),
    /// Unknown discriminator or undecodable payload.
    Unknown(Value),
}

impl SessionUpdate {
    pub fn parse(update: Value) -> SessionUpdate {
        fn de<T: serde::de::DeserializeOwned>(
            v: &Value,
            wrap: fn(T) -> SessionUpdate,
        ) -> SessionUpdate {
            match serde_json::from_value::<T>(v.clone()) {
                Ok(t) => wrap(t),
                Err(_) => SessionUpdate::Unknown(v.clone()),
            }
        }
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match kind.as_str() {
            "user_message_chunk" => de(&update, SessionUpdate::UserMessageChunk),
            "agent_message_chunk" => de(&update, SessionUpdate::AgentMessageChunk),
            "agent_thought_chunk" => de(&update, SessionUpdate::AgentThoughtChunk),
            "tool_call" => de(&update, SessionUpdate::ToolCall),
            "tool_call_update" => de(&update, SessionUpdate::ToolCallUpdate),
            "plan" => de(&update, SessionUpdate::Plan),
            "available_commands_update" => de(&update, SessionUpdate::AvailableCommands),
            "current_mode_update" => de(&update, SessionUpdate::CurrentMode),
            "config_option_update" => de(&update, SessionUpdate::ConfigOptions),
            "session_info_update" => de(&update, SessionUpdate::SessionInfo),
            "usage_update" => de(&update, SessionUpdate::Usage),
            _ => SessionUpdate::Unknown(update),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContentChunk {
    pub content: Value,
    #[serde(default)]
    pub message_id: Option<String>,
}

/// Fields of `tool_call` and `tool_call_update`. For an update every field except the id is
/// optional and, when present, replaces the previous value (ACP semantics).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallFields {
    #[serde(default)]
    pub tool_call_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub content: Option<Vec<Value>>,
    #[serde(default)]
    pub locations: Option<Vec<Value>>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default)]
    pub raw_output: Option<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanUpdate {
    #[serde(default)]
    pub entries: Vec<PlanEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandsUpdate {
    #[serde(default)]
    pub available_commands: Vec<AvailableCommand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input: Option<CommandInput>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommandInput {
    #[serde(default)]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModeUpdate {
    #[serde(default)]
    pub current_mode_id: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoUpdate {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageUpdate {
    #[serde(default)]
    pub used: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub cost: Option<Cost>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    #[serde(default)]
    pub amount: f64,
    #[serde(default)]
    pub currency: String,
}

// ----- elicitation ---------------------------------------------------------------------------

/// Parameters of `elicitation/create` (stable in ACP v1 since schema 1.21.0). The mode-specific
/// fields and both scopes (session / request) are flattened; which ones are present depends on
/// `mode`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateElicitationParams {
    #[serde(default)]
    pub message: String,
    /// `form`, `url`, or a custom/future mode.
    #[serde(default)]
    pub mode: String,
    /// Session scope.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Request scope (outside any session).
    #[serde(default)]
    pub request_id: Option<Value>,
    /// Form mode: a JSON Schema object with primitive properties.
    #[serde(default)]
    pub requested_schema: Option<Value>,
    /// URL mode.
    #[serde(default)]
    pub elicitation_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Parameters of the `elicitation/complete` notification (URL mode).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompleteElicitationParams {
    #[serde(default)]
    pub elicitation_id: String,
}

// ----- session/request_permission ------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub tool_call: ToolCallFields,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    #[serde(default)]
    pub option_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
}

/// `RequestPermissionResponse` body.
pub fn permission_selected(option_id: &str) -> Value {
    serde_json::json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
}

/// `RequestPermissionResponse` body for a cancelled turn.
pub fn permission_cancelled() -> Value {
    serde_json::json!({ "outcome": { "outcome": "cancelled" } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_capabilities_require_objects() {
        let caps: SessionCapabilities =
            serde_json::from_value(json!({"list": {}, "resume": null, "fork": true})).unwrap();
        assert!(caps.list());
        assert!(!caps.resume());
        assert!(!caps.fork(), "only an object advertises a capability");
    }

    #[test]
    fn config_option_values_flatten_groups() {
        let opt: ConfigOption = serde_json::from_value(json!({
            "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "b",
            "options": [
                {"group": "g1", "name": "G1", "options": [{"value": "a", "name": "A"}]},
                {"group": "g2", "name": "G2", "options": [{"value": "b", "name": "B", "description": "bee"}]}
            ]
        }))
        .unwrap();
        let values: Vec<_> = opt.values().into_iter().map(|v| v.value).collect();
        assert_eq!(values, vec!["a", "b"]);
        assert_eq!(opt.current(), Some("b"));
        assert!(opt.is_select());
    }

    #[test]
    fn unknown_and_malformed_updates_stay_raw() {
        let raw = json!({"sessionUpdate": "brand_new_thing", "x": 1});
        assert_eq!(
            SessionUpdate::parse(raw.clone()),
            SessionUpdate::Unknown(raw)
        );
        let bad = json!({"sessionUpdate": "agent_message_chunk"});
        assert!(matches!(
            SessionUpdate::parse(bad),
            SessionUpdate::Unknown(_)
        ));
        let ok = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}});
        assert!(matches!(
            SessionUpdate::parse(ok),
            SessionUpdate::AgentMessageChunk(_)
        ));
    }
}
