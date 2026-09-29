//! JSON-RPC 2.0 envelope and error model.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// JSON-RPC protocol marker.
pub const JSONRPC_VERSION: &str = "2.0";

/// Request identifier. Clients use increasing integers per connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::String(s) => f.write_str(s),
        }
    }
}

/// Any JSON-RPC message. The shape is classified with [`RpcMessage::kind`].
///
/// A single flat struct (instead of an untagged enum) keeps parsing errors precise and lets
/// both sides build messages without intermediate types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<RpcError>,
}

/// Classification of an [`RpcMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Notification,
    Response,
    Invalid,
}

impl RpcMessage {
    pub fn request(id: RequestId, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: Some(id),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: None,
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn response_ok(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    pub fn response_err(id: Option<RequestId>, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            method: None,
            params: None,
            result: None,
            error: Some(error),
        }
    }

    pub fn kind(&self) -> MessageKind {
        if self.jsonrpc != JSONRPC_VERSION {
            return MessageKind::Invalid;
        }
        match (&self.id, &self.method, &self.result, &self.error) {
            (Some(_), Some(_), None, None) => MessageKind::Request,
            (None, Some(_), None, None) => MessageKind::Notification,
            (_, None, Some(_), None) | (_, None, None, Some(_)) => MessageKind::Response,
            _ => MessageKind::Invalid,
        }
    }
}

/// Error kinds of the protocol. `code()` gives the JSON-RPC error code; the camelCase name is
/// carried in `error.data.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    Internal,
    NotInitialized,
    Unauthorized,
    NotFound,
    InvalidState,
    CapabilityUnsupported,
    HarnessUnavailable,
    IdempotencyKeyReused,
    PathNotAllowed,
    RateLimited,
    ProtocolVersionUnsupported,
    AlreadyExists,
    AdapterError,
    PayloadTooLarge,
    Draining,
    /// The input starts with a command of the harness that would switch the thread's agent to
    /// another native session (`data.command`, `data.harnessId`).
    SessionSwitchingCommand,
}

impl ErrorKind {
    pub const ALL: [ErrorKind; 20] = [
        ErrorKind::ParseError,
        ErrorKind::InvalidRequest,
        ErrorKind::MethodNotFound,
        ErrorKind::InvalidParams,
        ErrorKind::Internal,
        ErrorKind::NotInitialized,
        ErrorKind::Unauthorized,
        ErrorKind::NotFound,
        ErrorKind::InvalidState,
        ErrorKind::CapabilityUnsupported,
        ErrorKind::HarnessUnavailable,
        ErrorKind::IdempotencyKeyReused,
        ErrorKind::PathNotAllowed,
        ErrorKind::RateLimited,
        ErrorKind::ProtocolVersionUnsupported,
        ErrorKind::AlreadyExists,
        ErrorKind::AdapterError,
        ErrorKind::PayloadTooLarge,
        ErrorKind::Draining,
        ErrorKind::SessionSwitchingCommand,
    ];

    pub fn code(self) -> i32 {
        match self {
            ErrorKind::ParseError => -32700,
            ErrorKind::InvalidRequest => -32600,
            ErrorKind::MethodNotFound => -32601,
            ErrorKind::InvalidParams => -32602,
            ErrorKind::Internal => -32603,
            ErrorKind::NotInitialized => -32000,
            ErrorKind::Unauthorized => -32001,
            ErrorKind::NotFound => -32002,
            ErrorKind::InvalidState => -32003,
            ErrorKind::CapabilityUnsupported => -32004,
            ErrorKind::HarnessUnavailable => -32005,
            ErrorKind::IdempotencyKeyReused => -32006,
            ErrorKind::PathNotAllowed => -32007,
            ErrorKind::RateLimited => -32008,
            ErrorKind::ProtocolVersionUnsupported => -32009,
            ErrorKind::AlreadyExists => -32010,
            ErrorKind::AdapterError => -32011,
            ErrorKind::PayloadTooLarge => -32012,
            ErrorKind::Draining => -32013,
            ErrorKind::SessionSwitchingCommand => -32014,
        }
    }

