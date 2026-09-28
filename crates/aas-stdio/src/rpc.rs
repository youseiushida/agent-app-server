use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};

use crate::lines::{JsonLinesReader, LineError, ReadLine, SharedJsonLinesWriter, WriteError};

/// Configuration of an [`RpcPeer`].
#[derive(Debug, Clone)]
pub struct RpcPeerConfig {
    /// Whether outgoing messages carry `"jsonrpc":"2.0"` (Codex omits it, ACP requires it).
    pub emit_jsonrpc_field: bool,
    /// Upper bound of a single incoming line.
    pub max_line_bytes: usize,
    /// Name used in logs (e.g. `codex[thr_…]`).
    pub label: String,
}

/// JSON-RPC error object received from (or sent to) the peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct RpcWireError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<Value>,
}

impl RpcWireError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("method not supported by client: {method}"))
    }
}

/// Failure of an outgoing request.
#[derive(Debug, thiserror::Error)]
pub enum RpcCallError {
    #[error("peer returned an error: {0}")]
    Rpc(RpcWireError),
    #[error("connection to the peer is closed")]
    Closed,
    #[error("i/o error: {0}")]
    Io(String),
    #[error("could not decode the response: {0}")]
    Decode(String),
    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),
}

/// A message the peer sent us.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request(IncomingRequest),
    /// A line that is not a JSON-RPC message (kept for diagnostics).
    Malformed(String),
}

/// A request initiated by the peer; answer it with [`RpcPeer::respond`] or
/// [`RpcPeer::respond_error`].
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

type Pending = HashMap<i64, oneshot::Sender<Result<Value, RpcWireError>>>;

struct Inner {
    writer: SharedJsonLinesWriter,
    pending: std::sync::Mutex<Option<Pending>>,
    next_id: AtomicI64,
    config: RpcPeerConfig,
    closed_tx: watch::Sender<bool>,
}

/// A JSON-RPC 2.0 endpoint over JSON Lines.
///
/// Incoming notifications and requests are delivered on an unbounded channel: the reader
/// must never block on the consumer, otherwise a consumer awaiting a response while the
/// reader waits for channel capacity would deadlock. Consumers drain it in a dedicated task.
#[derive(Clone)]
pub struct RpcPeer {
    inner: Arc<Inner>,
}

impl RpcPeer {
    /// Starts reading from `reader`. Returns the peer and the incoming message channel,
    /// which closes when the peer's output ends.
    pub fn start<R, W>(
        reader: R,
        writer: W,
        config: RpcPeerConfig,
    ) -> (RpcPeer, mpsc::UnboundedReceiver<Incoming>)
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (closed_tx, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            writer: SharedJsonLinesWriter::new(writer),
            pending: std::sync::Mutex::new(Some(HashMap::new())),
            next_id: AtomicI64::new(1),
            config,
            closed_tx,
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let reader_inner = inner.clone();
        tokio::spawn(async move {
            read_loop(reader_inner, reader, tx).await;
        });
        (RpcPeer { inner }, rx)
    }

    /// Sends a request and waits for its response.
    pub async fn request<P: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<T, RpcCallError> {
        let params =
            serde_json::to_value(params).map_err(|e| RpcCallError::Decode(e.to_string()))?;
        let value = self.request_value(method, params).await?;
        serde_json::from_value(value).map_err(|e| RpcCallError::Decode(format!("{method}: {e}")))
    }

