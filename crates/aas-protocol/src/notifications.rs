//! Server → client notifications.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::events::EventEnvelope;
use crate::types::Millis;

/// A server notification (method + params).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum ServerNotification {
    #[serde(rename = "stream/batch")]
    StreamBatch(StreamBatch),
    #[serde(rename = "heartbeat")]
    Heartbeat(Heartbeat),
    #[serde(rename = "connection/replaced")]
    ConnectionReplaced(ConnectionReplaced),
    #[serde(rename = "server/shuttingDown")]
    ServerShuttingDown(ServerShuttingDown),
}

impl ServerNotification {
    pub fn method(&self) -> &'static str {
        match self {
            ServerNotification::StreamBatch(_) => "stream/batch",
            ServerNotification::Heartbeat(_) => "heartbeat",
            ServerNotification::ConnectionReplaced(_) => "connection/replaced",
            ServerNotification::ServerShuttingDown(_) => "server/shuttingDown",
        }
    }

    pub fn params_json(&self) -> serde_json::Value {
        match self {
            ServerNotification::StreamBatch(p) => serde_json::to_value(p),
            ServerNotification::Heartbeat(p) => serde_json::to_value(p),
            ServerNotification::ConnectionReplaced(p) => serde_json::to_value(p),
            ServerNotification::ServerShuttingDown(p) => serde_json::to_value(p),
        }
        .expect("notification params always serialize")
    }

    /// Parses a notification received by a client.
    pub fn parse(method: &str, params: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(serde_json::json!({ "method": method, "params": params }))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StreamBatch {
    pub stream: String,
    /// Head of the stream when the batch was read.
    pub head: u64,
    /// Events after the subscription's position, in `seq` order. Empty when nothing is left
    /// between the position and `head` (the events were deleted by retention): the client then
    /// moves its position to `head` (protocol.md §2.1).
    pub events: Vec<EventEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub server_time: Millis,
    /// Heads of the streams this connection subscribes to (diagnostics).
    pub heads: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConnectionReplaced {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerShuttingDown {
    pub reason: ShutdownReason,
    pub restart_expected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ShutdownReason {
    Drain,
    Shutdown,
    /// The daemon stopped itself because it could no longer persist its event log (the
    /// fail-stop); the watchdog restarts it, and resent requests are handled then.
    StorageFailure,
}
