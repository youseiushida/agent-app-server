//! Reference client implementing the client obligations of docs/protocol.md §7:
//!
//! 1. events are applied together with the cursor, and `seq <= cursor` is ignored; a batch
//!    without events moves the cursor to its `head` (the events up to it were deleted);
//! 2. every mutating call goes through an outbox with a fixed `clientRequestId` and is resent
//!    after reconnecting (or, after a non-definitive error, after a short delay) until a
//!    response (success or definitive error) arrives;
//! 3. a connection with no inbound frame for `clientTimeoutMs` is closed and re-established
//!    (the watchdog also bounds sends, which can stall on a dead path);
//! 4. after reconnecting, streams are resubscribed from their cursors.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use aas_protocol::events::EventEnvelope;
use aas_protocol::notifications::StreamBatch;
use aas_protocol::rpc::{MessageKind, RpcMessage};
use aas_protocol::{RequestId, RpcError};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub url: String,
    pub token: String,
    /// Upper bound of the reconnect backoff.
    pub backoff_max: Duration,
}

#[derive(Debug, Clone)]
struct Pending {
    crid: String,
    method: String,
    params: Value,
}

/// Observable client state.
#[derive(Debug, Default)]
pub struct ClientState {
    pub cursors: BTreeMap<String, u64>,
    pub events: BTreeMap<String, Vec<EventEnvelope>>,
    outbox: VecDeque<Pending>,
    pub results: HashMap<String, Result<Value, RpcError>>,
    pub epoch: Option<String>,
    /// Successful `initialize` handshakes (first connection included).
    pub connects: u32,
    /// Sends of outbox entries beyond their first.
    pub resent: u32,
    /// Number of sends per clientRequestId.
    pub attempts: HashMap<String, u32>,
    subscriptions: BTreeSet<String>,
}

struct Inner {
    config: ClientConfig,
    state: Mutex<ClientState>,
    changed: Notify,
    wake: mpsc::UnboundedSender<()>,
}

pub struct ReliableClient {
    inner: Arc<Inner>,
    task: JoinHandle<()>,
}

impl Drop for ReliableClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ReliableClient {
    pub fn start(config: ClientConfig) -> Self {
        let (wake, wake_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            config,
            state: Mutex::new(ClientState::default()),
            changed: Notify::new(),
            wake,
        });
        inner.state.lock().subscriptions.insert("workspace".into());
        let task = tokio::spawn(run(inner.clone(), wake_rx));
        Self { inner, task }
    }

    /// Adds a stream subscription (resubscribed from its cursor after every reconnect).
    pub fn subscribe(&self, stream: &str) {
        self.inner
            .state
            .lock()
            .subscriptions
            .insert(stream.to_owned());
        let _ = self.inner.wake.send(());
    }

    /// Queues a mutating call; returns its clientRequestId.
    pub fn mutate(&self, method: &str, mut params: Value) -> String {
        let crid = uuid::Uuid::new_v4().to_string();
        params["clientRequestId"] = Value::String(crid.clone());
        self.inner.state.lock().outbox.push_back(Pending {
            crid: crid.clone(),
            method: method.to_owned(),
            params,
        });
        let _ = self.inner.wake.send(());
        crid
    }

    /// Waits for the result of a queued call.
    pub async fn result(&self, crid: &str, timeout: Duration) -> Result<Value, RpcError> {
        let crid = crid.to_owned();
        self.wait_until(timeout, |s| s.results.contains_key(&crid))
            .await;
        self.inner
            .state
            .lock()
            .results
            .get(&crid)
            .cloned()
            .expect("result present")
    }

    /// Waits until `pred` holds; panics after `timeout`.
    pub async fn wait_until(&self, timeout: Duration, pred: impl Fn(&ClientState) -> bool) {
        if !self.wait_for(timeout, pred).await {
            panic!(
                "client condition not met in {timeout:?}; {}",
                self.describe()
            );
        }
    }

    /// Waits until `pred` holds; returns `false` if it still does not after `timeout`.
    pub async fn wait_for(&self, timeout: Duration, pred: impl Fn(&ClientState) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.inner.changed.notified();
            if pred(&self.inner.state.lock()) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return pred(&self.inner.state.lock());
            }
        }
    }

    /// One-line summary of the client state for failure messages.
    pub fn describe(&self) -> String {
        let st = self.inner.state.lock();
        let outbox: Vec<String> = st
            .outbox
            .iter()
            .map(|p| {
                format!(
                    "{} ({}, sent {}×)",
                    p.method,
                    p.crid,
                    st.attempts.get(&p.crid).copied().unwrap_or(0)
                )
            })
            .collect();
        format!(
            "connects={} resent={} outbox={outbox:?} cursors={:?}",
            st.connects, st.resent, st.cursors
        )
    }

    pub fn with_state<T>(&self, f: impl FnOnce(&ClientState) -> T) -> T {
        f(&self.inner.state.lock())
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// First reconnect delay, and the delay after a connection that worked (got through
/// `initialize`); failed attempts double it up to [`ClientConfig::backoff_max`].
const BACKOFF_MIN: Duration = Duration::from_millis(50);
/// Delay before resending a request that failed with a non-definitive error while the
/// connection stays up (protocol §1.3: such errors are not stored, the client resends later).
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Upper bound for TCP connect + WebSocket upgrade.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Watchdog used until `initialize` reports the server's `clientTimeoutMs`.
const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(45);

async fn run(inner: Arc<Inner>, mut wake: mpsc::UnboundedReceiver<()>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let connects_before = inner.state.lock().connects;
        match session(&inner, &mut wake).await {
            // The wake channel closed: nobody can queue work any more.
            Ok(()) => return,
            Err(e) => tracing::debug!(error = %e, "client session ended"),
        }
        if inner.state.lock().connects > connects_before {
            backoff = BACKOFF_MIN;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(inner.config.backoff_max);
    }
}

async fn connect(config: &ClientConfig) -> anyhow::Result<Ws> {
    let mut req = config.url.as_str().into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", config.token).parse()?);
    let (ws, _) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req)).await??;
    Ok(ws)
}