    /// Like [`request`](Self::request) with a deadline.
    pub async fn request_timeout<P: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        timeout: std::time::Duration,
    ) -> Result<T, RpcCallError> {
        match tokio::time::timeout(timeout, self.request(method, params)).await {
            Ok(r) => r,
            Err(_) => Err(RpcCallError::Timeout(timeout)),
        }
    }

    /// Sends a request with raw JSON params and returns the raw result.
    pub async fn request_value(&self, method: &str, params: Value) -> Result<Value, RpcCallError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            match pending.as_mut() {
                Some(map) => {
                    map.insert(id, tx);
                }
                None => return Err(RpcCallError::Closed),
            }
        }
        // Forgets the pending entry however this call ends, also when the caller drops it (a
        // deadline such as `request_timeout`): a late response then finds no entry.
        let _pending = PendingGuard { peer: self, id };
        let mut msg = Map::new();
        msg.insert("id".into(), json!(id));
        msg.insert("method".into(), json!(method));
        msg.insert("params".into(), params);
        self.write(msg).await?;
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(RpcCallError::Rpc(e)),
            Err(_) => Err(RpcCallError::Closed),
        }
    }

    /// Sends a notification.
    pub async fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), RpcCallError> {
        let params =
            serde_json::to_value(params).map_err(|e| RpcCallError::Decode(e.to_string()))?;
        let mut msg = Map::new();
        msg.insert("method".into(), json!(method));
        msg.insert("params".into(), params);
        self.write(msg).await
    }

    /// Answers a peer request successfully.
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), RpcCallError> {
        let mut msg = Map::new();
        msg.insert("id".into(), id);
        msg.insert("result".into(), result);
        self.write(msg).await
    }

    /// Answers a peer request with an error.
    pub async fn respond_error(&self, id: Value, error: RpcWireError) -> Result<(), RpcCallError> {
        let mut msg = Map::new();
        msg.insert("id".into(), id);
        msg.insert(
            "error".into(),
            serde_json::to_value(error).expect("error serializes"),
        );
        self.write(msg).await
    }

    /// Closes our side of the stream (the child sees EOF on stdin). Pending requests keep
    /// waiting for responses until the peer's output ends. Returns promptly even while a
    /// write is stuck on a full pipe (that write fails with [`RpcCallError::Closed`]).
    pub async fn close_writer(&self) {
        self.inner.writer.close().await;
    }

    /// Whether the peer's output has ended.
    pub fn is_closed(&self) -> bool {
        *self.inner.closed_tx.borrow()
    }

    /// Resolves once the peer's output has ended.
    pub async fn closed(&self) {
        let mut rx = self.inner.closed_tx.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    fn take_pending(&self, id: i64) {
        if let Some(map) = self.inner.pending.lock().expect("pending lock").as_mut() {
            map.remove(&id);
        }
    }

    /// Requests waiting for a response (tests).
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .as_ref()
            .map_or(0, HashMap::len)
    }

    async fn write(&self, mut msg: Map<String, Value>) -> Result<(), RpcCallError> {
        if self.inner.config.emit_jsonrpc_field {
            msg.insert("jsonrpc".into(), json!("2.0"));
        }
        self.inner
            .writer
            .send(&Value::Object(msg))
            .await
            .map_err(|e| match e {
                WriteError::Closed => RpcCallError::Closed,
                WriteError::Io(e) => RpcCallError::Io(e.to_string()),
            })
    }
}
/// Removes a request's entry from the pending map when dropped (see
/// [`RpcPeer::request_value`]). After a response the entry is already gone.
struct PendingGuard<'a> {
    peer: &'a RpcPeer,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.peer.take_pending(self.id);
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    inner: Arc<Inner>,
    reader: R,
    tx: mpsc::UnboundedSender<Incoming>,
) {
    let label = inner.config.label.clone();
    let mut reader = JsonLinesReader::new(reader, inner.config.max_line_bytes);
    loop {
        match reader.next().await {
            Ok(Some(ReadLine::Json(value))) => dispatch(&inner, &tx, value),
            Ok(Some(ReadLine::NotJson(line))) => {
                tracing::debug!(peer = %label, "non-JSON line from peer");
                let _ = tx.send(Incoming::Malformed(line));
            }
            Err(LineError::TooLong { max }) => {
                tracing::warn!(peer = %label, max, "peer sent an oversized line; skipped");
                let _ = tx.send(Incoming::Malformed(format!(
                    "<line longer than {max} bytes skipped>"
                )));
            }
            Ok(None) => break,
            Err(LineError::Io(e)) => {
                tracing::debug!(peer = %label, error = %e, "peer output failed");
                break;
            }
        }
    }
    // Fail everything still waiting and mark the peer closed.
    let pending = inner.pending.lock().expect("pending lock").take();
    drop(pending);
    inner.closed_tx.send_replace(true);
}

