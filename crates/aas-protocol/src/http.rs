//! Bodies of the plain HTTP endpoints.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::*;
use crate::types::*;

/// `POST /v1/pair` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub code: String,
    pub device_name: String,
    pub platform: String,
}

/// `POST /v1/pair` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub device_id: DeviceId,
    pub token: String,
    pub server: PairServerInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairServerInfo {
    pub name: String,
    pub epoch: String,
}

/// `POST /v1/blobs` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BlobUploadResponse {
    pub blob_id: BlobId,
    pub mime: String,
    pub size: u64,
}

/// Error body of every HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HttpError {
    pub kind: String,
    pub message: String,
}

/// `GET /v1/healthz` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Health {
    pub ok: bool,
}

/// `POST /v1/admin/pairing-codes` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminPairingCodeRequest {}

/// `POST /v1/admin/pairing-codes` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminPairingCodeResponse {
    pub code: String,
    pub expires_at: Millis,
    /// `aas://pair?u=…&c=…&n=…`
    pub pair_url: String,
}

/// `GET /v1/admin/devices` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminDevicesResponse {
    pub devices: Vec<Device>,
}

/// `GET /v1/admin/status` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminStatusResponse {
    pub version: String,
    pub epoch: String,
    pub uptime_ms: u64,
    pub listen: String,
    pub public_url: Option<String>,
    pub running_processes: u32,
    pub running_turns: u32,
    /// Background tasks that keep an agent's process alive (see `server/status`).
    #[serde(default)]
    pub running_background_tasks: u32,
    pub connected_devices: u32,
    pub draining: bool,
}

/// `POST /v1/admin/stop` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminStopRequest {
    #[serde(default)]
    pub drain: bool,
}

/// Builds the pairing URL placed in the QR code.
pub fn pair_url(ws_url: &str, code: &str, server_name: &str) -> String {
    fn enc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    format!(
        "aas://pair?u={}&c={}&n={}",
        enc(ws_url),
        enc(code),
        enc(server_name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_url_encodes_components() {
        let url = pair_url("wss://pc.tail-x.ts.net/v1/ws", "ABCD-1234", "home pc");
        assert_eq!(
            url,
            "aas://pair?u=wss%3A%2F%2Fpc.tail-x.ts.net%2Fv1%2Fws&c=ABCD-1234&n=home%20pc"
        );
    }
}