struct Conn {
    ws: Ws,
    next_id: i64,
    /// JSON-RPC id → what it was for.
    waiting: HashMap<i64, Waiting>,
    /// Watchdog: no inbound frame for this long ends the connection.
    timeout: Duration,
    last_frame: Instant,
}

enum Waiting {
    Init,
    Subscribe,
    Outbox(String),
}

impl Conn {
    fn deadline(&self) -> Instant {
        self.last_frame + self.timeout
    }

    async fn send(&mut self, method: &str, params: Value, what: Waiting) -> anyhow::Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        self.waiting.insert(id, what);
        let msg = RpcMessage::request(RequestId::Number(id), method, params);
        // A send can stall on a dead path (full socket buffers); the watchdog covers it too.
        match tokio::time::timeout_at(
            self.deadline(),
            self.ws.send(Message::text(serde_json::to_string(&msg)?)),
        )
        .await
        {
            Ok(result) => Ok(result?),
            Err(_) => anyhow::bail!("watchdog: sending stalled for {:?}", self.timeout),
        }
    }
}

async fn session(inner: &Arc<Inner>, wake: &mut mpsc::UnboundedReceiver<()>) -> anyhow::Result<()> {
    let ws = connect(&inner.config).await?;
    let epoch = inner.state.lock().epoch.clone();
    let mut conn = Conn {
        ws,
        next_id: 1,
        waiting: HashMap::new(),
        timeout: DEFAULT_CLIENT_TIMEOUT,
        last_frame: Instant::now(),
    };
    conn.send(
        "initialize",
        json!({"protocolVersion": 1, "client": {"name": "aas-testkit", "version": "1", "platform": "test"}, "lastKnownEpoch": epoch}),
        Waiting::Init,
    )
    .await?;
    let mut initialized = false;
    // Outbox entries sent on this connection that have no final answer yet.
    let mut in_flight: BTreeSet<String> = BTreeSet::new();
    // Entries that failed with a non-definitive error, with the time of their resend.
    let mut retry_at: BTreeMap<String, Instant> = BTreeMap::new();
    let mut subscribed: BTreeSet<String> = BTreeSet::new();
    loop {
        if initialized {
            let now = Instant::now();
            retry_at.retain(|crid, at| {
                let due = *at <= now;
                if due {
                    in_flight.remove(crid);
                }
                !due
            });
            // Subscribe new streams and send outbox entries not yet in flight on this connection.
            let (subs, pending) = {
                let mut st = inner.state.lock();
                let subs: Vec<(String, u64)> = st
                    .subscriptions
                    .iter()
                    .filter(|s| !subscribed.contains(*s))
                    .map(|s| (s.clone(), st.cursors.get(s).copied().unwrap_or(0)))
                    .collect();
                let pending: Vec<Pending> = st
                    .outbox
                    .iter()
                    .filter(|p| !in_flight.contains(&p.crid))
                    .cloned()
                    .collect();
                for p in &pending {
                    let attempts = st.attempts.entry(p.crid.clone()).or_insert(0);
                    *attempts += 1;
                    if *attempts > 1 {
                        st.resent += 1;
                    }
                }
                (subs, pending)
            };
            if !subs.is_empty() {
                let list: Vec<Value> = subs
                    .iter()
                    .map(|(s, a)| json!({"stream": s, "after": a}))
                    .collect();
                conn.send(
                    "subscribe",
                    json!({ "subscriptions": list }),
                    Waiting::Subscribe,
                )
                .await?;
                subscribed.extend(subs.into_iter().map(|(s, _)| s));
            }
            for p in pending {
                in_flight.insert(p.crid.clone());
                conn.send(&p.method, p.params.clone(), Waiting::Outbox(p.crid.clone()))
                    .await?;
            }
        }
        let next_retry = retry_at.values().min().copied();
        let frame = tokio::select! {
            f = tokio::time::timeout_at(conn.deadline(), conn.ws.next()) => match f {
                Err(_) => anyhow::bail!("watchdog: no frame within {:?}", conn.timeout),
                Ok(None) => anyhow::bail!("connection closed"),
                Ok(Some(Err(e))) => anyhow::bail!("connection error: {e}"),
                Ok(Some(Ok(m))) => {
                    conn.last_frame = Instant::now();
                    Some(m)
                }
            },
            w = wake.recv() => {
                if w.is_none() {
                    return Ok(());
                }
                None
            }
            _ = tokio::time::sleep_until(next_retry.unwrap_or_else(Instant::now)), if next_retry.is_some() => None,
        };
        let Some(Message::Text(text)) = frame else {
            continue;
        };
        let msg: RpcMessage = serde_json::from_str(&text)?;
        match msg.kind() {
            MessageKind::Response => {
                let Some(RequestId::Number(id)) = msg.id else {
                    continue;
                };
                match conn.waiting.remove(&id) {
                    Some(Waiting::Init) => {
                        let r = msg
                            .result
                            .ok_or_else(|| anyhow::anyhow!("initialize failed: {:?}", msg.error))?;
                        let epoch = r["server"]["epoch"].as_str().map(str::to_owned);
                        conn.timeout = r["policy"]["clientTimeoutMs"]
                            .as_u64()
                            .map_or(DEFAULT_CLIENT_TIMEOUT, Duration::from_millis);
                        let mut st = inner.state.lock();
                        if r["epochChanged"] == true {
                            st.cursors.clear();
                            st.events.clear();
                        }
                        st.epoch = epoch;
                        st.connects += 1;
                        initialized = true;
                        drop(st);
                        inner.changed.notify_waiters();
                    }
                    Some(Waiting::Subscribe) => {
                        // A failed subscription means a stream is not delivered: start over.
                        if let Some(e) = msg.error {
                            anyhow::bail!("subscribe failed: {e:?}");
                        }
                    }
                    Some(Waiting::Outbox(crid)) => {
                        let outcome = match (msg.result, msg.error) {
                            (Some(r), _) => Some(Ok(r)),
                            (_, Some(e)) if e.kind().is_some_and(|k| k.is_definitive()) => {
                                Some(Err(e))
                            }
                            (_, e) => {
                                tracing::debug!(%crid, error = ?e, "non-definitive error; resending later");
                                None
                            }
                        };
                        match outcome {
                            Some(outcome) => {
                                in_flight.remove(&crid);
                                let mut st = inner.state.lock();
                                st.outbox.retain(|p| p.crid != crid);
                                st.results.insert(crid, outcome);
                                drop(st);
                                inner.changed.notify_waiters();
                            }
                            None => {
                                retry_at.insert(crid, Instant::now() + RETRY_DELAY);
                            }
                        }
                    }
                    None => {}
                }
            }
            MessageKind::Notification if msg.method.as_deref() == Some("stream/batch") => {
                let batch: StreamBatch = serde_json::from_value(msg.params.unwrap_or_default())?;
                let mut st = inner.state.lock();
                let cursor = st.cursors.get(&batch.stream).copied().unwrap_or(0);
                let mut new_cursor = cursor;
                if batch.events.is_empty() {
                    // Nothing is left up to the head (protocol.md §2.1).
                    new_cursor = new_cursor.max(batch.head);
                }
                let mut applied = Vec::new();
                for ev in batch.events {
                    if ev.seq > new_cursor {
                        new_cursor = ev.seq;
                        applied.push(ev);
                    }
                }
                st.events
                    .entry(batch.stream.clone())
                    .or_default()
                    .extend(applied);
                st.cursors.insert(batch.stream, new_cursor);
                drop(st);
                inner.changed.notify_waiters();
            }
            _ => {}
        }
    }
}