fn dispatch(inner: &Inner, tx: &mpsc::UnboundedSender<Incoming>, value: Value) {
    let Value::Object(mut obj) = value else {
        let _ = tx.send(Incoming::Malformed(value.to_string()));
        return;
    };
    let id = obj.remove("id").filter(|v| !v.is_null());
    let method = obj.remove("method");
    match (id, method) {
        (Some(id), Some(Value::String(method))) => {
            let params = obj.remove("params").unwrap_or(Value::Null);
            let _ = tx.send(Incoming::Request(IncomingRequest { id, method, params }));
        }
        (None, Some(Value::String(method))) => {
            let params = obj.remove("params").unwrap_or(Value::Null);
            let _ = tx.send(Incoming::Notification { method, params });
        }
        (Some(id), None) => {
            let key = match &id {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            };
            let sender = key.and_then(|k| {
                inner
                    .pending
                    .lock()
                    .expect("pending lock")
                    .as_mut()
                    .and_then(|m| m.remove(&k))
            });
            let Some(sender) = sender else {
                tracing::warn!(peer = %inner.config.label, id = %id, "response for unknown request id");
                return;
            };
            let outcome = if let Some(err) = obj.remove("error") {
                Err(serde_json::from_value::<RpcWireError>(err.clone())
                    .unwrap_or_else(|_| RpcWireError::new(-32603, err.to_string())))
            } else {
                Ok(obj.remove("result").unwrap_or(Value::Null))
            };
            let _ = sender.send(outcome);
        }
        (id, method) => {
            let mut rebuilt = obj;
            if let Some(id) = id {
                rebuilt.insert("id".into(), id);
            }
            if let Some(m) = method {
                rebuilt.insert("method".into(), m);
            }
            let _ = tx.send(Incoming::Malformed(Value::Object(rebuilt).to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn config(emit: bool) -> RpcPeerConfig {
        RpcPeerConfig {
            emit_jsonrpc_field: emit,
            max_line_bytes: 1 << 20,
            label: "test".into(),
        }
    }

    #[tokio::test]
    async fn request_response_notification_and_server_request() {
        // client <-> fake server over in-memory duplex pipes
        let (client_out, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, client_in) = tokio::io::duplex(64 * 1024);
        let (peer, mut incoming) = RpcPeer::start(client_in, client_out, config(false));

        let server = tokio::spawn(async move {
            let mut lines = BufReader::new(server_in).lines();
            let mut out = server_out;
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(
                req.get("jsonrpc").is_none(),
                "codex-style peer must omit jsonrpc"
            );
            assert_eq!(req["method"], "thread/start");
            // A notification and a server-initiated request before the response.
            out.write_all(b"{\"method\":\"thread/started\",\"params\":{\"x\":1}}\n")
                .await
                .unwrap();
            out.write_all(b"{\"id\":\"s1\",\"method\":\"item/commandExecution/requestApproval\",\"params\":{}}\n")
                .await
                .unwrap();
            let resp = json!({"id": req["id"], "result": {"thread": {"id": "t"}}});
            out.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
            // Our answer to the server request.
            let answer: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(
                answer,
                json!({"id": "s1", "result": {"decision": "accept"}})
            );
            // An error response to the second request.
            let req2: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let err = json!({"id": req2["id"], "error": {"code": -32000, "message": "nope"}});
            out.write_all(format!("{err}\n").as_bytes()).await.unwrap();
            out.shutdown().await.unwrap();
        });

        let consumer_peer = peer.clone();
        let consumer = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(msg) = incoming.recv().await {
                if let Incoming::Request(req) = &msg {
                    consumer_peer
                        .respond(req.id.clone(), json!({"decision": "accept"}))
                        .await
                        .unwrap();
                }
                seen.push(msg);
            }
            seen
        });

        let result: Value = peer.request("thread/start", json!({})).await.unwrap();
        assert_eq!(result, json!({"thread": {"id": "t"}}));
        let err = peer
            .request_value("turn/start", json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcCallError::Rpc(ref e) if e.code == -32000),
            "{err:?}"
        );

        server.await.unwrap();
        let seen = consumer.await.unwrap();
        assert_eq!(seen.len(), 2);
        assert!(
            matches!(&seen[0], Incoming::Notification { method, .. } if method == "thread/started")
        );
        peer.closed().await;
        assert!(peer.is_closed());
        assert!(matches!(
            peer.request_value("x", json!({})).await,
            Err(RpcCallError::Closed)
        ));
    }

    #[tokio::test]
    async fn a_request_that_times_out_is_forgotten() {
        let (client_out, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, client_in) = tokio::io::duplex(64 * 1024);
        let (peer, _incoming) = RpcPeer::start(client_in, client_out, config(false));
        let err = peer
            .request_timeout::<_, Value>(
                "turn/interrupt",
                json!({}),
                std::time::Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcCallError::Timeout(_)), "{err:?}");
        assert_eq!(
            peer.pending_count(),
            0,
            "the abandoned request leaves no pending entry"
        );
        // Its late response is ignored; the next request still gets its own.
        let mut lines = BufReader::new(server_in).lines();
        let late: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let mut out = server_out;
        out.write_all(format!("{}\n", json!({"id": late["id"], "result": {}})).as_bytes())
            .await
            .unwrap();
        let next = {
            let peer = peer.clone();
            tokio::spawn(async move { peer.request_value("model/list", json!({})).await })
        };
        let req: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        out.write_all(format!("{}\n", json!({"id": req["id"], "result": {"data": []}})).as_bytes())
            .await
            .unwrap();
        assert_eq!(next.await.unwrap().unwrap(), json!({"data": []}));
        assert_eq!(peer.pending_count(), 0);
    }

    #[tokio::test]
    async fn pending_requests_fail_when_peer_exits() {
        let (client_out, _server_in) = tokio::io::duplex(1024);
        let (server_out, client_in) = tokio::io::duplex(1024);
        let (peer, _incoming) = RpcPeer::start(client_in, client_out, config(true));
        let waiter = {
            let peer = peer.clone();
            tokio::spawn(async move { peer.request_value("slow", json!({})).await })
        };
        tokio::task::yield_now().await;
        drop(server_out);
        let err = waiter.await.unwrap().unwrap_err();
        assert!(matches!(err, RpcCallError::Closed), "{err:?}");
    }

    #[tokio::test]
    async fn close_writer_is_not_blocked_by_a_stuck_request() {
        // The peer never reads its stdin: a large request blocks on the full pipe.
        let (client_out, _server_in) = tokio::io::duplex(64);
        let (_server_out, client_in) = tokio::io::duplex(64);
        let (peer, _incoming) = RpcPeer::start(client_in, client_out, config(true));
        let stuck = {
            let peer = peer.clone();
            tokio::spawn(async move {
                peer.request_value("session/prompt", json!({"text": "p".repeat(70 * 1024)}))
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!stuck.is_finished());
        tokio::time::timeout(std::time::Duration::from_secs(5), peer.close_writer())
            .await
            .expect("close_writer returned");
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), stuck)
            .await
            .expect("request ended")
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, RpcCallError::Closed), "{err:?}");
        assert!(matches!(
            peer.notify("x", json!({})).await,
            Err(RpcCallError::Closed)
        ));
    }

    #[tokio::test]
    async fn emits_jsonrpc_field_when_configured() {
        let (client_out, server_in) = tokio::io::duplex(1024);
        let (_server_out, client_in) = tokio::io::duplex(1024);
        let (peer, _incoming) = RpcPeer::start(client_in, client_out, config(true));
        peer.notify("session/cancel", json!({"sessionId": "s"}))
            .await
            .unwrap();
        let mut lines = BufReader::new(server_in).lines();
        let msg: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            msg,
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s"}})
        );
    }
}