    pub fn from_code(code: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.code() == code)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::ParseError => "parseError",
            ErrorKind::InvalidRequest => "invalidRequest",
            ErrorKind::MethodNotFound => "methodNotFound",
            ErrorKind::InvalidParams => "invalidParams",
            ErrorKind::Internal => "internal",
            ErrorKind::NotInitialized => "notInitialized",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::NotFound => "notFound",
            ErrorKind::InvalidState => "invalidState",
            ErrorKind::CapabilityUnsupported => "capabilityUnsupported",
            ErrorKind::HarnessUnavailable => "harnessUnavailable",
            ErrorKind::IdempotencyKeyReused => "idempotencyKeyReused",
            ErrorKind::PathNotAllowed => "pathNotAllowed",
            ErrorKind::RateLimited => "rateLimited",
            ErrorKind::ProtocolVersionUnsupported => "protocolVersionUnsupported",
            ErrorKind::AlreadyExists => "alreadyExists",
            ErrorKind::AdapterError => "adapterError",
            ErrorKind::PayloadTooLarge => "payloadTooLarge",
            ErrorKind::Draining => "draining",
            ErrorKind::SessionSwitchingCommand => "sessionSwitchingCommand",
        }
    }

    /// Whether resending the same request can never produce a different outcome.
    ///
    /// Definitive errors are stored in the idempotency table (a resend returns the same
    /// error) and clients drop the request from their outbox. Non-definitive errors are not
    /// stored; clients keep the request and resend it later.
    pub fn is_definitive(self) -> bool {
        match self {
            ErrorKind::ParseError
            | ErrorKind::InvalidRequest
            | ErrorKind::MethodNotFound
            | ErrorKind::InvalidParams
            | ErrorKind::NotFound
            | ErrorKind::InvalidState
            | ErrorKind::CapabilityUnsupported
            | ErrorKind::IdempotencyKeyReused
            | ErrorKind::PathNotAllowed
            | ErrorKind::ProtocolVersionUnsupported
            | ErrorKind::AlreadyExists
            | ErrorKind::PayloadTooLarge
            | ErrorKind::SessionSwitchingCommand => true,
            ErrorKind::Internal
            | ErrorKind::NotInitialized
            | ErrorKind::Unauthorized
            | ErrorKind::HarnessUnavailable
            | ErrorKind::RateLimited
            | ErrorKind::AdapterError
            | ErrorKind::Draining => false,
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// JSON-RPC error object. `data.kind` always carries the [`ErrorKind`] name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, thiserror::Error)]
#[error("{message} ({code})")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        let mut data = Map::new();
        data.insert("kind".to_owned(), Value::String(kind.as_str().to_owned()));
        Self {
            code: kind.code(),
            message: message.into(),
            data: Some(Value::Object(data)),
        }
    }

    /// Adds a detail field to `data`.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        let data = self.data.get_or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(map) = data {
            map.insert(key.to_owned(), value.into());
        }
        self
    }

    /// The error kind, read from `data.kind` (falling back to the code).
    pub fn kind(&self) -> Option<ErrorKind> {
        self.data
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok())
            .or_else(|| ErrorKind::from_code(self.code))
    }

    pub fn not_found(entity: &str, id: impl std::fmt::Display) -> Self {
        Self::new(ErrorKind::NotFound, format!("{entity} {id} not found"))
            .with("entity", entity)
            .with("id", id.to_string())
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidParams, message)
    }

    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidState, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_round_trip() {
        let mut codes = std::collections::HashSet::new();
        for kind in ErrorKind::ALL {
            assert!(codes.insert(kind.code()), "duplicate code for {kind}");
            assert_eq!(ErrorKind::from_code(kind.code()), Some(kind));
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, Value::String(kind.as_str().to_owned()));
        }
    }

    #[test]
    fn error_kind_is_carried_in_data() {
        let err = RpcError::not_found("thread", "thr_x");
        assert_eq!(err.kind(), Some(ErrorKind::NotFound));
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["data"]["kind"], "notFound");
        assert_eq!(json["data"]["entity"], "thread");
    }

    #[test]
    fn message_classification() {
        let req = RpcMessage::request(RequestId::Number(1), "x", Value::Null);
        assert_eq!(req.kind(), MessageKind::Request);
        let note = RpcMessage::notification("x", Value::Null);
        assert_eq!(note.kind(), MessageKind::Notification);
        let ok = RpcMessage::response_ok(RequestId::Number(1), serde_json::json!({}));
        assert_eq!(ok.kind(), MessageKind::Response);
        let err = RpcMessage::response_err(None, RpcError::internal("boom"));
        assert_eq!(err.kind(), MessageKind::Response);
        let mut bad = ok.clone();
        bad.jsonrpc = "1.0".into();
        assert_eq!(bad.kind(), MessageKind::Invalid);
    }
}
